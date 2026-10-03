use super::*;

#[derive(Debug, Default)]
pub(crate) struct DerivedMembership {
    read_through_derived_tables: Vec<(String, String)>,
    read_directly: Vec<(String, String)>,
}

impl DerivedMembership {
    pub(crate) fn reads_through_a_derived_table(&self, table: &str, reference: &str) -> bool {
        let named = |tables: &[(String, String)]| {
            tables
                .iter()
                .any(|(read, _)| read.eq_ignore_ascii_case(table))
        };
        !named(&self.read_directly)
            && self
                .read_through_derived_tables
                .iter()
                .any(|(read, under)| {
                    read.eq_ignore_ascii_case(table) && under.eq_ignore_ascii_case(reference)
                })
    }
}

pub(crate) fn write_derived_tables_out_of_membership_tests(
    condition: &mut Expr,
    found: &mut DerivedMembership,
) {
    match condition {
        Expr::InSubquery { subquery, .. } => write_out_of_the_subquery(subquery, found),
        Expr::BinaryOp { left, right, .. } => {
            write_derived_tables_out_of_membership_tests(left, found);
            write_derived_tables_out_of_membership_tests(right, found);
        }
        Expr::Nested(inner) | Expr::UnaryOp { expr: inner, .. } => {
            write_derived_tables_out_of_membership_tests(inner, found);
        }
        Expr::Exists { subquery, .. } | Expr::Subquery(subquery) => {
            note_the_tables_read(subquery, &mut found.read_directly);
        }
        _ => {}
    }
}

fn write_out_of_the_subquery(subquery: &mut sqlparser::ast::Query, found: &mut DerivedMembership) {
    if let Some(body) = body_passing_its_column_through(subquery) {
        note_the_tables_read(&body, &mut found.read_through_derived_tables);
        *subquery = body;
    } else {
        note_the_tables_read(subquery, &mut found.read_directly);
    }
    let SetExpr::Select(select) = subquery.body.as_mut() else {
        return;
    };
    for source in &mut select.from {
        write_whole_table_out(&mut source.relation, found);
        for join in &mut source.joins {
            write_whole_table_out(&mut join.relation, found);
        }
    }
    if let Some(selection) = &mut select.selection {
        write_derived_tables_out_of_membership_tests(selection, found);
    }
}

fn body_passing_its_column_through(
    subquery: &sqlparser::ast::Query,
) -> Option<sqlparser::ast::Query> {
    if !reads_its_body_as_written(subquery) {
        return None;
    }
    let SetExpr::Select(select) = subquery.body.as_ref() else {
        return None;
    };
    if !selects_nothing_but_its_source(select) {
        return None;
    }
    let [sqlparser::ast::TableWithJoins { relation, joins }] = select.from.as_slice() else {
        return None;
    };
    let TableFactor::Derived {
        lateral: false,
        subquery: body,
        alias: Some(alias),
        sample: None,
    } = relation
    else {
        return None;
    };
    if !joins.is_empty() || !alias.columns.is_empty() || !reads_its_body_as_written(body) {
        return None;
    }
    let column = match select.projection.as_slice() {
        [SelectItem::UnnamedExpr(Expr::Identifier(column))] => column,
        [SelectItem::UnnamedExpr(Expr::CompoundIdentifier(parts))]
            if parts.len() == 2 && parts[0].value.eq_ignore_ascii_case(&alias.name.value) =>
        {
            &parts[1]
        }
        _ => return None,
    };
    let SetExpr::Select(inner) = body.body.as_ref() else {
        return None;
    };
    if !groups_nothing(inner) || inner.having.is_some() {
        return None;
    }
    let mut answering = inner
        .projection
        .iter()
        .filter_map(|item| answered_as(item, &column.value));
    let (Some(expr), None) = (answering.next(), answering.next()) else {
        return None;
    };
    let mut written_out = body.as_ref().clone();
    let SetExpr::Select(inner) = written_out.body.as_mut() else {
        unreachable!("the body was read as one SELECT above");
    };
    inner.projection = vec![SelectItem::UnnamedExpr(expr.clone())];
    Some(written_out)
}

fn reads_its_body_as_written(query: &sqlparser::ast::Query) -> bool {
    query.with.is_none()
        && query.order_by.is_none()
        && query.limit_clause.is_none()
        && query.fetch.is_none()
        && query.locks.is_empty()
        && query.for_clause.is_none()
        && query.settings.is_none()
        && query.format_clause.is_none()
        && query.pipe_operators.is_empty()
}

fn selects_nothing_but_its_source(select: &sqlparser::ast::Select) -> bool {
    select.distinct.is_none()
        && select.selection.is_none()
        && select.having.is_none()
        && select.qualify.is_none()
        && select.named_window.is_empty()
        && groups_nothing(select)
}

fn groups_nothing(select: &sqlparser::ast::Select) -> bool {
    matches!(
        &select.group_by,
        sqlparser::ast::GroupByExpr::Expressions(keys, modifiers)
            if keys.is_empty() && modifiers.is_empty()
    )
}

fn answered_as<'a>(item: &'a SelectItem, name: &str) -> Option<&'a Expr> {
    match item {
        SelectItem::ExprWithAlias { expr, alias } if alias.value.eq_ignore_ascii_case(name) => {
            Some(expr)
        }
        SelectItem::UnnamedExpr(expr @ Expr::Identifier(column))
            if column.value.eq_ignore_ascii_case(name) =>
        {
            Some(expr)
        }
        SelectItem::UnnamedExpr(expr @ Expr::CompoundIdentifier(parts))
            if parts.len() == 2 && parts[1].value.eq_ignore_ascii_case(name) =>
        {
            Some(expr)
        }
        _ => None,
    }
}

fn write_whole_table_out(relation: &mut TableFactor, found: &mut DerivedMembership) {
    let TableFactor::Derived {
        lateral: false,
        subquery,
        alias: Some(alias),
        sample: None,
    } = relation
    else {
        return;
    };
    if !alias.columns.is_empty() || !reads_its_body_as_written(subquery) {
        return;
    }
    let SetExpr::Select(select) = subquery.body.as_ref() else {
        return;
    };
    let reads_every_column = matches!(
        select.projection.as_slice(),
        [SelectItem::Wildcard(options)] if wildcard_options_are_empty(options)
    );
    if !selects_nothing_but_its_source(select) || !reads_every_column {
        return;
    }
    let [sqlparser::ast::TableWithJoins {
        relation: table,
        joins,
    }] = select.from.as_slice()
    else {
        return;
    };
    let TableFactor::Table {
        name,
        alias: None,
        args: None,
        with_hints,
        version: None,
        with_ordinality: false,
        partitions,
        json_path: None,
        sample: None,
        index_hints,
    } = table
    else {
        return;
    };
    if !joins.is_empty()
        || !with_hints.is_empty()
        || !partitions.is_empty()
        || !index_hints.is_empty()
    {
        return;
    }
    note_table_name(name, Some(alias), &mut found.read_through_derived_tables);
    *relation = TableFactor::Table {
        name: name.clone(),
        alias: Some(alias.clone()),
        args: None,
        with_hints: Vec::new(),
        version: None,
        with_ordinality: false,
        partitions: Vec::new(),
        json_path: None,
        sample: None,
        index_hints: Vec::new(),
    };
}

fn note_the_tables_read(query: &sqlparser::ast::Query, tables: &mut Vec<(String, String)>) {
    let SetExpr::Select(select) = query.body.as_ref() else {
        return;
    };
    for source in &select.from {
        let relations =
            std::iter::once(&source.relation).chain(source.joins.iter().map(|join| &join.relation));
        for relation in relations {
            if let TableFactor::Table { name, alias, .. } = relation {
                note_table_name(name, alias.as_ref(), tables);
            }
        }
    }
}

fn note_table_name(
    name: &ObjectName,
    alias: Option<&sqlparser::ast::TableAlias>,
    tables: &mut Vec<(String, String)>,
) {
    if let Some(ObjectNamePart::Identifier(table)) = name.0.last() {
        let reference = alias.map_or(&table.value, |alias| &alias.name.value);
        tables.push((table.value.clone(), reference.clone()));
    }
}
