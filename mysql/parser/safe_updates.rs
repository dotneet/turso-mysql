//! What `sql_safe_updates` asks of an `UPDATE` or a `DELETE`: a `LIMIT`, or a
//! `WHERE` that can use an index.
//!
//! Measured on MySQL 8.4.11, a table with a primary key `id`, an index on `k`
//! and an unindexed `v`: `id = 1`, `id > 0` (every row), `id <> 1`,
//! `id IS NULL`, `id BETWEEN 1 AND 2`, `id IN (1, 2)`, `id = '1'`, `id = 1.5`,
//! `k > 0`, `id = 1 OR id = 2`, `v = 9 AND id = 1` and a `LIMIT` with any
//! `WHERE` or none run, while no `WHERE`, `v = 1`, `1 = 1`, `id = id`,
//! `id + 0 = 1` and `id = 1 OR v = 2` answer 1175. A prepared `id = ?` runs and
//! `v = ?` answers 1175 when it is executed.
//!
//! This reads the statement; the caller knows the table's indexes and the
//! kinds of its columns, and decides.

use super::*;

/// What an `UPDATE` or a `DELETE` gives `sql_safe_updates` to decide on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SafeUpdateReading {
    /// It has a `LIMIT`, which MySQL runs whatever the `WHERE` says.
    Limited,
    /// It changes one table named plainly, with the conjuncts of its `WHERE`
    /// — none when there is no `WHERE`.
    OneTable {
        table: String,
        conjuncts: Vec<SafeUpdateConjunct>,
    },
    /// A shape this does not read: several tables, a qualified name.
    Unread,
}

/// One conjunct of the `WHERE`, the parts `AND` joins at the top.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SafeUpdateConjunct {
    /// Compares one column with written values or `?` in a way an index on
    /// the column can serve: `=`, `<>`, `<`, `>`, `<=`, `>=`, `IN`, `BETWEEN`,
    /// `IS NULL`, or an `OR` of those over the one column.
    ColumnAgainstValues {
        column: String,
        values: ComparedValues,
    },
    /// Names these columns, and no other, in some other way; none at all for
    /// `1 = 1`.
    OtherUse { columns: Vec<String> },
    /// Holds something this does not read, a subquery say.
    Unread,
}

/// The kinds of value a column was compared with.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ComparedValues {
    /// A quoted word.
    pub words: bool,
    /// A written number.
    pub numbers: bool,
    /// A `?`, whose kind is known only when the statement is executed.
    pub bound: bool,
}

/// Reads an `UPDATE` or a `DELETE` for `sql_safe_updates`, or `None` for any
/// other statement.
pub fn read_safe_update(sql: &str, mode: SessionSqlMode) -> Option<SafeUpdateReading> {
    let statement = match parse_one_statement(sql, mode) {
        Ok(statement) => statement,
        Err(_) => {
            let first = sql
                .trim_start()
                .split(|character: char| !character.is_ascii_alphabetic())
                .next()
                .unwrap_or_default();
            return (first.eq_ignore_ascii_case("UPDATE") || first.eq_ignore_ascii_case("DELETE"))
                .then_some(SafeUpdateReading::Unread);
        }
    };
    let (limited, table, selection) = match &statement {
        Statement::Update(update) => {
            let table = (update.from.is_none() && update.table.joins.is_empty())
                .then(|| plain_table_name(&update.table.relation))
                .flatten();
            (update.limit.is_some(), table, update.selection.as_ref())
        }
        Statement::Delete(delete) => {
            let table = match &delete.from {
                FromTable::WithFromKeyword(tables) | FromTable::WithoutKeyword(tables)
                    if delete.tables.is_empty() && delete.using.is_none() =>
                {
                    match tables.as_slice() {
                        [table] if table.joins.is_empty() => plain_table_name(&table.relation),
                        _ => None,
                    }
                }
                _ => None,
            };
            (delete.limit.is_some(), table, delete.selection.as_ref())
        }
        _ => return None,
    };
    if limited {
        return Some(SafeUpdateReading::Limited);
    }
    let Some(table) = table else {
        return Some(SafeUpdateReading::Unread);
    };
    let mut conjuncts = Vec::new();
    if let Some(selection) = selection {
        read_conjuncts(selection, &mut conjuncts);
    }
    Some(SafeUpdateReading::OneTable { table, conjuncts })
}

fn plain_table_name(relation: &TableFactor) -> Option<String> {
    let TableFactor::Table { name, .. } = relation else {
        return None;
    };
    let [ObjectNamePart::Identifier(name)] = name.0.as_slice() else {
        return None;
    };
    Some(name.value.clone())
}

fn read_conjuncts(expr: &Expr, conjuncts: &mut Vec<SafeUpdateConjunct>) {
    match expr {
        Expr::BinaryOp {
            left,
            op: BinaryOperator::And,
            right,
        } => {
            read_conjuncts(left, conjuncts);
            read_conjuncts(right, conjuncts);
        }
        Expr::Nested(inner) => read_conjuncts(inner, conjuncts),
        _ => conjuncts.push(read_conjunct(expr)),
    }
}

fn read_conjunct(expr: &Expr) -> SafeUpdateConjunct {
    if let Some((column, values)) = column_against_values(expr) {
        return SafeUpdateConjunct::ColumnAgainstValues { column, values };
    }
    let mut columns = Vec::new();
    if columns_named_by(expr, &mut columns) {
        SafeUpdateConjunct::OtherUse { columns }
    } else {
        SafeUpdateConjunct::Unread
    }
}

fn column_against_values(expr: &Expr) -> Option<(String, ComparedValues)> {
    match expr {
        Expr::Nested(inner) => column_against_values(inner),
        Expr::BinaryOp {
            left,
            op:
                BinaryOperator::Eq
                | BinaryOperator::NotEq
                | BinaryOperator::Lt
                | BinaryOperator::LtEq
                | BinaryOperator::Gt
                | BinaryOperator::GtEq,
            right,
        } => {
            let mut values = ComparedValues::default();
            if let Some(column) = column_name(left) {
                note_value(right, &mut values).then_some((column, values))
            } else {
                let column = column_name(right)?;
                note_value(left, &mut values).then_some((column, values))
            }
        }
        Expr::BinaryOp {
            left,
            op: BinaryOperator::Or,
            right,
        } => {
            let (left_column, left_values) = column_against_values(left)?;
            let (right_column, right_values) = column_against_values(right)?;
            left_column.eq_ignore_ascii_case(&right_column).then_some((
                left_column,
                ComparedValues {
                    words: left_values.words || right_values.words,
                    numbers: left_values.numbers || right_values.numbers,
                    bound: left_values.bound || right_values.bound,
                },
            ))
        }
        Expr::InList {
            expr,
            list,
            negated: false,
        } => {
            let column = column_name(expr)?;
            let mut values = ComparedValues::default();
            list.iter()
                .all(|value| note_value(value, &mut values))
                .then_some((column, values))
        }
        Expr::Between {
            expr,
            negated: false,
            low,
            high,
        } => {
            let column = column_name(expr)?;
            let mut values = ComparedValues::default();
            (note_value(low, &mut values) && note_value(high, &mut values))
                .then_some((column, values))
        }
        Expr::IsNull(expr) => Some((column_name(expr)?, ComparedValues::default())),
        _ => None,
    }
}

fn column_name(expr: &Expr) -> Option<String> {
    match expr {
        Expr::Identifier(ident) => Some(ident.value.clone()),
        Expr::CompoundIdentifier(parts) => match parts.as_slice() {
            [_, column] => Some(column.value.clone()),
            _ => None,
        },
        Expr::Nested(inner) => column_name(inner),
        _ => None,
    }
}

/// Notes the kind of one written value, or answers `false` for anything
/// that is not one.
fn note_value(expr: &Expr, values: &mut ComparedValues) -> bool {
    match expr {
        Expr::Value(value) => match &value.value {
            Value::Number(..) => values.numbers = true,
            Value::SingleQuotedString(_) => values.words = true,
            Value::Placeholder(placeholder) if placeholder == "?" => values.bound = true,
            _ => return false,
        },
        Expr::UnaryOp {
            op: UnaryOperator::Minus | UnaryOperator::Plus,
            expr,
        } if matches!(expr.as_ref(), Expr::Value(value) if matches!(value.value, Value::Number(..))) =>
        {
            values.numbers = true;
        }
        Expr::Nested(inner) => return note_value(inner, values),
        _ => return false,
    }
    true
}

/// Gathers the columns an expression names, answering `false` for a part
/// this does not read.
fn columns_named_by(expr: &Expr, columns: &mut Vec<String>) -> bool {
    match expr {
        Expr::Identifier(_) | Expr::CompoundIdentifier(_) => match column_name(expr) {
            Some(column) => {
                columns.push(column);
                true
            }
            None => false,
        },
        Expr::Value(_) => true,
        Expr::BinaryOp { left, right, .. } => {
            columns_named_by(left, columns) && columns_named_by(right, columns)
        }
        Expr::UnaryOp { expr, .. }
        | Expr::Nested(expr)
        | Expr::IsNull(expr)
        | Expr::IsNotNull(expr)
        | Expr::IsTrue(expr)
        | Expr::IsFalse(expr) => columns_named_by(expr, columns),
        Expr::InList { expr, list, .. } => {
            columns_named_by(expr, columns)
                && list.iter().all(|item| columns_named_by(item, columns))
        }
        Expr::Between {
            expr, low, high, ..
        } => {
            columns_named_by(expr, columns)
                && columns_named_by(low, columns)
                && columns_named_by(high, columns)
        }
        Expr::Like { expr, pattern, .. } => {
            columns_named_by(expr, columns) && columns_named_by(pattern, columns)
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read(sql: &str) -> SafeUpdateReading {
        read_safe_update(sql, SessionSqlMode::default()).unwrap()
    }

    fn conjuncts(sql: &str) -> Vec<SafeUpdateConjunct> {
        match read(sql) {
            SafeUpdateReading::OneTable { table, conjuncts } => {
                assert_eq!(table, "s", "{sql}");
                conjuncts
            }
            other => panic!("{sql}: {other:?}"),
        }
    }

    fn against(column: &str, words: bool, numbers: bool, bound: bool) -> SafeUpdateConjunct {
        SafeUpdateConjunct::ColumnAgainstValues {
            column: column.to_owned(),
            values: ComparedValues {
                words,
                numbers,
                bound,
            },
        }
    }

    #[test]
    fn reads_the_where_of_an_update_or_a_delete() {
        assert_eq!(conjuncts("UPDATE s SET v = 0"), []);
        assert_eq!(conjuncts("DELETE FROM s"), []);
        assert_eq!(
            read("UPDATE s SET v = 0 WHERE v = 1 LIMIT 1"),
            SafeUpdateReading::Limited
        );
        assert_eq!(read("DELETE FROM s LIMIT 5"), SafeUpdateReading::Limited);
        for (sql, expected) in [
            (
                "UPDATE s SET v = 0 WHERE id = 1",
                against("id", false, true, false),
            ),
            (
                "UPDATE s SET v = 0 WHERE s.id > 0",
                against("id", false, true, false),
            ),
            (
                "UPDATE s SET v = 0 WHERE 1 <> id",
                against("id", false, true, false),
            ),
            (
                "UPDATE s SET v = 0 WHERE id = '1'",
                against("id", true, false, false),
            ),
            (
                "UPDATE s SET v = 0 WHERE id = -1",
                against("id", false, true, false),
            ),
            (
                "UPDATE s SET v = 0 WHERE id IN (1, 2)",
                against("id", false, true, false),
            ),
            (
                "UPDATE s SET v = 0 WHERE id BETWEEN 1 AND 2",
                against("id", false, true, false),
            ),
            (
                "UPDATE s SET v = 0 WHERE id IS NULL",
                against("id", false, false, false),
            ),
            (
                "UPDATE s SET v = 0 WHERE (id = 1)",
                against("id", false, true, false),
            ),
            (
                "UPDATE s SET v = 0 WHERE id = 1 OR id = 2",
                against("id", false, true, false),
            ),
            (
                "UPDATE s SET v = 0 WHERE id = ?",
                against("id", false, false, true),
            ),
            (
                "UPDATE s SET v = 0 WHERE id = 1 OR v = 2",
                SafeUpdateConjunct::OtherUse {
                    columns: vec!["id".to_owned(), "v".to_owned()],
                },
            ),
            (
                "UPDATE s SET v = 0 WHERE 1 = 1",
                SafeUpdateConjunct::OtherUse { columns: vec![] },
            ),
            (
                "UPDATE s SET v = 0 WHERE id = id",
                SafeUpdateConjunct::OtherUse {
                    columns: vec!["id".to_owned(), "id".to_owned()],
                },
            ),
            (
                "UPDATE s SET v = 0 WHERE id + 0 = 1",
                SafeUpdateConjunct::OtherUse {
                    columns: vec!["id".to_owned()],
                },
            ),
            (
                "DELETE FROM s WHERE v IN (SELECT 1)",
                SafeUpdateConjunct::Unread,
            ),
        ] {
            assert_eq!(conjuncts(sql), [expected], "{sql}");
        }
        assert_eq!(
            conjuncts("UPDATE s SET v = 0 WHERE v = 9 AND (id = 1)"),
            [
                against("v", false, true, false),
                against("id", false, true, false)
            ]
        );
        for sql in [
            "UPDATE s, t SET s.v = 0 WHERE s.id = t.id",
            "UPDATE db.s SET v = 0 WHERE id = 1",
            "DELETE s FROM s JOIN t ON s.id = t.id",
            "UPDATE s SET v = 0 WHERE id = 1 GARBAGE",
        ] {
            assert_eq!(read(sql), SafeUpdateReading::Unread, "{sql}");
        }
        assert_eq!(
            read_safe_update("SELECT 1", SessionSqlMode::default()),
            None
        );
        assert_eq!(
            read_safe_update("INSERT INTO s VALUES (1)", SessionSqlMode::default()),
            None
        );
    }
}
