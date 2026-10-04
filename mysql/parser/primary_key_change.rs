//! `ALTER TABLE` statements that change a table's primary key, the columns it
//! is over, or the column the table counts its ids on.
//!
//! Each of them writes the table again: the key decides whether the engine
//! keeps it as the table's rowid or as an index of its own, and the counted
//! column is the rowid, so none of them can be made in place.

use sqlparser::ast::{
    AlterTableOperation, CharacterLength, ColumnDef, ColumnOption, ColumnOptionDef, CreateTable,
    DataType, Expr, Ident, IndexColumn, MySQLColumnPosition, ObjectNamePart, OrderByExpr,
    OrderByOptions, PrimaryKeyConstraint, Statement, TableConstraint, Value,
};

use super::{
    column_has_auto_increment, parse_one_statement, read_one_statement, render_table_written_again,
    restated_column, unsupported, MySqlTableRewrite, ParseError, SessionSqlMode,
};

/// What an `ALTER TABLE` that changes a table's key does to the table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MySqlKeyChange {
    /// The table written again as the statement leaves it.
    TableWrittenAgain(MySqlKeyRewrite),
    /// A column the table has not got: measured on MySQL 8.4.11, 1054.
    NoSuchColumn(String),
    /// A `CHANGE` onto a name the table already has: 1060.
    DuplicateColumn(String),
    /// `ADD PRIMARY KEY` over a column the table has not got: 1072.
    KeyColumnMissing(String),
    /// `DROP PRIMARY KEY` on a table without one: 1091.
    NoKeyToDrop,
    /// A key added beside the one the table has: 1068.
    SecondKey,
    /// A key column declared `NULL` or `DEFAULT NULL`: 1171.
    KeyColumnMayBeNull,
    /// A counted column that no key starts with: 1075.
    CountedColumnNotAKey(String),
}

/// The table one key-changing `ALTER TABLE` makes of another.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MySqlKeyRewrite {
    /// The `CREATE TABLE` the table becomes and the columns carried across.
    pub table: MySqlTableRewrite,
    /// The key's columns before the statement, under the names they had.
    pub old_key: Vec<String>,
    /// The key's columns after the statement, under the names they have then.
    pub new_key: Vec<String>,
    /// The column the table counted its ids on before, under its old name.
    pub counted_before: Option<String>,
    /// The column the table counts its ids on after, under its new name.
    pub counted_after: Option<String>,
    /// Each column whose type the statement changes into one a foreign key
    /// cannot pair with the old one, as its new name and its old one. A
    /// display width or the length of a word is not such a change.
    pub retyped: Vec<(String, String)>,
    /// Each column of words that stays one, by its new name. Measured on
    /// MySQL 8.4.11, a word too long for such a column's new width is 1265
    /// as the rows are copied, where one written from a number is 1406.
    pub words_kept_as_words: Vec<String>,
    /// Whether MySQL copies the rows to make the change rather than changing
    /// the table in place: a column takes another type, the table starts or
    /// stops counting, or its key goes with no key in its place.
    pub copies_the_rows: bool,
    /// Each column, by its new name, whose collation the statement changes.
    /// MySQL copies the rows for one an index is over.
    pub recollated: Vec<String>,
}

/// Reads an `ALTER TABLE` against the table it changes, and answers what it
/// does to the table's key.
///
/// The statement is one of these where it drops or adds the primary key, or
/// restates — `MODIFY`, `CHANGE` — a column the key is over or the table
/// counts on, or gives a column `AUTO_INCREMENT` or `PRIMARY KEY`. Each
/// clause may be any of those, or a restatement of any other column, and they
/// are applied in the order written. Answers `None` for every other
/// statement, which keeps its own path.
pub fn table_with_its_key_changed(
    stored_ddl: &str,
    alter_sql: &str,
    mode: SessionSqlMode,
) -> Result<Option<MySqlKeyChange>, ParseError> {
    let read_statement = read_one_statement(alter_sql, mode);
    let Ok(Statement::AlterTable(alter)) = read_statement.as_ref() else {
        return Ok(None);
    };
    if !alter.operations.iter().all(is_a_key_or_column_clause) {
        return Ok(None);
    }
    let Ok(Statement::CreateTable(stored)) = parse_one_statement(stored_ddl, mode) else {
        return Err(ParseError::ExpectedCreateTable);
    };
    let old_key = key_columns(&stored);
    let counted_before = stored
        .columns
        .iter()
        .find(|column| column_has_auto_increment(column))
        .map(|column| column.name.value.clone());
    // A column the table's own foreign key is over is held to the column it
    // names, which the engine's own `ALTER COLUMN` does not do, so a
    // restatement of one is written again as well.
    let held_columns = old_key
        .iter()
        .cloned()
        .chain(stored.constraints.iter().flat_map(|constraint| {
            match constraint {
                TableConstraint::ForeignKey(foreign_key) => foreign_key
                    .columns
                    .iter()
                    .map(|column| column.value.clone())
                    .collect::<Vec<_>>(),
                _ => Vec::new(),
            }
        }))
        .collect::<Vec<_>>();
    let touches_the_key = alter.operations.iter().any(|operation| match operation {
        AlterTableOperation::DropPrimaryKey { .. } | AlterTableOperation::AddConstraint { .. } => {
            true
        }
        AlterTableOperation::ModifyColumn {
            col_name, options, ..
        } => names_the_key_or_the_counter(col_name, options, &held_columns, &counted_before),
        AlterTableOperation::ChangeColumn {
            old_name, options, ..
        } => names_the_key_or_the_counter(old_name, options, &held_columns, &counted_before),
        // A key's column renamed alone is the engine's own rename; the column
        // a table counts on is named in how the table counts, so it is
        // written again.
        AlterTableOperation::RenameColumn {
            old_column_name, ..
        } => counted_before
            .as_ref()
            .is_some_and(|counted| counted.eq_ignore_ascii_case(&old_column_name.value)),
        _ => false,
    });
    if !touches_the_key {
        return Ok(None);
    }
    let mut table = stored.clone();
    let mut key = Some(old_key.clone()).filter(|key| !key.is_empty());
    take_the_key_out(&mut table);
    let mut carried_columns = table
        .columns
        .iter()
        .map(|column| (column.name.value.clone(), column.name.value.clone()))
        .collect::<Vec<_>>();
    let mut retyped = Vec::new();
    for operation in &alter.operations {
        match operation {
            AlterTableOperation::DropPrimaryKey {
                drop_behavior: None,
            } => {
                if key.take().is_none() {
                    return Ok(Some(MySqlKeyChange::NoKeyToDrop));
                }
            }
            AlterTableOperation::AddConstraint {
                constraint: TableConstraint::PrimaryKey(added),
                not_valid: false,
            } => {
                let columns = added_key_columns(added)?;
                if key.is_some() {
                    return Ok(Some(MySqlKeyChange::SecondKey));
                }
                if let Some(missing) = columns.iter().find(|named| {
                    !table
                        .columns
                        .iter()
                        .any(|column| column.name.value.eq_ignore_ascii_case(named))
                }) {
                    return Ok(Some(MySqlKeyChange::KeyColumnMissing(missing.clone())));
                }
                key = Some(columns);
            }
            AlterTableOperation::ModifyColumn {
                col_name,
                data_type,
                options,
                column_position,
            } => {
                if let Some(refused) = restate(
                    &mut table,
                    &mut carried_columns,
                    &mut key,
                    &mut retyped,
                    col_name,
                    restated_column(col_name, data_type, options),
                    column_position.as_ref(),
                )? {
                    return Ok(Some(refused));
                }
            }
            AlterTableOperation::ChangeColumn {
                old_name,
                new_name,
                data_type,
                options,
                column_position,
            } => {
                if let Some(refused) = restate(
                    &mut table,
                    &mut carried_columns,
                    &mut key,
                    &mut retyped,
                    old_name,
                    restated_column(new_name, data_type, options),
                    column_position.as_ref(),
                )? {
                    return Ok(Some(refused));
                }
            }
            AlterTableOperation::RenameColumn {
                old_column_name,
                new_column_name,
            } => {
                let Some(column) = table.columns.iter().find(|column| {
                    column
                        .name
                        .value
                        .eq_ignore_ascii_case(&old_column_name.value)
                }) else {
                    return Ok(Some(MySqlKeyChange::NoSuchColumn(
                        old_column_name.value.clone(),
                    )));
                };
                let mut renamed = column.clone();
                renamed.name = new_column_name.clone();
                if let Some(refused) = restate(
                    &mut table,
                    &mut carried_columns,
                    &mut key,
                    &mut retyped,
                    old_column_name,
                    renamed,
                    None,
                )? {
                    return Ok(Some(refused));
                }
            }
            _ => return unsupported("ALTER TABLE clause beside a change of the primary key"),
        }
    }
    let new_key = key.unwrap_or_default();
    for named in &new_key {
        let Some(column) = table
            .columns
            .iter_mut()
            .find(|column| column.name.value.eq_ignore_ascii_case(named))
        else {
            return Ok(Some(MySqlKeyChange::KeyColumnMissing(named.clone())));
        };
        if may_be_null(column) {
            return Ok(Some(MySqlKeyChange::KeyColumnMayBeNull));
        }
        // Measured on MySQL 8.4.11: a key column restated without `NOT NULL`
        // reads back `NOT NULL`, as one the key is added over does.
        if !column
            .options
            .iter()
            .any(|option| matches!(option.option, ColumnOption::NotNull))
        {
            column.options.insert(
                0,
                ColumnOptionDef {
                    name: None,
                    option: ColumnOption::NotNull,
                },
            );
        }
    }
    let counted_after = table
        .columns
        .iter()
        .find(|column| column_has_auto_increment(column))
        .map(|column| column.name.value.clone());
    if let Some(counted) = &counted_after {
        if !new_key
            .first()
            .is_some_and(|first| first.eq_ignore_ascii_case(counted))
        {
            return Ok(Some(MySqlKeyChange::CountedColumnNotAKey(counted.clone())));
        }
    }
    let words_kept_as_words = carried_columns
        .iter()
        .filter(|(new, old)| {
            let holds_words = |table: &CreateTable, name: &str| {
                table.columns.iter().any(|column| {
                    column.name.value.eq_ignore_ascii_case(name) && holds_words(&column.data_type)
                })
            };
            holds_words(&stored, old) && holds_words(&table, new)
        })
        .map(|(new, _)| new.clone())
        .collect();
    let starts_or_stops_counting = match (&counted_before, &counted_after) {
        (None, None) => false,
        (Some(before), Some(after)) => !carried_columns
            .iter()
            .any(|(new, old)| new.eq_ignore_ascii_case(after) && old.eq_ignore_ascii_case(before)),
        _ => true,
    };
    let copies_the_rows = starts_or_stops_counting
        || (!old_key.is_empty() && new_key.is_empty())
        || retyped_in_a_copy(&stored, &table, &carried_columns);
    let recollated = recollated_columns(&stored, &table, &carried_columns);
    put_the_key_back(&mut table, &new_key);
    Ok(Some(MySqlKeyChange::TableWrittenAgain(MySqlKeyRewrite {
        table: MySqlTableRewrite {
            create_sql: render_table_written_again(&table, mode)?,
            carried_columns,
        },
        old_key,
        new_key,
        counted_before,
        counted_after,
        retyped,
        words_kept_as_words,
        copies_the_rows,
        recollated,
    })))
}

/// The table one `ALTER TABLE t ENGINE=InnoDB`, `FORCE` or `OPTIMIZE TABLE`
/// writes again: the one it is, every column carried across.
pub fn table_as_it_stands(
    stored_ddl: &str,
    mode: SessionSqlMode,
) -> Result<MySqlKeyRewrite, ParseError> {
    let Ok(Statement::CreateTable(table)) = parse_one_statement(stored_ddl, mode) else {
        return Err(ParseError::ExpectedCreateTable);
    };
    let key = key_columns(&table);
    let counted = table
        .columns
        .iter()
        .find(|column| column_has_auto_increment(column))
        .map(|column| column.name.value.clone());
    Ok(MySqlKeyRewrite {
        table: MySqlTableRewrite {
            create_sql: render_table_written_again(&table, mode)?,
            carried_columns: table
                .columns
                .iter()
                .map(|column| (column.name.value.clone(), column.name.value.clone()))
                .collect(),
        },
        old_key: key.clone(),
        new_key: key,
        counted_before: counted.clone(),
        counted_after: counted,
        retyped: Vec::new(),
        words_kept_as_words: Vec::new(),
        copies_the_rows: false,
        recollated: Vec::new(),
    })
}

fn is_a_key_or_column_clause(operation: &AlterTableOperation) -> bool {
    matches!(
        operation,
        AlterTableOperation::DropPrimaryKey { .. }
            | AlterTableOperation::AddConstraint {
                constraint: TableConstraint::PrimaryKey(_),
                ..
            }
            | AlterTableOperation::ModifyColumn { .. }
            | AlterTableOperation::ChangeColumn { .. }
            | AlterTableOperation::RenameColumn { .. }
    )
}

fn names_the_key_or_the_counter(
    column: &Ident,
    options: &[ColumnOption],
    key: &[String],
    counted: &Option<String>,
) -> bool {
    key.iter()
        .any(|named| named.eq_ignore_ascii_case(&column.value))
        || counted
            .as_ref()
            .is_some_and(|named| named.eq_ignore_ascii_case(&column.value))
        || options.iter().any(|option| {
            matches!(option, ColumnOption::PrimaryKey(_))
                || matches!(option, ColumnOption::DialectSpecific(tokens)
                    if super::is_auto_increment_tokens(tokens))
        })
}

/// The columns a table's primary key is over, in key order: the one column
/// declaring `PRIMARY KEY`, or the columns a `PRIMARY KEY (...)` clause names.
fn key_columns(table: &CreateTable) -> Vec<String> {
    if let Some(column) = table.columns.iter().find(|column| {
        column
            .options
            .iter()
            .any(|option| matches!(option.option, ColumnOption::PrimaryKey(_)))
    }) {
        return vec![column.name.value.clone()];
    }
    table
        .constraints
        .iter()
        .find_map(|constraint| match constraint {
            TableConstraint::PrimaryKey(key) => Some(
                key.columns
                    .iter()
                    .filter_map(|part| match &part.column.expr {
                        Expr::Identifier(named) => Some(named.value.clone()),
                        _ => None,
                    })
                    .collect(),
            ),
            _ => None,
        })
        .unwrap_or_default()
}

/// Takes the key off the columns and out of the clauses, leaving it to be
/// written back once the statement's clauses have run.
fn take_the_key_out(table: &mut CreateTable) {
    for column in &mut table.columns {
        column
            .options
            .retain(|option| !matches!(option.option, ColumnOption::PrimaryKey(_)));
    }
    table
        .constraints
        .retain(|constraint| !matches!(constraint, TableConstraint::PrimaryKey(_)));
}

/// The columns an added `PRIMARY KEY (...)` names.
///
/// Measured on MySQL 8.4.11, a constraint name and an index name on the key
/// are dropped, the key always being called `PRIMARY`; an index type, an
/// index option, a part of a column and a descending column are refused.
fn added_key_columns(added: &PrimaryKeyConstraint) -> Result<Vec<String>, ParseError> {
    if added.index_type.is_some()
        || !added.index_options.is_empty()
        || added.characteristics.is_some()
    {
        return unsupported("PRIMARY KEY index attribute");
    }
    added
        .columns
        .iter()
        .map(|part| match part {
            IndexColumn {
                column:
                    OrderByExpr {
                        expr: Expr::Identifier(named),
                        options:
                            OrderByOptions {
                                asc: None | Some(true),
                                nulls_first: None,
                            },
                        with_fill: None,
                    },
                operator_class: None,
            } => Ok(named.value.clone()),
            _ => unsupported("PRIMARY KEY part other than a whole column"),
        })
        .collect()
}

/// Applies one `MODIFY` or `CHANGE`: the column named `old` is replaced by
/// `restated`, moved where the clause says, and the key follows it.
fn restate(
    table: &mut CreateTable,
    carried_columns: &mut Vec<(String, String)>,
    key: &mut Option<Vec<String>>,
    retyped: &mut Vec<(String, String)>,
    old: &Ident,
    mut restated: ColumnDef,
    position: Option<&MySQLColumnPosition>,
) -> Result<Option<MySqlKeyChange>, ParseError> {
    let Some(at) = table
        .columns
        .iter()
        .position(|column| column.name.value.eq_ignore_ascii_case(&old.value))
    else {
        return Ok(Some(MySqlKeyChange::NoSuchColumn(old.value.clone())));
    };
    let new_name = restated.name.value.clone();
    if !new_name.eq_ignore_ascii_case(&old.value)
        && table
            .columns
            .iter()
            .any(|column| column.name.value.eq_ignore_ascii_case(&new_name))
    {
        return Ok(Some(MySqlKeyChange::DuplicateColumn(new_name)));
    }
    if table.columns[at].name.value != new_name {
        let old_name = table.columns[at].name.value.clone();
        rename_in_the_foreign_keys(table, &old_name, &new_name);
    }
    let declares_the_key = restated
        .options
        .iter()
        .any(|option| matches!(option.option, ColumnOption::PrimaryKey(_)));
    if declares_the_key {
        if restated.options.iter().any(|option| {
            matches!(&option.option, ColumnOption::PrimaryKey(declared)
                if !super::is_plain_inline_primary_key(&ColumnOption::PrimaryKey(declared.clone())))
        }) {
            return unsupported("PRIMARY KEY attribute");
        }
        if key.is_some() {
            return Ok(Some(MySqlKeyChange::SecondKey));
        }
        restated
            .options
            .retain(|option| !matches!(option.option, ColumnOption::PrimaryKey(_)));
        *key = Some(vec![new_name.clone()]);
    }
    // Measured on MySQL 8.4.11, every stored `DECIMAL` is written again in
    // its column's new form, a rule the copy here does not keep.
    let before = &table.columns[at].data_type;
    if before != &restated.data_type
        && (matches!(before, DataType::Decimal(_) | DataType::DecimalUnsigned(_))
            || matches!(
                restated.data_type,
                DataType::Decimal(_) | DataType::DecimalUnsigned(_)
            ))
    {
        return unsupported("changing a DECIMAL column's size, sign or type beside the key");
    }
    let old_name = carried_columns[at].1.clone();
    if !same_type_for_a_foreign_key(&table.columns[at].data_type, &restated.data_type) {
        retyped.push((new_name.clone(), old_name));
    }
    if let Some(key) = key.as_mut() {
        for named in key.iter_mut() {
            if named.eq_ignore_ascii_case(&old.value) {
                named.clone_from(&new_name);
            }
        }
    }
    let Some(position) = position else {
        table.columns[at] = restated;
        carried_columns[at].0 = new_name;
        return Ok(None);
    };
    // The column being moved leaves the list before the place is counted,
    // which is how MySQL counts one: measured, `MODIFY n ... AFTER s` over
    // (id, n, s) leaves (id, s, n).
    table.columns.remove(at);
    let mut carried = carried_columns.remove(at);
    carried.0 = new_name;
    let to = match position {
        MySQLColumnPosition::First => 0,
        MySQLColumnPosition::After(named) => {
            let Some(after) = table
                .columns
                .iter()
                .position(|column| column.name.value.eq_ignore_ascii_case(&named.value))
            else {
                return Ok(Some(MySqlKeyChange::NoSuchColumn(named.value.clone())));
            };
            after + 1
        }
    };
    table.columns.insert(to, restated);
    carried_columns.insert(to, carried);
    Ok(None)
}

/// Whether a foreign key over a column of type `before` still pairs with the
/// same column of type `after`. Measured on MySQL 8.4.11, an `INT` key made a
/// `BIGINT` while another table's key names it is 3780; a display width is
/// no type of its own, and the length of a word need not match.
fn same_type_for_a_foreign_key(before: &DataType, after: &DataType) -> bool {
    match (before, after) {
        (DataType::Decimal(before), DataType::Decimal(after)) => before == after,
        (before, after) => std::mem::discriminant(before) == std::mem::discriminant(after),
    }
}

fn holds_words(data_type: &DataType) -> bool {
    matches!(
        data_type,
        DataType::Char(_)
            | DataType::Varchar(_)
            | DataType::Text
            | DataType::TinyText
            | DataType::MediumText
            | DataType::LongText
    )
}

fn may_be_null(column: &ColumnDef) -> bool {
    column.options.iter().any(|option| {
        matches!(option.option, ColumnOption::Null)
            || matches!(&option.option, ColumnOption::Default(Expr::Value(value))
                if matches!(value.value, Value::Null))
    })
}

/// Writes the key back: on its one column, the way every stored table with a
/// single-column key is written, or as a clause over several.
fn put_the_key_back(table: &mut CreateTable, key: &[String]) {
    match key {
        [] => {}
        [named] => {
            let column = table
                .columns
                .iter_mut()
                .find(|column| column.name.value.eq_ignore_ascii_case(named))
                .expect("the key's column was found above");
            column.options.push(ColumnOptionDef {
                name: None,
                option: ColumnOption::PrimaryKey(PrimaryKeyConstraint {
                    name: None,
                    index_name: None,
                    index_type: None,
                    columns: Vec::new(),
                    index_options: Vec::new(),
                    characteristics: None,
                }),
            });
        }
        columns => table.constraints.insert(
            0,
            TableConstraint::PrimaryKey(PrimaryKeyConstraint {
                name: None,
                index_name: None,
                index_type: None,
                columns: columns
                    .iter()
                    .map(|named| IndexColumn {
                        column: OrderByExpr {
                            expr: Expr::Identifier(Ident::with_quote('`', named.clone())),
                            options: OrderByOptions {
                                asc: None,
                                nulls_first: None,
                            },
                            with_fill: None,
                        },
                        operator_class: None,
                    })
                    .collect(),
                index_options: Vec::new(),
                characteristics: None,
            }),
        ),
    }
}

/// Renames a column in the table's own foreign keys: among the columns a key
/// is over, and among the columns it names where it names the table itself.
/// Measured on MySQL 8.4.11, `CHANGE pid parent INT` leaves `FOREIGN KEY
/// (parent)`, and `CHANGE id sid INT NOT NULL` on a table whose key names its
/// own `id` leaves `REFERENCES s (sid)`.
fn rename_in_the_foreign_keys(table: &mut CreateTable, old: &str, new: &str) {
    let own_name = match table.name.0.as_slice() {
        [.., ObjectNamePart::Identifier(name)] => name.value.clone(),
        _ => String::new(),
    };
    let rename = |columns: &mut Vec<Ident>| {
        for column in columns.iter_mut() {
            if column.value.eq_ignore_ascii_case(old) {
                new.clone_into(&mut column.value);
            }
        }
    };
    for constraint in &mut table.constraints {
        let TableConstraint::ForeignKey(foreign_key) = constraint else {
            continue;
        };
        rename(&mut foreign_key.columns);
        let names_itself = matches!(foreign_key.foreign_table.0.as_slice(),
            [ObjectNamePart::Identifier(parent)] if parent.value.eq_ignore_ascii_case(&own_name));
        if names_itself {
            rename(&mut foreign_key.referred_columns);
        }
    }
}

/// Whether a column carried across takes a type MySQL copies the rows for.
fn retyped_in_a_copy(
    stored: &CreateTable,
    written: &CreateTable,
    carried_columns: &[(String, String)],
) -> bool {
    carried_columns.iter().any(|(new, old)| {
        match (column_named(stored, old), column_named(written, new)) {
            (Some(before), Some(after)) => !kept_in_place(before, after),
            _ => false,
        }
    })
}

/// Whether MySQL changes a column from `before` to `after` without copying
/// the rows. Measured on MySQL 8.4.11, beside the rename of a column a
/// foreign key names: a display width (`INT` to `INT(11)`) is taken, and so
/// is a longer `VARCHAR` whose length still takes as many bytes to write —
/// at most 255, or more than 255, counting the most bytes one character of
/// its character set takes, so a `utf8mb4` column grows from 10 to 20 or from
/// 70 to 100 characters and a `utf8mb3` one from 70 to 80 — while `INT
/// UNSIGNED`, `BIGINT`, a `VARCHAR` growing from 10 to 70 characters, a
/// `utf8mb3` one from 80 to 90, a shorter `VARCHAR`, `CHAR`, another
/// `DECIMAL`, `DATETIME(3)` and `MEDIUMTEXT` are 1846.
fn kept_in_place(before: &ColumnDef, after: &ColumnDef) -> bool {
    let whole_number = |data_type: &DataType| {
        matches!(
            data_type,
            DataType::TinyInt(_)
                | DataType::SmallInt(_)
                | DataType::MediumInt(_)
                | DataType::Int(_)
                | DataType::Integer(_)
                | DataType::BigInt(_)
                | DataType::TinyIntUnsigned(_)
                | DataType::SmallIntUnsigned(_)
                | DataType::MediumIntUnsigned(_)
                | DataType::IntUnsigned(_)
                | DataType::IntegerUnsigned(_)
                | DataType::BigIntUnsigned(_)
        )
    };
    match (&before.data_type, &after.data_type) {
        (before, after) if before == after => true,
        (before, after) if whole_number(before) && whole_number(after) => {
            let canonical = |data_type: &DataType| match data_type {
                DataType::Integer(_) => std::mem::discriminant(&DataType::Int(None)),
                DataType::IntegerUnsigned(_) => {
                    std::mem::discriminant(&DataType::IntUnsigned(None))
                }
                other => std::mem::discriminant(other),
            };
            canonical(before) == canonical(after)
        }
        (DataType::Varchar(Some(shorter)), DataType::Varchar(Some(longer))) => {
            let widest = |column: &ColumnDef| {
                super::widest_character_of_collation(
                    collation_of(column).as_deref().unwrap_or("utf8mb4"),
                )
            };
            let (widest_before, widest_after) = (widest(before), widest(after));
            match (character_count(shorter), character_count(longer)) {
                (Some(shorter), Some(longer)) if widest_before == widest_after => {
                    const ONE_LENGTH_BYTE: u64 = 255;
                    longer >= shorter
                        && (shorter * widest_before <= ONE_LENGTH_BYTE)
                            == (longer * widest_after <= ONE_LENGTH_BYTE)
                }
                _ => false,
            }
        }
        _ => false,
    }
}

fn character_count(length: &CharacterLength) -> Option<u64> {
    match length {
        CharacterLength::IntegerLength { length, unit: None } => Some(*length),
        _ => None,
    }
}

/// The columns carried across, by their new names, whose collation the
/// statement changes.
fn recollated_columns(
    stored: &CreateTable,
    written: &CreateTable,
    carried_columns: &[(String, String)],
) -> Vec<String> {
    carried_columns
        .iter()
        .filter(
            |(new, old)| match (column_named(stored, old), column_named(written, new)) {
                (Some(before), Some(after)) => collation_of(before) != collation_of(after),
                _ => false,
            },
        )
        .map(|(new, _)| new.clone())
        .collect()
}

fn collation_of(column: &ColumnDef) -> Option<String> {
    column
        .options
        .iter()
        .find_map(|option| match &option.option {
            ColumnOption::Collation(name) => Some(name.to_string().to_ascii_lowercase()),
            _ => None,
        })
}

fn column_named<'a>(table: &'a CreateTable, name: &str) -> Option<&'a ColumnDef> {
    table
        .columns
        .iter()
        .find(|column| column.name.value.eq_ignore_ascii_case(name))
}
