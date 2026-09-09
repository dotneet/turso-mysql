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
) -> Result<Vec<super::MySqlInformationSchemaTablesColumn>, ParseError> {
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

    // Any of the columns this answers, in any order a query names them, which
    // is the order MySQL answers in. A column outside that set is refused
    // rather than answered with a value that would be made up.
    let columns = projected_columns(
        &select.projection,
        super::MySqlInformationSchemaTablesColumn::named,
    )
    .ok_or(ParseError::Unsupported {
        feature: "information_schema.TABLES projection",
    })?;

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

    // The rows come back in table-name order whether or not the query asks
    // for it, so an absent ORDER BY is taken and the one MySQL clients write
    // is taken as well. Any other ordering is refused.
    if let Some(order_by) = query.order_by.as_ref() {
        let sqlparser::ast::OrderByKind::Expressions(expressions) = &order_by.kind else {
            return unsupported("information_schema.TABLES ORDER BY clause");
        };
        let [order] = expressions.as_slice() else {
            return unsupported("information_schema.TABLES ORDER BY clause");
        };
        if order_by.interpolate.is_some()
            || !an_ordering_these_rows_already_have(&order.options)
            || order.with_fill.is_some()
            || !matches!(&order.expr, Expr::Identifier(identifier) if is_identifier_named(identifier, "TABLE_NAME"))
        {
            return unsupported("information_schema.TABLES ORDER BY clause");
        }
    }
    Ok(columns)
}

/// Reads a projection as the columns it names, in the order it names them.
///
/// Every item has to be a plain column of the table being read: a wildcard
/// would name columns this does not answer, and an alias or an expression is
/// a shape the result metadata is not built for.
fn projected_columns<T: PartialEq>(
    projection: &[SelectItem],
    named: impl Fn(&str) -> Option<T>,
) -> Option<Vec<T>> {
    if projection.is_empty() {
        return None;
    }
    let mut columns: Vec<T> = Vec::with_capacity(projection.len());
    for item in projection {
        let SelectItem::UnnamedExpr(Expr::Identifier(identifier)) = item else {
            return None;
        };
        let column = named(&identifier.value)?;
        // MySQL takes the same column named twice and answers it twice. It is
        // refused here so that what a row holds is never wider than what the
        // whole row would hold, which is what the result's size is measured
        // against.
        if columns.contains(&column) {
            return None;
        }
        columns.push(column);
    }
    Some(columns)
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
) -> Result<
    (
        Option<String>,
        MySqlTableName,
        Vec<super::MySqlInformationSchemaColumnsColumn>,
    ),
    ParseError,
> {
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

    let columns = projected_columns(
        &select.projection,
        super::MySqlInformationSchemaColumnsColumn::named,
    )
    .ok_or(ParseError::Unsupported {
        feature: "information_schema.COLUMNS projection",
    })?;

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
    let schema = information_schema_columns_schema(schema_predicate)?;
    let table = information_schema_columns_table_name(table_predicate)?;

    // The rows come back in declaration order whether or not the query asks
    // for it, so an absent ORDER BY is taken and so is the one MySQL clients
    // write.
    if let Some(order_by) = query.order_by.as_ref() {
        let sqlparser::ast::OrderByKind::Expressions(expressions) = &order_by.kind else {
            return unsupported("information_schema.COLUMNS ORDER BY clause");
        };
        let [order] = expressions.as_slice() else {
            return unsupported("information_schema.COLUMNS ORDER BY clause");
        };
        if order_by.interpolate.is_some()
            || !an_ordering_these_rows_already_have(&order.options)
            || order.with_fill.is_some()
            || !matches!(
                &order.expr,
                Expr::Identifier(identifier) if is_identifier_named(identifier, "ORDINAL_POSITION")
            )
        {
            return unsupported("information_schema.COLUMNS ORDER BY clause");
        }
    }
    Ok((schema, table, columns))
}

/// The database one `information_schema.COLUMNS` query asks about.
///
/// `None` says it asked for the selected one with `DATABASE()`; a name says it
/// wrote the database out, which a migration tool that knows which database it
/// is working on does.
fn information_schema_columns_schema(expr: &Expr) -> Result<Option<String>, ParseError> {
    let Expr::BinaryOp {
        left,
        op: BinaryOperator::Eq,
        right,
    } = expr
    else {
        return unsupported("information_schema.COLUMNS WHERE clause");
    };
    if !matches!(left.as_ref(), Expr::Identifier(identifier) if is_identifier_named(identifier, "TABLE_SCHEMA"))
    {
        return unsupported("information_schema.COLUMNS WHERE clause");
    }
    if is_database_function(right) {
        return Ok(None);
    }
    let Expr::Value(value) = right.as_ref() else {
        return unsupported("information_schema.COLUMNS WHERE clause");
    };
    let Value::SingleQuotedString(name) = &value.value else {
        return unsupported("information_schema.COLUMNS WHERE clause");
    };
    Ok(Some(name.clone()))
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

/// Reports whether an `ORDER BY` clause asks for the order the rows come back
/// in anyway.
///
/// These rows are answered in one order whether or not the query asks for it,
/// so a clause naming that order says nothing. Measured on MySQL 8.4.11: an
/// explicit `ASC` reads the same rows as no `ASC` at all, and a `DESC` reads
/// them the other way round, which this does not do.
fn an_ordering_these_rows_already_have(options: &sqlparser::ast::OrderByOptions) -> bool {
    options.nulls_first.is_none() && matches!(options.asc, None | Some(true))
}
