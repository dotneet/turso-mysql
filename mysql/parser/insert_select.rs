use super::{parse_one_statement, unsupported, MySqlTableName, ParseError, SessionSqlMode};
use sqlparser::ast::{ObjectNamePart, SetExpr, Statement};

/// One `INSERT INTO t <SELECT>` written with no column list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MySqlInsertSelectWithoutColumns {
    table: MySqlTableName,
    replaces: bool,
    select_sql: String,
}

impl MySqlInsertSelectWithoutColumns {
    /// Returns the table the statement writes.
    pub fn table(&self) -> &MySqlTableName {
        &self.table
    }

    /// Reports whether the statement was written as a `REPLACE`.
    pub const fn replaces(&self) -> bool {
        self.replaces
    }

    /// Returns the `SELECT` as MySQL, for the statement written with the
    /// column list the table gives it.
    pub fn select_sql(&self) -> &str {
        &self.select_sql
    }
}

/// Reads an `INSERT INTO t <SELECT>` that names no columns.
///
/// MySQL takes every column of the table, in order — measured on 8.4.11,
/// `INSERT INTO dst SELECT * FROM src` copies all three columns, and a `SELECT`
/// answering a different number of them answers 1136. The caller writes the
/// column list out and runs the ordinary statement, which is what the form
/// means.
///
/// Returns `None` for anything else, so every other `INSERT` keeps its own
/// path. A statement carrying `IGNORE` or an upsert clause is refused, as those
/// forms are wherever they are written.
pub fn parse_optional_insert_select_without_columns(
    sql: &str,
    mode: SessionSqlMode,
) -> Result<Option<MySqlInsertSelectWithoutColumns>, ParseError> {
    let Ok(Statement::Insert(insert)) = parse_one_statement(sql, mode) else {
        return Ok(None);
    };
    if !insert.columns.is_empty() {
        return Ok(None);
    }
    let Some(source) = insert.source.as_deref() else {
        return Ok(None);
    };
    if matches!(source.body.as_ref(), SetExpr::Values(_)) {
        return Ok(None);
    }
    if insert.ignore || insert.on.is_some() || insert.partitioned.is_some() {
        return unsupported("INSERT SELECT option");
    }
    let sqlparser::ast::TableObject::TableName(name) = &insert.table else {
        return unsupported("INSERT target");
    };
    let [ObjectNamePart::Identifier(table)] = name.0.as_slice() else {
        return unsupported("schema-qualified INSERT target");
    };
    let table = MySqlTableName::parse(&table.value).map_err(|_| ParseError::Unsupported {
        feature: "INSERT target name",
    })?;
    Ok(Some(MySqlInsertSelectWithoutColumns {
        table,
        replaces: insert.replace_into,
        select_sql: source.to_string(),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn insert_select_without_columns_reads_its_table_and_select() {
        let checked = parse_optional_insert_select_without_columns(
            "INSERT INTO `Dst` SELECT * FROM src WHERE id > 1",
            SessionSqlMode::default(),
        )
        .unwrap()
        .unwrap();
        assert_eq!(checked.table().as_str(), "dst");
        assert!(!checked.replaces());
        assert_eq!(checked.select_sql(), "SELECT * FROM src WHERE id > 1");

        let replaced = parse_optional_insert_select_without_columns(
            "REPLACE INTO dst SELECT id FROM src",
            SessionSqlMode::default(),
        )
        .unwrap()
        .unwrap();
        assert!(replaced.replaces());
    }

    #[test]
    fn insert_select_without_columns_leaves_every_other_insert_alone() {
        for sql in [
            "INSERT INTO dst (id) SELECT id FROM src",
            "INSERT INTO dst (id) VALUES (1)",
            "INSERT INTO dst VALUES (1)",
            "SELECT 1",
            "UPDATE dst SET id = 1",
        ] {
            assert_eq!(
                parse_optional_insert_select_without_columns(sql, SessionSqlMode::default())
                    .unwrap(),
                None,
                "{sql}"
            );
        }
    }

    #[test]
    fn insert_select_without_columns_refuses_the_forms_it_cannot_write_out() {
        for sql in [
            "INSERT IGNORE INTO dst SELECT id FROM src",
            "INSERT INTO dst SELECT id FROM src ON DUPLICATE KEY UPDATE id = 1",
            "INSERT INTO db.dst SELECT id FROM src",
        ] {
            assert!(
                parse_optional_insert_select_without_columns(sql, SessionSqlMode::default())
                    .is_err(),
                "{sql}"
            );
        }
    }
}
