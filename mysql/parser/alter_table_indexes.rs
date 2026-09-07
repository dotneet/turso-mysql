use super::{
    inline_index_columns, parse_one_statement, unsupported, MySqlTableName, ParseError,
    SessionSqlMode,
};
use sqlparser::ast::{AlterTableOperation, ObjectNamePart, Statement, TableConstraint};

/// The `ALTER TABLE` operations that add or remove one index.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MySqlAlterTableIndexOperation {
    Add {
        name: String,
        unique: bool,
        columns: Vec<String>,
    },
    Drop {
        name: String,
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
/// An unnamed key is refused: MySQL names one after its first column and
/// disambiguates with `_2` and `_3`, which is a rule this does not implement.
/// So are the index options MySQL takes here, since none of them could be
/// printed back, and a statement that mixes index and column operations, which
/// would have to apply two kinds of change together.
pub fn parse_optional_alter_table_indexes(
    sql: &str,
    mode: SessionSqlMode,
) -> Result<Option<MySqlAlterTableIndexes>, ParseError> {
    let Ok(Statement::AlterTable(alter)) = parse_one_statement(sql, mode) else {
        return Ok(None);
    };
    if !alter.operations.iter().any(is_index_operation) {
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

fn is_index_operation(operation: &AlterTableOperation) -> bool {
    matches!(
        operation,
        AlterTableOperation::DropIndex { .. }
            | AlterTableOperation::AddConstraint {
                constraint: TableConstraint::Index(_) | TableConstraint::Unique(_),
                ..
            }
    )
}

fn checked_index_operation(
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
            let Some(name) = index.name.as_ref() else {
                return unsupported("unnamed ADD INDEX");
            };
            Ok(MySqlAlterTableIndexOperation::Add {
                name: checked_index_name(&name.value)?,
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
            let Some(name) = unique.index_name.as_ref().or(unique.name.as_ref()) else {
                return unsupported("unnamed ADD UNIQUE");
            };
            Ok(MySqlAlterTableIndexOperation::Add {
                name: checked_index_name(&name.value)?,
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
    let name = MySqlTableName::parse(name)
        .map_err(|_| ParseError::Unsupported {
            feature: "index name",
        })?
        .as_str()
        .to_owned();
    // MySQL calls the primary key's index `PRIMARY`, and adding or dropping a
    // primary key is a different operation than adding or dropping an index.
    if name == "primary" {
        return unsupported("index named PRIMARY");
    }
    Ok(name)
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
                    name: "idx_c".to_owned(),
                    unique: false,
                    columns: vec!["c".to_owned()],
                },
                MySqlAlterTableIndexOperation::Add {
                    name: "idx_d".to_owned(),
                    unique: false,
                    columns: vec!["d".to_owned()],
                },
            ]
        );

        let unique = parsed("ALTER TABLE records ADD UNIQUE INDEX uniq_cd (c, d)");
        assert_eq!(
            unique.operations(),
            [MySqlAlterTableIndexOperation::Add {
                name: "uniq_cd".to_owned(),
                unique: true,
                columns: vec!["c".to_owned(), "d".to_owned()],
            }]
        );

        let dropped = parsed("ALTER TABLE records DROP INDEX idx_c");
        assert_eq!(
            dropped.operations(),
            [MySqlAlterTableIndexOperation::Drop {
                name: "idx_c".to_owned(),
            }]
        );
        // `DROP KEY` is MySQL's other spelling for the same thing, and
        // `sqlparser` reads only `DROP INDEX`, so it is left to the ordinary
        // path to refuse.
        assert_eq!(
            parse_optional_alter_table_indexes(
                "ALTER TABLE records DROP KEY idx_c",
                SessionSqlMode::default()
            )
            .unwrap(),
            None
        );
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
            // MySQL names one after its first column and disambiguates with
            // `_2` and `_3`, a rule this does not implement.
            "ALTER TABLE records ADD INDEX (c)",
            "ALTER TABLE records ADD UNIQUE (c)",
            "ALTER TABLE records ADD INDEX idx_c USING BTREE (c)",
            "ALTER TABLE records ADD INDEX idx_c (c(4))",
            // Two kinds of change would have to apply together.
            "ALTER TABLE records ADD COLUMN c INT, ADD INDEX idx_c (c)",
            "ALTER TABLE db.records ADD INDEX idx_c (c)",
        ] {
            assert!(
                parse_optional_alter_table_indexes(sql, SessionSqlMode::default()).is_err(),
                "{sql}"
            );
        }
    }
}
