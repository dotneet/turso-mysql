//! The column a statement over one table names that the table does not have.
//!
//! MySQL resolves every name a statement uses before it runs it, and answers
//! 1054 for the first one that names no column, saying in which clause. A
//! frontend that refuses such a statement for another reason first can ask
//! this which name MySQL would have answered 1054 for.

use sqlparser::ast::{
    AssignmentTarget, Expr, FromTable, FunctionArg, FunctionArgExpr, FunctionArguments, Ident,
    ObjectName, ObjectNamePart, Query, SelectItem, SetExpr, Statement, TableFactor, TableObject,
    TableWithJoins,
};

use crate::{parse_one_statement, SessionSqlMode};

/// A name a statement used that is no column of its table, as written, and
/// the clause MySQL names for it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnknownColumn {
    pub written: String,
    pub clause: &'static str,
}

/// The first name `sql` uses that is no column of the one table it reads or
/// writes, in the order MySQL checks them.
///
/// Measured on MySQL 8.4.11: a `SELECT` checks its select list (`field list`)
/// before its `WHERE` (`where clause`); an `UPDATE` checks its `WHERE` before
/// its `SET`, whose targets and values are the `field list`; a `DELETE` checks
/// its `WHERE`; an `INSERT` checks its column list and then its `VALUES`, both
/// the `field list`. A name qualified by anything but the table or its alias
/// is unknown as written, `p.id`. `columns` answers a table's columns by name.
/// `None` when the statement reads more than one table, uses a shape this
/// does not follow, or names only columns the table has.
pub fn unknown_column_named_by(
    sql: &str,
    mode: SessionSqlMode,
    columns: &mut dyn FnMut(&str) -> Option<Vec<String>>,
) -> Option<UnknownColumn> {
    let statement = parse_one_statement(sql, mode).ok()?;
    let (table, alias, clauses) = clauses_in_checking_order(&statement)?;
    let scope = Scope {
        columns: columns(&table)?,
        table,
        alias,
        ansi_quotes: mode.ansi_quotes,
    };
    for (clause, names) in clauses {
        for name in names {
            match name {
                Checked::Expr(expr) => {
                    if let Some(unknown) = scope.expr(expr, clause).ok()? {
                        return Some(unknown);
                    }
                }
                Checked::Column(name) => {
                    if let Some(unknown) = scope.object_name(name, clause)? {
                        return Some(unknown);
                    }
                }
            }
        }
    }
    None
}

/// One thing a clause names: an expression, or a bare column name.
enum Checked<'a> {
    Expr(&'a Expr),
    Column(&'a ObjectName),
}

type Clauses<'a> = Vec<(&'static str, Vec<Checked<'a>>)>;

/// The one table a statement names, its alias, and its clauses in the order
/// MySQL checks them.
fn clauses_in_checking_order(
    statement: &Statement,
) -> Option<(String, Option<String>, Clauses<'_>)> {
    match statement {
        Statement::Query(query) => {
            let select = single_select(query)?;
            let [from] = select.from.as_slice() else {
                return None;
            };
            let (table, alias) = single_table(from)?;
            let mut field_list = Vec::new();
            for item in &select.projection {
                match item {
                    SelectItem::UnnamedExpr(expr) | SelectItem::ExprWithAlias { expr, .. } => {
                        field_list.push(Checked::Expr(expr));
                    }
                    SelectItem::Wildcard(_) => {}
                    SelectItem::QualifiedWildcard(..) | SelectItem::ExprWithAliases { .. } => {
                        return None
                    }
                }
            }
            let mut clauses = vec![("field list", field_list)];
            if let Some(selection) = &select.selection {
                clauses.push(("where clause", vec![Checked::Expr(selection)]));
            }
            Some((table, alias, clauses))
        }
        Statement::Update(update) => {
            if update.from.is_some() {
                return None;
            }
            let (table, alias) = single_table(&update.table)?;
            let mut clauses = Vec::new();
            if let Some(selection) = &update.selection {
                clauses.push(("where clause", vec![Checked::Expr(selection)]));
            }
            let mut field_list = Vec::new();
            for assignment in &update.assignments {
                let AssignmentTarget::ColumnName(target) = &assignment.target else {
                    return None;
                };
                field_list.push(Checked::Column(target));
                field_list.push(Checked::Expr(&assignment.value));
            }
            clauses.push(("field list", field_list));
            Some((table, alias, clauses))
        }
        Statement::Delete(delete) => {
            if !delete.tables.is_empty() || delete.using.is_some() {
                return None;
            }
            let (FromTable::WithFromKeyword(from) | FromTable::WithoutKeyword(from)) = &delete.from;
            let [from] = from.as_slice() else {
                return None;
            };
            let (table, alias) = single_table(from)?;
            let mut clauses = Vec::new();
            if let Some(selection) = &delete.selection {
                clauses.push(("where clause", vec![Checked::Expr(selection)]));
            }
            Some((table, alias, clauses))
        }
        Statement::Insert(insert) => {
            let TableObject::TableName(name) = &insert.table else {
                return None;
            };
            let table = single_part(name)?;
            let mut field_list = insert
                .columns
                .iter()
                .map(Checked::Column)
                .collect::<Vec<_>>();
            if let Some(source) = &insert.source {
                let SetExpr::Values(values) = source.body.as_ref() else {
                    return None;
                };
                for row in &values.rows {
                    field_list.extend(row.content.iter().map(Checked::Expr));
                }
            }
            Some((table, None, vec![("field list", field_list)]))
        }
        _ => None,
    }
}

fn single_select(query: &Query) -> Option<&sqlparser::ast::Select> {
    if query.with.is_some() {
        return None;
    }
    match query.body.as_ref() {
        SetExpr::Select(select) => Some(select),
        _ => None,
    }
}

fn single_table(from: &TableWithJoins) -> Option<(String, Option<String>)> {
    if !from.joins.is_empty() {
        return None;
    }
    let TableFactor::Table { name, alias, .. } = &from.relation else {
        return None;
    };
    Some((
        single_part(name)?,
        alias.as_ref().map(|alias| alias.name.value.clone()),
    ))
}

fn single_part(name: &ObjectName) -> Option<String> {
    match name.0.as_slice() {
        [ObjectNamePart::Identifier(ident)] => Some(ident.value.clone()),
        _ => None,
    }
}

/// The names one table's statement may use.
struct Scope {
    columns: Vec<String>,
    table: String,
    alias: Option<String>,
    ansi_quotes: bool,
}

/// Words MySQL reads as a value or a call with no parentheses, which a name
/// written bare may be.
const WORDS_THAT_ARE_NO_COLUMN: [&str; 11] = [
    "DEFAULT",
    "UNKNOWN",
    "CURRENT_DATE",
    "CURRENT_TIME",
    "CURRENT_TIMESTAMP",
    "CURRENT_USER",
    "LOCALTIME",
    "LOCALTIMESTAMP",
    "UTC_DATE",
    "UTC_TIME",
    "UTC_TIMESTAMP",
];

/// An expression this does not follow, which ends the search without an
/// answer rather than risk naming the wrong clause.
struct NotFollowed;

impl Scope {
    fn expr(
        &self,
        expr: &Expr,
        clause: &'static str,
    ) -> Result<Option<UnknownColumn>, NotFollowed> {
        let unknown = match expr {
            Expr::Identifier(ident) => self.bare_name(ident, clause),
            Expr::CompoundIdentifier(parts) => match parts.as_slice() {
                [qualifier, column] => Ok(self.qualified_name(qualifier, column, clause)),
                _ => Err(NotFollowed),
            },
            Expr::Value(_) => Ok(None),
            Expr::BinaryOp { left, right, .. } => {
                return Ok(self.expr(left, clause)?.or(self.expr(right, clause)?));
            }
            Expr::UnaryOp { expr, .. }
            | Expr::Nested(expr)
            | Expr::IsNull(expr)
            | Expr::IsNotNull(expr)
            | Expr::IsTrue(expr)
            | Expr::IsFalse(expr) => return self.expr(expr, clause),
            Expr::InList { expr, list, .. } => {
                let mut unknown = self.expr(expr, clause)?;
                for item in list {
                    unknown = unknown.or(self.expr(item, clause)?);
                }
                return Ok(unknown);
            }
            Expr::Between {
                expr, low, high, ..
            } => {
                return Ok(self
                    .expr(expr, clause)?
                    .or(self.expr(low, clause)?)
                    .or(self.expr(high, clause)?));
            }
            Expr::Like { expr, pattern, .. } => {
                return Ok(self.expr(expr, clause)?.or(self.expr(pattern, clause)?));
            }
            Expr::Function(function) => match &function.args {
                FunctionArguments::None => Ok(None),
                FunctionArguments::List(list) => {
                    let mut unknown = None;
                    for arg in &list.args {
                        match arg {
                            FunctionArg::Unnamed(FunctionArgExpr::Expr(arg)) => {
                                unknown = unknown.or(self.expr(arg, clause)?);
                            }
                            FunctionArg::Unnamed(FunctionArgExpr::Wildcard) => {}
                            _ => return Err(NotFollowed),
                        }
                    }
                    if !list.clauses.is_empty() {
                        return Err(NotFollowed);
                    }
                    Ok(unknown)
                }
                FunctionArguments::Subquery(_) => Err(NotFollowed),
            },
            _ => Err(NotFollowed),
        };
        unknown
    }

    fn bare_name(
        &self,
        ident: &Ident,
        clause: &'static str,
    ) -> Result<Option<UnknownColumn>, NotFollowed> {
        match ident.quote_style {
            // Without ANSI_QUOTES a double-quoted word is a string.
            Some('"') if !self.ansi_quotes => return Ok(None),
            Some('"' | '`') => {}
            Some(_) => return Err(NotFollowed),
            None if ident.value.starts_with('@')
                || WORDS_THAT_ARE_NO_COLUMN
                    .iter()
                    .any(|word| ident.value.eq_ignore_ascii_case(word)) =>
            {
                return Err(NotFollowed);
            }
            None => {}
        }
        Ok(self.column(&ident.value, &ident.value, clause))
    }

    fn qualified_name(
        &self,
        qualifier: &Ident,
        column: &Ident,
        clause: &'static str,
    ) -> Option<UnknownColumn> {
        let written = format!("{}.{}", qualifier.value, column.value);
        let names_the_table = match &self.alias {
            Some(alias) => alias.eq_ignore_ascii_case(&qualifier.value),
            None => self.table.eq_ignore_ascii_case(&qualifier.value),
        };
        if !names_the_table {
            return Some(UnknownColumn { written, clause });
        }
        self.column(&column.value, &written, clause)
    }

    fn object_name(
        &self,
        name: &ObjectName,
        clause: &'static str,
    ) -> Option<Option<UnknownColumn>> {
        let parts = name
            .0
            .iter()
            .map(|part| match part {
                ObjectNamePart::Identifier(ident) => Some(ident),
                ObjectNamePart::Function(_) => None,
            })
            .collect::<Option<Vec<_>>>()?;
        match parts.as_slice() {
            [column] => Some(self.column(&column.value, &column.value, clause)),
            [qualifier, column] => Some(self.qualified_name(qualifier, column, clause)),
            _ => None,
        }
    }

    fn column(&self, column: &str, written: &str, clause: &'static str) -> Option<UnknownColumn> {
        if self
            .columns
            .iter()
            .any(|known| known.eq_ignore_ascii_case(column))
        {
            return None;
        }
        Some(UnknownColumn {
            written: written.to_owned(),
            clause,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unknown(sql: &str) -> Option<(String, &'static str)> {
        unknown_column_named_by(sql, SessionSqlMode::default(), &mut |table| {
            table
                .eq_ignore_ascii_case("posts")
                .then(|| vec!["id".to_owned(), "title".to_owned()])
        })
        .map(|unknown| (unknown.written, unknown.clause))
    }

    /// Every expectation measured on MySQL 8.4.11.
    #[test]
    fn names_the_first_unknown_column_and_its_clause_as_mysql_checks_them() {
        for (sql, expected) in [
            (
                "SELECT nope FROM posts WHERE nope2 = 1",
                Some(("nope", "field list")),
            ),
            (
                "SELECT id FROM posts WHERE nope = 1",
                Some(("nope", "where clause")),
            ),
            (
                "SELECT id FROM posts WHERE `nope` = 1",
                Some(("nope", "where clause")),
            ),
            (
                "SELECT id AS a FROM posts WHERE a = 1",
                Some(("a", "where clause")),
            ),
            (
                "SELECT id FROM posts p WHERE p.nope = 1",
                Some(("p.nope", "where clause")),
            ),
            ("SELECT NOPE FROM Posts", Some(("NOPE", "field list"))),
            ("SELECT nope.id FROM posts", Some(("nope.id", "field list"))),
            (
                "UPDATE posts SET nope = 1 WHERE nope2 = 1",
                Some(("nope2", "where clause")),
            ),
            ("UPDATE posts SET nope = 1", Some(("nope", "field list"))),
            (
                "UPDATE posts SET title = nope3 WHERE id = 1",
                Some(("nope3", "field list")),
            ),
            (
                "DELETE FROM posts WHERE nope = 1",
                Some(("nope", "where clause")),
            ),
            (
                "DELETE FROM posts WHERE posts.nope = 1",
                Some(("posts.nope", "where clause")),
            ),
            (
                "DELETE FROM posts WHERE p.id = 1",
                Some(("p.id", "where clause")),
            ),
            (
                "INSERT INTO posts (id, nope) VALUES (1, 2)",
                Some(("nope", "field list")),
            ),
            (
                "INSERT INTO posts (id, title) VALUES (1, nope)",
                Some(("nope", "field list")),
            ),
            ("SELECT id FROM posts WHERE \"x\" = 'x'", None),
            (
                "SELECT id, title FROM posts WHERE id = 1 AND title LIKE 'a%'",
                None,
            ),
            ("SELECT COUNT(*) FROM posts WHERE id IN (1, 2)", None),
            ("INSERT INTO posts (id, title) VALUES (1, DEFAULT)", None),
            ("SELECT id FROM posts WHERE title = CURRENT_USER", None),
            ("SELECT id FROM posts WHERE title < UTC_TIMESTAMP", None),
            ("SELECT id FROM posts WHERE title < LOCALTIME", None),
            ("SELECT id FROM posts WHERE @x = 1", None),
            (
                "SELECT id FROM posts JOIN other ON 1 = 1 WHERE nope = 1",
                None,
            ),
            (
                "SELECT id FROM posts WHERE id IN (SELECT nope FROM posts)",
                None,
            ),
            ("SELECT id FROM unknown_table WHERE nope = 1", None),
        ] {
            assert_eq!(
                unknown(sql),
                expected.map(|(written, clause)| (written.to_owned(), clause)),
                "{sql}"
            );
        }
    }
}
