//! A prepared `SELECT` of written values beside bound ones, reading no table:
//! sqlx's `SELECT CAST(? AS SIGNED) AS i, CAST(? AS DECIMAL(10,2)) AS d, ? AS
//! s, CAST(NULL AS CHAR) AS n, NOW(6) AS t`.
//!
//! Each bound value is written into the statement once it is bound, and the
//! statement is answered the way the same one written out is. What the
//! columns of the bound values report is MySQL's own for a bound value, which
//! the caller works out from the cast each stands in.

use super::{
    byte_offset_of_location, read_one_statement, written_value, ParseError, SessionMySqlDialect,
    SessionSqlMode,
};
use crate::statement_reads;
use sqlparser::ast::{CastKind, DataType, Expr, SelectItem, SetExpr, Statement, Value};
use sqlparser::tokenizer::Token;

/// A `SELECT` of written and bound values reading no table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BoundWrittenValues {
    /// Where each `?` stands in the statement, in order.
    placeholders: Vec<usize>,
    columns: Vec<BoundWrittenColumn>,
}

/// One column of a [`BoundWrittenValues`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BoundWrittenColumn {
    /// A value written out, which reports what it reports written.
    Written,
    /// A `?`, alone or inside the one cast, named by its alias.
    Bound {
        ordinal: usize,
        name: String,
        cast: BoundCast,
    },
}

/// What a bound value is read as.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BoundCast {
    /// The `?` alone.
    Nothing,
    /// `CAST(? AS SIGNED)`.
    Signed,
    /// `CAST(? AS DECIMAL(p,s))`.
    Decimal { precision: u32, scale: u32 },
}

impl BoundWrittenValues {
    /// How many `?` the statement binds.
    pub fn parameter_count(&self) -> usize {
        self.placeholders.len()
    }

    pub fn columns(&self) -> &[BoundWrittenColumn] {
        &self.columns
    }

    /// The statement with each `?` replaced by the SQL written for its value,
    /// in order.
    pub fn written_with(&self, sql: &str, values: &[String]) -> String {
        assert_eq!(
            values.len(),
            self.placeholders.len(),
            "one value for each `?`"
        );
        let mut written = String::with_capacity(sql.len());
        let mut from = 0;
        for (&at, value) in self.placeholders.iter().zip(values) {
            written.push_str(&sql[from..at]);
            written.push_str(value);
            from = at + 1;
        }
        written.push_str(&sql[from..]);
        written
    }
}

/// Recognizes a `SELECT` reading no table whose every `?` is a column of its
/// own under an alias, alone or as `CAST(? AS SIGNED)` or `CAST(? AS
/// DECIMAL(p,s))`, and whose other columns bind nothing.
pub fn parse_optional_bound_written_values(
    sql: &str,
    mode: SessionSqlMode,
) -> Result<Option<BoundWrittenValues>, ParseError> {
    let read_statement = read_one_statement(sql, mode);
    let Ok(Statement::Query(query)) = read_statement.as_ref() else {
        return Ok(None);
    };
    if query.with.is_some()
        || query.order_by.is_some()
        || query.limit_clause.is_some()
        || query.fetch.is_some()
        || !query.locks.is_empty()
        || query.for_clause.is_some()
        || query.settings.is_some()
        || query.format_clause.is_some()
        || !query.pipe_operators.is_empty()
    {
        return Ok(None);
    }
    let SetExpr::Select(select) = query.body.as_ref() else {
        return Ok(None);
    };
    if !select.from.is_empty()
        || select.selection.is_some()
        || select.having.is_some()
        || select.distinct.is_some()
        || select.into.is_some()
        || !matches!(&select.group_by, sqlparser::ast::GroupByExpr::Expressions(exprs, _) if exprs.is_empty())
    {
        return Ok(None);
    }
    let mut columns = Vec::with_capacity(select.projection.len());
    let mut bound = 0;
    for item in &select.projection {
        let (expr, alias) = match item {
            SelectItem::UnnamedExpr(expr) => (expr, None),
            SelectItem::ExprWithAlias { expr, alias } => (expr, Some(alias)),
            _ => return Ok(None),
        };
        match (bound_cast(expr), alias) {
            (Some(cast), Some(alias)) => {
                columns.push(BoundWrittenColumn::Bound {
                    ordinal: bound,
                    name: alias.value.clone(),
                    cast,
                });
                bound += 1;
            }
            // Unnamed, MySQL names the column after the statement's own
            // spelling of it, which is not read back here.
            (Some(_), None) => return Ok(None),
            // A written value binds nothing. A `'?'` inside a word is taken
            // for a `?` here, which refuses the statement rather than
            // misreading it.
            (None, _) if expr.to_string().contains('?') => return Ok(None),
            (None, _) => columns.push(BoundWrittenColumn::Written),
        }
    }
    if bound == 0 {
        return Ok(None);
    }
    let dialect = SessionMySqlDialect::new(mode);
    let tokens = statement_reads::tokens_with_location(&dialect, sql)
        .map_err(|error| ParseError::Sqlparser(error.to_string()))?;
    let placeholders = tokens
        .iter()
        .filter(|token| matches!(&token.token, Token::Placeholder(marker) if marker == "?"))
        .map(|token| byte_offset_of_location(sql, token.span.start))
        .collect::<Option<Vec<_>>>();
    let Some(placeholders) = placeholders else {
        return Ok(None);
    };
    if placeholders.len() != bound {
        return Ok(None);
    }
    Ok(Some(BoundWrittenValues {
        placeholders,
        columns,
    }))
}

fn bound_cast(expr: &Expr) -> Option<BoundCast> {
    let is_a_placeholder = |expr: &Expr| matches!(expr, Expr::Value(value) if matches!(&value.value, Value::Placeholder(marker) if marker == "?"));
    if is_a_placeholder(expr) {
        return Some(BoundCast::Nothing);
    }
    let Expr::Cast {
        kind: CastKind::Cast,
        expr,
        data_type,
        format: None,
        array: false,
    } = expr
    else {
        return None;
    };
    if !is_a_placeholder(expr) {
        return None;
    }
    match data_type {
        DataType::Signed | DataType::SignedInteger => Some(BoundCast::Signed),
        DataType::Decimal(size) => {
            let (precision, scale) = written_value::decimal_size(size)?;
            Some(BoundCast::Decimal { precision, scale })
        }
        _ => None,
    }
}
