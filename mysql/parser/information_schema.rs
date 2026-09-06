//! Checking the `information_schema` queries clients send at startup.
//!
//! These are recognized rather than translated. A client asks a narrow,
//! predictable question about the catalog, and anything wider is refused, so
//! this is all validation and no rendering.

use super::*;

pub(crate) fn tokenize_information_schema_query(
    sql: &str,
    mode: SessionSqlMode,
) -> Result<Vec<Token>, ParseError> {
    Tokenizer::new(&SessionMySqlDialect::without_executable_comments(mode), sql)
        .tokenize()
        .map_err(|error| ParseError::Sqlparser(error.to_string()))
}

pub(crate) fn contains_information_schema_tables(tokens: &[Token]) -> bool {
    contains_information_schema_object(tokens, "TABLES")
}

pub(crate) fn contains_information_schema_object(tokens: &[Token], expected_object: &str) -> bool {
    let significant = tokens
        .iter()
        .filter(|token| !matches!(token, Token::Whitespace(_)))
        .collect::<Vec<_>>();
    significant.windows(3).any(|window| {
        is_information_schema_identifier_token(window[0], "information_schema")
            && matches!(window[1], Token::Period)
            && is_information_schema_identifier_token(window[2], expected_object)
    })
}

fn is_information_schema_identifier_token(token: &Token, expected: &str) -> bool {
    matches!(
        token,
        Token::Word(word) if word.value.eq_ignore_ascii_case(expected)
    )
}

pub(crate) fn reject_information_schema_query_tokens(tokens: &[Token]) -> Result<(), ParseError> {
    if tokens.iter().any(|token| {
        matches!(
            token,
            Token::Whitespace(
                Whitespace::SingleLineComment { .. } | Whitespace::MultiLineComment(_)
            )
        )
    }) {
        return unsupported("comments in information_schema.TABLES query");
    }

    let semicolon_count = tokens
        .iter()
        .filter(|token| matches!(token, Token::SemiColon))
        .count();
    if semicolon_count > 1 {
        return unsupported("multiple information_schema.TABLES statements");
    }
    if semicolon_count == 1 {
        let last = tokens
            .iter()
            .rposition(|token| !matches!(token, Token::Whitespace(_)));
        if !matches!(
            last.and_then(|index| tokens.get(index)),
            Some(Token::SemiColon)
        ) {
            return unsupported("information_schema.TABLES semicolon position");
        }
    }
    Ok(())
}

pub(crate) fn validate_information_schema_tables_query(
    query: &sqlparser::ast::Query,
) -> Result<(), ParseError> {
    if query.with.is_some()
        || query.limit_clause.is_some()
        || query.fetch.is_some()
        || !query.locks.is_empty()
        || query.for_clause.is_some()
        || query.settings.is_some()
        || query.format_clause.is_some()
        || !query.pipe_operators.is_empty()
    {
        return unsupported("information_schema.TABLES query clause");
    }
    let SetExpr::Select(select) = query.body.as_ref() else {
        return unsupported("information_schema.TABLES compound query");
    };
    if !matches!(select.flavor, SelectFlavor::Standard)
        || !select.optimizer_hints.is_empty()
        || select.distinct.is_some()
        || select.select_modifiers.is_some()
        || select.top.is_some()
        || select.top_before_distinct
        || select.exclude.is_some()
        || select.into.is_some()
        || !select.lateral_views.is_empty()
        || select.prewhere.is_some()
        || !select.connect_by.is_empty()
        || !matches!(
            &select.group_by,
            sqlparser::ast::GroupByExpr::Expressions(exprs, _) if exprs.is_empty()
        )
        || !select.cluster_by.is_empty()
        || !select.distribute_by.is_empty()
        || !select.sort_by.is_empty()
        || select.having.is_some()
        || !select.named_window.is_empty()
        || select.qualify.is_some()
        || select.window_before_qualify
        || select.value_table_mode.is_some()
    {
        return unsupported("information_schema.TABLES SELECT feature");
    }

    let [SelectItem::UnnamedExpr(Expr::Identifier(table_schema)), SelectItem::UnnamedExpr(Expr::Identifier(table_name)), SelectItem::UnnamedExpr(Expr::Identifier(table_type))] =
        select.projection.as_slice()
    else {
        return unsupported("information_schema.TABLES projection");
    };
    if !is_identifier_named(table_schema, "TABLE_SCHEMA")
        || !is_identifier_named(table_name, "TABLE_NAME")
        || !is_identifier_named(table_type, "TABLE_TYPE")
    {
        return unsupported("information_schema.TABLES projection");
    }

    let [from] = select.from.as_slice() else {
        return unsupported("information_schema.TABLES table source");
    };
    if !from.joins.is_empty() {
        return unsupported("information_schema.TABLES JOIN");
    }
    let TableFactor::Table {
        name,
        alias,
        args,
        with_hints,
        version,
        with_ordinality,
        partitions,
        json_path,
        sample,
        index_hints,
    } = &from.relation
    else {
        return unsupported("information_schema.TABLES table source");
    };
    if alias.is_some()
        || args.is_some()
        || !with_hints.is_empty()
        || version.is_some()
        || *with_ordinality
        || !partitions.is_empty()
        || json_path.is_some()
        || sample.is_some()
        || !index_hints.is_empty()
    {
        return unsupported("information_schema.TABLES table option");
    }
    let [ObjectNamePart::Identifier(database), ObjectNamePart::Identifier(table)] =
        name.0.as_slice()
    else {
        return unsupported("qualified information_schema.TABLES source");
    };
    if !is_identifier_named(database, "information_schema") || !is_identifier_named(table, "TABLES")
    {
        return unsupported("information_schema.TABLES source");
    }

    let Some(selection) = select.selection.as_ref() else {
        return unsupported("information_schema.TABLES WHERE clause");
    };
    let Expr::BinaryOp { left, op, right } = selection else {
        return unsupported("information_schema.TABLES WHERE clause");
    };
    if !matches!(op, BinaryOperator::Eq)
        || !matches!(left.as_ref(), Expr::Identifier(identifier) if is_identifier_named(identifier, "TABLE_SCHEMA"))
        || !is_database_function(right)
    {
        return unsupported("information_schema.TABLES WHERE clause");
    }

    let Some(order_by) = query.order_by.as_ref() else {
        return unsupported("information_schema.TABLES ORDER BY clause");
    };
    let sqlparser::ast::OrderByKind::Expressions(expressions) = &order_by.kind else {
        return unsupported("information_schema.TABLES ORDER BY clause");
    };
    let [order] = expressions.as_slice() else {
        return unsupported("information_schema.TABLES ORDER BY clause");
    };
    if order_by.interpolate.is_some()
        || order.options != sqlparser::ast::OrderByOptions::default()
        || order.with_fill.is_some()
        || !matches!(&order.expr, Expr::Identifier(identifier) if is_identifier_named(identifier, "TABLE_NAME"))
    {
        return unsupported("information_schema.TABLES ORDER BY clause");
    }
    Ok(())
}

pub(crate) fn validate_information_schema_schemata_query(
    query: &sqlparser::ast::Query,
) -> Result<(), ParseError> {
    if query.with.is_some()
        || query.limit_clause.is_some()
        || query.fetch.is_some()
        || !query.locks.is_empty()
        || query.for_clause.is_some()
        || query.settings.is_some()
        || query.format_clause.is_some()
        || !query.pipe_operators.is_empty()
        || query.order_by.is_some()
    {
        return unsupported("information_schema.SCHEMATA query clause");
    }
    let SetExpr::Select(select) = query.body.as_ref() else {
        return unsupported("information_schema.SCHEMATA compound query");
    };
    if !matches!(select.flavor, SelectFlavor::Standard)
        || !select.optimizer_hints.is_empty()
        || select.distinct.is_some()
        || select.select_modifiers.is_some()
        || select.top.is_some()
        || select.top_before_distinct
        || select.exclude.is_some()
        || select.into.is_some()
        || !select.lateral_views.is_empty()
        || select.prewhere.is_some()
        || !select.connect_by.is_empty()
        || !matches!(
            &select.group_by,
            sqlparser::ast::GroupByExpr::Expressions(exprs, _) if exprs.is_empty()
        )
        || !select.cluster_by.is_empty()
        || !select.distribute_by.is_empty()
        || !select.sort_by.is_empty()
        || select.having.is_some()
        || !select.named_window.is_empty()
        || select.qualify.is_some()
        || select.window_before_qualify
        || select.value_table_mode.is_some()
    {
        return unsupported("information_schema.SCHEMATA SELECT feature");
    }

    let [SelectItem::UnnamedExpr(Expr::Identifier(schema_name))] = select.projection.as_slice()
    else {
        return unsupported("information_schema.SCHEMATA projection");
    };
    if !is_identifier_named(schema_name, "SCHEMA_NAME") {
        return unsupported("information_schema.SCHEMATA projection");
    }

    let [from] = select.from.as_slice() else {
        return unsupported("information_schema.SCHEMATA table source");
    };
    if !from.joins.is_empty() {
        return unsupported("information_schema.SCHEMATA JOIN");
    }
    let TableFactor::Table {
        name,
        alias,
        args,
        with_hints,
        version,
        with_ordinality,
        partitions,
        json_path,
        sample,
        index_hints,
    } = &from.relation
    else {
        return unsupported("information_schema.SCHEMATA table source");
    };
    if alias.is_some()
        || args.is_some()
        || !with_hints.is_empty()
        || version.is_some()
        || *with_ordinality
        || !partitions.is_empty()
        || json_path.is_some()
        || sample.is_some()
        || !index_hints.is_empty()
    {
        return unsupported("information_schema.SCHEMATA table option");
    }
    let [ObjectNamePart::Identifier(database), ObjectNamePart::Identifier(table)] =
        name.0.as_slice()
    else {
        return unsupported("qualified information_schema.SCHEMATA source");
    };
    if !is_identifier_named(database, "information_schema")
        || !is_identifier_named(table, "SCHEMATA")
    {
        return unsupported("information_schema.SCHEMATA source");
    }
    if select.selection.is_some() {
        return unsupported("information_schema.SCHEMATA WHERE clause");
    }
    Ok(())
}

pub(crate) fn reject_information_schema_schemata_query_tokens(
    tokens: &[Token],
) -> Result<(), ParseError> {
    if tokens.iter().any(|token| {
        matches!(
            token,
            Token::Whitespace(
                Whitespace::SingleLineComment { .. } | Whitespace::MultiLineComment(_)
            )
        )
    }) {
        return unsupported("comments in information_schema.SCHEMATA query");
    }

    let semicolon_count = tokens
        .iter()
        .filter(|token| matches!(token, Token::SemiColon))
        .count();
    if semicolon_count > 1 {
        return unsupported("multiple information_schema.SCHEMATA statements");
    }
    if semicolon_count == 1 {
        let last = tokens
            .iter()
            .rposition(|token| !matches!(token, Token::Whitespace(_)));
        if !matches!(
            last.and_then(|index| tokens.get(index)),
            Some(Token::SemiColon)
        ) {
            return unsupported("information_schema.SCHEMATA semicolon position");
        }
    }
    Ok(())
}

pub(crate) fn reject_information_schema_columns_query_tokens(
    tokens: &[Token],
) -> Result<(), ParseError> {
    if tokens.iter().any(|token| {
        matches!(
            token,
            Token::Whitespace(
                Whitespace::SingleLineComment { .. } | Whitespace::MultiLineComment(_)
            )
        )
    }) {
        return unsupported("comments in information_schema.COLUMNS query");
    }

    let semicolon_count = tokens
        .iter()
        .filter(|token| matches!(token, Token::SemiColon))
        .count();
    if semicolon_count > 1 {
        return unsupported("multiple information_schema.COLUMNS statements");
    }
    if semicolon_count == 1 {
        let last = tokens
            .iter()
            .rposition(|token| !matches!(token, Token::Whitespace(_)));
        if !matches!(
            last.and_then(|index| tokens.get(index)),
            Some(Token::SemiColon)
        ) {
            return unsupported("information_schema.COLUMNS semicolon position");
        }
    }
    Ok(())
}

pub(crate) fn validate_information_schema_columns_query(
    query: &sqlparser::ast::Query,
) -> Result<MySqlTableName, ParseError> {
    if query.with.is_some()
        || query.limit_clause.is_some()
        || query.fetch.is_some()
        || !query.locks.is_empty()
        || query.for_clause.is_some()
        || query.settings.is_some()
        || query.format_clause.is_some()
        || !query.pipe_operators.is_empty()
    {
        return unsupported("information_schema.COLUMNS query clause");
    }
    let SetExpr::Select(select) = query.body.as_ref() else {
        return unsupported("information_schema.COLUMNS compound query");
    };
    if !matches!(select.flavor, SelectFlavor::Standard)
        || !select.optimizer_hints.is_empty()
        || select.distinct.is_some()
        || select.select_modifiers.is_some()
        || select.top.is_some()
        || select.top_before_distinct
        || select.exclude.is_some()
        || select.into.is_some()
        || !select.lateral_views.is_empty()
        || select.prewhere.is_some()
        || !select.connect_by.is_empty()
        || !matches!(
            &select.group_by,
            sqlparser::ast::GroupByExpr::Expressions(exprs, _) if exprs.is_empty()
        )
        || !select.cluster_by.is_empty()
        || !select.distribute_by.is_empty()
        || !select.sort_by.is_empty()
        || select.having.is_some()
        || !select.named_window.is_empty()
        || select.qualify.is_some()
        || select.window_before_qualify
        || select.value_table_mode.is_some()
    {
        return unsupported("information_schema.COLUMNS SELECT feature");
    }

    let [SelectItem::UnnamedExpr(Expr::Identifier(column_name)), SelectItem::UnnamedExpr(Expr::Identifier(ordinal_position)), SelectItem::UnnamedExpr(Expr::Identifier(column_default)), SelectItem::UnnamedExpr(Expr::Identifier(is_nullable)), SelectItem::UnnamedExpr(Expr::Identifier(column_type)), SelectItem::UnnamedExpr(Expr::Identifier(column_key)), SelectItem::UnnamedExpr(Expr::Identifier(extra))] =
        select.projection.as_slice()
    else {
        return unsupported("information_schema.COLUMNS projection");
    };
    for (identifier, expected) in [
        (column_name, "COLUMN_NAME"),
        (ordinal_position, "ORDINAL_POSITION"),
        (column_default, "COLUMN_DEFAULT"),
        (is_nullable, "IS_NULLABLE"),
        (column_type, "COLUMN_TYPE"),
        (column_key, "COLUMN_KEY"),
        (extra, "EXTRA"),
    ] {
        if !is_identifier_named(identifier, expected) {
            return unsupported("information_schema.COLUMNS projection");
        }
    }

    let [from] = select.from.as_slice() else {
        return unsupported("information_schema.COLUMNS table source");
    };
    if !from.joins.is_empty() {
        return unsupported("information_schema.COLUMNS JOIN");
    }
    let TableFactor::Table {
        name,
        alias,
        args,
        with_hints,
        version,
        with_ordinality,
        partitions,
        json_path,
        sample,
        index_hints,
    } = &from.relation
    else {
        return unsupported("information_schema.COLUMNS table source");
    };
    if alias.is_some()
        || args.is_some()
        || !with_hints.is_empty()
        || version.is_some()
        || *with_ordinality
        || !partitions.is_empty()
        || json_path.is_some()
        || sample.is_some()
        || !index_hints.is_empty()
    {
        return unsupported("information_schema.COLUMNS table option");
    }
    let [ObjectNamePart::Identifier(database), ObjectNamePart::Identifier(table)] =
        name.0.as_slice()
    else {
        return unsupported("qualified information_schema.COLUMNS source");
    };
    if !is_identifier_named(database, "information_schema")
        || !is_identifier_named(table, "COLUMNS")
    {
        return unsupported("information_schema.COLUMNS source");
    }

    let Some(selection) = select.selection.as_ref() else {
        return unsupported("information_schema.COLUMNS WHERE clause");
    };
    let Expr::BinaryOp {
        left: schema_predicate,
        op: BinaryOperator::And,
        right: table_predicate,
    } = selection
    else {
        return unsupported("information_schema.COLUMNS WHERE clause");
    };
    if !is_information_schema_columns_schema_predicate(schema_predicate) {
        return unsupported("information_schema.COLUMNS WHERE clause");
    }
    let table = information_schema_columns_table_name(table_predicate)?;

    let Some(order_by) = query.order_by.as_ref() else {
        return unsupported("information_schema.COLUMNS ORDER BY clause");
    };
    let sqlparser::ast::OrderByKind::Expressions(expressions) = &order_by.kind else {
        return unsupported("information_schema.COLUMNS ORDER BY clause");
    };
    let [order] = expressions.as_slice() else {
        return unsupported("information_schema.COLUMNS ORDER BY clause");
    };
    if order_by.interpolate.is_some()
        || order.options != sqlparser::ast::OrderByOptions::default()
        || order.with_fill.is_some()
        || !matches!(
            &order.expr,
            Expr::Identifier(identifier) if is_identifier_named(identifier, "ORDINAL_POSITION")
        )
    {
        return unsupported("information_schema.COLUMNS ORDER BY clause");
    }
    Ok(table)
}

fn is_information_schema_columns_schema_predicate(expr: &Expr) -> bool {
    let Expr::BinaryOp {
        left,
        op: BinaryOperator::Eq,
        right,
    } = expr
    else {
        return false;
    };
    matches!(left.as_ref(), Expr::Identifier(identifier) if is_identifier_named(identifier, "TABLE_SCHEMA"))
        && is_database_function(right)
}

fn information_schema_columns_table_name(expr: &Expr) -> Result<MySqlTableName, ParseError> {
    let Expr::BinaryOp {
        left,
        op: BinaryOperator::Eq,
        right,
    } = expr
    else {
        return unsupported("information_schema.COLUMNS WHERE clause");
    };
    if !matches!(left.as_ref(), Expr::Identifier(identifier) if is_identifier_named(identifier, "TABLE_NAME"))
    {
        return unsupported("information_schema.COLUMNS WHERE clause");
    }
    let Expr::Value(value) = right.as_ref() else {
        return unsupported("information_schema.COLUMNS WHERE clause");
    };
    let Value::SingleQuotedString(name) = &value.value else {
        return unsupported("information_schema.COLUMNS WHERE clause");
    };
    MySqlTableName::parse(name)
}

fn is_identifier_named(identifier: &Ident, expected: &str) -> bool {
    identifier.value.eq_ignore_ascii_case(expected)
}

fn is_database_function(expr: &Expr) -> bool {
    let Expr::Function(function) = expr else {
        return false;
    };
    matches!(
        function.name.0.as_slice(),
        [ObjectNamePart::Identifier(identifier)] if is_identifier_named(identifier, "DATABASE")
    ) && !function.uses_odbc_syntax
        && matches!(function.parameters, FunctionArguments::None)
        && matches!(
            &function.args,
            FunctionArguments::List(arguments)
                if arguments.args.is_empty()
                    && arguments.duplicate_treatment.is_none()
                    && arguments.clauses.is_empty()
        )
        && function.filter.is_none()
        && function.null_treatment.is_none()
        && function.over.is_none()
        && function.within_group.is_empty()
}
