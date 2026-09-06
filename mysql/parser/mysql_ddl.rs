//! Rendering in the MySQL direction: a checked Turso AST back out as the DDL
//! text a MySQL client expects.
//!
//! Everything here is the mirror of the SQLite-direction rendering in the crate
//! root, and it is kept apart because the two directions share almost nothing
//! but the error type: this side reads `turso_parser` ASTs and quotes with
//! backticks, the other reads `sqlparser` ASTs and quotes with double quotes.

use super::*;

/// Renders one checked Turso `CREATE TABLE` AST as normalized MySQL DDL.
///
/// This is deliberately a checked renderer. It accepts only the AST shapes that
/// [`parse_create_table_ast`] can produce, so callers cannot accidentally turn a
/// broader SQLite AST into MySQL DDL with different behavior.
pub fn render_create_table_mysql(statement: &Stmt) -> Result<String, ParseError> {
    render_create_table_mysql_with_mode(statement, SessionSqlMode::default())
}

/// Renders a checked Turso `CREATE TABLE` AST using one session's string rules.
pub fn render_create_table_mysql_with_mode(
    statement: &Stmt,
    mode: SessionSqlMode,
) -> Result<String, ParseError> {
    let Stmt::CreateTable {
        temporary,
        if_not_exists,
        tbl_name,
        body,
    } = statement
    else {
        return Err(ParseError::ExpectedCreateTable);
    };
    let TursoCreateTableBody::ColumnsAndConstraints {
        columns,
        constraints,
        options,
    } = body
    else {
        return unsupported("CREATE TABLE AS SELECT");
    };
    if options.without_rowid_text.is_some() || options.strict_text.is_some() {
        return unsupported("SQLite table options");
    }
    if columns.is_empty() {
        return unsupported("CREATE TABLE without columns");
    }

    let mut definitions = columns
        .iter()
        .map(|column| render_mysql_column(column, mode))
        .collect::<Result<Vec<_>, _>>()?;
    definitions.extend(
        constraints
            .iter()
            .map(|constraint| render_mysql_table_constraint(constraint, mode))
            .collect::<Result<Vec<_>, _>>()?,
    );

    let temporary = if *temporary { "TEMPORARY " } else { "" };
    let if_not_exists = if *if_not_exists { "IF NOT EXISTS " } else { "" };
    Ok(format!(
        "CREATE {temporary}TABLE {if_not_exists}{} ({})",
        render_mysql_qualified_name(tbl_name)?,
        definitions.join(", ")
    ))
}

/// Renders one checked Turso `CREATE INDEX` AST as normalized MySQL DDL.
pub fn render_create_index_mysql(statement: &Stmt) -> Result<String, ParseError> {
    render_create_index_mysql_with_mode(statement, SessionSqlMode::default())
}

/// Renders one checked Turso `CREATE INDEX` AST using one session's string rules.
pub fn render_create_index_mysql_with_mode(
    statement: &Stmt,
    _mode: SessionSqlMode,
) -> Result<String, ParseError> {
    let Stmt::CreateIndex {
        unique,
        if_not_exists,
        idx_name,
        tbl_name,
        using,
        columns,
        with_clause,
        where_clause,
    } = statement
    else {
        return Err(ParseError::ExpectedCreateIndex);
    };
    if *if_not_exists
        || idx_name.db_name.is_some()
        || idx_name.alias.is_some()
        || using.is_some()
        || !with_clause.is_empty()
        || where_clause.is_some()
    {
        return unsupported("CREATE INDEX option");
    }
    let columns = render_mysql_sorted_columns(columns)?;
    let unique = if *unique { "UNIQUE " } else { "" };
    Ok(format!(
        "CREATE {unique}INDEX {} ON {} ({columns})",
        render_mysql_name(&idx_name.name),
        render_mysql_name(tbl_name)
    ))
}

/// Renders one checked Turso `CREATE VIEW` AST as normalized MySQL DDL.
pub fn render_create_view_mysql(statement: &Stmt) -> Result<String, ParseError> {
    render_create_view_mysql_with_mode(statement, SessionSqlMode::default())
}

/// Renders one checked Turso `CREATE VIEW` AST using one session's string rules.
pub fn render_create_view_mysql_with_mode(
    statement: &Stmt,
    _mode: SessionSqlMode,
) -> Result<String, ParseError> {
    let Stmt::CreateView {
        temporary,
        if_not_exists,
        view_name,
        columns,
        select,
    } = statement
    else {
        return Err(ParseError::ExpectedCreateView);
    };
    if *temporary
        || *if_not_exists
        || view_name.db_name.is_some()
        || view_name.alias.is_some()
        || !columns.is_empty()
    {
        return unsupported("CREATE VIEW option");
    }
    Ok(format!(
        "CREATE VIEW {} AS {}",
        render_mysql_name(&view_name.name),
        render_mysql_view_select(select)?
    ))
}

/// Renders one checked Turso `CREATE TRIGGER` AST as normalized MySQL DDL.
pub fn render_create_trigger_mysql(statement: &Stmt) -> Result<String, ParseError> {
    render_create_trigger_mysql_with_mode(statement, SessionSqlMode::default())
}

/// Renders one checked Turso `CREATE TRIGGER` AST using one session's string rules.
pub fn render_create_trigger_mysql_with_mode(
    statement: &Stmt,
    mode: SessionSqlMode,
) -> Result<String, ParseError> {
    let Stmt::CreateTrigger {
        temporary,
        if_not_exists,
        trigger_name,
        time,
        event,
        tbl_name,
        for_each_row,
        when_clause,
        commands,
    } = statement
    else {
        return Err(ParseError::ExpectedCreateTrigger);
    };
    if *temporary
        || *if_not_exists
        || trigger_name.db_name.is_some()
        || trigger_name.alias.is_some()
        || tbl_name.db_name.is_some()
        || tbl_name.alias.is_some()
        || *time != Some(turso_parser::ast::TriggerTime::After)
        || !matches!(event, turso_parser::ast::TriggerEvent::Insert)
        || !*for_each_row
        || when_clause.is_some()
    {
        return unsupported("CREATE TRIGGER option");
    }
    let [turso_parser::ast::TriggerCmd::Insert {
        or_conflict: None,
        tbl_name: target_table,
        col_names,
        select,
        upsert: None,
        returning,
    }] = commands.as_slice()
    else {
        return unsupported("CREATE TRIGGER body");
    };
    if col_names.is_empty() || !returning.is_empty() {
        return unsupported("CREATE TRIGGER INSERT option");
    }
    if select.with.is_some() || !select.order_by.is_empty() || select.limit.is_some() {
        return unsupported("CREATE TRIGGER INSERT source");
    }
    let OneSelect::Values(rows) = &select.body.select else {
        return unsupported("CREATE TRIGGER INSERT SELECT");
    };
    if !select.body.compounds.is_empty() {
        return unsupported("CREATE TRIGGER INSERT source");
    }
    let [values] = rows.as_slice() else {
        return unsupported("CREATE TRIGGER INSERT VALUES rows");
    };
    if values.len() != col_names.len() {
        return unsupported("CREATE TRIGGER INSERT value count");
    }
    let values = values
        .iter()
        .map(|value| render_mysql_trigger_value(value, mode))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(format!(
        "CREATE TRIGGER {} AFTER INSERT ON {} FOR EACH ROW BEGIN INSERT INTO {} ({}) VALUES ({}); END",
        render_mysql_name(&trigger_name.name),
        render_mysql_name(&tbl_name.name),
        render_mysql_name(target_table),
        col_names
            .iter()
            .map(render_mysql_name)
            .collect::<Vec<_>>()
            .join(", "),
        values.join(", ")
    ))
}

fn render_mysql_trigger_value(
    value: &TursoExpr,
    mode: SessionSqlMode,
) -> Result<String, ParseError> {
    match value {
        TursoExpr::Qualified(prefix, column) if prefix.as_str().eq_ignore_ascii_case("NEW") => {
            Ok(format!("NEW.{}", render_mysql_name(column)))
        }
        TursoExpr::Literal(literal) => render_mysql_literal(literal, mode),
        _ => unsupported("CREATE TRIGGER value expression"),
    }
}

fn render_mysql_view_select(select: &turso_parser::ast::Select) -> Result<String, ParseError> {
    if select.with.is_some() || !select.order_by.is_empty() || select.limit.is_some() {
        return unsupported("CREATE VIEW query clause");
    }
    if !select.body.compounds.is_empty() {
        return unsupported("CREATE VIEW compound query");
    }
    let OneSelect::Select {
        distinctness,
        columns,
        from,
        where_clause,
        group_by,
        window_clause,
    } = &select.body.select
    else {
        return unsupported("CREATE VIEW query");
    };
    if distinctness.is_some()
        || where_clause.is_some()
        || group_by.is_some()
        || !window_clause.is_empty()
    {
        return unsupported("CREATE VIEW SELECT feature");
    }
    let Some(from) = from else {
        return unsupported("CREATE VIEW FROM clause");
    };
    if !from.joins.is_empty() {
        return unsupported("CREATE VIEW JOIN");
    }
    let SelectTable::Table(table_name, alias, indexed) = from.select.as_ref() else {
        return unsupported("CREATE VIEW table source");
    };
    if table_name.db_name.is_some()
        || table_name.alias.is_some()
        || alias.is_some()
        || indexed.is_some()
    {
        return unsupported("CREATE VIEW table option");
    }
    let columns = columns
        .iter()
        .map(|column| {
            let ResultColumn::Expr(expr, alias) = column else {
                return unsupported("CREATE VIEW projection");
            };
            if alias.as_ref().is_some_and(|alias| alias.is_explicit()) {
                return unsupported("CREATE VIEW projection alias");
            }
            let (TursoExpr::Name(name) | TursoExpr::Id(name)) = expr.as_ref() else {
                return unsupported("CREATE VIEW projection");
            };
            Ok(render_mysql_name(name))
        })
        .collect::<Result<Vec<_>, _>>()?;
    if columns.is_empty() {
        return unsupported("CREATE VIEW without projections");
    }
    Ok(format!(
        "SELECT {} FROM {}",
        columns.join(", "),
        render_mysql_name(&table_name.name)
    ))
}

fn render_mysql_qualified_name(name: &QualifiedName) -> Result<String, ParseError> {
    if name.alias.is_some() {
        return unsupported("table name alias");
    }
    match &name.db_name {
        Some(database) => Ok(format!(
            "{}.{}",
            render_mysql_name(database),
            render_mysql_name(&name.name)
        )),
        None => Ok(render_mysql_name(&name.name)),
    }
}

pub(crate) fn render_mysql_column(
    column: &turso_parser::ast::ColumnDefinition,
    mode: SessionSqlMode,
) -> Result<String, ParseError> {
    let data_type = render_mysql_type(column.col_type.as_ref())?;
    reject_duplicate_nullable_column_constraints(&column.constraints)?;
    let constraints = column
        .constraints
        .iter()
        .map(|constraint| render_mysql_column_constraint(constraint, mode))
        .collect::<Result<Vec<_>, _>>()?;
    let mut definition = format!("{} {data_type}", render_mysql_name(&column.col_name));
    if !constraints.is_empty() {
        definition.push(' ');
        definition.push_str(&constraints.join(" "));
    }
    Ok(definition)
}

fn reject_duplicate_nullable_column_constraints(
    constraints: &[NamedColumnConstraint],
) -> Result<(), ParseError> {
    let mut nullable_constraints = 0;
    for constraint in constraints {
        if matches!(
            &constraint.constraint,
            TursoColumnConstraint::NotNull { .. }
        ) {
            nullable_constraints += 1;
        }
    }
    if nullable_constraints > 1 {
        return unsupported("multiple column NULL constraints");
    }
    Ok(())
}

fn render_mysql_type(data_type: Option<&TursoType>) -> Result<String, ParseError> {
    let Some(data_type) = data_type else {
        return unsupported("column without type");
    };
    if data_type.array_dimensions != 0 {
        return unsupported("column type modifier");
    }
    for sized in ["VARCHAR", "CHAR", "VARBINARY"] {
        if data_type.name.eq_ignore_ascii_case(sized) {
            return Ok(format!("{sized}({})", stored_character_length(data_type)?));
        }
    }
    if data_type.name.eq_ignore_ascii_case("DECIMAL") {
        let (precision, scale) = stored_decimal_size(data_type)?;
        return Ok(format!("DECIMAL({precision},{scale})"));
    }
    if data_type.size.is_some() {
        return unsupported("column type modifier");
    }
    let name = if data_type.name.eq_ignore_ascii_case("TINYINT") {
        "TINYINT"
    } else if data_type.name.eq_ignore_ascii_case("SMALLINT") {
        "SMALLINT"
    } else if data_type.name.eq_ignore_ascii_case("MEDIUMINT") {
        "MEDIUMINT"
    } else if data_type.name.eq_ignore_ascii_case("BIGINT") {
        "BIGINT"
    } else if data_type.name.eq_ignore_ascii_case("INT") {
        "INT"
    } else if data_type.name.eq_ignore_ascii_case("INTEGER") {
        "INTEGER"
    } else if data_type.name.eq_ignore_ascii_case("TINYINT UNSIGNED") {
        "TINYINT UNSIGNED"
    } else if data_type.name.eq_ignore_ascii_case("SMALLINT UNSIGNED") {
        "SMALLINT UNSIGNED"
    } else if data_type.name.eq_ignore_ascii_case("MEDIUMINT UNSIGNED") {
        "MEDIUMINT UNSIGNED"
    } else if data_type.name.eq_ignore_ascii_case("INT UNSIGNED") {
        "INT UNSIGNED"
    } else if data_type.name.eq_ignore_ascii_case("INTEGER UNSIGNED") {
        "INTEGER UNSIGNED"
    } else if data_type.name.eq_ignore_ascii_case("TEXT") {
        "TEXT"
    } else if data_type.name.eq_ignore_ascii_case("BLOB") {
        "BLOB"
    } else if data_type.name.eq_ignore_ascii_case("DOUBLE") {
        "DOUBLE"
    } else if data_type.name.eq_ignore_ascii_case("FLOAT") {
        "FLOAT"
    } else if data_type.name.eq_ignore_ascii_case("BOOLEAN") {
        "BOOLEAN"
    } else if data_type.name.eq_ignore_ascii_case("DATETIME") {
        "DATETIME"
    } else if data_type.name.eq_ignore_ascii_case("TIMESTAMP") {
        "TIMESTAMP"
    } else {
        return unsupported("column type");
    };
    Ok(name.to_owned())
}

/// Reads the character count from a sized text type already stored as SQLite
/// DDL.
///
/// The engine keeps the declared size as an expression, so this accepts only
/// the one shape this frontend writes: a single integer literal.
pub fn stored_character_length(data_type: &TursoType) -> Result<u32, ParseError> {
    let Some(TursoTypeSize::MaxSize(length)) = data_type.size.as_ref() else {
        return unsupported("VARCHAR without a length");
    };
    let TursoExpr::Literal(TursoLiteral::Numeric(text)) = length.as_ref() else {
        return unsupported("VARCHAR length");
    };
    let length = text.parse::<u64>().map_err(|_| ParseError::Unsupported {
        feature: "VARCHAR length",
    })?;
    if length == 0 || length > MAX_VARCHAR_CHARACTERS {
        return unsupported("VARCHAR length");
    }
    u32::try_from(length).map_err(|_| ParseError::Unsupported {
        feature: "VARCHAR length",
    })
}

fn render_mysql_column_constraint(
    constraint: &NamedColumnConstraint,
    mode: SessionSqlMode,
) -> Result<String, ParseError> {
    let name = render_mysql_constraint_name(constraint.name.as_ref());
    match &constraint.constraint {
        TursoColumnConstraint::NotNull {
            nullable: true,
            conflict_clause: None,
        } if constraint.name.is_none() => Ok("NULL".to_owned()),
        TursoColumnConstraint::NotNull {
            nullable: false,
            conflict_clause: None,
        } if constraint.name.is_none() => Ok("NOT NULL".to_owned()),
        TursoColumnConstraint::Unique(None) => Ok(format!("{name}UNIQUE")),
        TursoColumnConstraint::Default(expr) if constraint.name.is_none() => {
            Ok(format!("DEFAULT {}", render_mysql_default(expr, mode)?))
        }
        TursoColumnConstraint::Check { expr, .. } => {
            Ok(format!("{name}CHECK {}", render_mysql_check(expr, mode)?))
        }
        TursoColumnConstraint::PrimaryKey { .. } => unsupported("PRIMARY KEY"),
        TursoColumnConstraint::ForeignKey { .. } => unsupported("column REFERENCES constraint"),
        TursoColumnConstraint::Default(_) => unsupported("named DEFAULT constraint"),
        _ => unsupported("column attribute"),
    }
}

fn render_mysql_table_constraint(
    constraint: &NamedTableConstraint,
    mode: SessionSqlMode,
) -> Result<String, ParseError> {
    let name = render_mysql_constraint_name(constraint.name.as_ref());
    match &constraint.constraint {
        TursoTableConstraint::Unique {
            columns,
            conflict_clause: None,
        } => Ok(format!(
            "{name}UNIQUE ({})",
            render_mysql_sorted_columns(columns)?
        )),
        TursoTableConstraint::Check { expr, .. } => {
            Ok(format!("{name}CHECK {}", render_mysql_check(expr, mode)?))
        }
        TursoTableConstraint::ForeignKey {
            columns,
            clause,
            defer_clause: None,
        } => Ok(format!(
            "{name}FOREIGN KEY ({}) {}",
            render_mysql_indexed_columns(columns)?,
            render_mysql_foreign_key(clause)?
        )),
        TursoTableConstraint::PrimaryKey { .. } => unsupported("PRIMARY KEY"),
        TursoTableConstraint::Unique { .. } => unsupported("UNIQUE conflict clause"),
        TursoTableConstraint::ForeignKey { .. } => unsupported("FOREIGN KEY attribute"),
    }
}

fn render_mysql_sorted_columns(
    columns: &[turso_parser::ast::SortedColumn],
) -> Result<String, ParseError> {
    if columns.is_empty() {
        return unsupported("empty UNIQUE column list");
    }
    columns
        .iter()
        .map(|column| {
            if column.order.is_some() || column.nulls.is_some() {
                return unsupported("indexed column ordering");
            }
            let TursoExpr::Id(name) = column.expr.as_ref() else {
                return unsupported("indexed column expression");
            };
            Ok(render_mysql_name(name))
        })
        .collect::<Result<Vec<_>, _>>()
        .map(|columns| columns.join(", "))
}

fn render_mysql_indexed_columns(
    columns: &[turso_parser::ast::IndexedColumn],
) -> Result<String, ParseError> {
    if columns.is_empty() {
        return unsupported("empty FOREIGN KEY column list");
    }
    columns
        .iter()
        .map(|column| {
            if column.collation_name.is_some() || column.order.is_some() {
                return unsupported("FOREIGN KEY column attribute");
            }
            Ok(render_mysql_name(&column.col_name))
        })
        .collect::<Result<Vec<_>, _>>()
        .map(|columns| columns.join(", "))
}

fn render_mysql_foreign_key(
    clause: &turso_parser::ast::ForeignKeyClause,
) -> Result<String, ParseError> {
    let columns = if clause.columns.is_empty() {
        String::new()
    } else {
        format!(" ({})", render_mysql_indexed_columns(&clause.columns)?)
    };
    let mut arguments = Vec::with_capacity(clause.args.len());
    let mut has_delete = false;
    let mut has_update = false;
    for argument in &clause.args {
        match argument {
            RefArg::OnDelete(action) if !has_delete => {
                has_delete = true;
                arguments.push(format!("ON DELETE {}", render_mysql_ref_action(*action)));
            }
            RefArg::OnUpdate(action) if !has_update => {
                has_update = true;
                arguments.push(format!("ON UPDATE {}", render_mysql_ref_action(*action)));
            }
            RefArg::OnInsert(_) | RefArg::Match(_) => return unsupported("FOREIGN KEY attribute"),
            _ => return unsupported("duplicate FOREIGN KEY action"),
        }
    }
    let arguments = if arguments.is_empty() {
        String::new()
    } else {
        format!(" {}", arguments.join(" "))
    };
    Ok(format!(
        "REFERENCES {}{columns}{arguments}",
        render_mysql_name(&clause.tbl_name)
    ))
}

fn render_mysql_ref_action(action: RefAct) -> &'static str {
    match action {
        RefAct::SetNull => "SET NULL",
        RefAct::SetDefault => "SET DEFAULT",
        RefAct::Cascade => "CASCADE",
        RefAct::Restrict => "RESTRICT",
        RefAct::NoAction => "NO ACTION",
    }
}

fn render_mysql_default(expr: &TursoExpr, mode: SessionSqlMode) -> Result<String, ParseError> {
    match expr {
        TursoExpr::Literal(literal) => render_mysql_literal(literal, mode),
        TursoExpr::Unary(operator, expression) => {
            let sign = match operator {
                TursoUnaryOperator::Positive => "+",
                TursoUnaryOperator::Negative => "-",
                _ => return unsupported("DEFAULT integer literal"),
            };
            let TursoExpr::Literal(TursoLiteral::Numeric(value)) = expression.as_ref() else {
                return unsupported("DEFAULT integer literal");
            };
            render_signed_integer_default(sign, value)
        }
        _ => unsupported("non-literal DEFAULT expression"),
    }
}

fn render_mysql_check(expr: &TursoExpr, mode: SessionSqlMode) -> Result<String, ParseError> {
    let expression = render_mysql_expr(expr, mode)?;
    if expression.starts_with('(') && expression.ends_with(')') {
        Ok(expression)
    } else {
        Ok(format!("({expression})"))
    }
}

fn render_mysql_expr(expr: &TursoExpr, mode: SessionSqlMode) -> Result<String, ParseError> {
    match expr {
        TursoExpr::Id(name) => Ok(render_mysql_name(name)),
        TursoExpr::Qualified(table, column) => Ok(format!(
            "{}.{}",
            render_mysql_name(table),
            render_mysql_name(column)
        )),
        TursoExpr::DoublyQualified(database, table, column) => Ok(format!(
            "{}.{}.{}",
            render_mysql_name(database),
            render_mysql_name(table),
            render_mysql_name(column)
        )),
        TursoExpr::Literal(literal) => render_mysql_literal(literal, mode),
        TursoExpr::Parenthesized(expressions) if expressions.len() == 1 => {
            Ok(format!("({})", render_mysql_expr(&expressions[0], mode)?))
        }
        TursoExpr::Unary(operator, expression) => {
            let operator = match operator {
                TursoUnaryOperator::Positive => "+",
                TursoUnaryOperator::Negative => "-",
                _ => return unsupported("CHECK unary operator"),
            };
            Ok(format!(
                "({operator}{})",
                render_mysql_expr(expression, mode)?
            ))
        }
        TursoExpr::Binary(left, operator, right) => Ok(format!(
            "({} {} {})",
            render_mysql_expr(left, mode)?,
            render_mysql_operator(*operator)?,
            render_mysql_expr(right, mode)?
        )),
        TursoExpr::IsNull(expression) => Ok(format!(
            "({} IS NULL)",
            render_mysql_expr(expression, mode)?
        )),
        TursoExpr::NotNull(expression) => Ok(format!(
            "({} IS NOT NULL)",
            render_mysql_expr(expression, mode)?
        )),
        TursoExpr::Between {
            lhs,
            not: false,
            start,
            end,
        } => Ok(format!(
            "({} BETWEEN {} AND {})",
            render_mysql_expr(lhs, mode)?,
            render_mysql_expr(start, mode)?,
            render_mysql_expr(end, mode)?
        )),
        TursoExpr::InList { lhs, not, rhs, .. } if !rhs.is_empty() => Ok(format!(
            "({} {}IN ({}))",
            render_mysql_expr(lhs, mode)?,
            if *not { "NOT " } else { "" },
            rhs.iter()
                .map(|expression| render_mysql_expr(expression, mode))
                .collect::<Result<Vec<_>, _>>()?
                .join(", ")
        )),
        _ => unsupported("CHECK expression"),
    }
}

fn render_mysql_operator(operator: TursoOperator) -> Result<&'static str, ParseError> {
    match operator {
        TursoOperator::Add => Ok("+"),
        TursoOperator::Subtract => Ok("-"),
        TursoOperator::Multiply => Ok("*"),
        TursoOperator::Modulus => Ok("%"),
        TursoOperator::Greater => Ok(">"),
        TursoOperator::Less => Ok("<"),
        TursoOperator::GreaterEquals => Ok(">="),
        TursoOperator::LessEquals => Ok("<="),
        TursoOperator::Equals => Ok("="),
        TursoOperator::NotEquals => Ok("<>"),
        TursoOperator::And => Ok("AND"),
        TursoOperator::Or => Ok("OR"),
        _ => unsupported("CHECK binary operator"),
    }
}

fn render_mysql_literal(
    literal: &TursoLiteral,
    mode: SessionSqlMode,
) -> Result<String, ParseError> {
    match literal {
        TursoLiteral::Numeric(value) => Ok(value.clone()),
        TursoLiteral::String(value) => render_mysql_string_literal(value, mode),
        TursoLiteral::Null => Ok("NULL".to_string()),
        TursoLiteral::True => Ok("TRUE".to_string()),
        TursoLiteral::False => Ok("FALSE".to_string()),
        _ => unsupported("literal"),
    }
}

fn render_mysql_string_literal(value: &str, mode: SessionSqlMode) -> Result<String, ParseError> {
    let Some(content) = value
        .strip_prefix('\'')
        .and_then(|value| value.strip_suffix('\''))
    else {
        return unsupported("non-single-quoted string literal");
    };
    let mut normalized = String::with_capacity(value.len());
    let mut chars = content.chars();
    while let Some(ch) = chars.next() {
        if ch == '\'' && chars.next() != Some('\'') {
            return unsupported("malformed string literal");
        }
        normalized.push(ch);
    }
    let normalized = if mode.no_backslash_escapes {
        normalized
    } else {
        normalized.replace('\\', "\\\\")
    };
    Ok(format!("'{}'", normalized.replace('\'', "''")))
}

fn render_mysql_constraint_name(name: Option<&TursoName>) -> String {
    name.map(|name| format!("CONSTRAINT {} ", render_mysql_name(name)))
        .unwrap_or_default()
}

fn render_mysql_name(name: &TursoName) -> String {
    format!("`{}`", name.as_str().replace('`', "``"))
}
