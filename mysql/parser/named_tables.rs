//! The tables a statement names.
//!
//! MySQL opens every table a statement names before it looks at anything
//! else, and answers 1146 for the first one that is not there. A frontend
//! that refuses the statement for another reason first can ask this which
//! tables it would have opened.

use sqlparser::ast::{
    Expr, FromTable, Ident, ObjectName, ObjectNamePart, Query, SelectItem, SetExpr,
    ShowCreateObject, ShowStatementIn, Spanned, Statement, TableFactor, TableObject,
    TableWithJoins, UpdateTableFromKind,
};
use sqlparser::tokenizer::Span;

use crate::{parse_one_statement, parse_optional_show_index, SessionSqlMode};

/// A table a statement names, with the database it was qualified by.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NamedTable {
    pub database: Option<String>,
    pub table: String,
}

/// The tables `sql` names, in the order written: those a `SELECT` reads, a
/// write writes, and `DESCRIBE`, `SHOW COLUMNS`, `SHOW INDEX` and
/// `SHOW CREATE TABLE` describe, leaving out a name a `WITH` clause defines.
/// `None` when the statement cannot be read.
pub fn tables_named_by(sql: &str, mode: SessionSqlMode) -> Option<Vec<NamedTable>> {
    // sqlparser reads no `SHOW INDEX`; the admin reader does.
    if let Ok(Some(show)) = parse_optional_show_index(sql, mode) {
        return Some(vec![NamedTable {
            database: show.database().map(|database| database.as_str().to_owned()),
            table: show.table().as_str().to_owned(),
        }]);
    }
    let statement = parse_one_statement(sql, mode).ok()?;
    let mut walk = Walk::default();
    walk.statement(&statement);
    Some(walk.found)
}

/// Where a statement writes a database before a table, and where it names a
/// result column after the text of an expression.
pub(crate) struct DatabaseQualifiers {
    /// Each table name written with its database, as the two names.
    pub(crate) tables: Vec<(Ident, Ident)>,
    /// Where each projected expression other than a column stands. MySQL names
    /// such a result column after the text as written, so leaving a database
    /// out of that text would rename the column.
    pub(crate) named_after_their_text: Vec<Span>,
}

/// The table names `statement` writes with a database, among the tables a
/// `SELECT`, `INSERT`, `UPDATE` or `DELETE` reads or writes.
pub(crate) fn database_qualifiers_in(statement: &Statement) -> DatabaseQualifiers {
    let mut walk = Walk::default();
    walk.statement(statement);
    DatabaseQualifiers {
        tables: walk.qualified,
        named_after_their_text: walk.named_after_their_text,
    }
}

#[derive(Default)]
struct Walk {
    /// Names `WITH` clauses have defined so far.
    defined: Vec<String>,
    found: Vec<NamedTable>,
    qualified: Vec<(Ident, Ident)>,
    named_after_their_text: Vec<Span>,
}

impl Walk {
    fn statement(&mut self, statement: &Statement) {
        match statement {
            Statement::Query(query) => self.query(query),
            Statement::Insert(insert) => {
                if let TableObject::TableName(name) = &insert.table {
                    self.name(name);
                }
                if let Some(source) = &insert.source {
                    self.query(source);
                }
            }
            Statement::Update(update) => {
                self.table_with_joins(&update.table);
                if let Some(
                    UpdateTableFromKind::BeforeSet(tables) | UpdateTableFromKind::AfterSet(tables),
                ) = &update.from
                {
                    tables.iter().for_each(|table| self.table_with_joins(table));
                }
                for assignment in &update.assignments {
                    self.expr(&assignment.value);
                }
                if let Some(selection) = &update.selection {
                    self.expr(selection);
                }
            }
            Statement::Delete(delete) => {
                let (FromTable::WithFromKeyword(tables) | FromTable::WithoutKeyword(tables)) =
                    &delete.from;
                tables.iter().for_each(|table| self.table_with_joins(table));
                if let Some(using) = &delete.using {
                    using.iter().for_each(|table| self.table_with_joins(table));
                }
                if let Some(selection) = &delete.selection {
                    self.expr(selection);
                }
            }
            Statement::ExplainTable { table_name, .. } => self.name(table_name),
            Statement::ShowColumns { show_options, .. } => {
                if let Some(ShowStatementIn {
                    parent_name: Some(name),
                    ..
                }) = &show_options.show_in
                {
                    self.name(name);
                }
            }
            Statement::ShowCreate {
                obj_type: ShowCreateObject::Table,
                obj_name,
            } => self.name(obj_name),
            _ => {}
        }
    }

    /// Measured on MySQL 8.4.11: a `WITH RECURSIVE` name is seen inside its
    /// own body, and a plain `WITH` name only after it — `WITH n AS (SELECT 1
    /// FROM n)` answers 1146 for `n`.
    fn query(&mut self, query: &Query) {
        if let Some(with) = &query.with {
            if with.recursive {
                self.defined.extend(
                    with.cte_tables
                        .iter()
                        .map(|cte| cte.alias.name.value.clone()),
                );
            }
            for cte in &with.cte_tables {
                self.query(&cte.query);
                self.defined.push(cte.alias.name.value.clone());
            }
        }
        self.set_expr(&query.body);
    }

    fn set_expr(&mut self, body: &SetExpr) {
        match body {
            SetExpr::Select(select) => {
                for item in &select.projection {
                    if let SelectItem::UnnamedExpr(expr) = item {
                        if !matches!(expr, Expr::Identifier(_) | Expr::CompoundIdentifier(_)) {
                            self.named_after_their_text.push(expr.span());
                        }
                    }
                    if let SelectItem::UnnamedExpr(expr) | SelectItem::ExprWithAlias { expr, .. } =
                        item
                    {
                        self.expr(expr);
                    }
                }
                select
                    .from
                    .iter()
                    .for_each(|table| self.table_with_joins(table));
                if let Some(selection) = &select.selection {
                    self.expr(selection);
                }
            }
            SetExpr::Query(query) => self.query(query),
            SetExpr::SetOperation { left, right, .. } => {
                self.set_expr(left);
                self.set_expr(right);
            }
            _ => {}
        }
    }

    fn table_with_joins(&mut self, table: &TableWithJoins) {
        self.table_factor(&table.relation);
        for join in &table.joins {
            self.table_factor(&join.relation);
        }
    }

    fn table_factor(&mut self, factor: &TableFactor) {
        match factor {
            TableFactor::Table { name, .. } => self.name(name),
            TableFactor::Derived { subquery, .. } => self.query(subquery),
            TableFactor::NestedJoin {
                table_with_joins, ..
            } => self.table_with_joins(table_with_joins),
            _ => {}
        }
    }

    /// Follows an expression into the subqueries in it.
    fn expr(&mut self, expr: &Expr) {
        match expr {
            Expr::Subquery(query)
            | Expr::Exists {
                subquery: query, ..
            } => self.query(query),
            Expr::InSubquery { expr, subquery, .. } => {
                self.expr(expr);
                self.query(subquery);
            }
            Expr::BinaryOp { left, right, .. } => {
                self.expr(left);
                self.expr(right);
            }
            Expr::UnaryOp { expr, .. } | Expr::Nested(expr) => self.expr(expr),
            Expr::InList { expr, list, .. } => {
                self.expr(expr);
                list.iter().for_each(|item| self.expr(item));
            }
            _ => {}
        }
    }

    fn name(&mut self, name: &ObjectName) {
        // A qualified name never reads a `WITH` name, so leaving its database
        // out would read something else.
        if let [ObjectNamePart::Identifier(database), ObjectNamePart::Identifier(table)] =
            name.0.as_slice()
        {
            if !self
                .defined
                .iter()
                .any(|defined| defined.eq_ignore_ascii_case(&table.value))
            {
                self.qualified.push((database.clone(), table.clone()));
            }
        }
        let parts = name
            .0
            .iter()
            .map(|part| match part {
                ObjectNamePart::Identifier(ident) => Some(ident.value.clone()),
                ObjectNamePart::Function(_) => None,
            })
            .collect::<Option<Vec<_>>>();
        match parts.as_deref() {
            // `DUAL` is MySQL's name for no table at all.
            Some([table]) if table.eq_ignore_ascii_case("DUAL") => {}
            Some([table]) => {
                if !self
                    .defined
                    .iter()
                    .any(|defined| defined.eq_ignore_ascii_case(table))
                {
                    self.push(None, table.clone());
                }
            }
            Some([database, table]) => self.push(Some(database.clone()), table.clone()),
            _ => {}
        }
    }

    fn push(&mut self, database: Option<String>, table: String) {
        self.found.push(NamedTable { database, table });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn named(sql: &str) -> Vec<(Option<&'static str>, String)> {
        tables_named_by(sql, SessionSqlMode::default())
            .unwrap_or_else(|| panic!("{sql} must be read"))
            .into_iter()
            .map(|named| {
                (
                    named.database.map(|database| {
                        assert_eq!(database, "probe");
                        "probe"
                    }),
                    named.table,
                )
            })
            .collect()
    }

    #[test]
    fn names_every_table_a_statement_opens_in_the_order_written() {
        for (sql, expected) in [
            ("SELECT id FROM missing", vec!["missing"]),
            (
                "SELECT * FROM posts p JOIN Missing m ON m.id = p.id",
                vec!["posts", "Missing"],
            ),
            ("INSERT INTO missing (a) VALUES (1)", vec!["missing"]),
            (
                "INSERT INTO posts SELECT * FROM missing",
                vec!["posts", "missing"],
            ),
            ("UPDATE missing SET a = 1 WHERE b = 2", vec!["missing"]),
            ("DELETE FROM missing WHERE a = 1", vec!["missing"]),
            ("DESCRIBE missing", vec!["missing"]),
            ("SHOW COLUMNS FROM missing", vec!["missing"]),
            ("SHOW FULL COLUMNS FROM missing", vec!["missing"]),
            ("SHOW CREATE TABLE missing", vec!["missing"]),
            ("SHOW INDEX FROM missing", vec!["missing"]),
            ("SHOW KEYS IN missing", vec!["missing"]),
            (
                "SELECT id FROM posts WHERE id IN (SELECT id FROM missing)",
                vec!["posts", "missing"],
            ),
            (
                "SELECT (SELECT 1 FROM first) FROM (SELECT 1 FROM second) d",
                vec!["first", "second"],
            ),
            ("SELECT 1 FROM a UNION SELECT 1 FROM b", vec!["a", "b"]),
            (
                "WITH c AS (SELECT id FROM posts) SELECT id FROM c",
                vec!["posts"],
            ),
            ("SELECT 1", vec![]),
            ("WITH n AS (SELECT 1 FROM n) SELECT * FROM n", vec!["n"]),
            (
                "WITH RECURSIVE n(x) AS (SELECT 1 UNION ALL SELECT x + 1 FROM n WHERE x < 3) SELECT x FROM n",
                vec![],
            ),
            ("SELECT 1 FROM DUAL", vec![]),
        ] {
            assert_eq!(
                named(sql)
                    .into_iter()
                    .map(|(_, table)| table)
                    .collect::<Vec<_>>(),
                expected,
                "{sql}"
            );
        }
        assert_eq!(
            named("SELECT id FROM probe.missing"),
            [(Some("probe"), "missing".to_owned())]
        );
    }

    #[test]
    fn a_statement_that_cannot_be_read_names_nothing() {
        assert_eq!(
            tables_named_by("SELECT FROM WHERE", SessionSqlMode::default()),
            None
        );
    }
}
