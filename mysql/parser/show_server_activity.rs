//! `SHOW STATUS` and `SHOW PROCESSLIST`, which describe the server rather
//! than a database, and the read of one status counter out of
//! `performance_schema`.

use sqlparser::ast::{
    BinaryOperator, Expr, GroupByExpr, ObjectNamePart, SelectItem, SetExpr, Statement, TableFactor,
    Value,
};

use super::{
    admin_command_ends, consume_admin_like_pattern, consume_admin_word,
    like_pattern::MySqlLikePattern, read_one_statement, skip_admin_comments,
    tokenize_admin_command, MySqlVariableScope, ParseError, SessionSqlMode,
};

/// `SHOW [GLOBAL | SESSION] STATUS [LIKE 'pattern']`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MySqlShowStatusCommand {
    scope: MySqlVariableScope,
    pattern: Option<MySqlLikePattern>,
}

impl MySqlShowStatusCommand {
    /// Returns the scope the command was written with.
    pub fn scope(&self) -> MySqlVariableScope {
        self.scope
    }

    /// Reports whether the command asks for the counter called `name`.
    pub fn selects(&self, name: &str) -> bool {
        self.pattern
            .as_ref()
            .is_none_or(|pattern| pattern.matches(name))
    }
}

/// Parses `SHOW STATUS`, or returns `None` for any other statement.
///
/// A `WHERE` is a predicate over the listing's two columns rather than a
/// pattern, and is refused the way `SHOW VARIABLES` refuses one.
pub fn parse_optional_show_status(
    sql: &str,
    mode: SessionSqlMode,
) -> Result<Option<MySqlShowStatusCommand>, ParseError> {
    let Ok(tokens) = tokenize_admin_command(sql, mode) else {
        return Ok(None);
    };
    let mut cursor = skip_admin_comments(&tokens, 0);
    if !consume_admin_word(&tokens, &mut cursor, "SHOW") {
        return Ok(None);
    }
    let scope = if consume_admin_word(&tokens, &mut cursor, "GLOBAL") {
        MySqlVariableScope::Global
    } else {
        let _ = consume_admin_word(&tokens, &mut cursor, "SESSION")
            || consume_admin_word(&tokens, &mut cursor, "LOCAL");
        MySqlVariableScope::Session
    };
    if !consume_admin_word(&tokens, &mut cursor, "STATUS") {
        return Ok(None);
    }
    let pattern = consume_admin_like_pattern(&tokens, &mut cursor, mode)?;
    if pattern.is_none() && consume_admin_word(&tokens, &mut cursor, "WHERE") {
        return Err(ParseError::Unsupported {
            feature: "SHOW STATUS with a WHERE",
        });
    }
    if !admin_command_ends(&tokens, cursor) {
        return Err(ParseError::TrailingAdminCommandTokens);
    }
    Ok(Some(MySqlShowStatusCommand { scope, pattern }))
}

/// `SELECT variable_value FROM performance_schema.session_status WHERE
/// variable_name = 'name'`, which reads one status counter.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MySqlStatusCounterRead {
    scope: MySqlVariableScope,
    counter: String,
    column_name: String,
}

impl MySqlStatusCounterRead {
    /// Returns `Session` for `session_status` and `Global` for
    /// `global_status`.
    pub fn scope(&self) -> MySqlVariableScope {
        self.scope
    }

    /// Returns the counter's name as the statement wrote it.
    pub fn counter(&self) -> &str {
        &self.counter
    }

    /// Returns the name MySQL gives the one result column: its alias, or
    /// `variable_value` as written.
    pub fn column_name(&self) -> &str {
        &self.column_name
    }
}

/// Parses the read of one status counter out of `performance_schema` —
/// Laravel's `db:show` asks for `select variable_value as `Value` from
/// performance_schema.session_status where variable_name =
/// 'threads_connected'` — or returns `None` for a statement reading no
/// status table.
///
/// Any other read of `session_status` or `global_status` is refused rather
/// than answered from what this server keeps.
pub fn parse_optional_status_counter_read(
    sql: &str,
    mode: SessionSqlMode,
) -> Result<Option<MySqlStatusCounterRead>, ParseError> {
    const REFUSED: ParseError = ParseError::Unsupported {
        feature: "a read of a performance_schema status table other than one counter's value",
    };
    let read_statement = read_one_statement(sql, mode);
    let Ok(Statement::Query(query)) = read_statement.as_ref() else {
        return Ok(None);
    };
    let SetExpr::Select(select) = query.body.as_ref() else {
        return Ok(None);
    };
    let [from] = select.from.as_slice() else {
        return Ok(None);
    };
    let TableFactor::Table { name, .. } = &from.relation else {
        return Ok(None);
    };
    let scope = match name.0.as_slice() {
        [ObjectNamePart::Identifier(database), ObjectNamePart::Identifier(table)]
            if database.value.eq_ignore_ascii_case("performance_schema") =>
        {
            if table.value.eq_ignore_ascii_case("session_status") {
                MySqlVariableScope::Session
            } else if table.value.eq_ignore_ascii_case("global_status") {
                MySqlVariableScope::Global
            } else {
                return Ok(None);
            }
        }
        _ => return Ok(None),
    };
    let TableFactor::Table {
        alias: None,
        args: None,
        with_hints,
        ..
    } = &from.relation
    else {
        return Err(REFUSED);
    };
    if !with_hints.is_empty()
        || !from.joins.is_empty()
        || query.with.is_some()
        || query.order_by.is_some()
        || query.limit_clause.is_some()
        || !query.locks.is_empty()
        || select.distinct.is_some()
        || select.having.is_some()
        || !matches!(&select.group_by, GroupByExpr::Expressions(expressions, _) if expressions.is_empty())
    {
        return Err(REFUSED);
    }
    let column_name = match select.projection.as_slice() {
        [SelectItem::UnnamedExpr(Expr::Identifier(column))]
            if column.value.eq_ignore_ascii_case("variable_value") =>
        {
            column.value.clone()
        }
        [SelectItem::ExprWithAlias {
            expr: Expr::Identifier(column),
            alias,
        }] if column.value.eq_ignore_ascii_case("variable_value") => alias.value.clone(),
        _ => return Err(REFUSED),
    };
    let Some(Expr::BinaryOp {
        left,
        op: BinaryOperator::Eq,
        right,
    }) = &select.selection
    else {
        return Err(REFUSED);
    };
    let (Expr::Identifier(column), Expr::Value(value)) = (left.as_ref(), right.as_ref()) else {
        return Err(REFUSED);
    };
    if !column.value.eq_ignore_ascii_case("variable_name") {
        return Err(REFUSED);
    }
    let Value::SingleQuotedString(counter) = &value.value else {
        return Err(REFUSED);
    };
    Ok(Some(MySqlStatusCounterRead {
        scope,
        counter: counter.clone(),
        column_name,
    }))
}

/// Parses `SHOW [FULL] PROCESSLIST`, answering whether it was the `FULL`
/// form, or `None` for any other statement.
pub fn parse_optional_show_processlist(
    sql: &str,
    mode: SessionSqlMode,
) -> Result<Option<bool>, ParseError> {
    let Ok(tokens) = tokenize_admin_command(sql, mode) else {
        return Ok(None);
    };
    let mut cursor = skip_admin_comments(&tokens, 0);
    if !consume_admin_word(&tokens, &mut cursor, "SHOW") {
        return Ok(None);
    }
    let full = consume_admin_word(&tokens, &mut cursor, "FULL");
    if !consume_admin_word(&tokens, &mut cursor, "PROCESSLIST") {
        return Ok(None);
    }
    if !admin_command_ends(&tokens, cursor) {
        return Err(ParseError::TrailingAdminCommandTokens);
    }
    Ok(Some(full))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_show_status_in_each_scope() {
        let status = |sql: &str| {
            parse_optional_show_status(sql, SessionSqlMode::default())
                .unwrap()
                .unwrap()
        };
        let uptime = status("SHOW GLOBAL STATUS LIKE 'Uptime'");
        assert_eq!(uptime.scope(), MySqlVariableScope::Global);
        assert!(uptime.selects("Uptime"));
        assert!(!uptime.selects("Uptime_since_flush_status"));
        let threads = status("show session status like 'Threads\\_%'");
        assert_eq!(threads.scope(), MySqlVariableScope::Session);
        assert!(threads.selects("Threads_connected"));
        assert!(status("SHOW STATUS").selects("Uptime"));
        assert!(parse_optional_show_status(
            "SHOW STATUS WHERE Variable_name = 'Uptime'",
            SessionSqlMode::default()
        )
        .is_err());
        assert_eq!(
            parse_optional_show_status("SHOW TABLE STATUS", SessionSqlMode::default()).unwrap(),
            None
        );
    }

    #[test]
    fn reads_one_status_counter_out_of_performance_schema() {
        let read = |sql: &str| parse_optional_status_counter_read(sql, SessionSqlMode::default());
        let laravel = read("select variable_value as `Value` from performance_schema.session_status where variable_name = 'threads_connected'")
            .unwrap()
            .unwrap();
        assert_eq!(laravel.scope(), MySqlVariableScope::Session);
        assert_eq!(laravel.counter(), "threads_connected");
        assert_eq!(laravel.column_name(), "Value");
        let uptime = read(
            "SELECT VARIABLE_VALUE FROM `performance_schema`.`global_status` WHERE VARIABLE_NAME = 'Uptime'",
        )
        .unwrap()
        .unwrap();
        assert_eq!(uptime.scope(), MySqlVariableScope::Global);
        assert_eq!(uptime.column_name(), "VARIABLE_VALUE");
        for sql in [
            "select * from performance_schema.session_status where variable_name = 'Uptime'",
            "select variable_name, variable_value from performance_schema.session_status where variable_name = 'Uptime'",
            "select variable_value from performance_schema.session_status",
            "select variable_value from performance_schema.session_status where variable_name like 'Up%'",
            "select variable_value from performance_schema.session_status s where variable_name = 'Uptime'",
            "select variable_value from performance_schema.session_status where variable_name = 'Uptime' limit 1",
        ] {
            assert!(read(sql).is_err(), "{sql}");
        }
        for sql in [
            "select variable_value from performance_schema.threads where variable_name = 'x'",
            "select variable_value from session_status where variable_name = 'Uptime'",
            "SHOW STATUS",
        ] {
            assert_eq!(read(sql).unwrap(), None, "{sql}");
        }
    }

    #[test]
    fn reads_show_processlist() {
        let full = |sql: &str| parse_optional_show_processlist(sql, SessionSqlMode::default());
        assert_eq!(full("SHOW PROCESSLIST").unwrap(), Some(false));
        assert_eq!(full("show full processlist;").unwrap(), Some(true));
        assert_eq!(full("SHOW FULL TABLES").unwrap(), None);
        assert!(full("SHOW PROCESSLIST LIKE 'x'").is_err());
    }
}
