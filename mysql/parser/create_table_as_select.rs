use super::{
    parse_one_statement, parse_select, unsupported, MySqlTableName, ParseError, SessionSqlMode,
};
use sqlparser::ast::{Expr, ObjectNamePart, SelectItem, Statement};

/// One column a `CREATE TABLE ... AS SELECT` copies.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MySqlCreateTableAsSelectColumn {
    source: String,
    name: String,
}

impl MySqlCreateTableAsSelectColumn {
    /// Returns the column read from the source table.
    pub fn source(&self) -> &str {
        &self.source
    }

    /// Returns the name the new table gives it, which an alias renames.
    pub fn name(&self) -> &str {
        &self.name
    }
}

/// One checked `CREATE TABLE ... AS SELECT`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MySqlCreateTableAsSelect {
    table: MySqlTableName,
    source_table: String,
    /// The projected columns, or `None` for a lone `*`, which stands for every
    /// column of the source table in order.
    columns: Option<Vec<MySqlCreateTableAsSelectColumn>>,
    select_sql: String,
}

impl MySqlCreateTableAsSelect {
    /// Returns the table the statement creates.
    pub fn table(&self) -> &MySqlTableName {
        &self.table
    }

    /// Returns the table the `SELECT` reads.
    pub fn source_table(&self) -> &str {
        &self.source_table
    }

    /// Returns the projected columns, or `None` for a lone `*`.
    pub fn columns(&self) -> Option<&[MySqlCreateTableAsSelectColumn]> {
        self.columns.as_deref()
    }

    /// Returns the `SELECT` as MySQL, for the `INSERT` that fills the table.
    pub fn select_sql(&self) -> &str {
        &self.select_sql
    }
}

/// Reads one `CREATE TABLE <name> AS SELECT ...`.
///
/// Returns `None` for anything that is not one, so the ordinary path keeps
/// answering those. The `SELECT` has to be one this frontend already takes over
/// one base table, and every projected item has to be a plain column, with or
/// without an alias, or a lone `*`: MySQL works the new table's columns out
/// from the ones the `SELECT` answers, and a column is the only thing whose
/// type this can read out of the source table.
pub fn parse_optional_create_table_as_select(
    sql: &str,
    mode: SessionSqlMode,
) -> Result<Option<MySqlCreateTableAsSelect>, ParseError> {
    let Ok(Statement::CreateTable(table)) = parse_one_statement(sql, mode) else {
        return Ok(None);
    };
    let Some(query) = table.query.as_ref() else {
        return Ok(None);
    };
    if !table.columns.is_empty() || !table.constraints.is_empty() {
        return unsupported("CREATE TABLE AS SELECT with declared columns");
    }
    if table.if_not_exists || table.temporary || table.or_replace {
        return unsupported("CREATE TABLE AS SELECT option");
    }
    let [ObjectNamePart::Identifier(table_ident)] = table.name.0.as_slice() else {
        return unsupported("schema-qualified CREATE TABLE name");
    };
    let table_name =
        MySqlTableName::parse(&table_ident.value).map_err(|_| ParseError::Unsupported {
            feature: "CREATE TABLE name",
        })?;

    // The SELECT goes through the ordinary checked reader, so the shapes it
    // refuses are refused here too and the same text fills the table.
    let select_sql = query.to_string();
    let translated = parse_select(&select_sql, mode)?;
    let Some(source_table) = translated.source_table() else {
        return unsupported("CREATE TABLE AS SELECT over more than one table");
    };
    let source_table = source_table.to_owned();

    let sqlparser::ast::SetExpr::Select(select) = query.body.as_ref() else {
        return unsupported("CREATE TABLE AS SELECT body");
    };
    let columns = checked_projection(&select.projection)?;
    Ok(Some(MySqlCreateTableAsSelect {
        table: table_name,
        source_table,
        columns,
        select_sql,
    }))
}

/// Reads the projected columns, answering `None` for a lone `*`.
fn checked_projection(
    projection: &[SelectItem],
) -> Result<Option<Vec<MySqlCreateTableAsSelectColumn>>, ParseError> {
    if let [SelectItem::Wildcard(options)] = projection {
        if options.opt_exclude.is_some()
            || options.opt_except.is_some()
            || options.opt_rename.is_some()
            || options.opt_replace.is_some()
        {
            return unsupported("SELECT wildcard option");
        }
        return Ok(None);
    }
    let mut columns = Vec::with_capacity(projection.len());
    for item in projection {
        let (column, name) = match item {
            SelectItem::UnnamedExpr(Expr::Identifier(column)) => (column, column.value.clone()),
            SelectItem::ExprWithAlias {
                expr: Expr::Identifier(column),
                alias,
            } => (column, alias.value.clone()),
            _ => return unsupported("CREATE TABLE AS SELECT projection"),
        };
        columns.push(MySqlCreateTableAsSelectColumn {
            source: column.value.clone(),
            name,
        });
    }
    if columns.is_empty() {
        return unsupported("CREATE TABLE AS SELECT without projections");
    }
    Ok(Some(columns))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_table_as_select_reads_the_columns_it_copies() {
        let starred = parse_optional_create_table_as_select(
            "CREATE TABLE `C` AS SELECT * FROM src",
            SessionSqlMode::default(),
        )
        .unwrap()
        .unwrap();
        assert_eq!(starred.table().as_str(), "c");
        assert_eq!(starred.source_table(), "src");
        assert_eq!(starred.columns(), None);
        assert_eq!(starred.select_sql(), "SELECT * FROM src");

        let listed = parse_optional_create_table_as_select(
            "CREATE TABLE c AS SELECT id, name AS label FROM src WHERE id > 1",
            SessionSqlMode::default(),
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            listed
                .columns()
                .unwrap()
                .iter()
                .map(|column| (column.source(), column.name()))
                .collect::<Vec<_>>(),
            [("id", "id"), ("name", "label")]
        );
        assert_eq!(
            listed.select_sql(),
            "SELECT id, name AS label FROM src WHERE id > 1"
        );
    }

    #[test]
    fn create_table_as_select_leaves_every_other_statement_alone() {
        for sql in [
            "CREATE TABLE c (id INT)",
            "SELECT * FROM src",
            "INSERT INTO c (id) SELECT id FROM src",
        ] {
            assert_eq!(
                parse_optional_create_table_as_select(sql, SessionSqlMode::default()).unwrap(),
                None,
                "{sql}"
            );
        }
    }

    #[test]
    fn create_table_as_select_refuses_what_it_cannot_read_a_type_for() {
        for sql in [
            // An expression column is a rule of its own: measured on MySQL
            // 8.4.11, `a + 1` becomes `bigint NOT NULL DEFAULT '0'`.
            "CREATE TABLE c AS SELECT id + 1 AS s FROM src",
            "CREATE TABLE c AS SELECT COUNT(*) AS n FROM src",
            "CREATE TABLE c AS SELECT *, id FROM src",
            "CREATE TABLE c (id INT) AS SELECT id FROM src",
            "CREATE TEMPORARY TABLE c AS SELECT id FROM src",
            "CREATE TABLE IF NOT EXISTS c AS SELECT id FROM src",
            "CREATE TABLE db.c AS SELECT id FROM src",
            "CREATE TABLE c AS SELECT a.id FROM src AS a JOIN other AS b ON a.id = b.id",
        ] {
            assert!(
                parse_optional_create_table_as_select(sql, SessionSqlMode::default()).is_err(),
                "{sql}"
            );
        }
    }
}
