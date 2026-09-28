//! Translating a checked MySQL `SELECT`, `INSERT`, `UPDATE` or `DELETE` into
//! the SQLite text the engine runs.
//!
//! This is the widest single job in the crate. A `SELECT` has to be rewritten
//! clause by clause, and along the way it has to record what the frontend
//! cannot see later: which table each result column came from, and which
//! projections have a shape the server still has to resolve into a wire type.
//!
//! The DML statements sit here too rather than in a file of their own, because
//! their `WHERE` clause is rendered by the same code, through the same
//! [`SelectRenderContext`].

use super::*;

pub(crate) use grouping::name_the_columns_grouped_by_place;
pub(crate) use one_table_columns::leave_the_one_table_out;

mod derived;
mod grouping;
mod json_condition;
mod one_table_columns;
mod recursive;
mod rollup;

pub use derived::{MySqlDerivedColumns, MySqlJoinedDerivedColumn};
pub use grouping::{MySqlColumnsTheKeysDecide, MySqlJoinedTable, MySqlNamedColumn};

/// One table a `SELECT` reads, with the name the engine reports for it.
///
/// A join reports each column against the reference in the statement, which is
/// the alias when there is one, so both spellings have to be kept.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MySqlSelectSource {
    reference: String,
    table: MySqlTableName,
    outer: bool,
    /// Whether a subquery standing as a result column names this table's
    /// columns from inside it.
    read_by_a_result_subquery: bool,
    branch: usize,
    subquery: bool,
    projected_columns: Vec<String>,
    /// What a derived table or a CTE projects, when this is one.
    derived: Option<MySqlDerivedColumns>,
    catalog: Option<MySqlCatalogTable>,
    hinted_indexes: Vec<String>,
}

/// One `information_schema` table, which the engine scans and whose columns
/// have shapes of their own rather than shapes read out of stored DDL.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MySqlCatalogTable {
    Tables,
    Views,
    Statistics,
    KeyColumnUsage,
    TableConstraints,
    ReferentialConstraints,
    Routines,
    Columns,
    Schemata,
    CheckConstraints,
    Events,
    Triggers,
}

impl MySqlCatalogTable {
    const ALL: [Self; 12] = [
        Self::Tables,
        Self::Views,
        Self::Statistics,
        Self::KeyColumnUsage,
        Self::TableConstraints,
        Self::ReferentialConstraints,
        Self::Routines,
        Self::Columns,
        Self::Schemata,
        Self::CheckConstraints,
        Self::Events,
        Self::Triggers,
    ];

    /// The name the engine knows this table by, which has no qualifier.
    pub const fn engine_name(self) -> &'static str {
        match self {
            Self::Tables => "mysql_information_schema_tables",
            Self::Views => "mysql_information_schema_views",
            Self::Statistics => "mysql_information_schema_statistics",
            Self::KeyColumnUsage => "mysql_information_schema_key_column_usage",
            Self::TableConstraints => "mysql_information_schema_table_constraints",
            Self::ReferentialConstraints => "mysql_information_schema_referential_constraints",
            Self::Routines => "mysql_information_schema_routines",
            Self::Columns => "mysql_information_schema_columns",
            Self::Schemata => "mysql_information_schema_schemata",
            Self::CheckConstraints => "mysql_information_schema_check_constraints",
            Self::Events => "mysql_information_schema_events",
            Self::Triggers => "mysql_information_schema_triggers",
        }
    }

    /// Answers whether the session works out this table's rows before a
    /// statement that scans it runs, rather than the table reading them out
    /// of the schema itself.
    pub const fn rows_come_from_the_session(self) -> bool {
        matches!(self, Self::Columns | Self::Schemata)
    }

    /// Answers whether this table holds every column MySQL gives it, which is
    /// what a wildcard over it asks for.
    ///
    /// `TABLES` leaves out columns this server has nothing true to answer
    /// with — storage statistics and times — so a wildcard over it would
    /// answer a row narrower than MySQL's.
    pub const fn answers_every_column(self) -> bool {
        !matches!(self, Self::Tables)
    }

    /// The columns this table answers, in the order MySQL declares them, each
    /// with the MySQL type a value compared against it has to fit.
    pub const fn columns(self) -> &'static [(&'static str, &'static str)] {
        match self {
            Self::Tables => &[
                ("TABLE_SCHEMA", "TEXT"),
                ("TABLE_NAME", "TEXT"),
                ("TABLE_TYPE", "TEXT"),
                ("ENGINE", "TEXT"),
                ("DATA_LENGTH", "BIGINT UNSIGNED"),
                ("INDEX_LENGTH", "BIGINT UNSIGNED"),
                ("TABLE_COLLATION", "TEXT"),
                ("TABLE_COMMENT", "TEXT"),
            ],
            Self::Views => &[
                ("TABLE_CATALOG", "TEXT"),
                ("TABLE_SCHEMA", "TEXT"),
                ("TABLE_NAME", "TEXT"),
                ("VIEW_DEFINITION", "TEXT"),
                ("CHECK_OPTION", "TEXT"),
                ("IS_UPDATABLE", "TEXT"),
                ("DEFINER", "TEXT"),
                ("SECURITY_TYPE", "TEXT"),
                ("CHARACTER_SET_CLIENT", "TEXT"),
                ("COLLATION_CONNECTION", "TEXT"),
            ],
            Self::Statistics => &[
                ("TABLE_CATALOG", "TEXT"),
                ("TABLE_SCHEMA", "TEXT"),
                ("TABLE_NAME", "TEXT"),
                ("NON_UNIQUE", "INT"),
                ("INDEX_SCHEMA", "TEXT"),
                ("INDEX_NAME", "TEXT"),
                ("SEQ_IN_INDEX", "INT"),
                ("COLUMN_NAME", "TEXT"),
                ("COLLATION", "TEXT"),
                ("CARDINALITY", "BIGINT"),
                ("SUB_PART", "BIGINT"),
                ("PACKED", "TEXT"),
                ("NULLABLE", "TEXT"),
                ("INDEX_TYPE", "TEXT"),
                ("COMMENT", "TEXT"),
                ("INDEX_COMMENT", "TEXT"),
                ("IS_VISIBLE", "TEXT"),
                ("EXPRESSION", "TEXT"),
            ],
            Self::KeyColumnUsage => &[
                ("CONSTRAINT_CATALOG", "TEXT"),
                ("CONSTRAINT_SCHEMA", "TEXT"),
                ("CONSTRAINT_NAME", "TEXT"),
                ("TABLE_CATALOG", "TEXT"),
                ("TABLE_SCHEMA", "TEXT"),
                ("TABLE_NAME", "TEXT"),
                ("COLUMN_NAME", "TEXT"),
                ("ORDINAL_POSITION", "INT"),
                ("POSITION_IN_UNIQUE_CONSTRAINT", "INT"),
                ("REFERENCED_TABLE_SCHEMA", "TEXT"),
                ("REFERENCED_TABLE_NAME", "TEXT"),
                ("REFERENCED_COLUMN_NAME", "TEXT"),
            ],
            Self::TableConstraints => &[
                ("CONSTRAINT_CATALOG", "TEXT"),
                ("CONSTRAINT_SCHEMA", "TEXT"),
                ("CONSTRAINT_NAME", "TEXT"),
                ("TABLE_SCHEMA", "TEXT"),
                ("TABLE_NAME", "TEXT"),
                ("CONSTRAINT_TYPE", "TEXT"),
                ("ENFORCED", "TEXT"),
            ],
            Self::ReferentialConstraints => &[
                ("CONSTRAINT_CATALOG", "TEXT"),
                ("CONSTRAINT_SCHEMA", "TEXT"),
                ("CONSTRAINT_NAME", "TEXT"),
                ("UNIQUE_CONSTRAINT_CATALOG", "TEXT"),
                ("UNIQUE_CONSTRAINT_SCHEMA", "TEXT"),
                ("UNIQUE_CONSTRAINT_NAME", "TEXT"),
                ("MATCH_OPTION", "TEXT"),
                ("UPDATE_RULE", "TEXT"),
                ("DELETE_RULE", "TEXT"),
                ("TABLE_NAME", "TEXT"),
                ("REFERENCED_TABLE_NAME", "TEXT"),
            ],
            Self::Routines => &[
                ("SPECIFIC_NAME", "TEXT"),
                ("ROUTINE_CATALOG", "TEXT"),
                ("ROUTINE_SCHEMA", "TEXT"),
                ("ROUTINE_NAME", "TEXT"),
                ("ROUTINE_TYPE", "TEXT"),
                ("DATA_TYPE", "TEXT"),
                ("CHARACTER_MAXIMUM_LENGTH", "BIGINT"),
                ("CHARACTER_OCTET_LENGTH", "BIGINT"),
                ("NUMERIC_PRECISION", "INT UNSIGNED"),
                ("NUMERIC_SCALE", "INT UNSIGNED"),
                ("DATETIME_PRECISION", "INT UNSIGNED"),
                ("CHARACTER_SET_NAME", "TEXT"),
                ("COLLATION_NAME", "TEXT"),
                ("DTD_IDENTIFIER", "TEXT"),
                ("ROUTINE_BODY", "TEXT"),
                ("ROUTINE_DEFINITION", "TEXT"),
                ("EXTERNAL_NAME", "TEXT"),
                ("EXTERNAL_LANGUAGE", "TEXT"),
                ("PARAMETER_STYLE", "TEXT"),
                ("IS_DETERMINISTIC", "TEXT"),
                ("SQL_DATA_ACCESS", "TEXT"),
                ("SQL_PATH", "TEXT"),
                ("SECURITY_TYPE", "TEXT"),
                ("CREATED", "DATETIME"),
                ("LAST_ALTERED", "DATETIME"),
                ("SQL_MODE", "TEXT"),
                ("ROUTINE_COMMENT", "TEXT"),
                ("DEFINER", "TEXT"),
                ("CHARACTER_SET_CLIENT", "TEXT"),
                ("COLLATION_CONNECTION", "TEXT"),
                ("DATABASE_COLLATION", "TEXT"),
            ],
            Self::Columns => &[
                ("TABLE_CATALOG", "TEXT"),
                ("TABLE_SCHEMA", "TEXT"),
                ("TABLE_NAME", "TEXT"),
                ("COLUMN_NAME", "TEXT"),
                ("ORDINAL_POSITION", "INT UNSIGNED"),
                ("COLUMN_DEFAULT", "TEXT"),
                ("IS_NULLABLE", "TEXT"),
                ("DATA_TYPE", "TEXT"),
                ("CHARACTER_MAXIMUM_LENGTH", "BIGINT"),
                ("CHARACTER_OCTET_LENGTH", "BIGINT"),
                ("NUMERIC_PRECISION", "BIGINT UNSIGNED"),
                ("NUMERIC_SCALE", "BIGINT UNSIGNED"),
                ("DATETIME_PRECISION", "INT UNSIGNED"),
                ("CHARACTER_SET_NAME", "TEXT"),
                ("COLLATION_NAME", "TEXT"),
                ("COLUMN_TYPE", "TEXT"),
                ("COLUMN_KEY", "TEXT"),
                ("EXTRA", "TEXT"),
                ("PRIVILEGES", "TEXT"),
                ("COLUMN_COMMENT", "TEXT"),
                ("GENERATION_EXPRESSION", "TEXT"),
                ("SRS_ID", "INT UNSIGNED"),
            ],
            Self::CheckConstraints => &[
                ("CONSTRAINT_CATALOG", "TEXT"),
                ("CONSTRAINT_SCHEMA", "TEXT"),
                ("CONSTRAINT_NAME", "TEXT"),
                ("CHECK_CLAUSE", "TEXT"),
            ],
            Self::Triggers => &[
                ("TRIGGER_CATALOG", "TEXT"),
                ("TRIGGER_SCHEMA", "TEXT"),
                ("TRIGGER_NAME", "TEXT"),
                ("EVENT_MANIPULATION", "TEXT"),
                ("EVENT_OBJECT_CATALOG", "TEXT"),
                ("EVENT_OBJECT_SCHEMA", "TEXT"),
                ("EVENT_OBJECT_TABLE", "TEXT"),
                ("ACTION_ORDER", "INT UNSIGNED"),
                ("ACTION_CONDITION", "TEXT"),
                ("ACTION_STATEMENT", "TEXT"),
                ("ACTION_ORIENTATION", "TEXT"),
                ("ACTION_TIMING", "TEXT"),
                ("ACTION_REFERENCE_OLD_TABLE", "TEXT"),
                ("ACTION_REFERENCE_NEW_TABLE", "TEXT"),
                ("ACTION_REFERENCE_OLD_ROW", "TEXT"),
                ("ACTION_REFERENCE_NEW_ROW", "TEXT"),
                ("CREATED", "DATETIME"),
                ("SQL_MODE", "TEXT"),
                ("DEFINER", "TEXT"),
                ("CHARACTER_SET_CLIENT", "TEXT"),
                ("COLLATION_CONNECTION", "TEXT"),
                ("DATABASE_COLLATION", "TEXT"),
            ],
            Self::Events => &[
                ("EVENT_CATALOG", "TEXT"),
                ("EVENT_SCHEMA", "TEXT"),
                ("EVENT_NAME", "TEXT"),
                ("DEFINER", "TEXT"),
                ("TIME_ZONE", "TEXT"),
                ("EVENT_BODY", "TEXT"),
                ("EVENT_DEFINITION", "TEXT"),
                ("EVENT_TYPE", "TEXT"),
                ("EXECUTE_AT", "DATETIME"),
                ("INTERVAL_VALUE", "TEXT"),
                ("INTERVAL_FIELD", "TEXT"),
                ("SQL_MODE", "TEXT"),
                ("STARTS", "DATETIME"),
                ("ENDS", "DATETIME"),
                ("STATUS", "TEXT"),
                ("ON_COMPLETION", "TEXT"),
                ("CREATED", "DATETIME"),
                ("LAST_ALTERED", "DATETIME"),
                ("LAST_EXECUTED", "DATETIME"),
                ("EVENT_COMMENT", "TEXT"),
                ("ORIGINATOR", "INT UNSIGNED"),
                ("CHARACTER_SET_CLIENT", "TEXT"),
                ("COLLATION_CONNECTION", "TEXT"),
                ("DATABASE_COLLATION", "TEXT"),
            ],
            Self::Schemata => &[
                ("CATALOG_NAME", "TEXT"),
                ("SCHEMA_NAME", "TEXT"),
                ("DEFAULT_CHARACTER_SET_NAME", "TEXT"),
                ("DEFAULT_COLLATION_NAME", "TEXT"),
                ("SQL_PATH", "TEXT"),
                ("DEFAULT_ENCRYPTION", "TEXT"),
            ],
        }
    }

    /// Reads a table back from the name MySQL reports as a column's original
    /// table, which is the upper-case name MySQL declares it under — a name
    /// no table this server makes can have, every one being folded to lower
    /// case.
    pub fn from_reported_name(name: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|table| table.mysql_name() == name)
    }

    /// Reads a table back from the name the engine knows it by.
    pub fn from_engine_name(name: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|table| table.engine_name().eq_ignore_ascii_case(name))
    }

    /// Returns the type a value compared against one of these columns has to
    /// fit, or nothing when this table has no such column.
    pub fn column_type(self, name: &str) -> Option<&'static str> {
        self.columns()
            .iter()
            .find(|(column, _)| column.eq_ignore_ascii_case(name))
            .map(|(_, declared)| *declared)
    }

    /// Reads one by the qualified name a query wrote, whatever its case.
    fn named(database: &str, table: &str) -> Option<Self> {
        if !database.eq_ignore_ascii_case("information_schema") {
            return None;
        }
        Self::ALL
            .into_iter()
            .find(|catalog| table.eq_ignore_ascii_case(catalog.mysql_name()))
    }

    /// The name MySQL knows this table by, without its `information_schema`
    /// qualifier.
    const fn mysql_name(self) -> &'static str {
        match self {
            Self::Tables => "TABLES",
            Self::Views => "VIEWS",
            Self::Statistics => "STATISTICS",
            Self::KeyColumnUsage => "KEY_COLUMN_USAGE",
            Self::TableConstraints => "TABLE_CONSTRAINTS",
            Self::ReferentialConstraints => "REFERENTIAL_CONSTRAINTS",
            Self::Routines => "ROUTINES",
            Self::Columns => "COLUMNS",
            Self::Schemata => "SCHEMATA",
            Self::CheckConstraints => "CHECK_CONSTRAINTS",
            Self::Events => "EVENTS",
            Self::Triggers => "TRIGGERS",
        }
    }
}

impl MySqlSelectSource {
    /// Returns the name the engine reports for this table's columns.
    pub fn reference(&self) -> &str {
        &self.reference
    }

    /// Returns the table itself.
    pub const fn table(&self) -> &MySqlTableName {
        &self.table
    }

    /// Returns the `information_schema` table this reads, if it reads one.
    pub const fn catalog(&self) -> Option<MySqlCatalogTable> {
        self.catalog
    }

    /// Returns the keys an index hint on this source named.
    ///
    /// The hint says which key to plan with and nothing about which rows come
    /// back, so it is dropped. It does say the key exists, which only the
    /// frontend can check — measured on MySQL 8.4.11, a hint naming a key the
    /// table has not got answers 1176.
    pub fn hinted_indexes(&self) -> &[String] {
        &self.hinted_indexes
    }

    /// Reports whether an outer join can leave this table's columns NULL.
    ///
    /// Measured on MySQL 8.4.11: a `NOT NULL` column on the outer side of a
    /// `LEFT JOIN` reports no `NOT_NULL` flag, while its key flags stay, and
    /// the inner side keeps everything. A `RIGHT JOIN` is the mirror image.
    pub const fn outer(&self) -> bool {
        self.outer
    }

    /// Reports whether a subquery standing as a result column names this
    /// table's columns from inside it — `SELECT users.*, (SELECT COUNT(*) FROM
    /// posts WHERE users.id = posts.user_id)`.
    ///
    /// Measured on MySQL 8.4.11: each of the table's columns read as a result
    /// column then reports no `NOT_NULL` flag, its key flags staying, as on
    /// the outer side of a `LEFT JOIN`. A call or arithmetic over one keeps
    /// its own shape, and an `EXISTS` naming the table changes nothing.
    pub const fn read_by_a_result_subquery(&self) -> bool {
        self.read_by_a_result_subquery
    }

    /// Returns the columns a `WITH` name projects, in order.
    ///
    /// Empty for an ordinary table, whose columns are the table's own. A CTE
    /// can project a table's columns in any order, so a result column naming
    /// the CTE and an ordinal is resolved through this list rather than
    /// straight into the table.
    pub fn projected_columns(&self) -> &[String] {
        &self.projected_columns
    }

    /// Returns what a derived table or a CTE projects, when this is one.
    pub const fn derived(&self) -> Option<&MySqlDerivedColumns> {
        self.derived.as_ref()
    }

    /// Reports whether a subquery reads this table rather than the statement
    /// itself.
    ///
    /// It is still authorized and still refused when it names an internal
    /// catalog table; what it does not do is name any of the result columns.
    pub const fn subquery(&self) -> bool {
        self.subquery
    }

    /// Returns which branch of a `UNION` reads this table, counting from zero.
    ///
    /// Every table a single statement reads is branch zero, joins included.
    /// A second branch means the result columns belong to no one table, which
    /// is what MySQL reports for a `UNION`.
    pub const fn branch(&self) -> usize {
        self.branch
    }
}

pub(crate) struct RenderedSelect {
    pub(crate) sqlite_sql: String,
    pub(crate) collation_sensitive_call_columns: Vec<String>,
    pub(crate) collation_sensitive_joined_columns: Vec<(String, String)>,
    pub(crate) json_reading_columns: Vec<String>,
    pub(crate) orders_a_bare_column: bool,
    pub(crate) checks_type_sensitive_expression: bool,
    pub(crate) renders_a_condition_without_column_types: bool,
    pub(crate) orders_wildcard_ordinal: bool,
    pub(crate) compares_a_placeholder: bool,
    pub(crate) counts_distinct_column: bool,
    pub(crate) tests_a_bare_column: bool,
    pub(crate) compares_a_written_day: bool,
    pub(crate) compares_a_written_number: bool,
    pub(crate) compares_a_large_decimal_integer: bool,
    pub(crate) checked_subquery_comparisons: Vec<CheckedSubqueryComparison>,
    pub(crate) source_table: Option<MySqlTableName>,
    pub(crate) source_tables: Vec<MySqlSelectSource>,
    pub(crate) checked_comparisons: Vec<CheckedSelectComparison>,
    /// Whether the statement asked to read the rows it is about to change.
    pub(crate) locks_rows: bool,
    /// Which parameters stand where a row count is written, so the frontend
    /// can hold each to the whole number a row count has to be.
    pub(crate) row_count_parameters: Vec<usize>,
    pub(crate) parameter_count: usize,
    /// Whether a `GROUP_CONCAT` is rendered, whose cut this rendering warns
    /// about rather than fails on.
    pub(crate) concatenates_groups: bool,
    /// Whether the statement notes the rows it answers without its `LIMIT`.
    pub(crate) calculates_found_rows: bool,
    /// The columns a grouped statement projects beside its keys, which the
    /// frontend holds to what the keys decide.
    pub(crate) columns_the_keys_decide: Option<MySqlColumnsTheKeysDecide>,
    /// Each name a subquery standing as a result column reads without a
    /// table, with the table the subquery reads.
    pub(crate) bare_names_in_result_subqueries: Vec<(MySqlTableName, String)>,
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn translate_select_query(
    query: &sqlparser::ast::Query,
    sql: &str,
    mode: SessionSqlMode,
    text_columns: &[String],
    table_columns: &[String],
    member_columns: &[(String, Vec<String>)],
    set_columns: &[(String, Vec<String>)],
    moment_columns: &[String],
    decimal_columns: &[(String, u32)],
    integer_columns: &[String],
    real_columns: &[String],
    json_columns: &[String],
    writes_its_rows: bool,
    knows_the_kinds_of_joined_columns: bool,
) -> Result<RenderedSelect, ParseError> {
    if query.fetch.is_some()
        || query.for_clause.is_some()
        || query.settings.is_some()
        || query.format_clause.is_some()
        || !query.pipe_operators.is_empty()
    {
        return unsupported("SELECT query clause");
    }
    let locks_rows = reads_to_write(&query.locks)?;
    let mut render_context = SelectRenderContext::new(
        sql,
        mode,
        text_columns,
        table_columns,
        member_columns,
        set_columns,
        moment_columns,
        &[],
    );
    render_context.decimal_columns = decimal_columns;
    render_context.integer_columns = integer_columns;
    render_context.real_columns = real_columns;
    render_context.json_columns = json_columns;
    render_context.writes_its_rows = writes_its_rows;
    render_context.knows_the_kinds_of_joined_columns = knows_the_kinds_of_joined_columns;
    let (mut prefix, mut cte_tables) = (String::new(), Vec::new());
    let mut sequence = None;
    if let Some(with) = &query.with {
        if with.recursive {
            let counted = recursive::read_counted_sequence(with)?;
            prefix = counted.render();
            sequence = Some(counted);
        } else {
            let (rendered, sources) = render_common_table_expressions(with, &mut render_context)?;
            prefix = rendered;
            cte_tables = sources;
        }
    }
    // An `ORDER BY` ordinal names a projected column, so the projection has to
    // outlive the body that rendered it. A compound query orders by its first
    // branch.
    let ordered_projection: &[SelectItem] = match query.body.as_ref() {
        SetExpr::Select(select) => &select.projection,
        SetExpr::SetOperation { left, .. } => match unwrap_select_body(left.as_ref()) {
            Ok(select) => &select.projection,
            Err(_) => &[],
        },
        _ => &[],
    };
    let (mut normalized, mut source_tables) = match query.body.as_ref() {
        SetExpr::Select(select) => {
            render_context.renders_the_outer_projection = true;
            match rollup::render_rollup(select, &mut render_context)? {
                Some(_) if query.order_by.is_some() => {
                    return unsupported("WITH ROLLUP with an ORDER BY");
                }
                Some(rendered) => rendered,
                None => render_select_body(select, &mut render_context)?,
            }
        }
        SetExpr::SetOperation {
            left,
            op,
            set_quantifier,
            right,
        } => {
            // MySQL's EXCEPT and INTERSECT arrived in 8.0.31 and answer rows a
            // UNION does not. Only their DISTINCT forms are taken: measured on
            // MySQL 8.4.11 over rows (1), (1), (2) against (2), `EXCEPT`
            // answers one 1 and `EXCEPT ALL` answers two, and the engine has no
            // spelling for the second, so it is refused rather than collapsed
            // into the first.
            let keyword = match (op, set_quantifier) {
                (
                    sqlparser::ast::SetOperator::Union,
                    sqlparser::ast::SetQuantifier::None | sqlparser::ast::SetQuantifier::Distinct,
                ) => "UNION",
                (sqlparser::ast::SetOperator::Union, sqlparser::ast::SetQuantifier::All) => {
                    "UNION ALL"
                }
                (
                    sqlparser::ast::SetOperator::Except,
                    sqlparser::ast::SetQuantifier::None | sqlparser::ast::SetQuantifier::Distinct,
                ) => "EXCEPT",
                (
                    sqlparser::ast::SetOperator::Intersect,
                    sqlparser::ast::SetQuantifier::None | sqlparser::ast::SetQuantifier::Distinct,
                ) => "INTERSECT",
                _ => return unsupported("SELECT set operation"),
            };
            if matches!(
                unwrap_query_wrappers(left.as_ref())?,
                SetExpr::SetOperation { .. }
            ) {
                render_catalog_union(query.body.as_ref(), &mut render_context)?
            } else {
                let (left, right) = (
                    unwrap_select_body(left.as_ref())?,
                    unwrap_select_body(right.as_ref())?,
                );
                let (left, mut sources) = render_select_body(left, &mut render_context)?;
                let (right, right_sources) = render_select_body(right, &mut render_context)?;
                // Measured on MySQL 8.4.11: the column a branch's
                // `GROUP_CONCAT` lands in is a `VAR_STRING` or a `BLOB` of a
                // width of its own, which the shape a `UNION` reports here
                // does not follow.
                if render_context.group_concat_calls > 0 {
                    return unsupported("GROUP_CONCAT in a set operation branch");
                }
                sources.extend(right_sources.into_iter().map(|mut source| {
                    source.branch = 1;
                    source
                }));
                (format!("{left} {keyword} {right}"), sources)
            }
        }
        _ => return unsupported("compound SELECT query"),
    };
    // A statement that names a CTE reads the CTE's own table under the CTE's
    // name, which is how its result columns find their metadata.
    for source in &mut source_tables {
        if let Some(cte) = cte_tables
            .iter()
            .find(|cte| cte.reference.eq_ignore_ascii_case(&source.reference))
        {
            source.table = cte.table.clone();
            source.projected_columns.clone_from(&cte.projected_columns);
            source.derived.clone_from(&cte.derived);
        }
    }
    source_tables.append(&mut render_context.subquery_tables);
    derived::hold_the_statement_reading_joined_tables_through_a_derived_table(
        query,
        &mut source_tables,
    )?;
    derived::hold_a_distinct_reading_one_table_through_a_derived_table(query, &mut source_tables)?;
    derived::resolve_comparisons_through_derived_columns(
        &mut render_context.checked_comparisons,
        &source_tables,
    )?;
    // A qualified comparison names the table its column belongs to, and that
    // has to be a table the statement reads — a join has several and so does a
    // statement with a subquery, and the qualifier is what says which of them
    // the frontend checks the value against.
    for comparison in &render_context.checked_comparisons {
        let Some(qualifier) = comparison.qualifier() else {
            continue;
        };
        if !source_tables
            .iter()
            .any(|source| qualifier.eq_ignore_ascii_case(source.reference()))
        {
            return unsupported(
                "SELECT comparison qualifier must name a table the statement reads",
            );
        }
    }
    if let Some(sequence) = &sequence {
        recursive::read_the_sequence(
            sequence,
            &mut source_tables,
            &mut render_context.checked_comparisons,
        )?;
    }
    normalized.insert_str(0, &prefix);
    let source_table = match source_tables
        .iter()
        .filter(|source| !source.subquery)
        .collect::<Vec<_>>()
        .as_slice()
    {
        [source] => Some(source.table.clone()),
        _ => None,
    };
    // Measured on MySQL 8.4.11: `FOUND_ROWS()` after `SQL_CALC_FOUND_ROWS`
    // answers the rows the statement answers without its `LIMIT` — its groups
    // or its distinct rows, and all of them past an offset beyond the last.
    // The count reads the statement as it stands before its `ORDER BY`, so a
    // bound value, which it would bind a second time, and a `WITH`, which it
    // would read outside of, are refused.
    let found_rows_counted = if render_context.calculates_found_rows {
        if render_context.parameter_count > 0 || query.with.is_some() {
            return unsupported("SQL_CALC_FOUND_ROWS with a bound value or a WITH");
        }
        Some(format!("(SELECT COUNT(*) FROM ({normalized}))"))
    } else {
        None
    };
    if let Some(order_by) = &query.order_by {
        if let SetExpr::Select(select) = query.body.as_ref() {
            grouping::hold_the_grouped_order_by(
                order_by,
                select,
                &mut render_context.columns_the_keys_decide,
            )?;
        }
        normalized.push_str(" ORDER BY ");
        normalized.push_str(&render_select_order_by(
            order_by,
            ordered_projection,
            &mut render_context,
        )?);
    }
    // The LIMIT is rendered last because it is written last: a parameter takes
    // its ordinal from where it stands in the statement, and a client binds by
    // that ordinal.
    let mut row_count_parameters = Vec::new();
    if let Some(counted) = &found_rows_counted {
        let limit = match &query.limit_clause {
            Some(limit) => render_select_limit(
                limit,
                &mut render_context,
                &mut row_count_parameters,
                Some(counted),
            )?,
            // `LIMIT -1` keeps every row, as no `LIMIT` does.
            None => format!(" LIMIT {}", note_found_rows(counted, "-1")),
        };
        normalized.push_str(&limit);
    } else if let Some(limit) = &query.limit_clause {
        normalized.push_str(&render_select_limit(
            limit,
            &mut render_context,
            &mut row_count_parameters,
            None,
        )?);
    }
    // Measured on MySQL 8.4.11: `ORDER BY GROUP_CONCAT(a)` with no such call
    // projected still cuts it and warns.
    if render_context.names_an_unprojected_group_concat {
        return unsupported("GROUP_CONCAT outside the projection");
    }
    if render_context.group_concat_orders_by_another_kind {
        return unsupported("GROUP_CONCAT ordered by a column holding neither words nor numbers");
    }
    Ok(RenderedSelect {
        sqlite_sql: normalized,
        collation_sensitive_call_columns: render_context.collation_sensitive_call_columns,
        collation_sensitive_joined_columns: render_context.collation_sensitive_joined_columns,
        json_reading_columns: render_context.json_reading_columns,
        orders_a_bare_column: render_context.orders_a_bare_column,
        checks_type_sensitive_expression: render_context.checks_type_sensitive_expression,
        renders_a_condition_without_column_types: render_context
            .renders_a_condition_without_column_types,
        orders_wildcard_ordinal: render_context.orders_wildcard_ordinal,
        compares_a_placeholder: render_context.compares_a_placeholder,
        counts_distinct_column: render_context.counts_distinct_column,
        tests_a_bare_column: render_context.tests_a_bare_column,
        compares_a_written_day: render_context.compares_a_written_day,
        compares_a_written_number: render_context.compares_a_written_number,
        compares_a_large_decimal_integer: render_context.compares_a_large_decimal_integer,
        checked_subquery_comparisons: render_context.checked_subquery_comparisons,
        source_table,
        source_tables,
        checked_comparisons: render_context.checked_comparisons,
        locks_rows,
        row_count_parameters,
        parameter_count: render_context.parameter_count,
        concatenates_groups: render_context.group_concat_calls > 0,
        calculates_found_rows: render_context.calculates_found_rows,
        columns_the_keys_decide: render_context.columns_the_keys_decide,
        bare_names_in_result_subqueries: render_context.bare_names_in_result_subqueries,
    })
}

/// Reports whether a statement asked to read the rows it is about to change.
///
/// `FOR UPDATE` and `FOR SHARE` are the two locks MySQL has — `LOCK IN SHARE
/// MODE` reaches here spelled as `FOR SHARE` — and both are read the same way
/// here: the engine holds one write lock over the
/// whole database rather than a lock for each row, so there is no weaker lock
/// to take for the sharing one.
///
/// `SKIP LOCKED` — Laravel's database queue takes its next job with it — asks
/// for rows no other session holds. Once this session holds the one lock, no
/// other session holds any row, so the rows it reads are those; it waits for
/// the lock where MySQL would skip the rows another session holds. `NOWAIT`
/// asks to be refused when a row read is held, and which rows another session
/// holds is not known here, so it is refused, as is naming which tables to
/// lock.
fn reads_to_write(locks: &[sqlparser::ast::LockClause]) -> Result<bool, ParseError> {
    let [lock] = locks else {
        if locks.is_empty() {
            return Ok(false);
        }
        return unsupported("SELECT locking clause written more than once");
    };
    if lock.of.is_some() || matches!(lock.nonblock, Some(sqlparser::ast::NonBlock::Nowait)) {
        return unsupported("SELECT locking clause option");
    }
    Ok(matches!(
        lock.lock_type,
        sqlparser::ast::LockType::Update | sqlparser::ast::LockType::Share
    ))
}

/// Renders a `UNION` of three or more branches, each reading the same
/// `information_schema` table.
///
/// TypeORM reads one table's catalog rows per branch, one branch for every
/// table it syncs. Every branch reads the same columns of the same table, so
/// the result column's shape is that table's, and no pair of kinds has to be
/// reconciled. A `UNION` of three over anything else is refused: which shape
/// MySQL answers for a column three kinds meet in has not been measured.
fn render_catalog_union(
    body: &SetExpr,
    render_context: &mut SelectRenderContext<'_>,
) -> Result<(String, Vec<MySqlSelectSource>), ParseError> {
    let (keyword, branches) = union_branches(body)?;
    if branches
        .iter()
        .any(|branch| branch.projection != branches[0].projection)
    {
        return unsupported("UNION of three or more branches naming different columns");
    }
    let mut rendered = Vec::with_capacity(branches.len());
    let mut sources = Vec::new();
    for (branch, select) in branches.into_iter().enumerate() {
        let (body, branch_sources) = render_select_body(select, render_context)?;
        let [source] = branch_sources.as_slice() else {
            return unsupported("UNION of three or more branches over a join");
        };
        if source.catalog.is_none() {
            return unsupported("UNION of three or more branches over a user table");
        }
        rendered.push(body);
        sources.extend(branch_sources.into_iter().map(|mut source| {
            source.branch = branch;
            source
        }));
    }
    let first = &sources[0];
    if sources.iter().any(|source| source.catalog != first.catalog) {
        return unsupported("UNION of three or more branches over different tables");
    }
    Ok((rendered.join(&format!(" {keyword} ")), sources))
}

/// Reads a chain of `UNION`s into its branches, in the order they are
/// written.
///
/// A chain mixing `UNION` with `UNION ALL`, `EXCEPT` or `INTERSECT` is
/// refused: MySQL lets a `UNION DISTINCT` undo the `UNION ALL`s left of it
/// and binds `INTERSECT` tighter than the rest, which a flat list would lose.
fn union_branches(
    body: &SetExpr,
) -> Result<(&'static str, Vec<&sqlparser::ast::Select>), ParseError> {
    let SetExpr::SetOperation {
        left,
        op: sqlparser::ast::SetOperator::Union,
        set_quantifier,
        right,
    } = unwrap_query_wrappers(body)?
    else {
        return unsupported("SELECT set operation chain");
    };
    let keyword = match set_quantifier {
        sqlparser::ast::SetQuantifier::None | sqlparser::ast::SetQuantifier::Distinct => "UNION",
        sqlparser::ast::SetQuantifier::All => "UNION ALL",
        _ => return unsupported("SELECT set operation"),
    };
    let mut branches = match unwrap_query_wrappers(left.as_ref())? {
        left @ SetExpr::SetOperation { .. } => {
            let (left_keyword, branches) = union_branches(left)?;
            if left_keyword != keyword {
                return unsupported("SELECT set operation chain mixing kinds");
            }
            branches
        }
        left => vec![unwrap_select_body(left)?],
    };
    branches.push(unwrap_select_body(right.as_ref())?);
    Ok((keyword, branches))
}

/// Unwraps parenthesised query wrappers around a compound branch, refusing any
/// branch that carries options like `ORDER BY` or `LIMIT` that cannot be
/// flattened into the set operation.
fn unwrap_select_body(expr: &SetExpr) -> Result<&sqlparser::ast::Select, ParseError> {
    match unwrap_query_wrappers(expr)? {
        SetExpr::Select(select) => Ok(select),
        _ => unsupported("SELECT set operation branch"),
    }
}

/// Takes the parentheses off a compound branch, refusing one that carries
/// options like `ORDER BY` or `LIMIT` that cannot be flattened into the set
/// operation.
fn unwrap_query_wrappers(expr: &SetExpr) -> Result<&SetExpr, ParseError> {
    match expr {
        SetExpr::Query(query) => {
            if query.fetch.is_some()
                || !query.locks.is_empty()
                || query.for_clause.is_some()
                || query.settings.is_some()
                || query.format_clause.is_some()
                || !query.pipe_operators.is_empty()
                || query.with.is_some()
                || query.order_by.is_some()
                || query.limit_clause.is_some()
            {
                return unsupported("compound branch query clause");
            }
            unwrap_query_wrappers(query.body.as_ref())
        }
        other => Ok(other),
    }
}

/// Renders one `SELECT` body, which is either the whole statement or one
/// branch of a `UNION`.
fn render_select_body(
    select: &sqlparser::ast::Select,
    render_context: &mut SelectRenderContext<'_>,
) -> Result<(String, Vec<MySqlSelectSource>), ParseError> {
    // A `WINDOW w AS (...)` names a window the calls then reach for by name.
    // Writing each call's window out where it stands is what it means, and it
    // leaves every check and every rendering below reading one shape.
    let resolved;
    let select = if select.named_window.is_empty() {
        select
    } else {
        resolved = resolve_named_windows(select)?;
        &resolved
    };
    if !matches!(select.flavor, SelectFlavor::Standard)
        || !select.optimizer_hints.is_empty()
        || !matches!(
            select.distinct,
            None | Some(sqlparser::ast::Distinct::Distinct)
        )
        || select.select_modifiers.as_ref().is_some_and(|modifiers| {
            modifiers.high_priority
                || modifiers.straight_join
                || modifiers.sql_small_result
                || modifiers.sql_big_result
                || modifiers.sql_buffer_result
                // MySQL answers 1234 for it anywhere but the statement's own
                // `SELECT`.
                || (modifiers.sql_calc_found_rows && !render_context.renders_the_outer_projection)
        })
        || select.top.is_some()
        || select.top_before_distinct
        || select.exclude.is_some()
        || select.into.is_some()
        || !select.lateral_views.is_empty()
        || select.prewhere.is_some()
        || !select.connect_by.is_empty()
        || !matches!(
            &select.group_by,
            sqlparser::ast::GroupByExpr::Expressions(_, modifiers) if modifiers.is_empty()
        )
        || !select.cluster_by.is_empty()
        || !select.distribute_by.is_empty()
        || !select.sort_by.is_empty()
        || !select.named_window.is_empty()
        || select.qualify.is_some()
        || select.window_before_qualify
        || select.value_table_mode.is_some()
    {
        return unsupported("SELECT feature");
    }

    // A value written out in full is worked out here only where it stands in
    // the statement's own result, whose shape is reported for it. Anywhere
    // else — a subquery, a branch of a `UNION` — the engine would read the
    // text it is worked out to.
    let outer_projection = std::mem::take(&mut render_context.renders_the_outer_projection);
    render_context.calculates_found_rows |= select
        .select_modifiers
        .as_ref()
        .is_some_and(|modifiers| modifiers.sql_calc_found_rows);
    render_context.renders_a_projection_item = false;
    let outer_counts_in_having = std::mem::replace(
        &mut render_context.counts_group_concat_in_having,
        select.having.is_some(),
    );
    let outer_group_concat_counts = std::mem::take(&mut render_context.group_concat_counts);
    let outer_projected_group_concats = std::mem::take(&mut render_context.projected_group_concats);
    // The projection is rendered before the `FROM`, so what a derived table
    // counts is read from how its body is written.
    let outer_derived_counts = std::mem::replace(
        &mut render_context.derived_counts,
        counts_derived_tables_work_out(select),
    );
    let projection = select
        .projection
        .iter()
        .map(|item| {
            render_context.renders_a_projection_item = true;
            render_context.takes_a_joined_aggregate =
                outer_projection && is_an_aggregate_over_a_joined_column(item);
            let rendered = match item {
                SelectItem::UnnamedExpr(expr) | SelectItem::ExprWithAlias { expr, .. }
                    if outer_projection =>
                {
                    match crate::written_value::read_written_value(expr) {
                        Some((_, rendered)) => render_written_value(item, rendered, render_context),
                        None => render_select_item(item, render_context),
                    }
                }
                _ => render_select_item(item, render_context),
            };
            render_context.renders_a_projection_item = false;
            render_context.takes_a_joined_aggregate = false;
            rendered
        })
        .collect::<Result<Vec<_>, _>>();
    render_context.derived_counts = outer_derived_counts;
    let projection = projection?;
    if projection.is_empty() {
        return unsupported("SELECT without projections");
    }

    render_context.counts_the_rows_of_the_derived_table =
        outer_projection && only_counts_the_rows_of_a_derived_table(select);
    let outer_from = std::mem::replace(
        &mut render_context.renders_the_statements_own_from,
        outer_projection,
    );
    let from = render_from_clause_with(&select.from, Some(render_context));
    render_context.renders_the_statements_own_from = outer_from;
    let (from, mut source_tables) = from?;
    render_context.counts_the_rows_of_the_derived_table = false;
    if outer_projection {
        note_what_result_subqueries_read(select, &mut source_tables, render_context)?;
    }
    // Some `information_schema` tables answer only a few of the columns MySQL
    // gives them, and a wildcard over one of those — which asks for all of
    // them — would answer a row of a different width than MySQL answers.
    if source_tables.iter().any(|source| {
        source
            .catalog()
            .is_some_and(|catalog| !catalog.answers_every_column())
    }) && select.projection.iter().any(|item| {
        matches!(
            item,
            SelectItem::Wildcard(_) | SelectItem::QualifiedWildcard(_, _)
        )
    }) {
        return unsupported("information_schema wildcard projection");
    }

    let mut normalized = format!(
        "SELECT {}{}",
        if select.distinct.is_some() {
            "DISTINCT "
        } else {
            ""
        },
        projection.join(", ")
    );
    if let Some(from) = from {
        normalized.push_str(" FROM ");
        normalized.push_str(&from);
    }
    let sqlparser::ast::GroupByExpr::Expressions(group_by, _) = &select.group_by else {
        unreachable!("the GROUP BY shape was checked above");
    };
    // A HAVING over a statement that groups nothing and aggregates nothing
    // filters rows, not groups, so it is written where the rows are filtered.
    // The engine would otherwise read the same statement as one group of every
    // row and answer one row back.
    // A name in a HAVING is the projection's alias before it is the table's
    // column, so the names are resolved before the clause is read at all —
    // what a name stands for decides whether the clause filters rows or
    // groups.
    if let (Some(having), false) = (&select.having, group_by.is_empty()) {
        grouping::hold_the_grouped_having(having, &select.projection, group_by)?;
    }
    let having = select
        .having
        .as_ref()
        .map(|having| having_with_aliases_resolved(having, &select.projection));
    let row_filter = having_filters_rows(having.as_ref(), select, group_by);
    let mut predicates = Vec::new();
    if let Some(selection) = &select.selection {
        predicates.push(render_select_predicate(selection, render_context)?);
    }
    if row_filter {
        if !render_context.group_concat_counts.is_empty() {
            return unsupported("GROUP_CONCAT beside a HAVING that filters rows");
        }
        let having = having.as_ref().expect("the HAVING was read above");
        predicates.push(render_select_predicate(having, render_context)?);
    }
    if !predicates.is_empty() {
        normalized.push_str(" WHERE ");
        normalized.push_str(&predicates.join(" AND "));
    }
    if !group_by.is_empty() {
        normalized.push_str(" GROUP BY ");
        normalized.push_str(&grouping::render_select_group_by(
            group_by,
            select,
            outer_projection,
            render_context,
        )?);
    }
    // A statement that aggregates and groups nothing has put every row it read
    // into one answer, so a bare column has no single row to come from. MySQL
    // says so with 1140.
    if group_by.is_empty() && projects_an_aggregate(select) {
        match grouping::columns_written_values_decide(select) {
            // Sequelize's `Post.max('views', { include: [{ model: User,
            // where: { email } }] })` projects the user's columns beside the
            // aggregate, which MySQL takes because the email decides the
            // user. With no row the one answer is NULL for each of them, so
            // none is reported NOT NULL.
            Some(claim) if outer_projection => {
                render_context.columns_the_keys_decide = Some(claim);
                for source in &mut source_tables {
                    source.outer = true;
                }
            }
            _ => hold_the_aggregated_projection(select)?,
        }
    }
    if let Some(having) = having.as_ref().filter(|_| !row_filter) {
        if group_by.is_empty() {
            // MySQL reads a HAVING with no GROUP BY over one implicit group of
            // every row, and the engine answers the same. Measured on MySQL
            // 8.4.11 over three rows: `SELECT COUNT(*) FROM t HAVING
            // COUNT(*) > 1` answers 3 and `... > 5` answers no rows at all.
            //
            // A HAVING aggregates the statement even when the projection does
            // not, so the projection is held to the same rule here, and a bare
            // column in the HAVING itself — 1054 — is turned away with it.
            hold_the_aggregated_projection(select)?;
            if !aggregates_or_literals_only(having) {
                return unsupported("HAVING without a GROUP BY naming an ungrouped column");
            }
        }
        normalized.push_str(" HAVING ");
        let rendered = render_having_predicate(having, render_context)?;
        let counts = std::mem::take(&mut render_context.group_concat_counts);
        if counts.is_empty() {
            normalized.push_str(&rendered);
        } else {
            normalized.push_str(&format!("{} AND ({rendered})", counts.join(" AND ")));
        }
    }
    render_context.counts_group_concat_in_having = outer_counts_in_having;
    render_context.group_concat_counts = outer_group_concat_counts;
    render_context.last_projected_group_concats = std::mem::replace(
        &mut render_context.projected_group_concats,
        outer_projected_group_concats,
    );
    Ok((normalized, source_tables))
}

/// Notes which of the statement's tables each subquery standing as a result
/// column names from inside it, and each name it reads without a table.
///
/// A qualified name says which table it reads. A bare name is the subquery's
/// own table's column when that table has one, which only the frontend can
/// see, so it is handed there to be held to that; a subquery written in a
/// form this does not read through is refused.
fn note_what_result_subqueries_read(
    select: &sqlparser::ast::Select,
    source_tables: &mut [MySqlSelectSource],
    render_context: &mut SelectRenderContext<'_>,
) -> Result<(), ParseError> {
    for item in &select.projection {
        let (SelectItem::UnnamedExpr(Expr::Subquery(subquery))
        | SelectItem::ExprWithAlias {
            expr: Expr::Subquery(subquery),
            ..
        }) = item
        else {
            continue;
        };
        let SetExpr::Select(inner) = subquery.body.as_ref() else {
            return unsupported("SELECT subquery body");
        };
        let [sqlparser::ast::TableWithJoins { relation, joins }] = inner.from.as_slice() else {
            return unsupported("SELECT subquery requires one table");
        };
        let Some(reference) = grouping::table_reference(relation).filter(|_| joins.is_empty())
        else {
            return unsupported("SELECT subquery requires one table");
        };
        let table = render_context
            .subquery_tables
            .iter()
            .find(|source| source.reference.eq_ignore_ascii_case(&reference))
            .map(|source| source.table.clone())
            .ok_or(ParseError::Unsupported {
                feature: "SELECT subquery requires one table",
            })?;
        let mut named = Vec::new();
        let sqlparser::ast::GroupByExpr::Expressions(group_by, _) = &inner.group_by else {
            return unsupported("SELECT subquery GROUP BY");
        };
        let read_through = inner.projection.iter().all(|item| match item {
            SelectItem::UnnamedExpr(expr) | SelectItem::ExprWithAlias { expr, .. } => {
                grouping::columns_named_in(expr, &mut named)
            }
            _ => false,
        }) && inner
            .selection
            .iter()
            .chain(inner.having.iter())
            .chain(group_by)
            .all(|expr| grouping::columns_named_in(expr, &mut named));
        if !read_through {
            return unsupported("a subquery in the projection written in a form not read here");
        }
        for column in named {
            match column.table() {
                Some(qualifier) if qualifier.eq_ignore_ascii_case(&reference) => {}
                Some(qualifier) => {
                    for source in source_tables.iter_mut() {
                        if source.reference.eq_ignore_ascii_case(qualifier) {
                            source.read_by_a_result_subquery = true;
                        }
                    }
                }
                None => render_context
                    .bare_names_in_result_subqueries
                    .push((table.clone(), column.column().to_owned())),
            }
        }
    }
    Ok(())
}

/// Reports whether a projection item is a `MIN`, `MAX`, `SUM` or
/// `GROUP_CONCAT` over a column named with its table, standing on its own.
fn is_an_aggregate_over_a_joined_column(item: &SelectItem) -> bool {
    matches!(item,
        SelectItem::UnnamedExpr(Expr::Function(function))
        | SelectItem::ExprWithAlias { expr: Expr::Function(function), .. }
            if static_select_metadata::qualified_aggregate_argument(function).is_some())
}

/// Reports whether a statement answers the count of its one derived table's
/// rows and nothing else — `SELECT COUNT(*) FROM (...) subquery_for_count`,
/// which is how Rails counts a relation carrying a limit.
fn only_counts_the_rows_of_a_derived_table(select: &sqlparser::ast::Select) -> bool {
    let [SelectItem::UnnamedExpr(Expr::Function(count))
    | SelectItem::ExprWithAlias {
        expr: Expr::Function(count),
        ..
    }] = select.projection.as_slice()
    else {
        return false;
    };
    let sqlparser::ast::FunctionArguments::List(arguments) = &count.args else {
        return false;
    };
    let counts_every_row = matches!(
        arguments.args.as_slice(),
        [sqlparser::ast::FunctionArg::Unnamed(
            sqlparser::ast::FunctionArgExpr::Wildcard
        )]
    ) || matches!(arguments.args.as_slice(), [argument] if static_select_metadata::counts_every_row(argument));
    let groups_nothing = matches!(
        &select.group_by,
        sqlparser::ast::GroupByExpr::Expressions(keys, _) if keys.is_empty()
    );
    static_select_metadata::is_count_call(count)
        && arguments.duplicate_treatment.is_none()
        && counts_every_row
        && select.distinct.is_none()
        && select.selection.is_none()
        && select.having.is_none()
        && groups_nothing
        && matches!(
            select.from.as_slice(),
            [sqlparser::ast::TableWithJoins {
                relation: TableFactor::Derived { .. },
                joins,
            }] if joins.is_empty()
        )
}

/// Writes a value worked out in full under the name MySQL gives it: its
/// alias, or the text it was written as.
fn render_written_value(
    item: &SelectItem,
    rendered: String,
    render_context: &SelectRenderContext<'_>,
) -> Result<String, ParseError> {
    match item {
        SelectItem::ExprWithAlias { alias, .. } => {
            Ok(format!("{rendered} AS {}", render_ident(alias)))
        }
        SelectItem::UnnamedExpr(Expr::Value(value))
            if matches!(
                value.value,
                Value::SingleQuotedString(_) | Value::DoubleQuotedString(_)
            ) =>
        {
            let (Value::SingleQuotedString(word) | Value::DoubleQuotedString(word)) = &value.value
            else {
                unreachable!("the guard requires a word in quotes");
            };
            // sqlparser joins `'a' 'b'` into one word, and MySQL names the
            // column after the first alone.
            let start = byte_offset(render_context.source, value.span.start).ok_or(
                ParseError::Unsupported {
                    feature: "SELECT written word whose source text cannot be recovered",
                },
            )?;
            if another_word_follows(
                render_context.source,
                start,
                render_context.no_backslash_escapes,
            ) != Some(false)
            {
                return unsupported("SELECT words written one after another");
            }
            let name = crate::written_value::word_column_name(word).replace('"', "\"\"");
            Ok(format!("{rendered} AS \"{name}\""))
        }
        SelectItem::UnnamedExpr(expr) => {
            // Measured: MySQL leaves a written plus sign out of the name,
            // `+1.5` being named `1.5`, and keeps a minus sign in it.
            let named = match expr {
                Expr::UnaryOp {
                    op: UnaryOperator::Plus,
                    expr: inner,
                } => inner.as_ref(),
                _ => expr,
            };
            let name = source_text(render_context.source, named)
                .ok_or(ParseError::Unsupported {
                    feature: "SELECT written value whose source text cannot be recovered",
                })?
                .replace('"', "\"\"");
            Ok(format!("{rendered} AS \"{name}\""))
        }
        _ => unreachable!("only an expression item carries a written value"),
    }
}

/// Reports whether the word in quotes written at `start` has another written
/// right after it, or `None` when no word in quotes starts there.
fn another_word_follows(source: &str, start: usize, no_backslash_escapes: bool) -> Option<bool> {
    let written = source.get(start..)?;
    let mut characters = written.char_indices().peekable();
    let (_, quote) = characters
        .next()
        .filter(|(_, quote)| matches!(quote, '\'' | '"'))?;
    let mut end = None;
    while let Some((offset, character)) = characters.next() {
        if character == '\\' && !no_backslash_escapes {
            characters.next();
        } else if character == quote {
            if characters.peek().map(|(_, next)| *next) == Some(quote) {
                characters.next();
            } else {
                end = Some(offset + character.len_utf8());
                break;
            }
        }
    }
    Some(written.get(end?..)?.trim_start().starts_with(['\'', '"']))
}

/// Reports whether a `HAVING` filters rows rather than groups.
///
/// MySQL reads one that way when the statement groups nothing and aggregates
/// nothing: measured on 8.4.11, `SELECT id FROM t HAVING id > 1` answers the
/// rows above one. It may then name only a column the projection carries,
/// which is what `only_full_group_by` holds it to — the same statement over an
/// unprojected `n` answers 1054 — so that is the shape read here.
fn having_filters_rows(
    having: Option<&Expr>,
    select: &sqlparser::ast::Select,
    group_by: &[Expr],
) -> bool {
    let Some(having) = having else {
        return false;
    };
    if !group_by.is_empty() || aggregates_or_literals_only(having) {
        return false;
    }
    let tested = match having {
        Expr::BinaryOp { left, .. } => left.as_ref(),
        Expr::IsNull(inner) | Expr::IsNotNull(inner) => inner.as_ref(),
        _ => return false,
    };
    let Expr::Identifier(tested) = tested else {
        return false;
    };
    // A column keeps filtering rows when the projection gives it a name —
    // measured on 8.4.11, `SELECT id AS x FROM t HAVING x > 1` answers the
    // rows above one, the same as the unaliased spelling.
    fn projected_column(item: &SelectItem) -> Option<&Ident> {
        match item {
            SelectItem::UnnamedExpr(Expr::Identifier(name))
            | SelectItem::ExprWithAlias {
                expr: Expr::Identifier(name),
                ..
            } => Some(name),
            _ => None,
        }
    }
    let plain_columns = || {
        select
            .projection
            .iter()
            .all(|item| projected_column(item).is_some())
    };
    let carries_the_tested_column = || {
        select.projection.iter().any(|item| {
            projected_column(item)
                .is_some_and(|projected| projected.value.eq_ignore_ascii_case(&tested.value))
        })
    };
    plain_columns() && carries_the_tested_column()
}

/// Resolves the names a `HAVING` uses against the projection's aliases.
///
/// MySQL reads a name there as the projection's alias before the table's
/// column — measured on 8.4.11, `SELECT team_id, COUNT(*) AS c FROM t GROUP BY
/// team_id HAVING c > 1` filters on the count even when the table carries a
/// column called `c` — which is what lets a grouped report name its own
/// answer. So each name is replaced by what it stands for before the clause is
/// read, and what is left is the spelling this already knows how to render.
fn having_with_aliases_resolved(expr: &Expr, projection: &[SelectItem]) -> Expr {
    match expr {
        Expr::Identifier(name) => projection
            .iter()
            .find_map(|item| match item {
                SelectItem::ExprWithAlias {
                    expr: aliased,
                    alias,
                } if alias.value.eq_ignore_ascii_case(&name.value) => Some(aliased.clone()),
                _ => None,
            })
            .unwrap_or_else(|| expr.clone()),
        Expr::BinaryOp { left, op, right } => Expr::BinaryOp {
            left: Box::new(having_with_aliases_resolved(left, projection)),
            op: op.clone(),
            right: Box::new(having_with_aliases_resolved(right, projection)),
        },
        Expr::UnaryOp { op, expr } => Expr::UnaryOp {
            op: *op,
            expr: Box::new(having_with_aliases_resolved(expr, projection)),
        },
        Expr::Nested(inner) => {
            Expr::Nested(Box::new(having_with_aliases_resolved(inner, projection)))
        }
        Expr::IsNull(inner) => {
            Expr::IsNull(Box::new(having_with_aliases_resolved(inner, projection)))
        }
        Expr::IsNotNull(inner) => {
            Expr::IsNotNull(Box::new(having_with_aliases_resolved(inner, projection)))
        }
        Expr::Between {
            expr,
            negated,
            low,
            high,
        } => Expr::Between {
            expr: Box::new(having_with_aliases_resolved(expr, projection)),
            negated: *negated,
            low: Box::new(having_with_aliases_resolved(low, projection)),
            high: Box::new(having_with_aliases_resolved(high, projection)),
        },
        _ => expr.clone(),
    }
}

/// Measures the `OVER ...` that follows a windowed call's arguments.
///
/// A call's span stops at its arguments, and MySQL names the column after the
/// whole call, `OVER` and all — which is either a window written out in
/// parentheses or the name of one.
fn window_clause_len(tail: &str) -> Option<usize> {
    let over = tail.to_ascii_uppercase().find("OVER")?;
    let rest = &tail[over + "OVER".len()..];
    let named = rest.len() - rest.trim_start().len();
    let bytes = rest.as_bytes();
    if bytes.get(named) == Some(&b'(') {
        let mut depth = 0usize;
        for (offset, byte) in rest.bytes().enumerate().skip(named) {
            match byte {
                b'(' => depth += 1,
                b')' => {
                    depth -= 1;
                    if depth == 0 {
                        return Some(over + "OVER".len() + offset + 1);
                    }
                }
                _ => {}
            }
        }
        return None;
    }
    let quote = bytes.get(named).copied().filter(|byte| *byte == b'`');
    let name_len = match quote {
        Some(quote) => rest[named + 1..]
            .bytes()
            .position(|byte| byte == quote)
            .map(|len| len + 2)?,
        None => rest[named..]
            .bytes()
            .position(|byte| !(byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'$'))
            .unwrap_or(rest.len() - named),
    };
    (name_len > 0).then_some(over + "OVER".len() + named + name_len)
}

/// Writes each `OVER <name>` out as the window that name stands for.
///
/// A name that stands for another name, and a window written on top of a named
/// one — `w AS (base ORDER BY ...)` — are refused: both are a second spelling
/// of the same thing, and one shape is enough to check.
fn resolve_named_windows(
    select: &sqlparser::ast::Select,
) -> Result<sqlparser::ast::Select, ParseError> {
    use sqlparser::ast::{NamedWindowExpr, WindowType};
    let mut windows = Vec::with_capacity(select.named_window.len());
    for definition in &select.named_window {
        let NamedWindowExpr::WindowSpec(spec) = &definition.1 else {
            return unsupported("WINDOW naming another window");
        };
        if spec.window_name.is_some() {
            return unsupported("WINDOW built on another window");
        }
        windows.push((definition.0.value.clone(), spec.clone()));
    }
    let mut resolved = select.clone();
    resolved.named_window.clear();
    // Only a note on where the clause was written, and there is no clause left.
    resolved.window_before_qualify = false;
    for item in &mut resolved.projection {
        let (SelectItem::UnnamedExpr(expr) | SelectItem::ExprWithAlias { expr, .. }) = item else {
            continue;
        };
        let Expr::Function(function) = expr else {
            continue;
        };
        let Some(WindowType::NamedWindow(name)) = function.over.as_ref() else {
            continue;
        };
        let Some((_, spec)) = windows
            .iter()
            .find(|(defined, _)| defined.eq_ignore_ascii_case(&name.value))
        else {
            return unsupported("OVER an undefined window name");
        };
        function.over = Some(WindowType::WindowSpec(spec.clone()));
    }
    Ok(resolved)
}

/// Reads the join keyword and what a checked join matches on.
///
/// Only an `ON` that equates whole columns, or a `USING` naming whole columns,
/// is taken. The two engines agree about a column-to-column equality without
/// any coercion question, which is what makes a join crossable while a literal
/// comparison still goes through the checked path.
fn checked_join(
    operator: &sqlparser::ast::JoinOperator,
) -> Result<(&'static str, CheckedJoinConstraint<'_>), ParseError> {
    use sqlparser::ast::{JoinConstraint, JoinOperator};
    let (keyword, constraint) = match operator {
        // A `CROSS JOIN` is the one join that matches on nothing at all.
        JoinOperator::CrossJoin(JoinConstraint::None) => {
            return Ok(("CROSS JOIN", CheckedJoinConstraint::Everything));
        }
        JoinOperator::Join(constraint) | JoinOperator::Inner(constraint) => ("JOIN", constraint),
        JoinOperator::Left(constraint) | JoinOperator::LeftOuter(constraint) => {
            ("LEFT JOIN", constraint)
        }
        JoinOperator::Right(constraint) | JoinOperator::RightOuter(constraint) => {
            ("RIGHT JOIN", constraint)
        }
        _ => return unsupported("SELECT JOIN form"),
    };
    match constraint {
        JoinConstraint::On(expr) => Ok((keyword, CheckedJoinConstraint::On(expr))),
        JoinConstraint::Using(columns) => Ok((keyword, CheckedJoinConstraint::Using(columns))),
        _ => unsupported("SELECT JOIN form"),
    }
}

/// What a checked join matches its two tables on.
enum CheckedJoinConstraint<'a> {
    On(&'a Expr),
    Using(&'a [sqlparser::ast::ObjectName]),
    /// A `CROSS JOIN`, which matches every row of one table against every row
    /// of the other.
    Everything,
}

/// Renders a `USING` list, and collects the names it merges.
///
/// Both engines merge the named column into one result column, so the engine's
/// own `USING` is what gets written. A merged name is the one unqualified name
/// a joined projection may carry, so each is collected for that check.
fn render_join_using(columns: &[sqlparser::ast::ObjectName]) -> Result<String, ParseError> {
    use sqlparser::ast::ObjectNamePart;
    if columns.is_empty() {
        return unsupported("SELECT JOIN USING without a column");
    }
    let mut rendered = Vec::with_capacity(columns.len());
    for column in columns {
        let [ObjectNamePart::Identifier(ident)] = column.0.as_slice() else {
            return unsupported("SELECT JOIN USING requires a plain column name");
        };
        rendered.push(render_ident(ident));
    }
    Ok(rendered.join(", "))
}

/// Renders a `FROM` clause and answers the tables it reads.
///
/// MySQL's comma join is a cross join, which is what the engine calls the join
/// that matches on nothing at all, so a second table source is folded into the
/// first as one.
pub(crate) fn render_from_clause(
    tables: &[sqlparser::ast::TableWithJoins],
) -> Result<(Option<String>, Vec<MySqlSelectSource>), ParseError> {
    render_from_clause_with(tables, None)
}

/// The same, told the statement being rendered when there is one.
///
/// A derived table is a whole statement in the `FROM`, so rendering one needs
/// what the statement is being rendered into. An `UPDATE` or a `DELETE` reads
/// its own table and passes nothing, which turns a derived table there away.
fn render_from_clause_with(
    tables: &[sqlparser::ast::TableWithJoins],
    mut render_context: Option<&mut SelectRenderContext<'_>>,
) -> Result<(Option<String>, Vec<MySqlSelectSource>), ParseError> {
    let mut rendered = String::new();
    let mut sources: Vec<MySqlSelectSource> = Vec::new();
    for from in tables {
        let (relation, source) =
            render_select_table(&from.relation, render_context.as_deref_mut())?;
        if sources.is_empty() {
            rendered = relation;
        } else {
            rendered.push_str(" CROSS JOIN ");
            rendered.push_str(&relation);
        }
        sources.push(source);
        for join in &from.joins {
            if let TableFactor::NestedJoin {
                table_with_joins,
                alias,
            } = &join.relation
            {
                if alias.is_some() {
                    return unsupported("SELECT nested join alias");
                }
                rendered.push_str(&render_nested_join(
                    &join.join_operator,
                    table_with_joins,
                    &mut sources,
                    render_context.as_deref_mut(),
                )?);
                continue;
            }
            let (joined, mut source) =
                render_select_table(&join.relation, render_context.as_deref_mut())?;
            let (keyword, constraint) = checked_join(&join.join_operator)?;
            match keyword {
                // The side that can go missing is the one whose columns
                // stop being NOT NULL.
                "LEFT JOIN" => source.outer = true,
                "RIGHT JOIN" => {
                    for earlier in &mut sources {
                        earlier.outer = true;
                    }
                }
                _ => {}
            }
            sources.push(source);
            rendered.push(' ');
            rendered.push_str(keyword);
            rendered.push(' ');
            rendered.push_str(&joined);
            match constraint {
                CheckedJoinConstraint::On(expr) => {
                    rendered.push_str(" ON ");
                    rendered.push_str(&render_join_predicate(expr, render_context.as_deref_mut())?);
                }
                CheckedJoinConstraint::Using(columns) => {
                    rendered.push_str(" USING (");
                    rendered.push_str(&render_join_using(columns)?);
                    rendered.push(')');
                }
                CheckedJoinConstraint::Everything => {}
            }
        }
    }
    if sources.is_empty() {
        return Ok((None, Vec::new()));
    }
    Ok((Some(rendered), sources))
}

/// Renders a join to two tables joined in parentheses —
/// `A LEFT JOIN (B JOIN C ON p) ON q` — which is how Sequelize joins through
/// a many-to-many table. The engine has no parenthesized join, so it is
/// written as joins one after another that answer the same rows.
///
/// Inner joins can be taken in any order, so `JOIN (B JOIN C ON p) ON q` is
/// `JOIN B JOIN C ON p AND q`.
///
/// A left join keeps a row of `B` only when some row of `C` matches it, so
/// `LEFT JOIN (B JOIN C ON p) ON q` is
/// `LEFT JOIN B ON q AND EXISTS (SELECT 1 FROM C WHERE p) LEFT JOIN C ON p`,
/// with the parts of `q` naming `C` moved into both places `C` is read. When
/// the first join leaves `B` missing, the second must find no row of `C`
/// either; `p` matching a column of `C` against a column of `B` is what makes
/// sure of that, a missing row's column equalling nothing.
fn render_nested_join(
    operator: &sqlparser::ast::JoinOperator,
    nested: &sqlparser::ast::TableWithJoins,
    sources: &mut Vec<MySqlSelectSource>,
    mut render_context: Option<&mut SelectRenderContext<'_>>,
) -> Result<String, ParseError> {
    let (keyword, CheckedJoinConstraint::On(outer_on)) = checked_join(operator)? else {
        return unsupported("SELECT nested join without ON");
    };
    if keyword != "JOIN" && keyword != "LEFT JOIN" {
        return unsupported("SELECT nested join form");
    }
    let [inner_join] = nested.joins.as_slice() else {
        return unsupported("SELECT nested join of other than two tables");
    };
    let ("JOIN", CheckedJoinConstraint::On(inner_on)) = checked_join(&inner_join.join_operator)?
    else {
        return unsupported("SELECT nested join form");
    };
    if !matches!(nested.relation, TableFactor::Table { .. })
        || !matches!(inner_join.relation, TableFactor::Table { .. })
    {
        return unsupported("SELECT nested join of other than tables");
    }
    let (first, mut first_source) =
        render_select_table(&nested.relation, render_context.as_deref_mut())?;
    let (second, mut second_source) =
        render_select_table(&inner_join.relation, render_context.as_deref_mut())?;
    let first_name = first_source.reference.clone();
    let second_name = second_source.reference.clone();
    if first_name.eq_ignore_ascii_case(&second_name) {
        return unsupported("SELECT nested join naming one table twice");
    }
    let names_the_second =
        |expr: &Expr| tables_named(expr).map(|tables| tables_include(&tables, &second_name));
    let mut outer_parts = Vec::new();
    let mut parts_naming_the_second = Vec::new();
    for part in conjuncts(outer_on) {
        match names_the_second(part) {
            Some(true) => parts_naming_the_second.push(part),
            Some(false) => outer_parts.push(part),
            None => return unsupported("SELECT nested join ON naming a column without its table"),
        }
    }
    let Some(inner_tables) = tables_named(inner_on) else {
        return unsupported("SELECT nested join ON naming a column without its table");
    };
    if inner_tables.iter().any(|table| {
        !table.eq_ignore_ascii_case(&first_name) && !table.eq_ignore_ascii_case(&second_name)
    }) {
        return unsupported("SELECT nested join ON naming a table outside it");
    }
    if keyword == "JOIN" {
        let mut on = render_join_predicate(inner_on, render_context.as_deref_mut())?;
        for part in conjuncts(outer_on) {
            on = format!(
                "({on} AND {})",
                render_join_predicate(part, render_context.as_deref_mut())?
            );
        }
        sources.push(first_source);
        sources.push(second_source);
        return Ok(format!(" JOIN {first} JOIN {second} ON {on}"));
    }
    if !conjuncts(inner_on)
        .into_iter()
        .any(|part| matches_the_two_tables(part, &first_name, &second_name))
    {
        return unsupported(
            "SELECT nested left join whose inner ON matches no column of each table",
        );
    }
    let mut second_on = render_join_predicate(inner_on, render_context.as_deref_mut())?;
    for part in parts_naming_the_second {
        second_on = format!(
            "({second_on} AND {})",
            render_join_predicate(part, render_context.as_deref_mut())?
        );
    }
    let mut first_on = format!("EXISTS (SELECT 1 FROM {second} WHERE {second_on})");
    for part in outer_parts.into_iter().rev() {
        first_on = format!(
            "({} AND {first_on})",
            render_join_predicate(part, render_context.as_deref_mut())?
        );
    }
    first_source.outer = true;
    second_source.outer = true;
    sources.push(first_source);
    sources.push(second_source);
    Ok(format!(
        " LEFT JOIN {first} ON {first_on} LEFT JOIN {second} ON {second_on}"
    ))
}

/// The parts of a condition joined by `AND`, parentheses around it dropped.
fn conjuncts(expr: &Expr) -> Vec<&Expr> {
    match expr {
        Expr::Nested(inner) => conjuncts(inner),
        Expr::BinaryOp {
            left,
            op: BinaryOperator::And,
            right,
        } => {
            let mut parts = conjuncts(left);
            parts.extend(conjuncts(right));
            parts
        }
        _ => vec![expr],
    }
}

/// Whether a condition is `x.a = y.b` with one side naming each table.
fn matches_the_two_tables(expr: &Expr, first: &str, second: &str) -> bool {
    let Expr::BinaryOp {
        left,
        op: BinaryOperator::Eq,
        right,
    } = expr
    else {
        return false;
    };
    let (Expr::CompoundIdentifier(left), Expr::CompoundIdentifier(right)) =
        (unwrapped(left), unwrapped(right))
    else {
        return false;
    };
    let [left_table, _] = left.as_slice() else {
        return false;
    };
    let [right_table, _] = right.as_slice() else {
        return false;
    };
    (left_table.value.eq_ignore_ascii_case(first) && right_table.value.eq_ignore_ascii_case(second))
        || (left_table.value.eq_ignore_ascii_case(second)
            && right_table.value.eq_ignore_ascii_case(first))
}

/// The tables a join condition names its columns through, or `None` when it
/// names a column without its table or holds anything else this does not
/// look inside.
fn tables_named(expr: &Expr) -> Option<Vec<String>> {
    let mut tables = Vec::new();
    collect_tables_named(expr, &mut tables)?;
    Some(tables)
}

fn collect_tables_named(expr: &Expr, tables: &mut Vec<String>) -> Option<()> {
    match expr {
        Expr::CompoundIdentifier(parts) => {
            let [table, _] = parts.as_slice() else {
                return None;
            };
            tables.push(table.value.clone());
        }
        Expr::Value(_) => {}
        Expr::Nested(inner)
        | Expr::UnaryOp { expr: inner, .. }
        | Expr::IsNull(inner)
        | Expr::IsNotNull(inner)
        | Expr::IsTrue(inner)
        | Expr::IsFalse(inner)
        | Expr::IsNotTrue(inner)
        | Expr::IsNotFalse(inner) => collect_tables_named(inner, tables)?,
        Expr::BinaryOp { left, right, .. } => {
            collect_tables_named(left, tables)?;
            collect_tables_named(right, tables)?;
        }
        Expr::InList { expr, list, .. } => {
            collect_tables_named(expr, tables)?;
            for item in list {
                collect_tables_named(item, tables)?;
            }
        }
        Expr::Between {
            expr, low, high, ..
        } => {
            collect_tables_named(expr, tables)?;
            collect_tables_named(low, tables)?;
            collect_tables_named(high, tables)?;
        }
        _ => return None,
    }
    Some(())
}

fn tables_include(tables: &[String], name: &str) -> bool {
    tables.iter().any(|table| table.eq_ignore_ascii_case(name))
}

/// Renders what a join matches its two tables on.
///
/// Matching one column against another is what a join is for, and that is
/// rendered here. Anything else the `ON` says is a comparison against a value
/// — `ON t.id = u.team_id AND t.name = 'red'` is how a statement narrows the
/// side it joins to — and goes through the reader a `WHERE` comparison goes
/// through, so the value is held to the column's own type the same way.
fn render_join_predicate(
    expr: &Expr,
    mut render_context: Option<&mut SelectRenderContext<'_>>,
) -> Result<String, ParseError> {
    match expr {
        Expr::Nested(inner) => Ok(format!(
            "({})",
            render_join_predicate(inner, render_context)?
        )),
        Expr::BinaryOp {
            left,
            op: BinaryOperator::And,
            right,
        } => Ok(format!(
            "({} AND {})",
            render_join_predicate(left, render_context.as_deref_mut())?,
            render_join_predicate(right, render_context)?
        )),
        // Inside a `SELECT` the two columns are recorded and held to each other
        // below; an `UPDATE` or a `DELETE` has nowhere to record them.
        Expr::BinaryOp {
            left,
            op: BinaryOperator::Eq,
            right,
        } if render_context.is_none()
            && render_join_column(left).is_ok()
            && render_join_column(right).is_ok() =>
        {
            Ok(format!(
                "({} = {})",
                render_join_column(left)?,
                render_join_column(right)?
            ))
        }
        Expr::BinaryOp { left, op, right } if is_checked_select_comparison_operator(op) => {
            // An `UPDATE` or a `DELETE` renders its own `FROM` without a
            // statement to record the comparison in, so a value there is
            // turned away rather than left unchecked.
            let Some(render_context) = render_context else {
                return unsupported("SELECT JOIN ON predicate");
            };
            render_checked_select_comparison(left, op, right, render_context)
        }
        _ => unsupported("SELECT JOIN ON predicate"),
    }
}

fn render_join_column(expr: &Expr) -> Result<String, ParseError> {
    match expr {
        Expr::CompoundIdentifier(parts) if parts.len() == 2 => Ok(format!(
            "{}.{}",
            render_ident(&parts[0]),
            render_ident(&parts[1])
        )),
        _ => unsupported("SELECT JOIN ON requires a qualified column on each side"),
    }
}

/// Renders one derived table — a whole statement standing where a table does.
///
/// Its body has to read one table and project its columns, for the reason a
/// CTE's body does: an expression or a wildcard leaves no name to resolve a
/// result column's ordinal through.
fn render_derived_table(
    lateral: bool,
    subquery: &sqlparser::ast::Query,
    alias: Option<&sqlparser::ast::TableAlias>,
    render_context: Option<&mut SelectRenderContext<'_>>,
) -> Result<(String, MySqlSelectSource), ParseError> {
    if lateral {
        return unsupported("LATERAL derived table");
    }
    // MySQL requires the alias: a derived table without one is 1248.
    let Some(alias) = alias else {
        return unsupported("derived table without an alias");
    };
    if !alias.columns.is_empty() || alias.at.is_some() {
        return unsupported("derived table naming its own columns");
    }
    let Some(render_context) = render_context else {
        return unsupported("derived table outside a SELECT");
    };
    if std::mem::take(&mut render_context.counts_the_rows_of_the_derived_table) {
        return render_counted_derived_table(subquery, alias, render_context);
    }
    if let Some(select) = derived::body_joining_tables(subquery) {
        if !render_context.renders_the_statements_own_from {
            return unsupported("derived table joining tables inside another statement");
        }
        return derived::render_derived_table_joining_tables(
            subquery,
            select,
            alias,
            render_context,
        );
    }
    let body = match unwrap_query_wrappers(subquery.body.as_ref())? {
        SetExpr::SetOperation { .. } => render_derived_catalog_union(subquery, render_context)?,
        _ => render_subquery(subquery, render_context)?.0,
    };
    let Some(source) = render_context.subquery_tables.pop() else {
        return unsupported("derived table requires one table");
    };
    let (projected_columns, derived) = derived::derived_columns(subquery, &source, render_context)?;
    Ok((
        format!("({body}) AS {}", render_ident(&alias.name)),
        MySqlSelectSource {
            reference: alias.name.value.clone(),
            table: source.table,
            outer: false,
            branch: 0,
            subquery: false,
            read_by_a_result_subquery: false,
            projected_columns,
            derived: Some(derived),
            catalog: source.catalog,
            hinted_indexes: Vec::new(),
        },
    ))
}

/// Renders a derived table the statement only counts the rows of, whose body
/// may cut its rows with a `LIMIT` and project what it likes.
///
/// Rails counts a relation carrying a limit as
/// `SELECT COUNT(*) FROM (SELECT 1 AS one FROM posts LIMIT 3 OFFSET 0)
/// subquery_for_count`. Measured on MySQL 8.4.11, that answers how many rows
/// the limit leaves — 3 of 3, and 1 past an offset of 2 — in the shape a
/// plain `COUNT(*)` answers. No column of the body is read, so what it
/// projects has no shape to report, and which rows a limit without an order
/// keeps does not change how many it keeps. An `ORDER BY`, which could still
/// name what MySQL refuses, and a bound row count are refused.
fn render_counted_derived_table(
    subquery: &sqlparser::ast::Query,
    alias: &sqlparser::ast::TableAlias,
    render_context: &mut SelectRenderContext<'_>,
) -> Result<(String, MySqlSelectSource), ParseError> {
    if subquery.with.is_some()
        || subquery.order_by.is_some()
        || subquery.fetch.is_some()
        || !subquery.locks.is_empty()
        || subquery.for_clause.is_some()
        || subquery.settings.is_some()
        || subquery.format_clause.is_some()
        || !subquery.pipe_operators.is_empty()
    {
        return unsupported("counted derived table clause");
    }
    let SetExpr::Select(select) = subquery.body.as_ref() else {
        return unsupported("counted derived table body");
    };
    let comparisons_before = render_context.checked_comparisons.len();
    let (mut body, _) = render_subquery_select(select, render_context)?;
    // The body's table is read under the derived table's name, so a
    // comparison naming the body's table names the one table the statement
    // reads.
    let inner = render_context
        .subquery_tables
        .last()
        .map(|source| source.reference.clone());
    for comparison in &mut render_context.checked_comparisons[comparisons_before..] {
        if comparison.qualifier.as_deref().is_some_and(|qualifier| {
            inner
                .as_deref()
                .is_some_and(|inner| inner.eq_ignore_ascii_case(qualifier))
        }) {
            comparison.qualifier = None;
        }
    }
    if let Some(limit) = &subquery.limit_clause {
        let mut bound_row_counts = Vec::new();
        body.push_str(&render_select_limit(
            limit,
            render_context,
            &mut bound_row_counts,
            None,
        )?);
        if !bound_row_counts.is_empty() {
            return unsupported("bound row count in a counted derived table");
        }
    }
    let Some(source) = render_context.subquery_tables.pop() else {
        return unsupported("derived table requires one table");
    };
    Ok((
        format!("({body}) AS {}", render_ident(&alias.name)),
        MySqlSelectSource {
            reference: alias.name.value.clone(),
            derived: Some(derived::only_counted(&source)),
            table: source.table,
            outer: false,
            branch: 0,
            subquery: false,
            read_by_a_result_subquery: false,
            projected_columns: Vec::new(),
            catalog: source.catalog,
            hinted_indexes: Vec::new(),
        },
    ))
}

/// Renders a derived table whose body is a `UNION` of branches that each
/// read the same columns of the same `information_schema` table, leaving that
/// one table recorded as what the derived table reads.
///
/// TypeORM reads a table's indexes and keys this way, one branch for every
/// table it syncs. Every branch reads the one table's columns, so the derived
/// table's columns are that table's.
fn render_derived_catalog_union(
    subquery: &sqlparser::ast::Query,
    render_context: &mut SelectRenderContext<'_>,
) -> Result<String, ParseError> {
    if subquery.with.is_some()
        || subquery.order_by.is_some()
        || subquery.limit_clause.is_some()
        || subquery.fetch.is_some()
        || !subquery.locks.is_empty()
        || subquery.for_clause.is_some()
        || subquery.settings.is_some()
        || subquery.format_clause.is_some()
        || !subquery.pipe_operators.is_empty()
    {
        return unsupported("SELECT subquery clause");
    }
    let (keyword, branches) = union_branches(subquery.body.as_ref())?;
    let held_before = render_context.subquery_tables.len();
    let mut rendered = Vec::with_capacity(branches.len());
    for select in branches {
        rendered.push(render_subquery_select(select, render_context)?.0);
    }
    // Two branches reading the same columns of the same table under the same
    // name are recorded as one table, so anything else leaves more than one.
    let read_one_catalog_table = render_context.subquery_tables.len() == held_before + 1
        && render_context
            .subquery_tables
            .last()
            .is_some_and(|source| source.catalog.is_some());
    if !read_one_catalog_table {
        return unsupported("derived table UNION over anything but one information_schema table");
    }
    Ok(rendered.join(&format!(" {keyword} ")))
}

/// Renders a `WITH` clause, and returns what each name stands for.
///
/// Each body has to read one table and project its columns in order, because a
/// result column reaching the frontend names the CTE and an ordinal, and the
/// only way to answer what type it has is to read that ordinal from the table
/// the CTE reads.
fn render_common_table_expressions(
    with: &sqlparser::ast::With,
    render_context: &mut SelectRenderContext<'_>,
) -> Result<(String, Vec<MySqlSelectSource>), ParseError> {
    if with.recursive {
        return unsupported("WITH RECURSIVE");
    }
    let mut rendered = Vec::with_capacity(with.cte_tables.len());
    let mut sources = Vec::with_capacity(with.cte_tables.len());
    for cte in &with.cte_tables {
        if cte.from.is_some()
            || cte.materialized.is_some()
            || !cte.alias.columns.is_empty()
            || cte.alias.at.is_some()
        {
            return unsupported("WITH option");
        }
        let (body, _) = render_subquery(&cte.query, render_context)?;
        let Some(source) = render_context.subquery_tables.pop() else {
            return unsupported("WITH body requires one table");
        };
        let (projected_columns, derived) =
            derived::derived_columns(&cte.query, &source, render_context)?;
        rendered.push(format!("{} AS ({body})", render_ident(&cte.alias.name)));
        sources.push(MySqlSelectSource {
            reference: cte.alias.name.value.clone(),
            table: source.table,
            outer: false,
            branch: 0,
            subquery: false,
            read_by_a_result_subquery: false,
            projected_columns,
            derived: Some(derived),
            catalog: source.catalog,
            hinted_indexes: Vec::new(),
        });
    }
    Ok((format!("WITH {} ", rendered.join(", ")), sources))
}

/// Renders `column IN (SELECT column FROM table)`.
///
/// The two columns have to be the same kind, which only the frontend can see,
/// so the pair is recorded for it to check — a membership test raises the same
/// coercion question a literal comparison does.
fn render_in_subquery(
    expr: &Expr,
    subquery: &sqlparser::ast::Query,
    negated: bool,
    render_context: &mut SelectRenderContext<'_>,
) -> Result<String, ParseError> {
    let Expr::Identifier(column) = expr else {
        return unsupported("SELECT IN requires one unqualified column");
    };
    let (rendered, projected) = render_subquery(subquery, render_context)?;
    let Some((inner_table, inner_column_name)) = projected else {
        return unsupported("SELECT IN requires a subquery projecting one column");
    };
    render_context
        .checked_subquery_comparisons
        .push(CheckedSubqueryComparison {
            qualifier: None,
            column_name: column.value.clone(),
            inner_table,
            inner_column_name,
            fixed_columns: Vec::new(),
        });
    Ok(format!(
        "({} {}IN ({rendered}))",
        render_ident(column),
        if negated { "NOT " } else { "" }
    ))
}

/// Renders one subquery, and returns the single column it projects when it
/// projects one.
///
/// Its tables are kept apart from the statement's own: they are authorized and
/// refused the same way, but they name none of the result columns, so the rules
/// about a joined projection do not apply to them.
fn render_subquery(
    subquery: &sqlparser::ast::Query,
    render_context: &mut SelectRenderContext<'_>,
) -> Result<(String, Option<(String, String)>), ParseError> {
    if subquery.with.is_some()
        || subquery.order_by.is_some()
        || subquery.limit_clause.is_some()
        || subquery.fetch.is_some()
        || !subquery.locks.is_empty()
        || subquery.for_clause.is_some()
        || subquery.settings.is_some()
        || subquery.format_clause.is_some()
        || !subquery.pipe_operators.is_empty()
    {
        return unsupported("SELECT subquery clause");
    }
    let SetExpr::Select(select) = subquery.body.as_ref() else {
        return unsupported("SELECT subquery body");
    };
    render_subquery_select(select, render_context)
}

/// Renders the one `SELECT` a subquery is, recording the table it reads.
fn render_subquery_select(
    select: &sqlparser::ast::Select,
    render_context: &mut SelectRenderContext<'_>,
) -> Result<(String, Option<(String, String)>), ParseError> {
    let may_join = std::mem::take(&mut render_context.an_exists_may_join);
    let comparisons_before = render_context.checked_comparisons.len();
    let (rendered, mut sources) = render_select_body(select, render_context)?;
    if sources.len() > 1 && may_join {
        // An unqualified name a subquery compares is the column of whichever
        // of its tables has one, so those tables travel with it.
        let references = sources
            .iter()
            .map(|source| source.reference.clone())
            .collect::<Vec<_>>();
        let references = references.iter().map(String::as_str).collect::<Vec<_>>();
        for comparison in &mut render_context.checked_comparisons[comparisons_before..] {
            comparison.name_the_inner_sources(&references);
        }
        for mut source in sources {
            source.subquery = true;
            if !render_context.subquery_tables.iter().any(|held| {
                held.reference == source.reference
                    && held.table == source.table
                    && held.projected_columns.is_empty()
            }) {
                render_context.subquery_tables.push(source);
            }
        }
        return Ok((rendered, None));
    }
    let [source] = sources.as_slice() else {
        return unsupported("SELECT subquery requires one table");
    };
    // An unqualified name a subquery compares is the subquery's column when it
    // has one, so the subquery it was written inside travels with it.
    for comparison in &mut render_context.checked_comparisons[comparisons_before..] {
        comparison.name_the_inner_sources(&[&source.reference]);
    }
    let projected = match select.projection.as_slice() {
        [SelectItem::UnnamedExpr(Expr::Identifier(column))] => {
            Some((source.table.as_str().to_owned(), column.value.clone()))
        }
        // A qualified name is the same column when the qualifier is the table
        // the subquery reads, which is how a correlated one is written.
        [SelectItem::UnnamedExpr(Expr::CompoundIdentifier(parts))]
            if parts.len() == 2 && parts[0].value.eq_ignore_ascii_case(&source.reference) =>
        {
            Some((source.table.as_str().to_owned(), parts[1].value.clone()))
        }
        _ => None,
    };
    let projected_columns = match (select.projection.as_slice(), source.catalog) {
        // An `information_schema` table's columns are known without reading
        // any stored DDL, so a wildcard over one names them all.
        ([SelectItem::Wildcard(options)], Some(catalog))
            if wildcard_options_are_empty(options) && catalog.answers_every_column() =>
        {
            Some(
                catalog
                    .columns()
                    .iter()
                    .map(|(column, _)| (*column).to_owned())
                    .collect(),
            )
        }
        _ => select
            .projection
            .iter()
            .map(|item| match item {
                SelectItem::UnnamedExpr(Expr::Identifier(column)) => Some(column.value.clone()),
                SelectItem::ExprWithAlias {
                    expr: Expr::Identifier(column),
                    ..
                } => Some(column.value.clone()),
                SelectItem::UnnamedExpr(Expr::CompoundIdentifier(parts)) if parts.len() == 2 => {
                    Some(parts[1].value.clone())
                }
                _ => None,
            })
            .collect::<Option<Vec<_>>>(),
    };
    for source in &mut sources {
        source.subquery = true;
        source.projected_columns = projected_columns.clone().unwrap_or_default();
    }
    // Two subqueries over the same table are still one table to authorize and
    // to look a column up in, so the source is recorded once. Recording it
    // twice would make a column name look ambiguous where it is not.
    for source in sources {
        let already = render_context.subquery_tables.iter().any(|held| {
            held.reference == source.reference
                && held.table == source.table
                && held.projected_columns == source.projected_columns
        });
        if !already {
            render_context.subquery_tables.push(source);
        }
    }
    Ok((rendered, projected))
}

/// Renders a `HAVING`, which sees an aggregate where a `WHERE` sees a column.
///
/// A comparison on a grouping column goes through the same checked path a
/// `WHERE` comparison does. One on an aggregate cannot, since there is no
/// column to compare types against — so the aggregate's own argument column is
/// recorded instead, which is what makes an integer literal safe to compare
/// against. `COUNT` records nothing, because it answers an integer whatever it
/// counts.
/// Answers whether an expression is built only from aggregate calls and
/// literals, with no column of its own.
///
/// A statement carrying a `HAVING` and no `GROUP BY` is aggregated over one
/// implicit group, and a bare column has no single row to come from. MySQL
/// says so with 1140 for one in the projection and 1054 for one in the
/// `HAVING`, so neither is let through.
fn aggregates_or_literals_only(expr: &Expr) -> bool {
    match expr {
        Expr::Function(function) => names_an_aggregate_call(function),
        Expr::Value(_) => true,
        Expr::Nested(inner) | Expr::UnaryOp { expr: inner, .. } => {
            aggregates_or_literals_only(inner)
        }
        Expr::BinaryOp { left, right, .. } => {
            aggregates_or_literals_only(left) && aggregates_or_literals_only(right)
        }
        Expr::Between {
            expr, low, high, ..
        } => {
            aggregates_or_literals_only(expr)
                && aggregates_or_literals_only(low)
                && aggregates_or_literals_only(high)
        }
        _ => false,
    }
}

/// Holds an aggregated statement's projection to what MySQL lets it name.
///
/// Measured on MySQL 8.4.11: `SELECT id, SUM(n) FROM t` answers 1140, and so
/// do `SELECT id + SUM(n)` and `SELECT UPPER(name), COUNT(*)` — a column
/// anywhere in the projection, not only one standing on its own. A literal is
/// fine, and so is arithmetic over the aggregate itself.
fn hold_the_aggregated_projection(select: &sqlparser::ast::Select) -> Result<(), ParseError> {
    for item in &select.projection {
        let projected = match item {
            SelectItem::UnnamedExpr(expr) | SelectItem::ExprWithAlias { expr, .. } => expr,
            // A wildcard hides whether anything is aggregated.
            _ => return unsupported("aggregated projection over a wildcard"),
        };
        if !aggregates_or_literals_only(projected) {
            return unsupported("aggregated projection naming an ungrouped column");
        }
    }
    Ok(())
}

/// Reports whether a statement's projection aggregates it.
///
/// A window does not: measured on MySQL 8.4.11, `SELECT id, ROW_NUMBER() OVER
/// (ORDER BY id)` and `SELECT id, COUNT(*) OVER ()` each answer a row per row.
/// Neither does a subquery, whose aggregate belongs to the statement inside it.
fn projects_an_aggregate(select: &sqlparser::ast::Select) -> bool {
    select.projection.iter().any(|item| {
        matches!(
            item,
            SelectItem::UnnamedExpr(expr) | SelectItem::ExprWithAlias { expr, .. }
                if names_an_aggregate(expr)
        )
    })
}

fn names_an_aggregate(expr: &Expr) -> bool {
    match expr {
        Expr::Function(function) => names_an_aggregate_call(function),
        Expr::Nested(inner) | Expr::UnaryOp { expr: inner, .. } => names_an_aggregate(inner),
        Expr::BinaryOp { left, right, .. } => names_an_aggregate(left) || names_an_aggregate(right),
        _ => false,
    }
}

/// Reports whether one call aggregates the rows it is given.
///
/// A count and an aggregate over a column do. So does an aggregate wrapped in
/// a fallback — `IFNULL(SUM(n), 0)` — which is why that is read here rather
/// than left to the two names above.
fn names_an_aggregate_call(function: &sqlparser::ast::Function) -> bool {
    static_select_metadata::is_count_call(function)
        || static_select_metadata::column_aggregate_argument(function).is_some()
        || static_select_metadata::qualified_aggregate_argument(function).is_some()
        || static_select_metadata::aggregate_over_branches(function).is_some()
        || matches!(
            static_select_metadata::scalar_call(function),
            Some(
                StaticSelectMetadata::DefaultedAggregate(_)
                    | StaticSelectMetadata::ScalarCall {
                        function: static_select_metadata::ScalarFunction::CollectsBuiltJson,
                        ..
                    }
                    | StaticSelectMetadata::RoundedAggregate { .. }
            )
        )
}

fn render_having_predicate(
    expr: &Expr,
    render_context: &mut SelectRenderContext<'_>,
) -> Result<String, ParseError> {
    match expr {
        Expr::BinaryOp { left, op, right }
            if matches!(op, BinaryOperator::And | BinaryOperator::Or) =>
        {
            let op = if matches!(op, BinaryOperator::And) {
                "AND"
            } else {
                "OR"
            };
            Ok(format!(
                "({} {op} {})",
                render_having_predicate(left, render_context)?,
                render_having_predicate(right, render_context)?
            ))
        }
        Expr::UnaryOp {
            op: UnaryOperator::Not,
            expr,
        } => Ok(format!(
            "(NOT {})",
            render_having_predicate(expr, render_context)?
        )),
        Expr::Nested(expr) => Ok(format!(
            "({})",
            render_having_predicate(expr, render_context)?
        )),
        Expr::BinaryOp { left, op, right }
            if is_checked_select_comparison_operator(op)
                && matches!(left.as_ref(), Expr::Function(function)
                    if static_select_metadata::is_count_call(function)
                        || aggregated_column(function).is_some()) =>
        {
            let Expr::Function(function) = left.as_ref() else {
                unreachable!("the guard requires a checked aggregate");
            };
            let (rendered_right, rhs) =
                render_checked_select_comparison_rhs(right, render_context)?;
            let operator =
                checked_select_comparison_operator(op).expect("comparison operator guard");
            // A count is a whole number whatever it counts, so a bound value
            // meets it the way it meets any whole number; the frontend holds
            // what binds there to the kinds MySQL reads alike.
            if matches!(rhs, CheckedSelectComparisonRhs::Placeholder { .. })
                && static_select_metadata::is_count_call(function)
            {
                render_context
                    .checked_comparisons
                    .push(CheckedSelectComparison {
                        qualifier: None,
                        inner_sources: Vec::new(),
                        column_name: "COUNT".to_owned(),
                        operator,
                        rhs,
                        collated: false,
                        answers: Some(crate::CheckedComparisonAnswer::RowCount),
                    });
                return Ok(format!(
                    "({} {} {rendered_right})",
                    render_aggregate_call(function, render_context),
                    checked_select_comparison_sql_operator(op)
                ));
            }
            // Measured on MySQL 8.4.11: a count against a word compares the
            // two as doubles, so a word naming a whole number is that number
            // — Rails writes `HAVING (COUNT(*) > '1')` — and says nothing.
            let (rendered_right, rhs) = if static_select_metadata::is_count_call(function) {
                whole_number_a_written_word_names(rendered_right, rhs)
            } else {
                (rendered_right, rhs)
            };
            let aggregated = aggregated_column(function);
            // A bound value meets a sum, a least or a greatest the way it
            // meets the column itself, which is how the frontend holds what
            // binds there: over a whole-number column MySQL's sum is an exact
            // number, and each compares with the value as a number.
            let bound_value_taken = matches!(rhs, CheckedSelectComparisonRhs::Placeholder { .. })
                && matches!(
                    aggregated,
                    Some((
                        static_select_metadata::ColumnAggregateKind::Sum
                            | static_select_metadata::ColumnAggregateKind::MinMax,
                        _,
                        _
                    ))
                );
            if !matches!(rhs, CheckedSelectComparisonRhs::SignedInteger(_)) && !bound_value_taken {
                return unsupported("HAVING comparison requires an exact signed integer");
            }
            if let Some((_, qualifier, column)) = aggregated {
                render_context.checks_type_sensitive_expression = true;
                if render_context
                    .decimal_columns
                    .iter()
                    .any(|(known, _)| known.eq_ignore_ascii_case(&column.value))
                {
                    return unsupported(
                        "HAVING aggregate over DECIMAL requires exact numeric comparison",
                    );
                }
                render_context
                    .checked_comparisons
                    .push(CheckedSelectComparison {
                        qualifier: qualifier.map(|qualifier| qualifier.value.clone()),
                        inner_sources: Vec::new(),
                        column_name: column.value.clone(),
                        operator,
                        rhs,
                        collated: false,
                        answers: None,
                    });
            }
            Ok(format!(
                "({} {} {rendered_right})",
                render_aggregate_call(function, render_context),
                checked_select_comparison_sql_operator(op)
            ))
        }
        Expr::BinaryOp { left, op, right } if is_checked_select_comparison_operator(op) => {
            render_checked_select_comparison(left, op, right, render_context)
        }
        Expr::Between {
            expr,
            negated,
            low,
            high,
        } => render_checked_between(*negated, expr, low, high, render_context),
        _ => unsupported("HAVING predicate"),
    }
}

/// Reads `COALESCE(t.c, 0)` or `IFNULL(t.c, 0)` over a count a derived table
/// the statement reads worked out — Prisma's relation count, `COALESCE(
/// aggr_selection_0_Post._aggr_count_posts, 0)` over a `LEFT JOIN` of a
/// grouped count — which answers the count's own shape, never NULL.
///
/// Measured on MySQL 8.4.11: a `LONGLONG` of 21 with the NOT NULL and binary
/// flags, naming no table.
fn defaults_a_count_the_select_reads(
    expr: &Expr,
    select: &sqlparser::ast::Select,
) -> Option<StaticSelectMetadata> {
    let Expr::Function(function) = expr else {
        return None;
    };
    let (table, column) = defaulted_derived_count(function)?;
    counts_derived_tables_work_out(select)
        .iter()
        .any(|(counted_table, counted_column)| {
            counted_table.eq_ignore_ascii_case(&table.value)
                && counted_column.eq_ignore_ascii_case(&column.value)
        })
        .then(|| StaticSelectMetadata::DefaultedAggregate(Box::new(StaticSelectMetadata::Count)))
}

/// Each column a derived table of the `SELECT` projects as `COUNT(...) AS
/// name`, with the name the `SELECT` reads the table under.
fn counts_derived_tables_work_out(select: &sqlparser::ast::Select) -> Vec<(String, String)> {
    let mut counts = Vec::new();
    let relations = select.from.iter().flat_map(|source| {
        std::iter::once(&source.relation).chain(source.joins.iter().map(|join| &join.relation))
    });
    for relation in relations {
        let TableFactor::Derived {
            subquery,
            alias: Some(alias),
            ..
        } = relation
        else {
            continue;
        };
        let SetExpr::Select(body) = subquery.body.as_ref() else {
            continue;
        };
        for item in &body.projection {
            if let SelectItem::ExprWithAlias {
                expr: Expr::Function(function),
                alias: name,
            } = item
            {
                if static_select_metadata::is_count_call(function) {
                    counts.push((alias.name.value.clone(), name.value.clone()));
                }
            }
        }
    }
    counts
}

/// The table and the column of `COALESCE(t.c, n)` or `IFNULL(t.c, n)`, `n` a
/// whole number, which falls a column named through its table back onto a
/// number.
fn defaulted_derived_count(
    function: &sqlparser::ast::Function,
) -> Option<(&sqlparser::ast::Ident, &sqlparser::ast::Ident)> {
    let [ObjectNamePart::Identifier(name)] = function.name.0.as_slice() else {
        return None;
    };
    if name.quote_style.is_some()
        || !(name.value.eq_ignore_ascii_case("COALESCE")
            || name.value.eq_ignore_ascii_case("IFNULL"))
    {
        return None;
    }
    let FunctionArguments::List(arguments) = &function.args else {
        return None;
    };
    if arguments.duplicate_treatment.is_some() || !arguments.clauses.is_empty() {
        return None;
    }
    let [sqlparser::ast::FunctionArg::Unnamed(sqlparser::ast::FunctionArgExpr::Expr(
        Expr::CompoundIdentifier(parts),
    )), sqlparser::ast::FunctionArg::Unnamed(sqlparser::ast::FunctionArgExpr::Expr(fallback))] =
        arguments.args.as_slice()
    else {
        return None;
    };
    let [table, column] = parts.as_slice() else {
        return None;
    };
    direct_signed_integer(fallback)?;
    Some((table, column))
}

fn defaulted_fallback(function: &sqlparser::ast::Function) -> &Expr {
    let FunctionArguments::List(arguments) = &function.args else {
        unreachable!("a defaulted count was read with its arguments");
    };
    let [_, sqlparser::ast::FunctionArg::Unnamed(sqlparser::ast::FunctionArgExpr::Expr(fallback))] =
        arguments.args.as_slice()
    else {
        unreachable!("a defaulted count was read with two arguments");
    };
    fallback
}

/// The kind, the table if one is named, and the column of an aggregate over
/// one column — `SUM(views)` or `SUM(post.views)`.
///
/// A column named with its table is taken under a sum, a least or a greatest,
/// which answer a number over a whole-number column. A `GROUP_CONCAT` answers
/// a word, which MySQL compares with a number as a double and the engine
/// orders after every number.
fn aggregated_column(
    function: &sqlparser::ast::Function,
) -> Option<(
    static_select_metadata::ColumnAggregateKind,
    Option<&sqlparser::ast::Ident>,
    &sqlparser::ast::Ident,
)> {
    if let Some((kind, column)) = static_select_metadata::column_aggregate_argument(function) {
        return Some((kind, None, column));
    }
    static_select_metadata::qualified_aggregate_argument(function)
        .filter(|(kind, _, _)| {
            matches!(
                kind,
                static_select_metadata::ColumnAggregateKind::Sum
                    | static_select_metadata::ColumnAggregateKind::MinMax
            )
        })
        .map(|(kind, table, column)| (kind, Some(table), column))
}

pub(crate) fn select_static_result_metadata(
    query: &sqlparser::ast::Query,
) -> Vec<StaticSelectProjectionMetadata> {
    let SetExpr::Select(select) = query.body.as_ref() else {
        return Vec::new();
    };
    // A call reaching for a window by name is the same call with the window
    // written out, and this reads the same shape out of either. A statement
    // whose names do not resolve is refused where it is rendered.
    let resolved = resolve_named_windows(select);
    let select = match &resolved {
        Ok(resolved) if !select.named_window.is_empty() => resolved,
        _ => select,
    };
    let grouped_by_an_expression = grouping::expression_grouping_keys(select);
    if let Some(counted) = recursive::counted_projection(query, select) {
        return counted;
    }
    let rolled_up_keys = match &select.group_by {
        sqlparser::ast::GroupByExpr::Expressions(keys, modifiers)
            if matches!(
                modifiers.as_slice(),
                [sqlparser::ast::GroupByWithModifier::Rollup]
            ) =>
        {
            Some(keys.as_slice())
        }
        _ => None,
    };
    select
        .projection
        .iter()
        .map(|item| match item {
            SelectItem::UnnamedExpr(expr) | SelectItem::ExprWithAlias { expr, .. } => {
                if let Some(keys) = rolled_up_keys {
                    return match rollup::rolled_up_key(expr, keys) {
                        Some(_) => StaticSelectProjectionMetadata::Literal(
                            StaticSelectMetadata::RolledUpKey {
                                column_name: match expr {
                                    Expr::CompoundIdentifier(parts) => parts[1].value.clone(),
                                    Expr::Identifier(column) => column.value.clone(),
                                    _ => unreachable!("a rolled-up key is a whole column"),
                                },
                            },
                        ),
                        None => classify_static_select_expr(expr).map_or(
                            StaticSelectProjectionMetadata::Other,
                            |answer| {
                                StaticSelectProjectionMetadata::Literal(
                                    StaticSelectMetadata::FromARollup(Box::new(answer)),
                                )
                            },
                        ),
                    };
                }
                let answer = crate::written_value::read_written_value(expr)
                    .map(|(shape, _)| StaticSelectMetadata::WrittenValue(shape))
                    .or_else(|| defaults_a_count_the_select_reads(expr, select))
                    .or_else(|| classify_static_select_expr(expr));
                match (answer, grouped_by_an_expression) {
                    (None, _) => StaticSelectProjectionMetadata::Other,
                    (Some(answer), None) => StaticSelectProjectionMetadata::Literal(answer),
                    (Some(answer), Some(group_by)) => StaticSelectProjectionMetadata::Literal(
                        StaticSelectMetadata::FromTheGroupingTable {
                            answer: Box::new(answer),
                            key: grouping::is_named_by_a_key(item, group_by),
                        },
                    ),
                }
            }
            SelectItem::ExprWithAliases { .. } => StaticSelectProjectionMetadata::Other,
            SelectItem::Wildcard(_) | SelectItem::QualifiedWildcard(_, _) => {
                StaticSelectProjectionMetadata::Wildcard
            }
        })
        .collect()
}

fn render_select_order_by(
    order_by: &sqlparser::ast::OrderBy,
    projection: &[SelectItem],
    render_context: &mut SelectRenderContext<'_>,
) -> Result<String, ParseError> {
    let sqlparser::ast::OrderByKind::Expressions(expressions) = &order_by.kind else {
        return unsupported("SELECT ORDER BY option");
    };
    if expressions.is_empty() || order_by.interpolate.is_some() {
        return unsupported("SELECT ORDER BY option");
    }
    let is_pure_wildcard = matches!(
        projection,
        [SelectItem::Wildcard(options)] if wildcard_options_are_empty(options)
    );
    let has_wildcard = projection.iter().any(|item| {
        matches!(
            item,
            SelectItem::Wildcard(_) | SelectItem::QualifiedWildcard(_, _)
        )
    });
    expressions
        .iter()
        .map(|expression| {
            if expression.options.nulls_first.is_some() || expression.with_fill.is_some() {
                return unsupported("SELECT ORDER BY option");
            }
            let direction = if expression.options.asc == Some(false) {
                "DESC"
            } else {
                "ASC"
            };
            if let Expr::Identifier(order_name) = &expression.expr {
                if let Some(aliased) = projection.iter().find_map(|item| match item {
                    SelectItem::ExprWithAlias { expr, alias }
                        if alias.value.eq_ignore_ascii_case(&order_name.value) =>
                    {
                        Some(expr)
                    }
                    _ => None,
                }) {
                    if order_expression_uses_decimal(aliased, render_context.decimal_columns) {
                        return unsupported(
                            "SELECT ORDER BY alias over DECIMAL expression requires exact numeric ordering",
                        );
                    }
                    let set_column = match aliased {
                        Expr::Identifier(column) => Some(&column.value),
                        Expr::CompoundIdentifier(parts) if parts.len() == 2 => {
                            Some(&parts[1].value)
                        }
                        _ => None,
                    };
                    if set_column.is_some_and(|column| render_context.set_column(column).is_some())
                    {
                        return render_order_by_expr(aliased, direction, render_context);
                    }
                    if render_context.set_column(&order_name.value).is_some() {
                        return Ok(format!("{} {direction}", render_ident(order_name)));
                    }
                }
            }
            if let Some(ordinal) = order_by_ordinal(&expression.expr) {
                if ordinal == 0 {
                    return unsupported("SELECT ORDER BY ordinal outside the projection");
                }
                if has_wildcard {
                    if !is_pure_wildcard {
                        return unsupported(
                            "SELECT ORDER BY an ordinal over a wildcard projection",
                        );
                    }
                    if render_context.table_columns.is_empty() {
                        render_context.orders_wildcard_ordinal = true;
                        return Ok(format!("{ordinal} {direction}"));
                    }
                    if ordinal > render_context.table_columns.len() {
                        return unsupported("SELECT ORDER BY ordinal outside the projection");
                    }
                    render_context.orders_wildcard_ordinal = true;
                    render_context.orders_a_bare_column = true;
                    let column_name = &render_context.table_columns[ordinal - 1];
                    if let Some(members) = render_context.set_column(column_name) {
                        return set_member_order(
                            &render_ident_str(column_name),
                            members,
                            direction,
                        );
                    }
                    if let Some(members) = render_context.member_column(column_name) {
                        let position = member_position(&render_ident_str(column_name), members);
                        return Ok(format!("{position} {direction}"));
                    }
                    return Ok(format!("{} {direction}", render_ident_str(column_name)));
                }
                let expr = projected_expr(projection, ordinal)?;
                return render_order_by_expr(expr, direction, render_context);
            }
            render_order_by_expr(&expression.expr, direction, render_context)
        })
        .collect::<Result<Vec<_>, _>>()
        .map(|expressions| expressions.join(", "))
}

fn render_order_by_expr(
    expr: &Expr,
    direction: &str,
    render_context: &mut SelectRenderContext<'_>,
) -> Result<String, ParseError> {
    if order_expression_uses_decimal(expr, render_context.decimal_columns) {
        return unsupported(
            "SELECT ORDER BY expression over DECIMAL requires exact numeric ordering",
        );
    }
    match expr {
        Expr::Identifier(_) => {}
        Expr::CompoundIdentifier(parts) if parts.len() == 2 => {}
        Expr::Function(function)
            if static_select_metadata::is_count_call(function)
                || static_select_metadata::column_aggregate_argument(function).is_some() =>
        {
            if !static_select_metadata::is_count_call(function) {
                render_context.checks_type_sensitive_expression = true;
            }
        }
        Expr::BinaryOp { .. } if static_select_metadata::classify_arithmetic(expr).is_some() => {
            render_context.checks_type_sensitive_expression = true;
        }
        // `ORDER BY n IS NULL, n` is how a statement asks for the rows holding
        // nothing to come last, which neither MySQL nor the engine has a word
        // for. Both answer the test as 0 or 1 and sort by that, so the two
        // order the rows the same way — measured on MySQL 8.4.11.
        Expr::IsNull(inner) | Expr::IsNotNull(inner)
            if matches!(inner.as_ref(), Expr::Identifier(_))
                || matches!(inner.as_ref(), Expr::CompoundIdentifier(parts) if parts.len() == 2) => {
        }
        // `ORDER BY name COLLATE utf8mb4_bin` asks for PAD SPACE byte order.
        Expr::Collate { expr: inner, .. } if matches!(inner.as_ref(), Expr::Identifier(_)) => {}
        // `ORDER BY LOWER(name)` is how a report asks for an order it has
        // worked out rather than one a column holds. Any call this already
        // knows the shape of is ordered by, and the answer is collated the way
        // a text column is: measured on MySQL 8.4.11 over 'beta', 'Alpha',
        // 'alpha', 'Zulu' and 'apple', `LOWER`, `UPPER` and `CONCAT` each
        // order the rows the way the bare column does, and `LENGTH`, `ABS` and
        // `DATE` order by what they answer. A collation says nothing about a
        // number in the engine, so the same rendering covers both.
        _ if static_select_metadata::classify_static_select_expr(expr).is_some() => {
            render_context.checks_type_sensitive_expression = true;
            // A call answering something new each time it is read orders the
            // rows by nothing a client can hold this to, and whether each
            // engine reads it once or once a row is a rule of its own.
            if matches!(
                static_select_metadata::classify_static_select_expr(expr),
                Some(StaticSelectMetadata::ScalarCall {
                    function: ScalarFunction::Randomises,
                    ..
                })
            ) {
                return unsupported("SELECT ORDER BY a random number");
            }
            record_the_columns_a_text_call_reads(expr, render_context);
            return Ok(format!(
                "{} COLLATE MYSQL_UCA9_AI_CI {direction}",
                render_select_expr(expr, render_context)?
            ));
        }
        _ => return unsupported("SELECT ORDER BY expression"),
    }
    let collation = match expr {
        Expr::Identifier(column) => {
            render_context.orders_a_bare_column = true;
            if render_context.is_json_column(&column.value) {
                return unsupported("SELECT ORDER BY JSON requires JSON type ordering");
            }
            if let Some(members) = render_context.set_column(&column.value) {
                return set_member_order(&render_ident(column), members, direction);
            }
            if let Some(members) = render_context.member_column(&column.value) {
                let position = member_position(&render_ident(column), members);
                return Ok(format!("{position} {direction}"));
            }
            ""
        }
        Expr::CompoundIdentifier(parts) if parts.len() == 2 => {
            render_context.orders_a_bare_column = true;
            if render_context.is_json_column(&parts[1].value) {
                return unsupported("SELECT ORDER BY JSON requires JSON type ordering");
            }
            if render_context.set_column(&parts[1].value).is_some() {
                let column = render_select_expr(expr, render_context)?;
                let members = render_context
                    .set_column(&parts[1].value)
                    .expect("the SET column was found above");
                return set_member_order(&column, members, direction);
            }
            ""
        }
        Expr::Collate {
            expr: inner,
            collation,
        } => {
            let Expr::Identifier(column) = inner.as_ref() else {
                unreachable!("the collated shape was checked above");
            };
            let Some(orders_by_bytes) = collation_orders_by_bytes(collation) else {
                return unsupported("SELECT ORDER BY collation");
            };
            render_context.orders_a_bare_column = true;
            // An ENUM orders by the order its members were declared in, which
            // is not an order a collation has anything to say about.
            if render_context.member_column(&column.value).is_some()
                || render_context.set_column(&column.value).is_some()
            {
                return unsupported("SELECT ORDER BY collation over a member column");
            }
            // A column of words is declared with the collation this server
            // matches words under, so an ordering that wants their bytes has
            // to say so rather than say nothing.
            match (
                orders_by_bytes,
                render_context.is_text_column(&column.value),
            ) {
                (false, true) => " COLLATE MYSQL_UCA9_AI_CI",
                (true, true) if collation_is_utf8mb4_bin(collation) => " COLLATE MYSQL_UTF8MB4_BIN",
                (true, true) => " COLLATE BINARY",
                (_, false) => "",
            }
        }
        _ => "",
    };
    let ordered = match expr {
        Expr::Collate { expr: inner, .. } => render_select_expr(inner, render_context)?,
        _ => render_select_expr(expr, render_context)?,
    };
    Ok(format!("{ordered}{collation} {direction}"))
}

fn order_expression_uses_decimal(expr: &Expr, decimal_columns: &[(String, u32)]) -> bool {
    if matches!(expr, Expr::Identifier(_) | Expr::CompoundIdentifier(_)) {
        return false;
    }
    if contains_decimal_operand(expr, decimal_columns) {
        return true;
    }
    let names_decimal = |column: &str| {
        decimal_columns
            .iter()
            .any(|(known, _)| known.eq_ignore_ascii_case(column))
    };
    match static_select_metadata::classify_static_select_expr(expr) {
        Some(StaticSelectMetadata::ScalarCall { columns, .. }) => {
            columns.iter().any(|column| names_decimal(column))
        }
        Some(StaticSelectMetadata::ColumnAggregate { column_name, .. }) => {
            names_decimal(&column_name)
        }
        // An average over a `CASE` answers a `DECIMAL` however whole its
        // branches are, written out as the engine's `DECIMAL` average writes
        // it.
        Some(StaticSelectMetadata::AggregateOverBranches {
            kind: ColumnAggregateKind::Avg,
            ..
        }) => true,
        _ => false,
    }
}

/// Reports whether a collation orders text by its bytes, or nothing when it is
/// not one of the collations this holds.
///
/// Measured on MySQL 8.4.11 over 'beta', 'Alpha', 'alpha', 'Beta', 'Zulu' and
/// 'apple': `utf8mb4_bin` puts every capital first, which is byte order, and
/// `utf8mb4_0900_ai_ci` orders them the way the statement orders them with no
/// collation named at all. A collation from
/// another character set is 1253 there and refused here.
fn collation_orders_by_bytes(collation: &ObjectName) -> Option<bool> {
    let [ObjectNamePart::Identifier(name)] = collation.0.as_slice() else {
        return None;
    };
    if name.value.eq_ignore_ascii_case("utf8mb4_bin") || name.value.eq_ignore_ascii_case("binary") {
        return Some(true);
    }
    name.value
        .eq_ignore_ascii_case("utf8mb4_0900_ai_ci")
        .then_some(false)
}

fn collation_is_utf8mb4_bin(collation: &ObjectName) -> bool {
    let [ObjectNamePart::Identifier(name)] = collation.0.as_slice() else {
        return false;
    };
    name.value.eq_ignore_ascii_case("utf8mb4_bin")
}

/// Reads the ordinal out of an `ORDER BY 2`, if that is what this is.
fn order_by_ordinal(expr: &Expr) -> Option<usize> {
    let Expr::Value(value) = expr else {
        return None;
    };
    let Value::Number(number, false) = &value.value else {
        return None;
    };
    number.parse::<usize>().ok()
}

/// Finds the expression an `ORDER BY` ordinal names.
///
/// MySQL answers 1054 for an ordinal outside the projection, and this refuses
/// instead. A wildcard is refused because its columns are not written down
/// here, so there is nothing to count through.
fn projected_expr(projection: &[SelectItem], ordinal: usize) -> Result<&Expr, ParseError> {
    if ordinal == 0 || ordinal > projection.len() {
        return unsupported("SELECT ORDER BY ordinal outside the projection");
    }
    match &projection[ordinal - 1] {
        SelectItem::UnnamedExpr(expr) | SelectItem::ExprWithAlias { expr, .. } => Ok(expr),
        _ => unsupported("SELECT ORDER BY an ordinal over a wildcard projection"),
    }
}

fn render_select_limit(
    clause: &sqlparser::ast::LimitClause,
    render_context: &mut SelectRenderContext<'_>,
    row_count_parameters: &mut Vec<usize>,
    found_rows_counted: Option<&str>,
) -> Result<String, ParseError> {
    use sqlparser::ast::{LimitClause, OffsetRows};

    // MySQL writes the two counts in either order — `LIMIT n OFFSET m` and
    // `LIMIT m, n` — and a parameter takes its ordinal from where it stands,
    // so which of them is read first depends on which was written first.
    let (limit, offset, offset_written_first) = match clause {
        LimitClause::LimitOffset {
            limit: Some(limit),
            offset,
            limit_by,
        } if limit_by.is_empty() => {
            if offset
                .as_ref()
                .is_some_and(|offset| offset.rows != OffsetRows::None)
            {
                return unsupported("SELECT OFFSET option");
            }
            (limit, offset.as_ref().map(|offset| &offset.value), false)
        }
        LimitClause::OffsetCommaLimit { offset, limit } => (limit, Some(offset), true),
        _ => return unsupported("SELECT LIMIT option"),
    };
    // The engine reads both spellings and means the same by each, so each is
    // rendered as it was written. That keeps a parameter in the place the
    // client bound it: the engine binds by where a `?` stands in the SQL it is
    // given, and the client binds by where it stood in the SQL it wrote.
    if offset_written_first {
        let offset = offset.expect("the comma spelling carries an offset");
        let offset = render_written_row_count(
            offset,
            RowCountKind::Skipped,
            render_context,
            row_count_parameters,
        )?;
        let limit = render_written_row_count(
            limit,
            RowCountKind::Kept,
            render_context,
            row_count_parameters,
        )?;
        let limit = match found_rows_counted {
            Some(counted) => note_found_rows(counted, &limit),
            None => limit,
        };
        return Ok(format!(" LIMIT {offset}, {limit}"));
    }
    let limit = render_written_row_count(
        limit,
        RowCountKind::Kept,
        render_context,
        row_count_parameters,
    )?;
    let limit = match found_rows_counted {
        Some(counted) => note_found_rows(counted, &limit),
        None => limit,
    };
    let mut rendered = format!(" LIMIT {limit}");
    if let Some(offset) = offset {
        rendered.push_str(&format!(
            " OFFSET {}",
            render_written_row_count(
                offset,
                RowCountKind::Skipped,
                render_context,
                row_count_parameters,
            )?
        ));
    }
    Ok(rendered)
}

/// Writes a `LIMIT`'s count so that working it out notes the rows the
/// statement answers without it, which `FOUND_ROWS()` reads next. The engine
/// works a `LIMIT` out once, before the first row, inside the statement's own
/// read of the database.
fn note_found_rows(counted: &str, limit: &str) -> String {
    format!("mysql_note_found_rows({counted}, {limit})")
}

/// Which of a `LIMIT`'s two counts a written number is.
///
/// The two are told apart because a count wider than the engine reads means
/// opposite things: a limit that wide keeps every row and an offset that wide
/// skips every row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RowCountKind {
    Kept,
    Skipped,
}

/// Renders the row count a `LIMIT` or an `OFFSET` was written with.
///
/// A parameter stands here as readily as a number, which is what a client that
/// prepares a paged query writes. What it binds is held to a whole number that
/// is not negative, because the engine reads a negative row count as no limit
/// at all where MySQL refuses one.
fn render_written_row_count(
    expr: &Expr,
    kind: RowCountKind,
    render_context: &mut SelectRenderContext<'_>,
    row_count_parameters: &mut Vec<usize>,
) -> Result<String, ParseError> {
    if matches!(expr, Expr::Value(value) if matches!(&value.value, Value::Placeholder(marker) if marker == "?"))
    {
        let ordinal = render_context.next_parameter_ordinal()?;
        row_count_parameters.push(ordinal);
        return Ok("?".to_owned());
    }
    let written = render_select_row_count(expr)?;
    let Ok(within_reach) = i64::try_from(written) else {
        // `LIMIT 18446744073709551615` is how MySQL is asked for every row
        // after an offset, and its counts run to a whole unsigned 64-bit
        // number where the engine's run to a signed one. No table holds that
        // many rows, so a limit that wide keeps every row — which the engine
        // spells as a negative count — and an offset that wide skips every
        // row, which the widest count it reads already does. Measured on
        // 8.4.11: that limit answers every row after the offset, that offset
        // answers none, and one past it is 1064.
        return Ok(match kind {
            RowCountKind::Kept => "-1".to_owned(),
            RowCountKind::Skipped => i64::MAX.to_string(),
        });
    };
    Ok(within_reach.to_string())
}

fn render_select_row_count(expr: &Expr) -> Result<u64, ParseError> {
    if let Expr::Value(value) = expr {
        if let Value::Number(number, false) = &value.value {
            if !number.is_empty() && number.bytes().all(|byte| byte.is_ascii_digit()) {
                if let Ok(number) = number.parse::<u64>() {
                    return Ok(number);
                }
            }
        }
    }
    unsupported("SELECT LIMIT/OFFSET requires an integer literal in 0..=18446744073709551615")
}

/// What one checked `INSERT` renders to.
pub(crate) struct RenderedInsert {
    pub(crate) sqlite_sql: String,
    /// Every table the statement reads, which an `INSERT ... SELECT` has and
    /// the `VALUES` forms do not.
    pub(crate) read_tables: Vec<MySqlSelectSource>,
    /// The table the `SELECT`'s own `WHERE` compares against, when there is one.
    pub(crate) compared_table: Option<String>,
    /// The comparisons that `WHERE` recorded, to be held to the column's type
    /// exactly as a `SELECT`'s are.
    pub(crate) checked_comparisons: Vec<CheckedSelectComparison>,
    /// Which parameters stand where the `SELECT` writes a row count.
    pub(crate) row_count_parameters: Vec<usize>,
    /// The columns a `VALUES` row writes a `CAST(... AS JSON)` into.
    pub(crate) json_cast_columns: Vec<String>,
}

/// Renders one checked `INSERT`. An `INSERT ... SELECT` whose `SELECT` has to
/// know its columns' types to be rendered takes it from `typed_select`, the
/// same `SELECT` rendered by the frontend knowing them.
pub(crate) fn translate_insert(
    insert: &Insert,
    sql: &str,
    mode: SessionSqlMode,
    decimal_columns: &[(String, u32)],
    typed_select: Option<&crate::TranslatedSelect>,
) -> Result<RenderedInsert, ParseError> {
    if !insert.optimizer_hints.is_empty()
        || insert.or.is_some()
        // MySQL's REPLACE already decides what a collision does, so IGNORE on
        // top of it is not a shape it accepts either.
        || (insert.ignore && insert.replace_into)
        || !insert.into
        || insert.table_alias.is_some()
        || insert.overwrite
        || insert.partitioned.is_some()
        || !insert.after_columns.is_empty()
        || insert.has_table_keyword
        // MySQL's own ON DUPLICATE KEY UPDATE is read below; the ON CONFLICT
        // spelling is the engine's, not something a MySQL client writes.
        || matches!(insert.on, Some(sqlparser::ast::OnInsert::OnConflict(_)))
        // REPLACE and IGNORE already decide what a collision does, so an
        // upsert on top of either is not a shape MySQL accepts.
        || (insert.on.is_some() && (insert.replace_into || insert.ignore))
        || insert.returning.is_some()
        || insert.output.is_some()
        || insert.priority.is_some()
        // An alias on the offered row is what names it in an
        // `ON DUPLICATE KEY UPDATE`, and names nothing anywhere else: Rails
        // writes one on every `insert_all!`, and measured on 8.4.11 a
        // `VALUES` insert carrying one writes, numbers and refuses a
        // duplicate exactly as the same insert without it. A list of column
        // aliases beside it renames what the row carries, which is a shape
        // this has not measured.
        || insert.insert_alias.as_ref().is_some_and(|alias| {
            alias.col_aliases.as_ref().is_some_and(|columns| !columns.is_empty())
                || (!matches!(insert.on, Some(sqlparser::ast::OnInsert::DuplicateKeyUpdate(_)))
                    && !insert.source.as_deref().is_some_and(|source| {
                        matches!(source.body.as_ref(), SetExpr::Values(_))
                    }))
        })
        || insert.settings.is_some()
        || insert.format_clause.is_some()
        || insert.multi_table_insert_type.is_some()
        || !insert.multi_table_into_clauses.is_empty()
        || !insert.multi_table_when_clauses.is_empty()
        || insert.multi_table_else_clause.is_some()
    {
        return unsupported("INSERT option");
    }
    let sqlparser::ast::TableObject::TableName(table) = &insert.table else {
        return unsupported("INSERT table source");
    };
    let table = render_unqualified_name(table)?;
    // MySQL's `INSERT ... SET a = 1, b = 2` names its columns and values in one
    // place instead of two, and means exactly what the column-list form means.
    // Measured on MySQL 8.4.11: `INSERT INTO s SET id = 1, a = 2, b = 'x'`
    // stores the same row `INSERT INTO s (id, a, b) VALUES (1, 2, 'x')` does,
    // and a column the SET leaves out takes its default. Rendering it as the
    // other form is what keeps one set of rules for both.
    if !insert.assignments.is_empty() {
        return render_insert_assignments(&table, insert);
    }
    let columns = insert
        .columns
        .iter()
        .map(render_unqualified_name)
        .collect::<Result<Vec<_>, _>>()?;
    let column_names = insert
        .columns
        .iter()
        .map(|column| match column.0.as_slice() {
            [ObjectNamePart::Identifier(ident)] => Ok(ident.value.as_str()),
            _ => unsupported("INSERT column name"),
        })
        .collect::<Result<Vec<_>, _>>()?;
    let verb = insert_verb(insert);
    let source = insert.source.as_deref().ok_or(ParseError::Unsupported {
        feature: "INSERT without VALUES",
    })?;
    // `INSERT ... SELECT` reads rows rather than listing them. The SELECT goes
    // through the same translator a bare one does, so it is held to the same
    // rules and names the same tables — which the caller has to see, or the
    // table it reads goes unauthorized.
    if !matches!(source.body.as_ref(), SetExpr::Values(_)) {
        if columns.is_empty() {
            return unsupported("INSERT SELECT without an explicit column list");
        }
        // The rendered copy has no room for the upsert clause, and dropping it
        // would turn MySQL's update of a colliding row into a key error.
        if insert.on.is_some() {
            return unsupported("INSERT SELECT with ON DUPLICATE KEY UPDATE");
        }
        let rendered = match typed_select {
            // The SELECT was rendered as a bare one, which warns about a cut
            // `GROUP_CONCAT`; a statement writing the cut value has to fail
            // instead, so the pair is refused rather than written short.
            Some(select) if select.concatenates_groups => {
                return unsupported("INSERT SELECT with a GROUP_CONCAT needing column types");
            }
            // Which columns a grouping's keys decide is the frontend's to
            // check, which it does for a statement it answers rows for.
            Some(select) if select.columns_the_keys_decide.is_some() => {
                return unsupported("INSERT SELECT projecting a column its grouping keys decide");
            }
            Some(select) => RenderedCopy {
                sqlite_sql: select.sqlite_sql.clone(),
                source_tables: select.source_tables.clone(),
                source_table: select.source_table.clone(),
                checked_comparisons: select.checked_comparisons.clone(),
                compares_through_a_subquery: !select.checked_subquery_comparisons.is_empty(),
                row_count_parameters: select.row_count_parameters.clone(),
            },
            None => {
                let rendered = translate_select_query(
                    source,
                    sql,
                    mode,
                    &[],
                    &[],
                    &[],
                    &[],
                    &[],
                    &[],
                    &[],
                    &[],
                    &[],
                    true,
                    false,
                )?;
                // A SELECT that needs a second rendering pass to learn its
                // column types is rendered by the frontend, which knows them,
                // and handed back here.
                if rendered.orders_a_bare_column || rendered.compares_a_placeholder {
                    return unsupported(crate::INSERT_SELECT_NEEDING_COLUMN_TYPES);
                }
                if rendered.columns_the_keys_decide.is_some() {
                    return unsupported(
                        "INSERT SELECT projecting a column its grouping keys decide",
                    );
                }
                RenderedCopy {
                    sqlite_sql: rendered.sqlite_sql,
                    source_tables: rendered.source_tables,
                    source_table: rendered.source_table,
                    checked_comparisons: rendered.checked_comparisons,
                    compares_through_a_subquery: !rendered.checked_subquery_comparisons.is_empty(),
                    row_count_parameters: rendered.row_count_parameters,
                }
            }
        };
        if rendered.compares_through_a_subquery {
            return unsupported("INSERT SELECT with a subquery comparison");
        }
        return Ok(RenderedInsert {
            sqlite_sql: format!(
                "{verb} {table} ({}) {}",
                columns.join(", "),
                rendered.sqlite_sql
            ),
            read_tables: rendered.source_tables,
            compared_table: rendered.source_table.map(|table| table.as_str().to_owned()),
            checked_comparisons: rendered.checked_comparisons,
            row_count_parameters: rendered.row_count_parameters,
            json_cast_columns: Vec::new(),
        });
    }
    if source.with.is_some()
        || source.order_by.is_some()
        || source.limit_clause.is_some()
        || source.fetch.is_some()
        || !source.locks.is_empty()
        || source.for_clause.is_some()
        || source.settings.is_some()
        || source.format_clause.is_some()
        || !source.pipe_operators.is_empty()
    {
        return unsupported("INSERT source query option");
    }
    let SetExpr::Values(values) = source.body.as_ref() else {
        return unsupported("INSERT source");
    };
    if values.explicit_row || values.value_keyword || values.rows.is_empty() {
        return unsupported("INSERT VALUES option");
    }
    if columns.is_empty() {
        if values.rows.len() == 1 && values.rows[0].is_empty() {
            // The empty-row form writes DEFAULT VALUES, which has no room for
            // an upsert clause after it. Refusing keeps the clause from being
            // dropped on the floor.
            if insert.on.is_some() {
                return unsupported("INSERT DEFAULT VALUES with ON DUPLICATE KEY UPDATE");
            }
            return Ok(RenderedInsert {
                sqlite_sql: format!("{verb} {table} DEFAULT VALUES"),
                read_tables: Vec::new(),
                compared_table: None,
                checked_comparisons: Vec::new(),
                row_count_parameters: Vec::new(),
                json_cast_columns: Vec::new(),
            });
        }
        return unsupported("INSERT without an explicit column list");
    }
    reject_ignored_null(
        insert,
        &values
            .rows
            .iter()
            .flat_map(|row| row.iter())
            .collect::<Vec<_>>(),
    )?;
    for row in &values.rows {
        if row.is_empty() || row.len() != columns.len() {
            return unsupported("INSERT VALUES column count");
        }
    }
    // Beside an upsert, the offered row carries the column's own default for a
    // column given `DEFAULT`, which is what the engine's `excluded` row
    // carries for a column left out: measured on 8.4.11, `VALUES (..., DEFAULT)
    // ... hits = VALUES(hits) + 1` over a column defaulting to 7 writes 8, and
    // so does naming the offered row.
    //
    // Several rows of an upsert on a counted table are written one statement
    // to a row, each leaving out the columns that row gives `DEFAULT`, so a
    // column may take it in some rows only. Written here as one statement,
    // such a row reads NULL there; the frontend refuses the statement on any
    // other table.
    let defaulted = if insert.on.is_some() && values.rows.len() > 1 {
        columns_given_their_default_in_every_row(&column_names, values)
    } else {
        columns_given_their_default(&column_names, values)?
    };
    let kept = |at: usize| !defaulted[at];
    let mut json_cast_columns = Vec::new();
    let rows = values
        .rows
        .iter()
        .map(|row| {
            row.iter()
                .enumerate()
                .filter(|(at, _)| kept(*at))
                .map(|(at, value)| match json_cast_operand(value) {
                    Some(operand) => {
                        json_cast_columns.push(column_names[at].to_owned());
                        render_json_cast(operand)
                    }
                    None if names_the_columns_default(value, column_names[at]) => {
                        Ok("NULL".to_owned())
                    }
                    None => render_inserted_value(value),
                })
                .collect::<Result<Vec<_>, _>>()
                .map(|values| format!("({})", values.join(", ")))
        })
        .collect::<Result<Vec<_>, _>>()?;
    let columns = columns
        .into_iter()
        .enumerate()
        .filter(|(at, _)| kept(*at))
        .map(|(_, column)| column)
        .collect::<Vec<_>>();
    // Every column was given `DEFAULT`, which is the row MySQL's own empty
    // column list writes.
    if columns.is_empty() {
        // The engine's `DEFAULT VALUES` has no room for an upsert clause after
        // it, and dropping the clause would turn an update into a key error.
        if insert.on.is_some() {
            return unsupported("INSERT of defaults alone with ON DUPLICATE KEY UPDATE");
        }
        return Ok(RenderedInsert {
            sqlite_sql: format!("{verb} {table} DEFAULT VALUES"),
            read_tables: Vec::new(),
            compared_table: None,
            checked_comparisons: Vec::new(),
            row_count_parameters: Vec::new(),
            json_cast_columns: Vec::new(),
        });
    }
    Ok(RenderedInsert {
        sqlite_sql: format!(
            "{verb} {table} ({}) VALUES {}{}",
            columns.join(", "),
            rows.join(", "),
            render_duplicate_key_update(insert, decimal_columns)?
        ),
        read_tables: Vec::new(),
        compared_table: None,
        checked_comparisons: Vec::new(),
        row_count_parameters: Vec::new(),
        json_cast_columns,
    })
}

/// What the `SELECT` of an `INSERT ... SELECT` renders to.
struct RenderedCopy {
    sqlite_sql: String,
    source_tables: Vec<MySqlSelectSource>,
    source_table: Option<MySqlTableName>,
    checked_comparisons: Vec<CheckedSelectComparison>,
    compares_through_a_subquery: bool,
    row_count_parameters: Vec<usize>,
}

/// Which columns are given `DEFAULT` in every row of an `INSERT`.
///
/// Such a column is left out of the statement instead, which asks the engine
/// for the same thing. Measured on MySQL 8.4.11, a column left out and a
/// column given `DEFAULT` both take the column's own default, both leave a
/// nullable column with none at NULL, and both answer 1364 when the column is
/// NOT NULL with no default of its own.
///
/// Every row has to agree, because leaving the column out would take the
/// default for all of them and a row that wrote a value would lose it.
///
/// The caller has to have checked that every row is as wide as `names`.
pub(crate) fn columns_given_their_default(
    names: &[&str],
    values: &sqlparser::ast::Values,
) -> Result<Vec<bool>, ParseError> {
    names
        .iter()
        .enumerate()
        .map(|(at, name)| {
            let mut written = values
                .rows
                .iter()
                .map(|row| names_the_columns_default(&row[at], name));
            let first = written.next().unwrap_or(false);
            if written.any(|other| other != first) {
                return unsupported("INSERT DEFAULT in some rows only");
            }
            Ok(first)
        })
        .collect()
}

/// Which columns are given `DEFAULT` in every row of an `INSERT`, where the
/// others may be given it in some rows only.
///
/// The caller has to have checked that every row is as wide as `names`.
fn columns_given_their_default_in_every_row(
    names: &[&str],
    values: &sqlparser::ast::Values,
) -> Vec<bool> {
    names
        .iter()
        .enumerate()
        .map(|(at, name)| {
            values
                .rows
                .iter()
                .all(|row| names_the_columns_default(&row[at], name))
        })
        .collect()
}

/// Whether a value written into `column` asks for that column's own default.
///
/// MySQL spells it as the bare word `DEFAULT` and also as `DEFAULT(col)`, and
/// measured on 8.4.11 the two write the same value. Neither is a name, so a
/// quoted `` `default` `` is an ordinary column and is left alone — measured,
/// MySQL takes it as one.
pub(crate) fn names_the_columns_default(value: &Expr, column: &str) -> bool {
    match value {
        Expr::Identifier(ident) => {
            ident.quote_style.is_none() && ident.value.eq_ignore_ascii_case("DEFAULT")
        }
        Expr::Function(function) => {
            let [ObjectNamePart::Identifier(name)] = function.name.0.as_slice() else {
                return false;
            };
            if name.quote_style.is_some() || !name.value.eq_ignore_ascii_case("DEFAULT") {
                return false;
            }
            let FunctionArguments::List(arguments) = &function.args else {
                return false;
            };
            let [sqlparser::ast::FunctionArg::Unnamed(sqlparser::ast::FunctionArgExpr::Expr(
                Expr::Identifier(named),
            ))] = arguments.args.as_slice()
            else {
                return false;
            };
            arguments.clauses.is_empty()
                && function.over.is_none()
                && named.value.eq_ignore_ascii_case(column)
        }
        _ => false,
    }
}

/// Renders MySQL's `ON DUPLICATE KEY UPDATE` as the engine's `ON CONFLICT DO
/// UPDATE`.
///
/// The two mean the same thing. MySQL's fires on a collision with any unique
/// key, and the engine's, written without a conflict target, does too.
/// `VALUES(col)` names the value the row would have been given, which the
/// engine spells `excluded.col`.
///
/// The affected count is the one thing that differs, and it is measured on
/// MySQL 8.4.11: a new row counts 1, a row the update changes counts 2 because
/// MySQL counts the attempted insert and the update, and a row the update
/// leaves identical counts 0. The engine counts the changed row once, so the
/// middle case reports 1 here.
fn render_duplicate_key_update(
    insert: &Insert,
    decimal_columns: &[(String, u32)],
) -> Result<String, ParseError> {
    let Some(on) = &insert.on else {
        return Ok(String::new());
    };
    // The table's own name and the name the offered row was given, which are
    // the two things a qualified column can name here.
    let table = match &insert.table {
        sqlparser::ast::TableObject::TableName(name) => match name.0.as_slice() {
            [ObjectNamePart::Identifier(ident)] => ident.value.clone(),
            _ => return unsupported("INSERT ON DUPLICATE KEY UPDATE over a qualified table"),
        },
        _ => return unsupported("INSERT ON DUPLICATE KEY UPDATE over a table this does not read"),
    };
    let offered = match insert.insert_alias.as_ref() {
        Some(alias) => match alias.row_alias.0.as_slice() {
            [ObjectNamePart::Identifier(name)] => Some(name.value.clone()),
            _ => return unsupported("INSERT ON DUPLICATE KEY UPDATE row alias"),
        },
        None => None,
    };
    let offered = offered.as_deref();
    let sqlparser::ast::OnInsert::DuplicateKeyUpdate(assignments) = on else {
        return unsupported("INSERT ON CONFLICT clause");
    };
    if assignments.is_empty() {
        return unsupported("INSERT ON DUPLICATE KEY UPDATE without assignments");
    }
    let mut rendered = Vec::with_capacity(assignments.len());
    let mut assigned = Vec::with_capacity(assignments.len());
    for assignment in assignments {
        let sqlparser::ast::AssignmentTarget::ColumnName(name) = &assignment.target else {
            return unsupported("INSERT ON DUPLICATE KEY UPDATE assignment target");
        };
        let [ObjectNamePart::Identifier(column)] = name.0.as_slice() else {
            return unsupported("INSERT ON DUPLICATE KEY UPDATE assignment target");
        };
        // Measured on MySQL 8.4.11: the clause takes its assignments left to
        // right, as an `UPDATE`'s `SET` does — `a = a + 10, b = a` over a row
        // holding 1 leaves `b` at 11 — where the engine reads the row as it
        // stood. The row offered is not written, so reading it is safe.
        if reads_a_column_the_clause_wrote(&assignment.value, offered, &assigned) {
            return unsupported(
                "INSERT ON DUPLICATE KEY UPDATE value reading a column the clause already wrote",
            );
        }
        if assigned
            .iter()
            .any(|earlier: &String| earlier.eq_ignore_ascii_case(&column.value))
        {
            return unsupported("INSERT ON DUPLICATE KEY UPDATE writing one column twice");
        }
        assigned.push(column.value.clone());
        let assigned_decimal = decimal_columns
            .iter()
            .any(|(known, _)| known.eq_ignore_ascii_case(&column.value));
        let value =
            match render_touch_unless_unchanged(&assignment.value, &column.value, &table, offered)?
            {
                Some(touch) => touch,
                None => render_duplicate_key_value(
                    &assignment.value,
                    &table,
                    offered,
                    decimal_columns,
                    assigned_decimal,
                )?,
            };
        rendered.push(format!("{} = {value}", render_unqualified_name(name)?));
    }
    Ok(format!(
        " ON CONFLICT DO UPDATE SET {}",
        rendered.join(", ")
    ))
}

/// Reports whether an `ON DUPLICATE KEY UPDATE` value reads a column of the
/// row already there that an earlier assignment of the same clause wrote.
///
/// `VALUES(col)` and a column qualified by the offered row's name read the row
/// offered, which the clause never writes.
fn reads_a_column_the_clause_wrote(
    value: &Expr,
    offered: Option<&str>,
    assigned: &[String],
) -> bool {
    match value {
        Expr::Function(function) if names_the_offered_row(function) => false,
        Expr::CompoundIdentifier(parts)
            if parts.len() == 2
                && offered.is_some_and(|offered| parts[0].value.eq_ignore_ascii_case(offered)) =>
        {
            false
        }
        Expr::Nested(inner) | Expr::UnaryOp { expr: inner, .. } => {
            reads_a_column_the_clause_wrote(inner, offered, assigned)
        }
        Expr::BinaryOp { left, right, .. } => {
            reads_a_column_the_clause_wrote(left, offered, assigned)
                || reads_a_column_the_clause_wrote(right, offered, assigned)
        }
        Expr::Case {
            operand,
            conditions,
            else_result,
            ..
        } => operand
            .iter()
            .chain(else_result)
            .map(Box::as_ref)
            .chain(
                conditions
                    .iter()
                    .flat_map(|arm| [&arm.condition, &arm.result]),
            )
            .any(|part| reads_a_column_the_clause_wrote(part, offered, assigned)),
        other => !reads_only_unassigned_columns(other, assigned),
    }
}

/// Renders the assignment Rails 8's `upsert_all` writes to touch a row's
/// timestamp only when the upsert changes it —
/// `updated_at = (CASE WHEN (t.name <=> offered.name AND t.balance <=>
/// offered.balance) THEN t.updated_at ELSE CURRENT_TIMESTAMP(6) END)` — or
/// `None` for any other value.
///
/// Only that shape is taken: one condition joining null-safe comparisons of a
/// column of the row already there with the same column of the row offered,
/// the column written read back off the row already there when nothing
/// differs, and a reading of the clock when something does. Two values of one
/// column compare alike in both: MySQL's `<=>` is the engine's `IS`, and the
/// engine compares a column of words under the column's own collation, as
/// MySQL does — measured on 8.4.11, offering `'BOB'` over `'Bob'` writes the
/// name and leaves the timestamp.
fn render_touch_unless_unchanged(
    value: &Expr,
    column: &str,
    table: &str,
    offered: Option<&str>,
) -> Result<Option<String>, ParseError> {
    let mut value = value;
    while let Expr::Nested(inner) = value {
        value = inner;
    }
    let Expr::Case {
        operand: None,
        conditions,
        else_result: Some(changed),
        ..
    } = value
    else {
        return Ok(None);
    };
    let [arm] = conditions.as_slice() else {
        return unsupported("ON DUPLICATE KEY UPDATE CASE with other than one condition");
    };
    if !reads_the_column_already_there(&arm.result, column, table, offered) {
        return unsupported("ON DUPLICATE KEY UPDATE CASE keeping anything but its own column");
    }
    if !is_clock_reading_value(changed) {
        return unsupported("ON DUPLICATE KEY UPDATE CASE writing anything but the clock");
    }
    Ok(Some(format!(
        "CASE WHEN {} THEN {} ELSE {} END",
        render_offered_row_unchanged(&arm.condition, table, offered)?,
        render_ident(&Ident::new(column)),
        render_inserted_value(changed)?
    )))
}

/// Renders a condition joining `a <=> b` comparisons of a column of the row
/// already there with the same column of the row offered.
fn render_offered_row_unchanged(
    condition: &Expr,
    table: &str,
    offered: Option<&str>,
) -> Result<String, ParseError> {
    match condition {
        Expr::Nested(inner) => render_offered_row_unchanged(inner, table, offered),
        Expr::BinaryOp {
            left,
            op: BinaryOperator::And,
            right,
        } => Ok(format!(
            "({} AND {})",
            render_offered_row_unchanged(left, table, offered)?,
            render_offered_row_unchanged(right, table, offered)?
        )),
        Expr::BinaryOp {
            left,
            op: BinaryOperator::Spaceship,
            right,
        } => {
            let pair = [left.as_ref(), right.as_ref()];
            let Some(column) = pair
                .iter()
                .find_map(|side| column_read_off_the_offered_row(side, offered))
            else {
                return unsupported(
                    "ON DUPLICATE KEY UPDATE <=> naming no column of the offered row",
                );
            };
            if !pair
                .iter()
                .any(|side| reads_the_column_already_there(side, &column, table, offered))
            {
                return unsupported(
                    "ON DUPLICATE KEY UPDATE <=> between different columns of the two rows",
                );
            }
            let column = render_ident(&Ident::new(column));
            Ok(format!("({column} IS \"excluded\".{column})"))
        }
        _ => unsupported("ON DUPLICATE KEY UPDATE CASE condition"),
    }
}

/// The table an `INSERT`'s upsert clause writes, the columns it assigns, and
/// the columns it compares with `<=>` between the row already there and the
/// row offered, with what each row offers for them.
pub(crate) fn checked_upsert(insert: &Insert) -> Result<Option<crate::CheckedUpsert>, ParseError> {
    let Some(sqlparser::ast::OnInsert::DuplicateKeyUpdate(assignments)) = &insert.on else {
        return Ok(None);
    };
    let sqlparser::ast::TableObject::TableName(name) = &insert.table else {
        return unsupported("INSERT ON DUPLICATE KEY UPDATE over a table this does not read");
    };
    let [ObjectNamePart::Identifier(table)] = name.0.as_slice() else {
        return unsupported("INSERT ON DUPLICATE KEY UPDATE over a qualified table");
    };
    let assigned = assignments
        .iter()
        .map(|assignment| match &assignment.target {
            sqlparser::ast::AssignmentTarget::ColumnName(name) => match name.0.as_slice() {
                [ObjectNamePart::Identifier(column)] => Ok(column.value.clone()),
                _ => unsupported("INSERT ON DUPLICATE KEY UPDATE assignment target"),
            },
            _ => unsupported("INSERT ON DUPLICATE KEY UPDATE assignment target"),
        })
        .collect::<Result<Vec<_>, _>>()?;
    let offered =
        insert
            .insert_alias
            .as_ref()
            .and_then(|alias| match alias.row_alias.0.as_slice() {
                [ObjectNamePart::Identifier(name)] => Some(name.value.as_str()),
                _ => None,
            });
    let mut compared = Vec::new();
    for assignment in assignments {
        collect_compared_columns(&assignment.value, offered, &mut compared);
    }
    let rows = match insert.source.as_deref().map(|source| source.body.as_ref()) {
        Some(SetExpr::Values(values)) => values.rows.as_slice(),
        _ => &[],
    };
    let comparisons = compared
        .into_iter()
        .map(|column| {
            let at = insert.columns.iter().position(|named| {
                matches!(named.0.as_slice(), [ObjectNamePart::Identifier(named)]
                    if named.value.eq_ignore_ascii_case(&column))
            });
            let offered = rows
                .iter()
                .map(|row| match at.and_then(|at| row.get(at)) {
                    Some(value) => offered_value(value),
                    None => crate::OfferedValue::Other,
                })
                .collect::<Vec<_>>();
            crate::OfferedRowComparison {
                column,
                offered: if rows.is_empty() {
                    vec![crate::OfferedValue::Other]
                } else {
                    offered
                },
            }
        })
        .collect();
    let defaults_in_some_rows = insert.columns.iter().enumerate().any(|(at, column)| {
        let [ObjectNamePart::Identifier(column)] = column.0.as_slice() else {
            return false;
        };
        let given_default = |row: &[Expr]| {
            row.get(at)
                .is_some_and(|value| names_the_columns_default(value, &column.value))
        };
        rows.iter().any(|row| given_default(row)) && !rows.iter().all(|row| given_default(row))
    });
    Ok(Some(crate::CheckedUpsert {
        table: table.value.clone(),
        assigned,
        comparisons,
        defaults_in_some_rows,
    }))
}

fn collect_compared_columns(expr: &Expr, offered: Option<&str>, compared: &mut Vec<String>) {
    match expr {
        Expr::Nested(inner) => collect_compared_columns(inner, offered, compared),
        Expr::BinaryOp {
            left,
            op: BinaryOperator::Spaceship,
            right,
        } => compared.extend(
            [left, right]
                .into_iter()
                .find_map(|side| column_read_off_the_offered_row(side, offered)),
        ),
        Expr::BinaryOp { left, right, .. } => {
            collect_compared_columns(left, offered, compared);
            collect_compared_columns(right, offered, compared);
        }
        Expr::Case {
            operand,
            conditions,
            else_result,
            ..
        } => {
            for part in operand.iter().chain(else_result).map(Box::as_ref).chain(
                conditions
                    .iter()
                    .flat_map(|arm| [&arm.condition, &arm.result]),
            ) {
                collect_compared_columns(part, offered, compared);
            }
        }
        _ => {}
    }
}

fn offered_value(value: &Expr) -> crate::OfferedValue {
    let written = match value {
        Expr::UnaryOp {
            op: UnaryOperator::Minus | UnaryOperator::Plus,
            expr,
        } if matches!(expr.as_ref(), Expr::Value(value) if matches!(value.value, Value::Number(..))) => {
            expr.as_ref()
        }
        other => other,
    };
    let Expr::Value(written) = written else {
        return crate::OfferedValue::Other;
    };
    match &written.value {
        Value::Null => crate::OfferedValue::Null,
        Value::SingleQuotedString(_) | Value::DoubleQuotedString(_) => crate::OfferedValue::Word,
        Value::Boolean(_) => crate::OfferedValue::WholeNumber,
        Value::Number(digits, false) if digits.bytes().all(|byte| byte.is_ascii_digit()) => {
            crate::OfferedValue::WholeNumber
        }
        Value::Number(digits, false)
            if digits.split_once('.').is_some_and(|(whole, fraction)| {
                !(whole.is_empty() && fraction.is_empty())
                    && whole
                        .bytes()
                        .chain(fraction.bytes())
                        .all(|byte| byte.is_ascii_digit())
            }) =>
        {
            crate::OfferedValue::NumberWithAPoint
        }
        _ => crate::OfferedValue::Other,
    }
}

/// The column an expression reads off the row offered — `VALUES(col)` or a
/// column qualified by the name that row carries.
fn column_read_off_the_offered_row(expr: &Expr, offered: Option<&str>) -> Option<String> {
    match expr {
        Expr::Function(function) if names_the_offered_row(function) => {
            let FunctionArguments::List(arguments) = &function.args else {
                return None;
            };
            match arguments.args.as_slice() {
                [sqlparser::ast::FunctionArg::Unnamed(sqlparser::ast::FunctionArgExpr::Expr(
                    Expr::Identifier(column),
                ))] => Some(column.value.clone()),
                _ => None,
            }
        }
        Expr::CompoundIdentifier(parts)
            if parts.len() == 2
                && offered.is_some_and(|offered| parts[0].value.eq_ignore_ascii_case(offered)) =>
        {
            Some(parts[1].value.clone())
        }
        _ => None,
    }
}

/// Whether an expression reads `column` off the row already there: the column
/// qualified by the table, or bare while the offered row carries no name —
/// once it does, MySQL answers a bare column 1052.
fn reads_the_column_already_there(
    expr: &Expr,
    column: &str,
    table: &str,
    offered: Option<&str>,
) -> bool {
    match expr {
        Expr::Identifier(ident) => offered.is_none() && ident.value.eq_ignore_ascii_case(column),
        Expr::CompoundIdentifier(parts) => {
            matches!(parts.as_slice(), [qualifier, ident]
                if qualifier.value.eq_ignore_ascii_case(table)
                    && ident.value.eq_ignore_ascii_case(column))
        }
        _ => false,
    }
}

/// Renders one `ON DUPLICATE KEY UPDATE` value.
///
/// `VALUES(col)` is MySQL's way of naming the value the row would have carried,
/// and since 8.0.19 an alias on the offered row names the same thing —
/// `... VALUES (...) AS offered ON DUPLICATE KEY UPDATE hits = offered.hits`.
/// The engine calls it `excluded.col` either way. A bare column is the row
/// already there, in both, and arithmetic joins the two: `hits = hits + 1` and
/// `hits = hits + VALUES(hits)` are how a counter is stepped.
///
/// Measured on MySQL 8.4.11: once the offered row carries an alias, a bare
/// column on the right is 1052, ambiguous between the two rows, so a qualified
/// one is the only way to name either.
fn render_duplicate_key_value(
    value: &Expr,
    table: &str,
    offered: Option<&str>,
    decimal_columns: &[(String, u32)],
    assigned_decimal: bool,
) -> Result<String, ParseError> {
    match value {
        Expr::Function(function) if names_the_offered_row(function) => {
            let sqlparser::ast::FunctionArguments::List(arguments) = &function.args else {
                return unsupported("INSERT ON DUPLICATE KEY UPDATE value");
            };
            let [sqlparser::ast::FunctionArg::Unnamed(sqlparser::ast::FunctionArgExpr::Expr(
                Expr::Identifier(column),
            ))] = arguments.args.as_slice()
            else {
                return unsupported("VALUES() requires one unqualified column");
            };
            if !assigned_decimal
                && decimal_columns
                    .iter()
                    .any(|(name, _)| name.eq_ignore_ascii_case(&column.value))
            {
                return unsupported(
                    "ON DUPLICATE KEY UPDATE DECIMAL value assigned to a non-DECIMAL column",
                );
            }
            Ok(format!("\"excluded\".{}", render_ident(column)))
        }
        // A column qualified by the alias names the offered row, and one
        // qualified by the table names the row already there.
        Expr::CompoundIdentifier(parts) if parts.len() == 2 => {
            let (qualifier, column) = (&parts[0].value, &parts[1]);
            if !assigned_decimal
                && decimal_columns
                    .iter()
                    .any(|(name, _)| name.eq_ignore_ascii_case(&column.value))
            {
                return unsupported(
                    "ON DUPLICATE KEY UPDATE DECIMAL value assigned to a non-DECIMAL column",
                );
            }
            if offered.is_some_and(|offered| qualifier.eq_ignore_ascii_case(offered)) {
                return Ok(format!("\"excluded\".{}", render_ident(column)));
            }
            if qualifier.eq_ignore_ascii_case(table) {
                return Ok(render_ident(column));
            }
            unsupported("INSERT ON DUPLICATE KEY UPDATE value naming another table")
        }
        // A bare column is the row already there — but only while nothing is
        // offered under a name, which is what makes one ambiguous.
        Expr::Identifier(column) => {
            if !assigned_decimal
                && decimal_columns
                    .iter()
                    .any(|(name, _)| name.eq_ignore_ascii_case(&column.value))
            {
                return unsupported(
                    "ON DUPLICATE KEY UPDATE DECIMAL value assigned to a non-DECIMAL column",
                );
            }
            if offered.is_some() {
                return unsupported(
                    "INSERT ON DUPLICATE KEY UPDATE bare column beside an aliased row",
                );
            }
            Ok(render_ident(column))
        }
        Expr::Nested(inner) => Ok(format!(
            "({})",
            render_duplicate_key_value(inner, table, offered, decimal_columns, assigned_decimal)?
        )),
        Expr::UnaryOp {
            op: UnaryOperator::Plus,
            expr,
        } if duplicate_key_decimal_operand(expr, decimal_columns) => {
            if !assigned_decimal {
                return unsupported(
                    "ON DUPLICATE KEY UPDATE DECIMAL arithmetic assigned to a non-DECIMAL column",
                );
            }
            render_duplicate_key_value(expr, table, offered, decimal_columns, assigned_decimal)
        }
        Expr::UnaryOp {
            op: UnaryOperator::Minus,
            expr,
        } if duplicate_key_decimal_operand(expr, decimal_columns) => {
            if !assigned_decimal {
                return unsupported(
                    "ON DUPLICATE KEY UPDATE DECIMAL arithmetic assigned to a non-DECIMAL column",
                );
            }
            Ok(format!(
                "numeric_sub('0', {})",
                render_duplicate_key_value(
                    expr,
                    table,
                    offered,
                    decimal_columns,
                    assigned_decimal
                )?
            ))
        }
        Expr::BinaryOp { left, op, right }
            if matches!(
                op,
                BinaryOperator::Plus | BinaryOperator::Minus | BinaryOperator::Multiply
            ) =>
        {
            let decimal = assigned_decimal
                || duplicate_key_decimal_operand(left, decimal_columns)
                || duplicate_key_decimal_operand(right, decimal_columns);
            if decimal {
                if !assigned_decimal {
                    return unsupported("ON DUPLICATE KEY UPDATE DECIMAL arithmetic assigned to a non-DECIMAL column");
                }
                for operand in [left.as_ref(), right.as_ref()] {
                    if assigned_decimal
                        && duplicate_key_names_column(operand)
                        && !duplicate_key_decimal_operand(operand, decimal_columns)
                    {
                        return unsupported(
                            "ON DUPLICATE KEY UPDATE mixed DECIMAL column arithmetic",
                        );
                    }
                }
                let function = match op {
                    BinaryOperator::Plus => "numeric_add",
                    BinaryOperator::Minus => "numeric_sub",
                    BinaryOperator::Multiply => "numeric_mul",
                    _ => unreachable!("the guard requires arithmetic"),
                };
                return Ok(format!(
                    "{function}({}, {})",
                    render_duplicate_key_value(
                        left,
                        table,
                        offered,
                        decimal_columns,
                        assigned_decimal
                    )?,
                    render_duplicate_key_value(
                        right,
                        table,
                        offered,
                        decimal_columns,
                        assigned_decimal
                    )?
                ));
            }
            Ok(format!(
                "({} {} {})",
                render_duplicate_key_value(
                    left,
                    table,
                    offered,
                    decimal_columns,
                    assigned_decimal
                )?,
                match op {
                    BinaryOperator::Plus => "+",
                    BinaryOperator::Minus => "-",
                    _ => "*",
                },
                render_duplicate_key_value(
                    right,
                    table,
                    offered,
                    decimal_columns,
                    assigned_decimal
                )?
            ))
        }
        _ => render_inserted_value(value),
    }
}

fn duplicate_key_decimal_operand(expr: &Expr, decimal_columns: &[(String, u32)]) -> bool {
    if decimal_operand_scale(expr, decimal_columns).is_some() {
        return true;
    }
    match expr {
        Expr::Nested(inner) => duplicate_key_decimal_operand(inner, decimal_columns),
        Expr::UnaryOp { expr, .. } => duplicate_key_decimal_operand(expr, decimal_columns),
        Expr::BinaryOp { left, right, .. } => {
            duplicate_key_decimal_operand(left, decimal_columns)
                || duplicate_key_decimal_operand(right, decimal_columns)
        }
        Expr::Function(function) if names_the_offered_row(function) => {
            let FunctionArguments::List(arguments) = &function.args else {
                return false;
            };
            matches!(arguments.args.as_slice(),
                [sqlparser::ast::FunctionArg::Unnamed(sqlparser::ast::FunctionArgExpr::Expr(Expr::Identifier(column)))]
                if decimal_columns.iter().any(|(name, _)| name.eq_ignore_ascii_case(&column.value)))
        }
        _ => false,
    }
}

fn duplicate_key_names_column(expr: &Expr) -> bool {
    match expr {
        Expr::Identifier(_) | Expr::CompoundIdentifier(_) => true,
        Expr::Nested(inner) => duplicate_key_names_column(inner),
        Expr::UnaryOp { expr, .. } => duplicate_key_names_column(expr),
        Expr::Function(function) if names_the_offered_row(function) => true,
        _ => false,
    }
}

/// Reports whether a call is MySQL's `VALUES(col)`, which names the row that
/// was offered rather than the one already there.
fn names_the_offered_row(function: &sqlparser::ast::Function) -> bool {
    matches!(
        function.name.0.as_slice(),
        [ObjectNamePart::Identifier(name)]
            if name.value.eq_ignore_ascii_case("VALUES") && name.quote_style.is_none()
    )
}

/// Counts the `VALUES(col)` calls an `INSERT`'s `ON DUPLICATE KEY UPDATE`
/// writes.
pub(crate) fn offered_row_calls(insert: &Insert) -> usize {
    offered_row_reads(insert)
        .iter()
        .filter(|read| read.through_a_values_call)
        .count()
}

/// One column an `ON DUPLICATE KEY UPDATE` reads off the row it was offered.
pub(crate) struct OfferedRowRead {
    pub(crate) column: String,
    /// Read as `VALUES(col)` rather than through a name on the offered row.
    pub(crate) through_a_values_call: bool,
}

/// Every column an `INSERT`'s `ON DUPLICATE KEY UPDATE` reads off the row it
/// was offered, as `VALUES(col)` or as a column of the name the row carries.
pub(crate) fn offered_row_reads(insert: &Insert) -> Vec<OfferedRowRead> {
    let Some(sqlparser::ast::OnInsert::DuplicateKeyUpdate(assignments)) = &insert.on else {
        return Vec::new();
    };
    let offered =
        insert
            .insert_alias
            .as_ref()
            .and_then(|alias| match alias.row_alias.0.as_slice() {
                [ObjectNamePart::Identifier(name)] => Some(name.value.as_str()),
                _ => None,
            });
    let mut reads = Vec::new();
    for assignment in assignments {
        collect_offered_row_reads(&assignment.value, offered, &mut reads);
    }
    reads
}

fn collect_offered_row_reads(expr: &Expr, offered: Option<&str>, reads: &mut Vec<OfferedRowRead>) {
    let mut collect = |inner: &Expr| collect_offered_row_reads(inner, offered, reads);
    match expr {
        Expr::Function(function) if names_the_offered_row(function) => {
            let column = match &function.args {
                FunctionArguments::List(arguments) => match arguments.args.as_slice() {
                    [sqlparser::ast::FunctionArg::Unnamed(
                        sqlparser::ast::FunctionArgExpr::Expr(Expr::Identifier(column)),
                    )] => column.value.clone(),
                    _ => String::new(),
                },
                _ => String::new(),
            };
            reads.push(OfferedRowRead {
                column,
                through_a_values_call: true,
            });
        }
        Expr::CompoundIdentifier(parts)
            if parts.len() == 2
                && offered.is_some_and(|offered| parts[0].value.eq_ignore_ascii_case(offered)) =>
        {
            reads.push(OfferedRowRead {
                column: parts[1].value.clone(),
                through_a_values_call: false,
            });
        }
        Expr::Function(function) => {
            if let FunctionArguments::List(arguments) = &function.args {
                for argument in &arguments.args {
                    if let sqlparser::ast::FunctionArg::Unnamed(
                        sqlparser::ast::FunctionArgExpr::Expr(inner),
                    ) = argument
                    {
                        collect(inner);
                    }
                }
            }
        }
        Expr::Nested(inner)
        | Expr::UnaryOp { expr: inner, .. }
        | Expr::IsNull(inner)
        | Expr::IsNotNull(inner)
        | Expr::Cast { expr: inner, .. } => collect(inner),
        Expr::BinaryOp { left, right, .. } => {
            collect(left);
            collect(right);
        }
        Expr::Case {
            operand,
            conditions,
            else_result,
            ..
        } => {
            if let Some(operand) = operand {
                collect(operand);
            }
            for arm in conditions {
                collect(&arm.condition);
                collect(&arm.result);
            }
            if let Some(result) = else_result {
                collect(result);
            }
        }
        _ => {}
    }
}

/// Renders `INSERT ... SET a = 1, b = 2` as the column-list form it means.
///
/// The SET form carries the column list and the values interleaved, so it has
/// to be unpicked before it can go through the rules a `VALUES` row goes
/// through. Only one row can be written this way, which is the whole of the
/// difference between the two forms.
fn render_insert_assignments(table: &str, insert: &Insert) -> Result<RenderedInsert, ParseError> {
    if !insert.columns.is_empty() || insert.source.is_some() {
        return unsupported("INSERT SET with a column list or a source query");
    }
    if insert.on.is_some() {
        return unsupported("INSERT SET with ON DUPLICATE KEY UPDATE");
    }
    if insert.assignments.is_empty() {
        return unsupported("INSERT SET without assignments");
    }
    let mut columns = Vec::with_capacity(insert.assignments.len());
    let mut values = Vec::with_capacity(insert.assignments.len());
    for assignment in &insert.assignments {
        let sqlparser::ast::AssignmentTarget::ColumnName(name) = &assignment.target else {
            return unsupported("INSERT SET assignment target");
        };
        columns.push(render_unqualified_name(name)?);
        values.push(render_inserted_value(&assignment.value)?);
    }
    reject_ignored_null(
        insert,
        &insert
            .assignments
            .iter()
            .map(|assignment| &assignment.value)
            .collect::<Vec<_>>(),
    )?;
    // REPLACE and IGNORE both take the SET form too, and mean there what they
    // mean on the other one.
    let verb = insert_verb(insert);
    Ok(RenderedInsert {
        sqlite_sql: format!(
            "{verb} {table} ({}) VALUES ({})",
            columns.join(", "),
            values.join(", ")
        ),
        read_tables: Vec::new(),
        compared_table: None,
        checked_comparisons: Vec::new(),
        row_count_parameters: Vec::new(),
        json_cast_columns: Vec::new(),
    })
}

/// Chooses the engine's insert verb for what the statement said about
/// collisions.
///
/// MySQL's `REPLACE` deletes the rows a unique key collides with and inserts,
/// which is what the engine's own `OR REPLACE` does. `INSERT IGNORE` skips a
/// colliding row, which is `OR IGNORE`. Measured on MySQL 8.4.11: an
/// `INSERT IGNORE` whose row collides on the primary key leaves the stored row
/// alone and counts 0, and a two-row one where only the second is new counts 1.
fn insert_verb(insert: &Insert) -> &'static str {
    if insert.replace_into {
        "INSERT OR REPLACE INTO"
    } else if insert.ignore {
        "INSERT OR IGNORE INTO"
    } else {
        "INSERT INTO"
    }
}

/// Refuses an `INSERT IGNORE` that writes a NULL.
///
/// This is the one place the two engines' IGNORE part company. MySQL treats a
/// NULL in a NOT NULL column as something to coerce rather than refuse —
/// measured on 8.4.11, `INSERT IGNORE` of NULL into a NOT NULL INT stores 0 —
/// while the engine's `OR IGNORE` skips the row and stores nothing. A row that
/// exists in one and not the other is a difference a client cannot see, so the
/// statement is refused instead. A NULL bound for a column that accepts one
/// would agree, but the column is not known here, so all of them are refused.
fn reject_ignored_null(insert: &Insert, values: &[&Expr]) -> Result<(), ParseError> {
    if !insert.ignore {
        return Ok(());
    }
    if values
        .iter()
        .any(|value| matches!(value, Expr::Value(value) if matches!(value.value, Value::Null)))
    {
        return unsupported("INSERT IGNORE writing NULL");
    }
    Ok(())
}

/// The same rendered value with each of its parameters named by its ordinal.
///
/// A bare `?` takes the next ordinal wherever it stands, so writing a value out
/// a second time would ask for parameters the caller never bound. Naming them
/// leaves the ordinals where the first copy put them, and a bare `?` after this
/// still takes the next one it would have taken — the engine counts an ordinal
/// as used, not as read.
fn parameters_named_by_their_ordinals(value: &str, first_parameter: usize) -> String {
    let mut named = String::with_capacity(value.len());
    let mut quote = None;
    let mut ordinal = first_parameter;
    for character in value.chars() {
        match quote {
            Some(delimiter) => {
                named.push(character);
                if character == delimiter {
                    quote = None;
                }
            }
            None if character == '\'' || character == '"' => {
                quote = Some(character);
                named.push(character);
            }
            None if character == '?' => {
                ordinal += 1;
                named.push_str(&format!("?{ordinal}"));
            }
            None => named.push(character),
        }
    }
    named
}

/// The assignments that rewrite a table's `ON UPDATE CURRENT_TIMESTAMP`
/// columns, for the columns the statement did not name itself.
///
/// Measured on MySQL 8.4.11: such a column is rewritten by an `UPDATE` only
/// where the row actually changes — `SET n = 1` over a row already holding 1
/// leaves it where it stood and counts no row — and a statement that names the
/// column writes what it says instead. So the moment is written under a
/// condition asking whether any assigned column is about to change, which the
/// engine reads against the row as it stands.
fn moments_the_update_rewrites(
    assigned: &[String],
    written: &[(String, String, usize)],
    render_context: &SelectRenderContext<'_>,
) -> Vec<String> {
    if render_context.rewritten_on_update.is_empty() || written.is_empty() {
        return Vec::new();
    }
    let changes = written
        .iter()
        .map(|(name, value, first_parameter)| {
            format!(
                "{name} IS NOT {}",
                parameters_named_by_their_ordinals(value, *first_parameter)
            )
        })
        .collect::<Vec<_>>()
        .join(" OR ");
    render_context
        .rewritten_on_update
        .iter()
        .filter(|(column, _)| {
            !assigned
                .iter()
                .any(|written| written.eq_ignore_ascii_case(column))
        })
        .map(|(column, digits)| {
            let name = render_ident_str(column);
            let moment = match digits {
                0 => "CURRENT_TIMESTAMP".to_owned(),
                digits => super::moment_with_fraction_sql(*digits),
            };
            format!("{name} = CASE WHEN {changes} THEN {moment} ELSE {name} END")
        })
        .collect()
}

pub(crate) fn translate_update(
    update: &Update,
    render_context: &mut SelectRenderContext<'_>,
) -> Result<(String, Vec<MySqlSelectSource>, CheckedUpdate), ParseError> {
    if !update.optimizer_hints.is_empty()
        || update.from.is_some()
        || update.returning.is_some()
        || update.output.is_some()
        || update.or.is_some()
    {
        return unsupported("UPDATE option");
    }
    // MySQL updates the rows a join finds, naming the table to change through
    // the columns the SET names.
    if !update.table.joins.is_empty() {
        // A joined `UPDATE` names the table it changes through the columns its
        // `SET` names, so which table's `ON UPDATE` columns are its own is a
        // question this has not answered.
        if !render_context.rewritten_on_update.is_empty() {
            return unsupported("joined UPDATE on a table with an ON UPDATE column");
        }
        return translate_joined_update(update, render_context);
    }
    let checked = checked_update(update)?;
    let table = render_update_table(&update.table.relation)?;
    if update.assignments.is_empty() {
        return unsupported("UPDATE without assignments");
    }
    let mut assigned = Vec::with_capacity(update.assignments.len());
    let mut assignments = Vec::with_capacity(update.assignments.len());
    let mut written: Vec<(String, String, usize)> = Vec::with_capacity(update.assignments.len());
    for assignment in &update.assignments {
        let sqlparser::ast::AssignmentTarget::ColumnName(column) = &assignment.target else {
            return unsupported("UPDATE assignment target");
        };
        // Rails writes every column it saves qualified by its table, `SET
        // users.name = ...`, which names the same column.
        let name = match column.0.as_slice() {
            [ObjectNamePart::Identifier(name)] => name,
            [ObjectNamePart::Identifier(qualifier), ObjectNamePart::Identifier(name)]
                if names_the_updated_table(qualifier, &update.table.relation) =>
            {
                name
            }
            _ => return unsupported("UPDATE assignment target"),
        };
        let rendered_name = render_ident(name);
        // A value carrying a `?` is written twice where a column is rewritten
        // on update, and a second bare `?` would be a second parameter. The
        // ordinals it took are noted here so the second copy can name them.
        let first_parameter = render_context.parameter_count;
        let rendered_value = render_update_assignment_value(
            &assignment.value,
            &name.value,
            &assigned,
            render_context,
        )?;
        // Most values are written without counting the `?`s in them, and the
        // parameters after them — the `WHERE`'s, which a prepared statement
        // holds to what they are compared with — are numbered past them.
        render_context.parameter_count = first_parameter + placeholders_in(&rendered_value);
        written.push((
            rendered_name.clone(),
            rendered_value.clone(),
            first_parameter,
        ));
        assignments.push(format!("{rendered_name} = {rendered_value}"));
        assigned.push(name.value.clone());
    }
    assignments.extend(moments_the_update_rewrites(
        &assigned,
        &written,
        render_context,
    ));

    if update.order_by.is_empty() && update.limit.is_some() {
        return unsupported("UPDATE LIMIT without ORDER BY");
    }

    if !update.order_by.is_empty() {
        let order_by_sql = render_dml_order_by(&update.order_by, render_context)?;
        let limit_sql = if let Some(limit_expr) = &update.limit {
            // A count wider than the engine reads means every row here, and
            // what MySQL does with an `UPDATE` or a `DELETE` written that way
            // has not been measured.
            let Ok(limit_val) = i64::try_from(render_select_row_count(limit_expr)?) else {
                return unsupported("DML LIMIT wider than a signed 64-bit count");
            };
            format!(" LIMIT {limit_val}")
        } else {
            String::new()
        };
        let sub_where = if let Some(selection) = &update.selection {
            format!(
                " WHERE {}",
                render_dml_predicate(selection, render_context)?
            )
        } else {
            String::new()
        };
        Ok((
            format!(
                "UPDATE {table} SET {} WHERE _rowid_ IN (SELECT _rowid_ FROM {table}{sub_where} ORDER BY {order_by_sql}{limit_sql})",
                assignments.join(", ")
            ),
            dml_subquery_tables(checked.table_name(), render_context)?,
            checked,
        ))
    } else {
        let mut normalized = format!("UPDATE {table} SET {}", assignments.join(", "));
        if let Some(selection) = &update.selection {
            normalized.push_str(" WHERE ");
            normalized.push_str(&render_dml_predicate(selection, render_context)?);
        }
        let read = dml_subquery_tables(checked.table_name(), render_context)?;
        Ok((normalized, read, checked))
    }
}

/// How many `?`s a rendered value holds outside its quoted words.
fn placeholders_in(value: &str) -> usize {
    let mut count = 0;
    let mut quote = None;
    for character in value.chars() {
        match quote {
            Some(delimiter) if character == delimiter => quote = None,
            Some(_) => {}
            None if character == '\'' || character == '"' => quote = Some(character),
            None if character == '?' => count += 1,
            None => {}
        }
    }
    count
}

/// Hands back the tables a `UPDATE` or `DELETE` reads through a subquery,
/// refusing one that reads the table being changed.
///
/// MySQL answers 1093 for that — measured on 8.4.11, `DELETE FROM t WHERE id
/// IN (SELECT id FROM t WHERE n > 100)` names the target table in the FROM
/// clause and is turned away — where the engine would answer it. The tables
/// come back so the statement authorizes them alongside the one it writes.
fn dml_subquery_tables(
    target: &str,
    render_context: &mut SelectRenderContext<'_>,
) -> Result<Vec<MySqlSelectSource>, ParseError> {
    let read = std::mem::take(&mut render_context.subquery_tables);
    if read
        .iter()
        .any(|source| source.table.as_str().eq_ignore_ascii_case(target))
    {
        return unsupported("DML subquery reading the table the statement changes");
    }
    Ok(read)
}

/// Reports whether a value a joined `UPDATE` assigns depends on the row being
/// changed and nothing else.
///
/// A value naming another table takes it from whichever row the join happened
/// to find — measured on MySQL 8.4.11, `SET a.n = b.m` over two matching rows
/// takes the first — which is not a rule this answers, so it is refused. A
/// column of the table being changed is written without its table.
fn names_one_row_alone(expr: &Expr) -> bool {
    match expr {
        Expr::Identifier(_) | Expr::Value(_) => true,
        Expr::UnaryOp { expr, .. } | Expr::Nested(expr) => names_one_row_alone(expr),
        Expr::BinaryOp { left, right, .. } => {
            names_one_row_alone(left) && names_one_row_alone(right)
        }
        _ => false,
    }
}

/// Renders an `UPDATE` that names its rows through a join.
///
/// The rows to change are the ones the join finds, so the join is written as a
/// subquery answering the target's own rowids and the update takes those —
/// the shape a `DELETE` naming its rows through a join already takes.
fn translate_joined_update(
    update: &Update,
    render_context: &mut SelectRenderContext<'_>,
) -> Result<(String, Vec<MySqlSelectSource>, CheckedUpdate), ParseError> {
    // Measured on MySQL 8.4.11: a joined UPDATE takes neither, answering 1221.
    if !update.order_by.is_empty() || update.limit.is_some() {
        return unsupported("joined UPDATE with ORDER BY or LIMIT");
    }
    if update.assignments.is_empty() {
        return unsupported("UPDATE without assignments");
    }
    let (Some(rendered_from), sources) = render_from_clause(std::slice::from_ref(&update.table))?
    else {
        return unsupported("UPDATE table source");
    };
    // Every assignment names the table it changes. MySQL takes an unqualified
    // name and resolves it against the joined tables; refusing that here is
    // what keeps this from picking a table MySQL would have called ambiguous.
    let mut target: Option<&str> = None;
    let mut assigned = Vec::with_capacity(update.assignments.len());
    let mut assignments = Vec::with_capacity(update.assignments.len());
    let mut columns = Vec::with_capacity(update.assignments.len());
    for assignment in &update.assignments {
        let sqlparser::ast::AssignmentTarget::ColumnName(name) = &assignment.target else {
            return unsupported("UPDATE assignment target");
        };
        let [ObjectNamePart::Identifier(qualifier), ObjectNamePart::Identifier(column)] =
            name.0.as_slice()
        else {
            return unsupported("joined UPDATE assignment target without a table");
        };
        if *target.get_or_insert(qualifier.value.as_str()) != qualifier.value {
            return unsupported("joined UPDATE changing more than one table");
        }
        if !names_one_row_alone(&assignment.value) {
            return unsupported("joined UPDATE assignment naming another table");
        }
        assignments.push(format!(
            "{} = {}",
            render_ident(column),
            render_update_assignment_value(
                &assignment.value,
                &column.value,
                &assigned,
                render_context
            )?
        ));
        assigned.push(column.value.clone());
        columns.push(CheckedUpdateAssignment {
            column_name: column.value.clone(),
            value: checked_update_assignment_value(&column.value, &assignment.value),
        });
    }
    let target = target.expect("an assignment was checked to name its table");
    let Some(source) = sources
        .iter()
        .find(|source| source.reference.eq_ignore_ascii_case(target))
    else {
        return unsupported("UPDATE target that the join does not read");
    };
    let reference = render_ident_str(&source.reference);
    let table = render_ident_str(source.table.as_str());
    let mut predicate = String::new();
    if let Some(selection) = &update.selection {
        predicate = format!(
            " WHERE {}",
            render_select_predicate(selection, render_context)?
        );
    }
    let mut read = sources.clone();
    read.append(&mut dml_subquery_tables(
        source.table.as_str(),
        render_context,
    )?);
    Ok((
        format!(
            "UPDATE {table} SET {} WHERE _rowid_ IN (SELECT {reference}._rowid_ FROM {rendered_from}{predicate})",
            assignments.join(", ")
        ),
        read,
        CheckedUpdate {
            table_name: source.table.as_str().to_owned(),
            assignments: columns,
        },
    ))
}

pub(crate) fn checked_update(update: &Update) -> Result<CheckedUpdate, ParseError> {
    let table_name = update_table_name(&update.table.relation)?;
    let assignments = update
        .assignments
        .iter()
        .map(|assignment| {
            let sqlparser::ast::AssignmentTarget::ColumnName(column) = &assignment.target else {
                return unsupported("UPDATE assignment target");
            };
            let column = match column.0.as_slice() {
                [ObjectNamePart::Identifier(column)] => column,
                [ObjectNamePart::Identifier(qualifier), ObjectNamePart::Identifier(column)]
                    if names_the_updated_table(qualifier, &update.table.relation) =>
                {
                    column
                }
                _ => return unsupported("qualified UPDATE assignment target"),
            };
            Ok(CheckedUpdateAssignment {
                column_name: column.value.clone(),
                value: checked_update_assignment_value(&column.value, &assignment.value),
            })
        })
        .collect::<Result<Vec<_>, _>>()?;
    Ok(CheckedUpdate {
        table_name,
        assignments,
    })
}

fn checked_update_assignment_value(
    column_name: &str,
    value: &Expr,
) -> CheckedUpdateAssignmentValue {
    if matches!(
        value,
        Expr::Identifier(identifier) if identifier.value.eq_ignore_ascii_case(column_name)
    ) {
        return CheckedUpdateAssignmentValue::SelfAssignment;
    }
    direct_signed_integer(value)
        .map(CheckedUpdateAssignmentValue::SignedInteger)
        .unwrap_or(CheckedUpdateAssignmentValue::Other)
}

pub(crate) fn direct_signed_integer(expr: &Expr) -> Option<i64> {
    match expr {
        Expr::Value(value) => match &value.value {
            Value::Number(value, false) => value.parse().ok(),
            _ => None,
        },
        Expr::UnaryOp {
            op: UnaryOperator::Plus,
            expr,
        } => match expr.as_ref() {
            Expr::Value(value) => match &value.value {
                Value::Number(value, false) => value.parse().ok(),
                _ => None,
            },
            _ => None,
        },
        Expr::UnaryOp {
            op: UnaryOperator::Minus,
            expr,
        } => match expr.as_ref() {
            Expr::Value(value) => match &value.value {
                Value::Number(value, false) => value.parse::<u64>().ok().and_then(|magnitude| {
                    if magnitude == (i64::MAX as u64) + 1 {
                        Some(i64::MIN)
                    } else {
                        i64::try_from(magnitude).ok().map(|value| -value)
                    }
                }),
                _ => None,
            },
            _ => None,
        },
        _ => None,
    }
}

/// Reads the one table a `DELETE` names, when it names one plainly.
pub(crate) fn delete_source_table(delete: &Delete) -> Option<String> {
    let FromTable::WithFromKeyword(tables) = &delete.from else {
        return None;
    };
    let [table] = tables.as_slice() else {
        return None;
    };
    let TableFactor::Table { name, .. } = &table.relation else {
        return None;
    };
    let [ObjectNamePart::Identifier(ident)] = name.0.as_slice() else {
        return None;
    };
    MySqlTableName::parse(&ident.value)
        .ok()
        .map(|name| name.as_str().to_owned())
}

pub(crate) fn translate_delete(
    delete: &Delete,
    render_context: &mut SelectRenderContext<'_>,
) -> Result<(String, Vec<MySqlSelectSource>), ParseError> {
    if !delete.optimizer_hints.is_empty() || delete.returning.is_some() || delete.output.is_some() {
        return unsupported("DELETE option");
    }
    let FromTable::WithFromKeyword(from) = &delete.from else {
        return unsupported("DELETE without FROM");
    };
    // MySQL names the table to delete from twice in a joined DELETE: once in
    // front of the FROM, or once after a USING. Either way, the rows to delete
    // are the ones the join finds.
    if !delete.tables.is_empty() || delete.using.is_some() {
        return translate_joined_delete(delete, from, render_context);
    }
    let (table, target) = match from.as_slice() {
        [from] if from.joins.is_empty() => (
            render_update_table(&from.relation)?,
            update_table_name(&from.relation)?,
        ),
        _ => return unsupported("DELETE table source"),
    };

    if delete.order_by.is_empty() && delete.limit.is_some() {
        return unsupported("DELETE LIMIT without ORDER BY");
    }

    if !delete.order_by.is_empty() {
        let order_by_sql = render_dml_order_by(&delete.order_by, render_context)?;
        let limit_sql = if let Some(limit_expr) = &delete.limit {
            // A count wider than the engine reads means every row here, and
            // what MySQL does with an `UPDATE` or a `DELETE` written that way
            // has not been measured.
            let Ok(limit_val) = i64::try_from(render_select_row_count(limit_expr)?) else {
                return unsupported("DML LIMIT wider than a signed 64-bit count");
            };
            format!(" LIMIT {limit_val}")
        } else {
            String::new()
        };
        let sub_where = if let Some(selection) = &delete.selection {
            format!(
                " WHERE {}",
                render_dml_predicate(selection, render_context)?
            )
        } else {
            String::new()
        };
        Ok((
            format!(
                "DELETE FROM {table} WHERE _rowid_ IN (SELECT _rowid_ FROM {table}{sub_where} ORDER BY {order_by_sql}{limit_sql})"
            ),
            dml_subquery_tables(&target, render_context)?,
        ))
    } else {
        let mut normalized = format!("DELETE FROM {table}");
        if let Some(selection) = &delete.selection {
            normalized.push_str(" WHERE ");
            normalized.push_str(&render_dml_predicate(selection, render_context)?);
        }
        let read = dml_subquery_tables(&target, render_context)?;
        Ok((normalized, read))
    }
}

/// Renders a `DELETE` that names its rows through a join.
///
/// The rows to delete are the ones the join finds, so the join is written as a
/// subquery answering the target's own rowids and the delete takes those. That
/// is the shape a `DELETE ... ORDER BY` already takes, and it holds for every
/// join a `SELECT` holds, an outer one included.
fn translate_joined_delete(
    delete: &Delete,
    from: &[sqlparser::ast::TableWithJoins],
    render_context: &mut SelectRenderContext<'_>,
) -> Result<(String, Vec<MySqlSelectSource>), ParseError> {
    if !delete.order_by.is_empty() || delete.limit.is_some() {
        return unsupported("joined DELETE with ORDER BY or LIMIT");
    }
    // `DELETE t1 FROM ...` names the target in front; `DELETE FROM t1 USING ...`
    // names it after the FROM. Only one target is taken: MySQL deletes from
    // several at once and each would need its own statement here.
    let (targets, sources_from) = match &delete.using {
        Some(using) => (
            match &delete.from {
                FromTable::WithFromKeyword(named) => named.as_slice(),
                FromTable::WithoutKeyword(named) => named.as_slice(),
            },
            using.as_slice(),
        ),
        None => (from, from),
    };
    let target = match (&delete.tables[..], targets) {
        ([name], _) if delete.using.is_none() => named_delete_target(name)?,
        (_, [one]) if delete.using.is_some() && delete.tables.is_empty() => {
            let TableFactor::Table { name, .. } = &one.relation else {
                return unsupported("DELETE USING target");
            };
            named_delete_target(name)?
        }
        _ => return unsupported("DELETE naming more than one table"),
    };
    let (Some(rendered_from), sources) = render_from_clause(sources_from)? else {
        return unsupported("DELETE table source");
    };
    // The target has to be one of the tables the join reads, and the name it
    // is read under is the one that qualifies its rowid.
    let Some(source) = sources
        .iter()
        .find(|source| source.reference.eq_ignore_ascii_case(&target))
    else {
        return unsupported("DELETE target that the join does not read");
    };
    let reference = render_ident_str(&source.reference);
    let table = render_ident_str(source.table.as_str());
    let target_table = source.table.as_str().to_owned();
    let mut predicate = String::new();
    if let Some(selection) = &delete.selection {
        predicate = format!(
            " WHERE {}",
            render_select_predicate(selection, render_context)?
        );
    }
    let mut read = sources;
    read.append(&mut dml_subquery_tables(&target_table, render_context)?);
    Ok((
        format!(
            "DELETE FROM {table} WHERE _rowid_ IN (SELECT {reference}._rowid_ FROM {rendered_from}{predicate})"
        ),
        read,
    ))
}

/// Reads the one name a joined `DELETE` deletes from.
fn named_delete_target(name: &ObjectName) -> Result<String, ParseError> {
    let [ObjectNamePart::Identifier(ident)] = name.0.as_slice() else {
        return unsupported("qualified DELETE target");
    };
    Ok(ident.value.clone())
}

fn render_dml_order_by(
    order_by: &[sqlparser::ast::OrderByExpr],
    render_context: &mut SelectRenderContext<'_>,
) -> Result<String, ParseError> {
    if order_by.is_empty() {
        return unsupported("DML ORDER BY option");
    }
    order_by
        .iter()
        .map(|expression| {
            if expression.options.nulls_first.is_some() || expression.with_fill.is_some() {
                return unsupported("DML ORDER BY option");
            }
            let direction = if expression.options.asc == Some(false) {
                "DESC"
            } else {
                "ASC"
            };
            if order_by_ordinal(&expression.expr).is_some() {
                return unsupported("DML ORDER BY ordinal");
            }
            match &expression.expr {
                Expr::Identifier(ident) => {
                    render_context
                        .ordered_columns
                        .push((None, ident.value.clone()));
                    Ok(format!("{} {direction}", render_ident(ident)))
                }
                Expr::CompoundIdentifier(parts) if parts.len() == 2 => {
                    let qualifier = MySqlTableName::parse(&parts[0].value)?;
                    render_context
                        .ordered_columns
                        .push((Some(qualifier.as_str().to_owned()), parts[1].value.clone()));
                    Ok(format!(
                        "{}.{} {direction}",
                        render_ident(&parts[0]),
                        render_ident(&parts[1])
                    ))
                }
                _ => unsupported("DML ORDER BY expression"),
            }
        })
        .collect::<Result<Vec<_>, _>>()
        .map(|expressions| expressions.join(", "))
}

/// Whether a qualifier names the one table an `UPDATE` changes.
fn names_the_updated_table(qualifier: &Ident, table: &TableFactor) -> bool {
    let TableFactor::Table { name, .. } = table else {
        return false;
    };
    matches!(name.0.last(), Some(ObjectNamePart::Identifier(table))
        if table.value.eq_ignore_ascii_case(&qualifier.value))
}

fn render_update_table(table: &TableFactor) -> Result<String, ParseError> {
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
    } = table
    else {
        return unsupported("UPDATE table source");
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
        return unsupported("UPDATE table option");
    }
    render_unqualified_name(name)
}

fn update_table_name(table: &TableFactor) -> Result<String, ParseError> {
    let TableFactor::Table { name, .. } = table else {
        return unsupported("UPDATE table source");
    };
    let [ObjectNamePart::Identifier(name)] = name.0.as_slice() else {
        return unsupported("qualified UPDATE table name");
    };
    Ok(name.value.clone())
}

/// Renders the value one `UPDATE ... SET` assignment writes.
///
/// A column is read here, and arithmetic over one, which is what makes
/// `SET n = n + 1` the ordinary way to count something up. Division is not:
/// measured on MySQL 8.4.11, `b / 2` over 101 answers 50.5 and the engine
/// answers 50, so the two would write different numbers.
///
/// MySQL reads the columns a `SET` has already assigned in the values after
/// them — measured, `SET a = 100, b = a` leaves `b` at 100 — where the engine
/// reads the row as it was. So a value naming a column the same statement has
/// already assigned is refused rather than answered differently.
fn render_update_assignment_value(
    value: &Expr,
    written: &str,
    assigned: &[String],
    render_context: &mut SelectRenderContext<'_>,
) -> Result<String, ParseError> {
    let refuse_if_assigned = |name: &str| {
        assigned
            .iter()
            .any(|earlier| earlier.eq_ignore_ascii_case(name))
    };
    match value {
        // `SET n = DEFAULT` writes the column's own default, which the engine
        // has no spelling for and this cannot work out from the statement
        // alone. Refusing keeps it from being read as a column of that name.
        _ if names_the_columns_default(value, written) => {
            unsupported("UPDATE assignment writing a column default")
        }
        Expr::Identifier(ident) => {
            if contains_decimal_operand(value, render_context.decimal_columns)
                && !render_context.decimal_columns.iter().any(|(column, _)| column.eq_ignore_ascii_case(written))
            {
                return unsupported("UPDATE DECIMAL value assigned to a non-DECIMAL column");
            }
            if refuse_if_assigned(&ident.value) {
                return unsupported("UPDATE assignment reading a column it has already assigned");
            }
            Ok(render_ident(ident))
        }
        Expr::CompoundIdentifier(parts) if parts.len() == 2 => {
            if contains_decimal_operand(value, render_context.decimal_columns)
                && !render_context.decimal_columns.iter().any(|(column, _)| column.eq_ignore_ascii_case(written))
            {
                return unsupported("UPDATE DECIMAL value assigned to a non-DECIMAL column");
            }
            if refuse_if_assigned(&parts[1].value) {
                return unsupported("UPDATE assignment reading a column it has already assigned");
            }
            Ok(format!(
                "{}.{}",
                render_ident(&parts[0]),
                render_ident(&parts[1])
            ))
        }
        Expr::Nested(inner) => Ok(format!(
            "({})",
            render_update_assignment_value(inner, written, assigned, render_context)?
        )),
        Expr::UnaryOp {
            op: UnaryOperator::Plus,
            expr,
        } => {
            let decimal = contains_decimal_operand(expr, render_context.decimal_columns);
            if decimal && !render_context.decimal_columns.iter().any(|(column, _)| column.eq_ignore_ascii_case(written)) {
                return unsupported("UPDATE DECIMAL arithmetic assigned to a non-DECIMAL column");
            }
            let rendered = render_update_assignment_value(expr, written, assigned, render_context)?;
            if decimal {
                Ok(rendered)
            } else {
                Ok(format!("(+{rendered})"))
            }
        }
        Expr::UnaryOp {
            op: UnaryOperator::Minus,
            expr,
        } if contains_decimal_operand(expr, render_context.decimal_columns) => {
            if !render_context.decimal_columns.iter().any(|(column, _)| column.eq_ignore_ascii_case(written)) {
                return unsupported("UPDATE DECIMAL arithmetic assigned to a non-DECIMAL column");
            }
            Ok(format!("numeric_sub('0', {})", render_update_assignment_value(expr, written, assigned, render_context)?))
        }
        // `SET ratio = score / 2` is how a statement scales a column down.
        // MySQL's `/` is decimal division where the engine's is integer
        // division, and what lands in the column is rounded to the column's
        // own scale on the way in — measured on 8.4.11, 10 / 3 into a
        // `DECIMAL(10,2)` is 3.33 and 5 / 2 is 2.50, which is what this
        // stores.
        //
        // The divisor has to be a written number that is not zero. Dividing by
        // zero answers NULL in the engine where MySQL raises 1365 for a write,
        // and only a written divisor says which of the two a statement would
        // get.
        Expr::BinaryOp {
            left,
            op: BinaryOperator::Divide,
            right,
        } if (decimal_operand_scale(left, render_context.decimal_columns).is_some()
            || render_context.decimal_columns.iter().any(|(column, _)| column.eq_ignore_ascii_case(written)))
            && direct_signed_integer(right).is_some_and(|divisor| divisor != 0) =>
        {
            if !render_context.decimal_columns.iter().any(|(column, _)| column.eq_ignore_ascii_case(written)) {
                return unsupported("UPDATE DECIMAL division assigned to a non-DECIMAL column");
            }
            let Some(input_scale) = decimal_operand_scale(left, render_context.decimal_columns)
                .or_else(|| direct_signed_integer(left).map(|_| 0))
                .or_else(|| known_typed_numeric_operand(left, render_context.integer_columns).then_some(0)) else {
                    return unsupported("UPDATE DECIMAL division over an untyped column");
                };
            let scale = input_scale.saturating_add(4).min(30);
            let divisor = direct_signed_integer(right).expect("the guard requires a divisor");
            Ok(format!(
                "mysql_decimal_div_round({}, '{divisor}', {scale})",
                render_update_assignment_value(left, written, assigned, render_context)?
            ))
        }
        Expr::BinaryOp { left, op: BinaryOperator::Divide, right }
            if render_context.decimal_columns.iter().any(|(column, _)| column.eq_ignore_ascii_case(written))
                || contains_decimal_operand(left, render_context.decimal_columns)
                || contains_decimal_operand(right, render_context.decimal_columns) =>
        {
            unsupported("UPDATE DECIMAL division requires a DECIMAL column or integer numerator and a written nonzero integer divisor")
        }
        Expr::BinaryOp {
            left,
            op: BinaryOperator::Divide,
            right,
        } if direct_signed_integer(right).is_some_and(|divisor| divisor != 0) => Ok(format!(
            "(CAST({} AS REAL) / {})",
            render_update_assignment_value(left, written, assigned, render_context)?,
            render_dml_expr(right)?
        )),
        // `SET expires_at = NOW() + INTERVAL 1 HOUR` is the operator spelling
        // of a `DATE_ADD`, and is written the way the call is.
        Expr::BinaryOp { .. }
            if static_select_metadata::interval_shift_as_call(value).is_some() =>
        {
            let call = Expr::Function(
                static_select_metadata::interval_shift_as_call(value)
                    .expect("the guard read the operator as a shift"),
            );
            render_update_assignment_value(&call, written, assigned, render_context)
        }
        Expr::BinaryOp { left, op, right }
            if matches!(
                op,
                BinaryOperator::Plus | BinaryOperator::Minus | BinaryOperator::Multiply
            ) =>
        {
            let left_is_decimal = contains_decimal_operand(left, render_context.decimal_columns);
            let right_is_decimal = contains_decimal_operand(right, render_context.decimal_columns);
            let assigned_decimal = render_context
                .decimal_columns
                .iter()
                .any(|(column, _)| column.eq_ignore_ascii_case(written));
            if assigned_decimal || left_is_decimal || right_is_decimal {
                if !assigned_decimal {
                    return unsupported("UPDATE DECIMAL arithmetic assigned to a non-DECIMAL column");
                }
                for operand in [left.as_ref(), right.as_ref()] {
                    if matches!(operand, Expr::Identifier(_) | Expr::CompoundIdentifier(_))
                        && decimal_operand_scale(operand, render_context.decimal_columns).is_none()
                    {
                        return unsupported("UPDATE mixed DECIMAL column arithmetic");
                    }
                }
                let function = match op {
                    BinaryOperator::Plus => "numeric_add",
                    BinaryOperator::Minus => "numeric_sub",
                    BinaryOperator::Multiply => "numeric_mul",
                    _ => unreachable!("the guard requires DECIMAL arithmetic"),
                };
                return Ok(format!(
                    "{function}({}, {})",
                    render_set_arithmetic_operand(left, written, assigned, render_context)?,
                    render_set_arithmetic_operand(right, written, assigned, render_context)?
                ));
            }
            Ok(format!(
                "({} {} {})",
                render_set_arithmetic_operand(left, written, assigned, render_context)?,
                match op {
                    BinaryOperator::Plus => "+",
                    BinaryOperator::Minus => "-",
                    _ => "*",
                },
                render_set_arithmetic_operand(right, written, assigned, render_context)?
            ))
        }
        _ if json_cast_operand(value).is_some() => {
            let operand = json_cast_operand(value).expect("the guard read a JSON cast");
            let rendered = render_json_cast(operand)?;
            for _ in 0..placeholders_in(&rendered) {
                render_context.next_parameter_ordinal()?;
            }
            render_context.json_cast_columns.push(written.to_owned());
            Ok(rendered)
        }
        // A call or a `CASE` writes a value worked out from the row, which is
        // how a statement trims a word or counts a default in. Each is
        // rendered the way a projection renders it, so what lands in the
        // column is the value that reading answers.
        Expr::Function(_)
        | Expr::Case { .. }
        | Expr::Trim { .. }
        | Expr::Substring { .. }
        | Expr::Floor { .. }
        | Expr::Ceil { .. }
        | Expr::Cast { .. }
        | Expr::Convert { .. }
            if static_select_metadata::classify_static_select_expr(value).is_some() =>
        {
            if contains_decimal_operand(value, render_context.decimal_columns)
                && !render_context
                    .decimal_columns
                    .iter()
                    .any(|(column, _)| column.eq_ignore_ascii_case(written))
                && !writes_whole_numbers_into_words(value, written, render_context)
            {
                return unsupported("UPDATE DECIMAL value assigned to a non-DECIMAL column");
            }
            if !reads_only_unassigned_columns(value, assigned) {
                return unsupported("UPDATE assignment reading a column it has already assigned");
            }
            // What MySQL writes for a `CASE` falling back onto a column of
            // words, or for an aggregate, has not been measured.
            if written_only_as_a_reading(value) {
                return unsupported("UPDATE assignment of an unmeasured conditional");
            }
            render_select_expr(value, render_context)
        }
        Expr::Function(function) if words_joined_with_fallbacks(function).is_some() => {
            render_set_words_joined_with_fallbacks(function, written, assigned, render_context)
        }
        // A fallback the arm above cannot read the shape of on its own — one
        // naming its column through its table, the way Rails does — is held
        // to the column's kind once the frontend has said what it is.
        _ if fallback_over_a_column(value).is_some() => {
            render_set_fallback(value, &refuse_if_assigned, render_context)
        }
        // `SET n = (SELECT MAX(m) FROM other)` takes one value out of another
        // table. An aggregate over one implicit group answers exactly one row,
        // which is what makes it a value; a plain column does not, and MySQL
        // answers 1242 for that. The table it reads comes back with the
        // statement, so reading the one being changed is 1093 there and
        // refused here with every other DML subquery.
        Expr::Subquery(query) => {
            let SetExpr::Select(select) = query.body.as_ref() else {
                return unsupported("UPDATE assignment subquery body");
            };
            let Some(ScalarSubqueryAnswer::TheColumnsOwnKind(read)) =
                subquery_answering_one_value(select)
            else {
                return unsupported("UPDATE assignment subquery");
            };
            let Some(source) = subquery_source_table(select) else {
                return unsupported("UPDATE assignment subquery table");
            };
            let (rendered, _) = render_subquery(query, render_context)?;
            // The column written and the column read have to be the same kind,
            // which only the frontend can see — the rule an
            // `IN (SELECT ...)` holds its pair to.
            render_context
                .checked_subquery_comparisons
                .push(CheckedSubqueryComparison {
                    qualifier: None,
                    column_name: written.to_owned(),
                    inner_table: source.as_str().to_owned(),
                    inner_column_name: read,
                    fixed_columns: Vec::new(),
                });
            Ok(format!("({rendered})"))
        }
        _ if crate::written_value::written_byte_string(value).is_some() => {
            crate::written_value::written_byte_string(value)
                .expect("the guard read a string of bytes")
                .ok_or(ParseError::Unsupported {
                    feature: "a string of bytes spelled some other way",
                })
        }
        // What is left is a value rather than a reading of the row, so none of
        // it can name a column. Each `?` in it still takes its place among the
        // statement's parameters.
        _ => {
            let rendered = render_dml_expr(value)?;
            for _ in 0..placeholders_in(&rendered) {
                render_context.next_parameter_ordinal()?;
            }
            Ok(rendered)
        }
    }
}

/// Whether a `SET` value is a call writing whole numbers out as words into a
/// column of words — GORM's backfill `SET slug = CONCAT('post-', id)` over a
/// `BIGINT UNSIGNED` id.
///
/// The engine reads a `BIGINT UNSIGNED` and a `DECIMAL` with no places out as
/// the digits MySQL writes them with, which is why a projection takes these
/// calls over one; written into a column of words, the word is the value.
/// Measured on MySQL 8.4.11, `CONCAT('post-', id)` over an id of
/// 18446744073709551615 writes `post-18446744073709551615`.
fn writes_whole_numbers_into_words(
    value: &Expr,
    written: &str,
    render_context: &SelectRenderContext<'_>,
) -> bool {
    let Expr::Function(function) = value else {
        return false;
    };
    let [ObjectNamePart::Identifier(name)] = function.name.0.as_slice() else {
        return false;
    };
    let FunctionArguments::List(arguments) = &function.args else {
        return false;
    };
    let writes_words = ["CONCAT", "CONCAT_WS", "LPAD", "RPAD", "LEFT", "RIGHT"]
        .iter()
        .any(|call| name.value.eq_ignore_ascii_case(call));
    let into_words = render_context
        .text_columns
        .iter()
        .any(|column| column.eq_ignore_ascii_case(written));
    writes_words
        && into_words
        && arguments.args.iter().all(|argument| match argument {
            sqlparser::ast::FunctionArg::Unnamed(sqlparser::ast::FunctionArgExpr::Expr(expr)) => {
                !contains_decimal_operand(expr, render_context.decimal_columns)
                    || matches!(expr, Expr::Identifier(_) | Expr::CompoundIdentifier(_))
                        && decimal_operand_scale(expr, render_context.decimal_columns) == Some(0)
            }
            _ => false,
        })
}

/// Renders `COALESCE(col, n)` in an `UPDATE`'s value — Rails counts a column
/// up with `SET views = COALESCE(views, 0) + 1`.
///
/// Measured on MySQL 8.4.11: over a whole-number column the fallback stands
/// for a NULL and the column's own value for anything else, so a NULL counts
/// up to 1 and 5 to 6, and counting past the column's range is 1690, which
/// the engine refuses the way it refuses `n + 1` there. The fallback has to be
/// a written whole number, and the column one of whole numbers the engine
/// does not hold as an exact number — a `BIGINT UNSIGNED` is held in a form
/// of its own, which `coalesce` would hand on as it is; the kinds are known
/// only on the second reading the frontend asks for.
fn render_set_fallback(
    value: &Expr,
    refuse_if_assigned: &impl Fn(&str) -> bool,
    render_context: &mut SelectRenderContext<'_>,
) -> Result<String, ParseError> {
    let (column, fallback) = fallback_over_a_column(value).expect("the caller read a fallback");
    let (qualifier, name) = named_column(column).expect("a fallback is read over a column");
    if refuse_if_assigned(&name.value) {
        return unsupported("UPDATE assignment reading a column it has already assigned");
    }
    let Some(fallback) = direct_signed_integer(fallback) else {
        return unsupported(
            "COALESCE in a SET falling back on something other than a whole number",
        );
    };
    render_context.falls_back_in_a_set = true;
    if render_context.knows_the_integer_columns {
        let holds_whole_numbers = render_context
            .integer_columns
            .iter()
            .any(|known| known.eq_ignore_ascii_case(&name.value))
            && !render_context
                .decimal_columns
                .iter()
                .any(|(known, _)| known.eq_ignore_ascii_case(&name.value));
        if !holds_whole_numbers {
            return unsupported("COALESCE in a SET over a column that holds no whole number");
        }
    }
    let rendered_column = match qualifier {
        Some(qualifier) => format!("{}.{}", render_ident(qualifier), render_ident(name)),
        None => render_ident(name),
    };
    Ok(format!("coalesce({rendered_column}, {fallback})"))
}

/// One argument of a `CONCAT` an `UPDATE` writes into a column of words.
enum JoinedPart<'a> {
    Column(&'a Ident),
    Word(&'a Expr),
    /// `COALESCE(col, 'word')` or `IFNULL(col, 'word')`.
    ColumnOrWord(&'a Ident, &'a Expr),
}

/// Reads `CONCAT` over columns, written words and a column falling back on a
/// written word — sqlx's raw statement appends to a column that may hold
/// nothing with `SET body = CONCAT(COALESCE(body, ''), '!')`. Answers `None`
/// for anything else, and for a `CONCAT` with no fallback, which the
/// projection's own reading takes.
fn words_joined_with_fallbacks(function: &sqlparser::ast::Function) -> Option<Vec<JoinedPart<'_>>> {
    let [ObjectNamePart::Identifier(name)] = function.name.0.as_slice() else {
        return None;
    };
    if name.quote_style.is_some()
        || !name.value.eq_ignore_ascii_case("CONCAT")
        || function.over.is_some()
        || function.filter.is_some()
        || !function.within_group.is_empty()
    {
        return None;
    }
    let FunctionArguments::List(arguments) = &function.args else {
        return None;
    };
    if arguments.duplicate_treatment.is_some() || !arguments.clauses.is_empty() {
        return None;
    }
    let is_a_word = |expr: &Expr| {
        matches!(expr, Expr::Value(value)
            if matches!(value.value, Value::SingleQuotedString(_) | Value::DoubleQuotedString(_)))
    };
    let parts = arguments
        .args
        .iter()
        .map(|argument| {
            let sqlparser::ast::FunctionArg::Unnamed(sqlparser::ast::FunctionArgExpr::Expr(expr)) =
                argument
            else {
                return None;
            };
            match expr {
                Expr::Identifier(column) => Some(JoinedPart::Column(column)),
                _ if is_a_word(expr) => Some(JoinedPart::Word(expr)),
                _ => {
                    let (Expr::Identifier(column), fallback) = fallback_over_a_column(expr)? else {
                        return None;
                    };
                    is_a_word(fallback).then_some(JoinedPart::ColumnOrWord(column, fallback))
                }
            }
        })
        .collect::<Option<Vec<_>>>()?;
    parts
        .iter()
        .any(|part| matches!(part, JoinedPart::ColumnOrWord(..)))
        .then_some(parts)
}

/// Renders a `CONCAT` [`words_joined_with_fallbacks`] reads, into a column of
/// words and over columns of words only, which the frontend says on the
/// second reading it asks for.
///
/// Measured on MySQL 8.4.11: a NULL part makes the whole answer NULL, and a
/// fallback stands for its column's NULL, so `CONCAT(COALESCE(body, ''),
/// '!')` writes `!` over a NULL and `First post!` over `First post`. The
/// engine's `||` answers NULL for a NULL part too; its own `concat` would
/// skip it.
fn render_set_words_joined_with_fallbacks(
    function: &sqlparser::ast::Function,
    written: &str,
    assigned: &[String],
    render_context: &mut SelectRenderContext<'_>,
) -> Result<String, ParseError> {
    let parts = words_joined_with_fallbacks(function).expect("the caller read the parts");
    let named = |part: &JoinedPart<'_>| match part {
        JoinedPart::Column(column) | JoinedPart::ColumnOrWord(column, _) => {
            Some(column.value.clone())
        }
        JoinedPart::Word(_) => None,
    };
    let columns = parts.iter().filter_map(named).collect::<Vec<_>>();
    if columns.iter().any(|column| {
        assigned
            .iter()
            .any(|earlier| earlier.eq_ignore_ascii_case(column))
    }) {
        return unsupported("UPDATE assignment reading a column it has already assigned");
    }
    render_context.falls_back_in_a_set = true;
    if render_context.knows_the_integer_columns
        && (!render_context.is_text_column(written)
            || !columns
                .iter()
                .all(|column| render_context.is_text_column(column)))
    {
        return unsupported("CONCAT with a fallback in a SET over anything but columns of words");
    }
    let rendered = parts
        .iter()
        .map(|part| match part {
            JoinedPart::Column(column) => Ok(render_ident(column)),
            JoinedPart::Word(word) => render_dml_expr(word),
            JoinedPart::ColumnOrWord(column, word) => Ok(format!(
                "coalesce({}, {})",
                render_ident(column),
                render_dml_expr(word)?
            )),
        })
        .collect::<Result<Vec<_>, _>>()?;
    Ok(format!("({})", rendered.join(" || ")))
}

/// Renders one side of `+`, `-` or `*` in an `UPDATE`'s value, noting a `?`
/// there with the column the answer is written into: what MySQL does with the
/// value bound there depends on that column's type, which only the frontend
/// knows.
fn render_set_arithmetic_operand(
    operand: &Expr,
    written: &str,
    assigned: &[String],
    render_context: &mut SelectRenderContext<'_>,
) -> Result<String, ParseError> {
    let ordinal = render_context.parameter_count;
    let rendered = render_update_assignment_value(operand, written, assigned, render_context)?;
    if is_a_bare_placeholder(unwrapped(operand)) {
        render_context
            .bound_arithmetic_operands
            .push(crate::BoundArithmeticOperand {
                ordinal,
                written_column: written.to_owned(),
            });
    }
    Ok(rendered)
}

/// Reads an expression without the parentheses written around it.
fn unwrapped(expr: &Expr) -> &Expr {
    match expr {
        Expr::Nested(inner) => unwrapped(inner),
        _ => expr,
    }
}

pub(crate) fn is_a_bare_placeholder(expr: &Expr) -> bool {
    matches!(expr, Expr::Value(value) if matches!(&value.value, Value::Placeholder(marker) if marker == "?"))
}

/// Reports whether a value reads only columns this `SET` has not written yet.
///
/// MySQL takes the assignments left to right, so a later one reads what an
/// earlier one wrote; the engine reads the row as it stood. A value naming a
/// column already assigned would answer different things in the two, so it is
/// refused — and a shape this cannot read through is refused with it, rather
/// than let past unread.
fn reads_only_unassigned_columns(expr: &Expr, assigned: &[String]) -> bool {
    let unassigned = |name: &str| {
        !assigned
            .iter()
            .any(|earlier| earlier.eq_ignore_ascii_case(name))
    };
    let every = |parts: &[&Expr]| {
        parts
            .iter()
            .all(|part| reads_only_unassigned_columns(part, assigned))
    };
    match expr {
        Expr::Identifier(ident) => unassigned(&ident.value),
        Expr::CompoundIdentifier(parts) if parts.len() == 2 => unassigned(&parts[1].value),
        Expr::Value(_) | Expr::Interval(_) | Expr::TypedString { .. } => true,
        Expr::Nested(inner)
        | Expr::UnaryOp { expr: inner, .. }
        | Expr::IsNull(inner)
        | Expr::IsNotNull(inner)
        | Expr::Cast { expr: inner, .. }
        | Expr::Collate { expr: inner, .. } => reads_only_unassigned_columns(inner, assigned),
        Expr::BinaryOp { left, right, .. } => every(&[left, right]),
        Expr::Function(function) => {
            let sqlparser::ast::FunctionArguments::List(arguments) = &function.args else {
                return matches!(function.args, sqlparser::ast::FunctionArguments::None);
            };
            arguments.args.iter().all(|argument| {
                matches!(
                    argument,
                    sqlparser::ast::FunctionArg::Unnamed(sqlparser::ast::FunctionArgExpr::Wildcard)
                ) || matches!(
                    argument,
                    sqlparser::ast::FunctionArg::Unnamed(sqlparser::ast::FunctionArgExpr::Expr(
                        inner,
                    )) if reads_only_unassigned_columns(inner, assigned)
                )
            })
        }
        Expr::Case {
            operand,
            conditions,
            else_result,
            ..
        } => {
            operand
                .as_ref()
                .is_none_or(|operand| reads_only_unassigned_columns(operand, assigned))
                && conditions.iter().all(|arm| {
                    reads_only_unassigned_columns(&arm.condition, assigned)
                        && reads_only_unassigned_columns(&arm.result, assigned)
                })
                && else_result
                    .as_ref()
                    .is_none_or(|result| reads_only_unassigned_columns(result, assigned))
        }
        Expr::Substring {
            expr,
            substring_from,
            substring_for,
            ..
        } => {
            reads_only_unassigned_columns(expr, assigned)
                && substring_from
                    .as_ref()
                    .is_none_or(|from| reads_only_unassigned_columns(from, assigned))
                && substring_for
                    .as_ref()
                    .is_none_or(|count| reads_only_unassigned_columns(count, assigned))
        }
        Expr::Trim {
            expr, trim_what, ..
        } => {
            reads_only_unassigned_columns(expr, assigned)
                && trim_what
                    .as_ref()
                    .is_none_or(|what| reads_only_unassigned_columns(what, assigned))
        }
        Expr::Floor { expr, .. } | Expr::Ceil { expr, .. } => {
            reads_only_unassigned_columns(expr, assigned)
        }
        _ => false,
    }
}

/// Reports whether a value is a conditional this reads in a projection and has
/// not measured being written into a column: one falling back onto another
/// column, one comparing an operand, one with a word beside a column, or an
/// aggregate over one.
fn written_only_as_a_reading(value: &Expr) -> bool {
    if let Expr::Function(function) = value {
        if static_select_metadata::aggregated_branches(function).is_some() {
            return true;
        }
    }
    if matches!(
        value,
        Expr::Case {
            operand: Some(_),
            ..
        }
    ) {
        return true;
    }
    match static_select_metadata::classify_static_select_expr(value) {
        Some(StaticSelectMetadata::Branches {
            branches,
            falls_back,
            ..
        }) => {
            falls_back
                || branches
                    .iter()
                    .any(|branch| matches!(branch, static_select_metadata::Branch::Word { .. }))
        }
        _ => false,
    }
}

/// Renders one value an `INSERT` writes into a column, a row of `VALUES`, an
/// assignment of the `SET` form or of an upsert clause.
///
/// Beside what [`render_dml_expr`] renders, this takes the moment read to a
/// count of places of a second — `CURRENT_TIMESTAMP(6)`, which Rails writes
/// into every `created_at` and `updated_at` its `insert_all` and `upsert_all`
/// fill. The engine's clock reads to the millisecond, so a fourth place and
/// beyond are zeros: a reading MySQL's own clock could have taken, as the
/// same reading in a projection argues. What lands in the column is then
/// held to the column's own places the way a written moment is — measured on
/// 8.4.11, `NOW(6)` into a `DATETIME(2)` rounds to two places and into a
/// `DATETIME` to the whole second.
fn render_inserted_value(expr: &Expr) -> Result<String, ParseError> {
    if let Some(bytes) = crate::written_value::written_byte_string(expr) {
        return bytes.ok_or(ParseError::Unsupported {
            feature: "a string of bytes spelled some other way",
        });
    }
    match expr {
        Expr::Function(function) if moment_read_to_places(function).is_some() => {
            match moment_read_to_places(function).expect("the guard read the places") {
                0 => Ok(CheckedComparisonNow::Moment.engine_call().to_owned()),
                places => Ok(render_clock_to_a_fraction("%Y-%m-%d %H:%M:%f", 20, places)),
            }
        }
        _ => render_dml_expr(expr),
    }
}

/// The places of a second a reading of the moment written with a count of
/// them keeps — 6 for `NOW(6)`, 0 for `CURRENT_TIMESTAMP(0)` — or `None` for
/// any other call, the bare `NOW()` included.
fn moment_read_to_places(function: &sqlparser::ast::Function) -> Option<u32> {
    if !matches!(&function.args, FunctionArguments::List(arguments) if !arguments.args.is_empty()) {
        return None;
    }
    match static_select_metadata::scalar_call(function)? {
        StaticSelectMetadata::ScalarCall {
            function: static_select_metadata::ScalarFunction::Now,
            ..
        } => Some(0),
        StaticSelectMetadata::ScalarCall {
            function: static_select_metadata::ScalarFunction::NowToAFraction { places },
            ..
        } => Some(places),
        _ => None,
    }
}

/// Reads the value inside `CAST(value AS JSON)`, or nothing when the
/// expression is not one.
pub(crate) fn json_cast_operand(expr: &Expr) -> Option<&Expr> {
    let Expr::Cast {
        kind: sqlparser::ast::CastKind::Cast,
        expr,
        data_type: DataType::JSON,
        format: None,
        array: false,
    } = expr
    else {
        return None;
    };
    Some(expr)
}

/// Renders the value of `CAST(value AS JSON)` written whole into a column,
/// which is how GORM writes a `datatypes.JSON`.
///
/// Measured on MySQL 8.4.11: a bound word is parsed as a document and stored
/// the way MySQL stores one, a bound whole number as that JSON number and a
/// NULL as NULL, and a word that is no document fails with 3141. The dialect
/// reads a bound value that way when it binds, refusing what MySQL would
/// answer 3141 for; a written word that is no document is refused here. The
/// frontend holds the column written to being a `JSON` one.
pub(crate) fn render_json_cast(operand: &Expr) -> Result<String, ParseError> {
    let Expr::Value(value) = operand else {
        return unsupported("CAST AS JSON of something other than a word or a ?");
    };
    match &value.value {
        Value::Placeholder(marker) if marker == "?" => Ok("mysql_cast_as_json(?)".to_owned()),
        Value::SingleQuotedString(written) | Value::DoubleQuotedString(written) => {
            if crate::normalize_json(written).is_err() {
                return unsupported("CAST AS JSON of a word that is no document");
            }
            Ok(format!(
                "mysql_cast_as_json('{}')",
                written.replace('\'', "''")
            ))
        }
        Value::Null => Ok("NULL".to_owned()),
        _ => unsupported("CAST AS JSON of something other than a word or a ?"),
    }
}

fn render_dml_expr(expr: &Expr) -> Result<String, ParseError> {
    match expr {
        Expr::Identifier(ident) => Ok(render_ident(ident)),
        Expr::CompoundIdentifier(parts) if parts.len() == 2 => Ok(format!(
            "{}.{}",
            render_ident(&parts[0]),
            render_ident(&parts[1])
        )),
        Expr::Value(value) => match &value.value {
            Value::Number(value, false) => render_dml_number(value),
            Value::SingleQuotedString(value) | Value::DoubleQuotedString(value) => {
                Ok(format!("'{}'", value.replace('\'', "''")))
            }
            Value::Boolean(value) => Ok(if *value { "TRUE" } else { "FALSE" }.to_string()),
            Value::Null => Ok("NULL".to_string()),
            Value::Placeholder(marker) if marker == "?" => Ok("?".to_string()),
            _ => unsupported("DML literal"),
        },
        Expr::UnaryOp {
            op: UnaryOperator::Minus,
            expr,
        } if matches!(expr.as_ref(), Expr::Value(value) if matches!(&value.value, Value::Number(_, false))) =>
        {
            let Expr::Value(value) = expr.as_ref() else {
                unreachable!("guard requires a numeric literal");
            };
            let Value::Number(value, false) = &value.value else {
                unreachable!("guard requires a numeric literal");
            };
            let Ok(magnitude) = value.parse::<u64>() else {
                return Ok(format!("'-{value}'"));
            };
            if magnitude > (i64::MAX as u64) + 1 {
                return Ok(format!("'-{value}'"));
            }
            Ok(format!("(-{magnitude})"))
        }
        Expr::UnaryOp {
            op: UnaryOperator::Plus,
            expr,
        } => Ok(format!("(+{})", render_dml_expr(expr)?)),
        Expr::Nested(expr) => Ok(format!("({})", render_dml_expr(expr)?)),
        // A reading of the moment is written as the engine call answering the
        // same value in the same form. What lands in the column is then put
        // into the form that column holds, the way a written one is: measured
        // on MySQL 8.4.11, `NOW()` into a `DATE` stores the day and `CURDATE()`
        // into a `DATETIME` stores that day's midnight.
        Expr::Function(function) if CheckedComparisonNow::read(function).is_some() => {
            Ok(CheckedComparisonNow::read(function)
                .expect("the guard requires a call answering the moment")
                .engine_call()
                .to_owned())
        }
        // A shift of one of those readings is written the same way, so a row
        // can record a moment that is not this one.
        Expr::Function(function) if render_shifted_clock_reading(function).is_some() => {
            Ok(render_shifted_clock_reading(function)
                .expect("the guard requires a shift of a clock reading")
                .0)
        }
        Expr::BinaryOp { .. } if shifted_clock_reading_operator(expr).is_some() => {
            Ok(shifted_clock_reading_operator(expr)
                .expect("the guard requires a shift of a clock reading")
                .0)
        }
        _ => unsupported("DML expression"),
    }
}

/// Whether one written value is a reading of the clock that
/// [`render_inserted_value`] writes as the engine's own call — `NOW()`,
/// `CURDATE()`, `CURTIME()`, their other spellings, one of them shifted by an
/// interval, or the moment read to a count of places, `NOW(6)`.
pub(crate) fn is_clock_reading_value(expr: &Expr) -> bool {
    match expr {
        Expr::Function(function) => {
            CheckedComparisonNow::read(function).is_some()
                || render_shifted_clock_reading(function).is_some()
                || moment_read_to_places(function).is_some()
        }
        Expr::BinaryOp { .. } => shifted_clock_reading_operator(expr).is_some(),
        _ => false,
    }
}

/// Renders a numeric literal a DML statement may carry.
///
/// An integer is normalized through `i64` so that `007` reads back as `7`. A
/// number outside i64 stays as text until the destination column converts it.
/// A DECIMAL column must see every written digit before rounding to scale.
fn render_dml_number(value: &str) -> Result<String, ParseError> {
    if let Ok(integer) = value.parse::<i64>() {
        return Ok(integer.to_string());
    }
    if value.parse::<f64>().is_ok_and(f64::is_finite) {
        return Ok(format!("'{value}'"));
    }
    unsupported("DML numeric literal outside signed 64-bit integer range")
}

/// Renders the `WHERE` of an `UPDATE` or a `DELETE`.
///
/// A comparison goes through the same checked path a `SELECT` comparison does,
/// and is recorded in `render_context` so the frontend can hold it to the same
/// rule: the two engines only agree about a comparison on a signed integer
/// column, which is what that rule was measured for.
fn render_dml_predicate(
    expr: &Expr,
    render_context: &mut SelectRenderContext<'_>,
) -> Result<String, ParseError> {
    match expr {
        Expr::BinaryOp { left, op, right }
            if matches!(op, BinaryOperator::And | BinaryOperator::Or) =>
        {
            let op = if matches!(op, BinaryOperator::And) {
                "AND"
            } else {
                "OR"
            };
            Ok(format!(
                "({} {op} {})",
                render_dml_predicate(left, render_context)?,
                render_dml_predicate(right, render_context)?
            ))
        }
        // A statement built up in pieces starts its WHERE with a comparison
        // that names no column at all — `WHERE 1 = 1 AND ...` — so the pieces
        // after it can each be written with an AND in front. Measured on MySQL
        // 8.4.11, the engine answers each of these the same way.
        Expr::BinaryOp { left, op, right }
            if is_checked_select_comparison_operator(op)
                && names_a_whole_number(left)
                && names_a_whole_number(right) =>
        {
            Ok(format!(
                "({} {} {})",
                render_dml_expr(left)?,
                checked_select_comparison_sql_operator(op),
                render_dml_expr(right)?
            ))
        }
        Expr::BinaryOp { left, op, right } if is_checked_select_comparison_operator(op) => {
            render_checked_select_comparison(left, op, right, render_context)
        }
        Expr::Like {
            negated,
            any,
            expr,
            pattern,
            escape_char,
        } => render_checked_like(
            *negated,
            *any,
            expr,
            pattern,
            escape_char.as_ref(),
            render_context,
        ),
        // `name REGEXP 'a.c'` asks whether a pattern matches anywhere in the
        // column. The engine keeps its own matching in an extension this
        // frontend does not register, and MySQL holds the match to a
        // collation rather than to the pattern, so the dialect answers it.
        Expr::RLike {
            negated,
            expr,
            pattern,
            regexp: _,
        } => render_checked_regexp(*negated, expr, pattern, render_context),
        Expr::IsNull(inner) | Expr::IsNotNull(inner)
            if json_condition::reads_a_json_column(inner) =>
        {
            json_condition::render_json_null_test(
                inner,
                matches!(expr, Expr::IsNotNull(_)),
                render_context,
            )
        }
        expr @ Expr::Function(_) if json_condition::reads_a_json_column(expr) => {
            json_condition::render_json_test(expr, render_context)
        }
        Expr::IsNull(expr) => Ok(format!("({} IS NULL)", render_dml_expr(expr)?)),
        Expr::IsNotNull(expr) => Ok(format!("({} IS NOT NULL)", render_dml_expr(expr)?)),
        Expr::UnaryOp {
            op: UnaryOperator::Not,
            expr,
        } => Ok(format!(
            "(NOT {})",
            render_dml_predicate(expr, render_context)?
        )),
        Expr::Nested(expr) => Ok(format!("({})", render_dml_predicate(expr, render_context)?)),
        Expr::Value(value) if matches!(&value.value, Value::Boolean(_)) => render_dml_expr(expr),
        // `WHERE 1` and `WHERE 0` are the same idiom without the comparison.
        // Measured: a number that is not zero keeps every row and zero keeps
        // none, which is how the engine reads one too.
        expr if names_a_whole_number(expr) => render_dml_expr(expr),
        Expr::Between {
            expr,
            negated,
            low,
            high,
        } => render_checked_between(*negated, expr, low, high, render_context),
        Expr::InList {
            expr,
            list,
            negated,
        } => render_checked_in_list(expr, list, *negated, render_context),
        Expr::InSubquery {
            expr,
            subquery,
            negated,
        } => render_in_subquery(expr, subquery, *negated, render_context),
        Expr::Exists { subquery, negated } => {
            let (rendered, _) = render_subquery(subquery, render_context)?;
            Ok(format!(
                "({}EXISTS ({rendered}))",
                if *negated { "NOT " } else { "" }
            ))
        }
        _ => unsupported("DML WHERE predicate"),
    }
}

#[derive(Default)]
pub(crate) struct SelectRenderContext<'a> {
    /// Whether the session runs with `NO_BACKSLASH_ESCAPES`, which is what
    /// decides whether a `LIKE` pattern has a default escape at all.
    no_backslash_escapes: bool,
    /// The statement as the client wrote it. MySQL names an unaliased
    /// expression column after its source text, spacing included, so the
    /// rendered alias has to come from here rather than from the AST.
    source: &'a str,
    /// Every table a subquery reads, which the statement authorizes alongside
    /// its own.
    subquery_tables: Vec<MySqlSelectSource>,
    /// The columns the caller knows to be text, when it knows.
    ///
    /// Only the frontend can see a column's type, so a first parse renders
    /// without this and a second one, for the statements that need it, renders
    /// with it. `orders_a_bare_column` says which those are.
    text_columns: &'a [String],
    pub(crate) collation_sensitive_call_columns: Vec<String>,
    /// The columns a JSON reading in a condition reads, each of which the
    /// frontend holds to being a `JSON` column.
    pub(crate) json_reading_columns: Vec<String>,
    decimal_columns: &'a [(String, u32)],
    integer_columns: &'a [String],
    real_columns: &'a [String],
    json_columns: &'a [String],
    table_columns: &'a [String],
    /// The members of each `ENUM` column the caller knows of, in the order
    /// they were declared. MySQL orders an `ENUM` by that order rather than by
    /// the member text, so a statement ordering by one renders differently.
    member_columns: &'a [(String, Vec<String>)],
    /// The members of each `SET` column, whose numeric bit value sets its order.
    set_columns: &'a [(String, Vec<String>)],
    /// The columns the caller knows an `UPDATE` rewrites to the moment it runs
    /// at. The engine has no such attribute, so what it means is written into
    /// the statement here.
    rewritten_on_update: &'a [(String, u8)],
    /// The columns the caller knows hold a moment — a `DATETIME` or a
    /// `TIMESTAMP` — when it knows. MySQL reads a written day against one of
    /// these as that day's midnight, which changes what the comparison
    /// renders as; `compares_a_written_day` says when it matters.
    moment_columns: &'a [String],
    /// Whether the next projection rendered is the statement's own result
    /// rather than a subquery's or a `UNION` branch's.
    renders_the_outer_projection: bool,
    /// Whether the `FROM` being rendered is the statement's own rather than a
    /// subquery's, a derived table's or a `UNION` branch's.
    renders_the_statements_own_from: bool,
    /// Whether the derived table about to be rendered is one the statement
    /// only counts the rows of, which nothing reads a column of.
    counts_the_rows_of_the_derived_table: bool,
    /// Each column of a derived table the `SELECT` being rendered reads that
    /// answers a `COUNT`, with the name it reads the table under.
    derived_counts: Vec<(String, String)>,
    /// Whether the statement's own `SELECT` asked with `SQL_CALC_FOUND_ROWS`
    /// for the rows it would answer without its `LIMIT`.
    calculates_found_rows: bool,
    /// Whether a `GROUP_CONCAT` is ordered by a column holding neither words
    /// nor whole numbers, whose order has not been measured.
    group_concat_orders_by_another_kind: bool,
    orders_a_bare_column: bool,
    checks_type_sensitive_expression: bool,
    /// Whether a `CASE`, `IF`, `IFNULL` or `COALESCE` naming a column was
    /// written before the kinds of its columns were known. Its rendering
    /// depends on them, so a statement ending up that way is refused.
    renders_a_condition_without_column_types: bool,
    orders_wildcard_ordinal: bool,
    compares_a_placeholder: bool,
    counts_distinct_column: bool,
    /// Whether a `WHERE` tests a column on its own, which is read as a
    /// comparison against zero and so has to know whether the column is text.
    tests_a_bare_column: bool,
    /// Whether a comparison names a column against a written day, which reads
    /// differently depending on whether the column holds a day or a moment.
    compares_a_written_day: bool,
    /// Whether a comparison names a column against a word naming a number,
    /// which MySQL reads as that number against a column holding numbers.
    /// `number_a_written_word_names` says when it matters.
    pub(crate) compares_a_written_number: bool,
    compares_a_large_decimal_integer: bool,
    /// Whether an `AVG` is being rendered as MySQL's exact decimal rather than
    /// the engine's float, which a comparison against one asks for.
    averages_exactly: bool,
    pub(crate) checked_subquery_comparisons: Vec<CheckedSubqueryComparison>,
    pub(crate) checked_comparisons: Vec<CheckedSelectComparison>,
    pub(crate) ordered_columns: Vec<(Option<String>, String)>,
    /// Each `?` an `UPDATE` adds to, takes from or multiplies a value by,
    /// with the column the answer is written into.
    pub(crate) bound_arithmetic_operands: Vec<crate::BoundArithmeticOperand>,
    /// The columns an `UPDATE` writes a `CAST(... AS JSON)` into.
    pub(crate) json_cast_columns: Vec<String>,
    /// Whether an `UPDATE` reads a column through `COALESCE(col, n)`, which is
    /// only taken once the kinds of the table's columns are known.
    pub(crate) falls_back_in_a_set: bool,
    /// Whether the caller said which of the table's columns hold whole
    /// numbers, which a second reading of a DML statement does.
    knows_the_integer_columns: bool,
    parameter_count: usize,
    /// How many `GROUP_CONCAT` calls the statement has rendered, which tells
    /// each one's count of joined values apart from the others'.
    group_concat_calls: i64,
    /// Whether a projection item is being rendered, the one place a
    /// `GROUP_CONCAT` warns about a cut: a `HAVING` or an `ORDER BY` naming
    /// the same call is the same value in MySQL, not a second one to warn
    /// about.
    renders_a_projection_item: bool,
    /// Whether that item is one of the statement's own result columns and is
    /// itself an aggregate over a joined column, the one place one is taken:
    /// its shape is reported there, where the server checks the column's
    /// kind.
    takes_a_joined_aggregate: bool,
    /// Whether the subquery about to be rendered stands in a `WHERE`'s
    /// `EXISTS`, the one subquery that may join tables.
    an_exists_may_join: bool,
    /// Whether the statement is read knowing the kinds of every table's
    /// columns by name, which the frontend gives a statement over several
    /// tables only when each name is of one kind in all of them.
    knows_the_kinds_of_joined_columns: bool,
    /// Each column named with its table whose words a `GROUP_CONCAT` orders,
    /// which the frontend holds to the collation that order follows.
    collation_sensitive_joined_columns: Vec<(String, String)>,
    /// Whether the statement writes the rows it reads — `INSERT ... SELECT`
    /// — where a cut `GROUP_CONCAT` fails the statement rather than warning.
    writes_its_rows: bool,
    /// Whether the body being rendered has a `HAVING`, which the engine
    /// tests before it works out the projection. MySQL counts and warns
    /// about a group the `HAVING` then drops, so the `HAVING` is where each
    /// `GROUP_CONCAT` of the projection is counted.
    counts_group_concat_in_having: bool,
    /// The calls counting each projected `GROUP_CONCAT` that the body's
    /// `HAVING` is to test first.
    group_concat_counts: Vec<String>,
    /// Each `GROUP_CONCAT` the body being rendered projects, as it gathers
    /// its rows and names its separator.
    projected_group_concats: Vec<String>,
    /// The same for the last body rendered, which the statement's own
    /// `ORDER BY` reads once the body is done.
    last_projected_group_concats: Vec<String>,
    /// Whether a `GROUP_CONCAT` stands outside the projection with no
    /// projected one of its own. MySQL counts and warns about that one too,
    /// which nothing here does, so the statement is refused.
    names_an_unprojected_group_concat: bool,
    /// The columns the statement's own `GROUP BY` has to decide.
    columns_the_keys_decide: Option<MySqlColumnsTheKeysDecide>,
    /// Each name a subquery standing as a result column reads without a
    /// table, with the table the subquery reads.
    bare_names_in_result_subqueries: Vec<(MySqlTableName, String)>,
}

impl<'a> SelectRenderContext<'a> {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        source: &'a str,
        mode: SessionSqlMode,
        text_columns: &'a [String],
        table_columns: &'a [String],
        member_columns: &'a [(String, Vec<String>)],
        set_columns: &'a [(String, Vec<String>)],
        moment_columns: &'a [String],
        rewritten_on_update: &'a [(String, u8)],
    ) -> Self {
        Self {
            no_backslash_escapes: mode.no_backslash_escapes,
            source,
            text_columns,
            collation_sensitive_call_columns: Vec::new(),
            json_reading_columns: Vec::new(),
            decimal_columns: &[],
            integer_columns: &[],
            real_columns: &[],
            json_columns: &[],
            table_columns,
            member_columns,
            set_columns,
            moment_columns,
            rewritten_on_update,
            subquery_tables: Vec::new(),
            renders_the_outer_projection: false,
            renders_the_statements_own_from: false,
            counts_the_rows_of_the_derived_table: false,
            derived_counts: Vec::new(),
            calculates_found_rows: false,
            group_concat_orders_by_another_kind: false,
            orders_a_bare_column: false,
            checks_type_sensitive_expression: false,
            renders_a_condition_without_column_types: false,
            orders_wildcard_ordinal: false,
            compares_a_placeholder: false,
            counts_distinct_column: false,
            tests_a_bare_column: false,
            compares_a_written_day: false,
            compares_a_written_number: false,
            compares_a_large_decimal_integer: false,
            averages_exactly: false,
            checked_subquery_comparisons: Vec::new(),
            checked_comparisons: Vec::new(),
            ordered_columns: Vec::new(),
            bound_arithmetic_operands: Vec::new(),
            json_cast_columns: Vec::new(),
            falls_back_in_a_set: false,
            knows_the_integer_columns: false,
            parameter_count: 0,
            group_concat_calls: 0,
            renders_a_projection_item: false,
            takes_a_joined_aggregate: false,
            an_exists_may_join: false,
            knows_the_kinds_of_joined_columns: false,
            collation_sensitive_joined_columns: Vec::new(),
            writes_its_rows: false,
            counts_group_concat_in_having: false,
            group_concat_counts: Vec::new(),
            projected_group_concats: Vec::new(),
            last_projected_group_concats: Vec::new(),
            names_an_unprojected_group_concat: false,
            columns_the_keys_decide: None,
            bare_names_in_result_subqueries: Vec::new(),
        }
    }

    pub(crate) fn knowing_decimal_columns(mut self, decimal_columns: &'a [(String, u32)]) -> Self {
        self.decimal_columns = decimal_columns;
        self
    }

    /// Says which of a DML statement's table's columns hold whole numbers,
    /// having read every column.
    pub(crate) fn knowing_the_integer_columns(mut self, integer_columns: &'a [String]) -> Self {
        self.integer_columns = integer_columns;
        self.knows_the_integer_columns = true;
        self
    }

    fn is_text_column(&self, name: &str) -> bool {
        self.text_columns
            .iter()
            .any(|column| column.eq_ignore_ascii_case(name))
    }

    fn is_json_column(&self, name: &str) -> bool {
        self.json_columns
            .iter()
            .any(|column| column.eq_ignore_ascii_case(name))
    }

    fn is_moment_column(&self, name: &str) -> bool {
        self.moment_columns
            .iter()
            .any(|column| column.eq_ignore_ascii_case(name))
    }

    fn member_column(&self, name: &str) -> Option<&[String]> {
        self.member_columns
            .iter()
            .find(|(column, _)| column.eq_ignore_ascii_case(name))
            .map(|(_, members)| members.as_slice())
    }

    fn set_column(&self, name: &str) -> Option<&[String]> {
        self.set_columns
            .iter()
            .find(|(column, _)| column.eq_ignore_ascii_case(name))
            .map(|(_, members)| members.as_slice())
    }

    fn next_parameter_ordinal(&mut self) -> Result<usize, ParseError> {
        let ordinal = self.parameter_count;
        self.parameter_count =
            self.parameter_count
                .checked_add(1)
                .ok_or(ParseError::Unsupported {
                    feature: "SELECT parameter count outside usize range",
                })?;
        Ok(ordinal)
    }
}

fn render_select_item(
    item: &SelectItem,
    render_context: &mut SelectRenderContext<'_>,
) -> Result<String, ParseError> {
    match item {
        // The engine names a result column after the expression text, which
        // quotes an identifier. MySQL names it after the call as written, so an
        // unnamed count carries that name as an alias.
        SelectItem::UnnamedExpr(expr @ Expr::Function(function))
            if static_select_metadata::is_count_call(function)
                || static_select_metadata::column_aggregate_argument(function).is_some()
                || static_select_metadata::qualified_aggregate_argument(function).is_some() =>
        {
            let name = source_text(render_context.source, expr)
                .unwrap_or_else(|| mysql_aggregate_column_name(function))
                .replace('"', "\"\"");
            Ok(format!(
                "{} AS \"{name}\"",
                render_select_expr(expr, render_context)?,
            ))
        }
        // MySQL names an unaliased call after its source text, as it does an
        // expression, so the engine's own spelling has to be aliased away.
        SelectItem::UnnamedExpr(expr @ Expr::Function(function))
            if static_select_metadata::scalar_call(function).is_some()
                || static_select_metadata::classify_window_call(function).is_some()
                || static_select_metadata::aggregate_over_branches(function).is_some() =>
        {
            let name = source_text(render_context.source, expr)
                .ok_or(ParseError::Unsupported {
                    feature: "SELECT call whose source text cannot be recovered",
                })?
                .replace('"', "\"\"");
            Ok(format!(
                "{} AS \"{name}\"",
                render_select_expr(expr, render_context)?
            ))
        }
        // MySQL names an unaliased expression column after the source text, so
        // `1+1` keeps its spelling where the engine would print `1 + 1`.
        SelectItem::UnnamedExpr(expr @ Expr::Case { .. })
            if static_select_metadata::classify_static_select_expr(expr).is_some() =>
        {
            let name = source_text(render_context.source, expr)
                .ok_or(ParseError::Unsupported {
                    feature: "SELECT CASE whose source text cannot be recovered",
                })?
                .replace('"', "\"\"");
            Ok(format!(
                "{} AS \"{name}\"",
                render_select_expr(expr, render_context)?
            ))
        }
        SelectItem::UnnamedExpr(
            expr @ (Expr::Substring { .. }
            | Expr::Trim { .. }
            | Expr::Floor { .. }
            | Expr::Ceil { .. }
            | Expr::Extract { .. }
            | Expr::Cast { .. }
            | Expr::Convert { .. }
            | Expr::Position { .. }
            | Expr::Subquery(_)),
        ) if static_select_metadata::classify_static_select_expr(expr).is_some() => {
            let name = source_text(render_context.source, expr)
                .ok_or(ParseError::Unsupported {
                    feature: "SELECT scalar expression whose source text cannot be recovered",
                })?
                .replace('"', "\"\"");
            Ok(format!(
                "{} AS \"{name}\"",
                render_select_expr(expr, render_context)?
            ))
        }
        SelectItem::UnnamedExpr(
            expr @ Expr::BinaryOp {
                op: BinaryOperator::Arrow | BinaryOperator::LongArrow,
                ..
            },
        ) if static_select_metadata::classify_static_select_expr(expr).is_some() => {
            let name = source_text(render_context.source, expr)
                .ok_or(ParseError::Unsupported {
                    feature: "SELECT JSON reading whose source text cannot be recovered",
                })?
                .replace('"', "\"\"");
            Ok(format!(
                "{} AS \"{name}\"",
                render_select_expr(expr, render_context)?
            ))
        }
        // Measured: MySQL leaves a written plus sign out of a whole number's
        // name, `+1` being named `1`, where the engine names it `(+1)`.
        SelectItem::UnnamedExpr(
            expr @ Expr::UnaryOp {
                op: UnaryOperator::Plus,
                expr: signed,
            },
        ) if matches!(
            static_select_metadata::classify_static_select_expr(expr),
            Some(StaticSelectMetadata::Integer { .. })
        ) =>
        {
            let name = source_text(render_context.source, signed)
                .ok_or(ParseError::Unsupported {
                    feature: "SELECT whole number whose source text cannot be recovered",
                })?
                .replace('"', "\"\"");
            Ok(format!(
                "{} AS \"{name}\"",
                render_select_expr(expr, render_context)?
            ))
        }
        SelectItem::UnnamedExpr(expr) if names_an_interval_shift(expr) => {
            let name = source_text(render_context.source, expr)
                .ok_or(ParseError::Unsupported {
                    feature: "SELECT interval shift whose source text cannot be recovered",
                })?
                .replace('"', "\"\"");
            Ok(format!(
                "{} AS \"{name}\"",
                render_select_expr(expr, render_context)?
            ))
        }
        SelectItem::UnnamedExpr(expr)
            if matches!(
                static_select_metadata::classify_static_select_expr(expr),
                Some(StaticSelectMetadata::Arithmetic(_))
            ) || matches!(
                (
                    expr,
                    static_select_metadata::classify_static_select_expr(expr)
                ),
                (
                    Expr::UnaryOp { .. }
                        | Expr::BinaryOp { .. }
                        | Expr::IsTrue(_)
                        | Expr::IsFalse(_)
                        | Expr::IsNotTrue(_)
                        | Expr::IsNotFalse(_),
                    Some(StaticSelectMetadata::ScalarCall { .. })
                ) | (
                    // Measured: `-1` is named `-1` and `- 1` `- 1`, as
                    // written, where the engine names it `(-1)`.
                    Expr::UnaryOp {
                        op: UnaryOperator::Minus,
                        ..
                    },
                    Some(StaticSelectMetadata::Integer { .. })
                )
            ) =>
        {
            let name = source_text(render_context.source, expr)
                .ok_or(ParseError::Unsupported {
                    feature: "SELECT expression whose source text cannot be recovered",
                })?
                .replace('"', "\"\"");
            Ok(format!(
                "{} AS \"{name}\"",
                render_select_expr(expr, render_context)?
            ))
        }
        SelectItem::UnnamedExpr(expr) => render_select_expr(expr, render_context),
        SelectItem::ExprWithAlias { expr, alias } => Ok(format!(
            "{} AS {}",
            render_select_expr(expr, render_context)?,
            render_ident(alias)
        )),
        SelectItem::Wildcard(options) if wildcard_options_are_empty(options) => Ok("*".to_string()),
        SelectItem::Wildcard(_) => unsupported("SELECT wildcard option"),
        // `a.*` asks for one source's columns, which is how a joined statement
        // takes a whole row from one side of the join. The engine spells it
        // the same way, over the name the FROM clause gave the source.
        SelectItem::QualifiedWildcard(kind, options) if wildcard_options_are_empty(options) => {
            let sqlparser::ast::SelectItemQualifiedWildcardKind::ObjectName(name) = kind else {
                return unsupported("SELECT wildcard over an expression");
            };
            let [sqlparser::ast::ObjectNamePart::Identifier(source)] = name.0.as_slice() else {
                return unsupported("SELECT wildcard qualified by more than a source name");
            };
            Ok(format!("{}.*", render_ident(source)))
        }
        _ => unsupported("SELECT projection"),
    }
}

/// Reports whether an expression is `x + INTERVAL n unit` or `x - INTERVAL n
/// unit`, in parentheses or not, over something a shift takes.
fn names_an_interval_shift(expr: &Expr) -> bool {
    match expr {
        Expr::Nested(inner) => names_an_interval_shift(inner),
        _ => static_select_metadata::interval_shift_as_call(expr)
            .is_some_and(|call| static_select_metadata::scalar_call(&call).is_some()),
    }
}

/// Returns the name MySQL gives an unaliased aggregate column.
///
/// Measured on MySQL 8.4.11: the call as written, case included, and with the
/// argument unquoted — `COUNT(n)`, not `COUNT("n")`.
fn mysql_aggregate_column_name(function: &sqlparser::ast::Function) -> String {
    format!("{}({})", function.name, aggregate_argument_name(function))
}

/// Renders a checked aggregate call, keeping the spelling it was written with.
///
/// MySQL names the result column after the call as written, case included:
/// measured, `count(*)` keeps its lower case. The engine names it the same way
/// from this text, so nothing else has to carry the name.
fn render_aggregate_call(
    function: &sqlparser::ast::Function,
    render_context: &mut SelectRenderContext<'_>,
) -> String {
    // The engine's `json_group_array` builds the same array and answers an
    // empty one over no rows, where MySQL answers NULL; its document is
    // written again the way MySQL writes one.
    if matches!(
        static_select_metadata::column_aggregate_argument(function),
        Some((
            static_select_metadata::ColumnAggregateKind::CollectsIntoJson,
            _
        ))
    ) {
        return format!(
            "CASE WHEN count(*) = 0 THEN NULL ELSE mysql_json_document(json_group_array({})) END",
            render_aggregate_argument(function, render_context)
        );
    }
    if matches!(
        static_select_metadata::column_aggregate_argument(function),
        Some((static_select_metadata::ColumnAggregateKind::Concatenated, _))
    ) {
        return render_group_concat(function, render_context);
    }
    // The engine calls the sample standard deviation `stddev`, where MySQL
    // keeps that name for the population one. Every other aggregate here is
    // spelled the same in both.
    let name = if function
        .name
        .to_string()
        .eq_ignore_ascii_case("STDDEV_SAMP")
    {
        "stddev".to_owned()
    } else if let Some((kind, column)) = static_select_metadata::column_aggregate_argument(function)
    {
        if kind == static_select_metadata::ColumnAggregateKind::Avg
            && render_context.averages_exactly
        {
            "mysql_decimal_avg".to_owned()
        } else if render_context
            .decimal_columns
            .iter()
            .any(|(known, _)| known.eq_ignore_ascii_case(&column.value))
        {
            match kind {
                static_select_metadata::ColumnAggregateKind::Sum => "mysql_decimal_sum".to_owned(),
                static_select_metadata::ColumnAggregateKind::Avg => "mysql_decimal_avg".to_owned(),
                _ => function.name.to_string(),
            }
        } else {
            function.name.to_string()
        }
    } else {
        function.name.to_string()
    };
    format!(
        "{name}({})",
        render_aggregate_argument(function, render_context)
    )
}

/// Renders a `MIN`, `MAX`, `SUM` or `GROUP_CONCAT` over a column named with
/// its table.
///
/// Which kind of column it reads is the server's to check once it knows the
/// table, and only a signed whole number is answered by the first three: the
/// engine sums and compares those as MySQL does, where a `DECIMAL` or a
/// `BIGINT UNSIGNED` is kept in a stored form of its own and words are
/// compared under a collation. Until then the statement is marked as one
/// whose types matter.
fn render_qualified_aggregate(
    function: &sqlparser::ast::Function,
    render_context: &mut SelectRenderContext<'_>,
) -> String {
    let (kind, table, column) = static_select_metadata::qualified_aggregate_argument(function)
        .expect("a qualified aggregate was checked to name a table's column");
    if kind == static_select_metadata::ColumnAggregateKind::Concatenated {
        return render_group_concat(function, render_context);
    }
    render_context.checks_type_sensitive_expression = true;
    format!(
        "{}({}.{})",
        function.name,
        render_ident(table),
        render_ident(column)
    )
}

/// Renders `GROUP_CONCAT(col [SEPARATOR s])`.
///
/// MySQL cuts the result at the session's `group_concat_max_len` and warns
/// with the number of the value the cut fell in, which needs each group's
/// rows one by one. The engine's `group_concat` gathers them, each written
/// with its length so nothing a value holds reads as a separator, and
/// `mysql_group_concat` joins them the way MySQL does. A separator is a
/// comma unless one is written.
fn render_group_concat(
    function: &sqlparser::ast::Function,
    render_context: &mut SelectRenderContext<'_>,
) -> String {
    let column = render_aggregate_argument(function, render_context);
    let separator = static_select_metadata::group_concat_separator(function)
        .unwrap_or(",")
        .replace('\'', "''");
    render_context.group_concat_calls += 1;
    let call = render_context.group_concat_calls;
    let ordered = match static_select_metadata::group_concat_order(function) {
        Some((key, from_the_last)) => Some((None, key, from_the_last)),
        None => static_select_metadata::qualified_aggregate_argument(function)
            .zip(static_select_metadata::qualified_group_concat_order(
                function,
            ))
            .map(|((_, table, key), from_the_last)| (Some(table), key, from_the_last)),
    };
    let (order, key) = match ordered {
        Some((table, key, from_the_last)) => {
            let order = group_concat_order_kind(table, key, from_the_last, render_context);
            let key = match table {
                Some(table) => format!("{}.{}", render_ident(table), render_ident(key)),
                None => render_ident(key),
            };
            (
                order,
                format!(
                    "CASE WHEN {key} IS NULL THEN 'Z' \
                     ELSE 'K' || length(CAST(({key} || '') AS BLOB)) || ':' || {key} END || "
                ),
            )
        }
        None => (GROUP_CONCAT_AS_READ, String::new()),
    };
    let gathered = format!(
        "group_concat({key}CASE WHEN {column} IS NULL THEN 'N' \
         ELSE 'V' || length(CAST(({column} || '') AS BLOB)) || ':' || {column} END, '')"
    );

    let on_cut = if render_context.writes_its_rows {
        GROUP_CONCAT_CUT_FAILS
    } else {
        GROUP_CONCAT_CUT_WARNS
    };
    let named = format!("{gathered}, '{separator}', {order}");
    if !render_context.renders_a_projection_item {
        if !render_context.projected_group_concats.contains(&named)
            && !render_context.last_projected_group_concats.contains(&named)
        {
            render_context.names_an_unprojected_group_concat = true;
        }
        return format!("mysql_group_concat({named}, {call}, {GROUP_CONCAT_NAMED_AGAIN})");
    }
    render_context.projected_group_concats.push(named);
    if render_context.counts_group_concat_in_having {
        render_context.group_concat_counts.push(format!(
            "mysql_group_concat_count({gathered}, '{separator}', {order}, {call}, {on_cut})"
        ));
        return format!(
            "mysql_group_concat({gathered}, '{separator}', {order}, {call}, {GROUP_CONCAT_NAMED_AGAIN})"
        );
    }
    format!("mysql_group_concat({gathered}, '{separator}', {order}, {call}, {on_cut})")
}

/// Says how a `GROUP_CONCAT`'s parts are ordered, from the kind of column
/// they are ordered by: words under `utf8mb4_0900_ai_ci`, which the frontend
/// holds the column to, or whole numbers. Until the kinds are known the
/// statement is marked to be read again knowing them, and a column of any
/// other kind is refused once they are. A joined table's column, named with
/// its table, is read knowing the kinds of every table's columns, which the
/// frontend gives only when each name is of one kind in all of them.
fn group_concat_order_kind(
    table: Option<&Ident>,
    key: &Ident,
    from_the_last: bool,
    render_context: &mut SelectRenderContext<'_>,
) -> u8 {
    render_context.checks_type_sensitive_expression = true;
    let named = |columns: &[String]| {
        columns
            .iter()
            .any(|column| column.eq_ignore_ascii_case(&key.value))
    };
    let kinds_known = match table {
        Some(_) => render_context.knows_the_kinds_of_joined_columns,
        None => !render_context.table_columns.is_empty(),
    };
    let (first, last) = if !kinds_known {
        render_context.renders_a_condition_without_column_types = true;
        (GROUP_CONCAT_AS_READ, GROUP_CONCAT_AS_READ)
    } else if named(render_context.integer_columns) {
        (GROUP_CONCAT_BY_NUMBER, GROUP_CONCAT_BY_NUMBER_FROM_THE_LAST)
    } else if named(render_context.text_columns) {
        match table {
            Some(table) => render_context
                .collation_sensitive_joined_columns
                .push((table.value.clone(), key.value.clone())),
            None => render_context
                .collation_sensitive_call_columns
                .push(key.value.clone()),
        }
        (GROUP_CONCAT_BY_WORD, GROUP_CONCAT_BY_WORD_FROM_THE_LAST)
    } else {
        render_context.group_concat_orders_by_another_kind = true;
        (GROUP_CONCAT_AS_READ, GROUP_CONCAT_AS_READ)
    };
    if from_the_last {
        last
    } else {
        first
    }
}

/// How `mysql_group_concat` orders the parts it joins: as the engine read
/// them, or by the words or the whole numbers each was gathered with, from
/// the first or from the last.
const GROUP_CONCAT_AS_READ: u8 = 0;
const GROUP_CONCAT_BY_WORD: u8 = 1;
const GROUP_CONCAT_BY_WORD_FROM_THE_LAST: u8 = 2;
const GROUP_CONCAT_BY_NUMBER: u8 = 3;
const GROUP_CONCAT_BY_NUMBER_FROM_THE_LAST: u8 = 4;

/// What `mysql_group_concat` does about a cut: nothing more for a call named
/// again in a `HAVING` or an `ORDER BY`, which MySQL does not count or warn
/// about twice; warn; or fail a statement that writes the result.
const GROUP_CONCAT_NAMED_AGAIN: u8 = 0;
const GROUP_CONCAT_CUT_WARNS: u8 = 1;
const GROUP_CONCAT_CUT_FAILS: u8 = 2;

fn aggregate_argument_name(function: &sqlparser::ast::Function) -> String {
    let sqlparser::ast::FunctionArguments::List(arguments) = &function.args else {
        unreachable!("a checked aggregate was checked to have an argument list")
    };
    let prefix = match arguments.duplicate_treatment {
        Some(sqlparser::ast::DuplicateTreatment::Distinct) => "DISTINCT ",
        _ => "",
    };
    match arguments.args.as_slice() {
        [sqlparser::ast::FunctionArg::Unnamed(sqlparser::ast::FunctionArgExpr::Wildcard)] => {
            format!("{prefix}*")
        }
        [argument @ sqlparser::ast::FunctionArg::Unnamed(sqlparser::ast::FunctionArgExpr::Expr(
            Expr::Value(value),
        ))] if static_select_metadata::counts_every_row(argument) => {
            format!("{prefix}{}", value.value)
        }
        [sqlparser::ast::FunctionArg::Unnamed(sqlparser::ast::FunctionArgExpr::Expr(
            Expr::Identifier(column),
        ))] => format!("{prefix}{}", column.value),
        [sqlparser::ast::FunctionArg::Unnamed(sqlparser::ast::FunctionArgExpr::Expr(
            Expr::CompoundIdentifier(parts),
        ))] if parts.len() == 2 => {
            format!("{prefix}{}.{}", parts[0].value, parts[1].value)
        }
        _ => unreachable!("a checked aggregate was checked to take one wildcard or column"),
    }
}

fn render_aggregate_argument(
    function: &sqlparser::ast::Function,
    render_context: &mut SelectRenderContext<'_>,
) -> String {
    let sqlparser::ast::FunctionArguments::List(arguments) = &function.args else {
        unreachable!("a checked aggregate was checked to have an argument list")
    };
    let is_distinct = matches!(
        arguments.duplicate_treatment,
        Some(sqlparser::ast::DuplicateTreatment::Distinct)
    );
    let prefix = if is_distinct { "DISTINCT " } else { "" };
    match arguments.args.as_slice() {
        [sqlparser::ast::FunctionArg::Unnamed(sqlparser::ast::FunctionArgExpr::Wildcard)] => {
            format!("{prefix}*")
        }
        [argument] if static_select_metadata::counts_every_row(argument) => "*".to_owned(),
        [sqlparser::ast::FunctionArg::Unnamed(sqlparser::ast::FunctionArgExpr::Expr(
            Expr::Identifier(column),
        ))] => {
            if is_distinct {
                render_context.counts_distinct_column = true;
            }
            format!("{prefix}{}", render_ident(column))
        }
        // Only a count reaches here qualified, and a count does not depend on
        // what the column holds, so there is no collation to ask for.
        [sqlparser::ast::FunctionArg::Unnamed(sqlparser::ast::FunctionArgExpr::Expr(
            Expr::CompoundIdentifier(parts),
        ))] if parts.len() == 2 => {
            if is_distinct {
                render_context.counts_distinct_column = true;
            }
            format!(
                "{prefix}{}.{}",
                render_ident(&parts[0]),
                render_ident(&parts[1])
            )
        }
        _ => unreachable!("a checked aggregate was checked to take one wildcard or column"),
    }
}

fn wildcard_options_are_empty(options: &sqlparser::ast::WildcardAdditionalOptions) -> bool {
    options.opt_ilike.is_none()
        && options.opt_exclude.is_none()
        && options.opt_except.is_none()
        && options.opt_replace.is_none()
        && options.opt_rename.is_none()
        && options.opt_alias.is_none()
}

fn render_select_table(
    table: &TableFactor,
    render_context: Option<&mut SelectRenderContext<'_>>,
) -> Result<(String, MySqlSelectSource), ParseError> {
    // A whole statement in the `FROM` reads its table under the alias it was
    // given, which is how its result columns find their metadata — the same
    // way a CTE's do, and for the same reason: a result column reaching the
    // frontend names the alias and an ordinal, and the only way to answer what
    // type it has is to read that ordinal from the table the statement reads.
    if let TableFactor::Derived {
        lateral,
        subquery,
        alias,
        sample,
    } = table
    {
        if sample.is_some() {
            return unsupported("derived table option");
        }
        return render_derived_table(*lateral, subquery, alias.as_ref(), render_context);
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
    } = table
    else {
        return unsupported("SELECT table source");
    };
    if args.is_some()
        || !with_hints.is_empty()
        || version.is_some()
        || *with_ordinality
        || !partitions.is_empty()
        || json_path.is_some()
        || sample.is_some()
    {
        return unsupported("SELECT table option");
    }
    // An index hint says which key to plan with and nothing about which rows
    // come back, so it is dropped. What it does say is that the key exists:
    // measured on MySQL 8.4.11, one naming a key the table has not got answers
    // 1176, so the names travel with the source for the frontend to check.
    let hinted_indexes = hinted_index_names(index_hints)?;
    // `information_schema.TABLES` is the one qualified source this takes. The
    // engine scans it under a name of its own, which has no qualifier.
    let catalog = match name.0.as_slice() {
        [ObjectNamePart::Identifier(database), ObjectNamePart::Identifier(table)] => Some(
            MySqlCatalogTable::named(&database.value, &table.value).ok_or(
                ParseError::Unsupported {
                    feature: "qualified SELECT table source",
                },
            )?,
        ),
        _ => None,
    };
    let (table, mut reference, mut rendered) = match catalog {
        Some(catalog) => (
            MySqlTableName::parse(catalog.engine_name())?,
            catalog.engine_name().to_owned(),
            render_ident_str(catalog.engine_name()),
        ),
        None => {
            let [ObjectNamePart::Identifier(ident)] = name.0.as_slice() else {
                return unsupported("qualified SELECT table source");
            };
            (
                MySqlTableName::parse(&ident.value)?,
                ident.value.clone(),
                render_unqualified_name(name)?,
            )
        }
    };
    if let Some(alias) = alias {
        if !alias.columns.is_empty() || alias.at.is_some() {
            return unsupported("SELECT table alias option");
        }
        reference.clone_from(&alias.name.value);
        rendered.push_str(" AS ");
        rendered.push_str(&render_ident(&alias.name));
    }
    Ok((
        rendered,
        MySqlSelectSource {
            reference,
            table,
            outer: false,
            branch: 0,
            subquery: false,
            read_by_a_result_subquery: false,
            projected_columns: Vec::new(),
            derived: None,
            catalog,
            hinted_indexes,
        },
    ))
}

/// Reads the keys an index hint names, refusing a shape whose effect on the
/// answer has not been measured.
///
/// The hint itself is dropped: measured on MySQL 8.4.11, `USE`, `FORCE` and
/// `IGNORE` each answer the rows the statement answers without one, on either
/// side of a join and under an alias, so a hint says which key to plan with
/// and nothing about which rows come back. What it does say is that the key
/// exists, and the names come back for the frontend to check that.
fn hinted_index_names(
    index_hints: &[sqlparser::ast::TableIndexHints],
) -> Result<Vec<String>, ParseError> {
    let mut named = Vec::new();
    for hint in index_hints {
        if !matches!(hint.index_type, sqlparser::ast::TableIndexType::Index)
            && !matches!(hint.index_type, sqlparser::ast::TableIndexType::Key)
        {
            return unsupported("SELECT index hint kind");
        }
        for index in &hint.index_names {
            named.push(index.value.clone());
        }
    }
    Ok(named)
}

fn render_select_expr(
    expr: &Expr,
    render_context: &mut SelectRenderContext<'_>,
) -> Result<String, ParseError> {
    match expr {
        Expr::Identifier(ident) => Ok(render_ident(ident)),
        Expr::CompoundIdentifier(parts) if parts.len() == 2 => Ok(format!(
            "{}.{}",
            render_ident(&parts[0]),
            render_ident(&parts[1])
        )),
        Expr::Value(value) => match &value.value {
            Value::Number(value, false) => value
                .parse::<i64>()
                .map(|value| value.to_string())
                .map_err(|_| ParseError::Unsupported {
                    feature: "SELECT numeric literal outside signed 64-bit integer range",
                }),
            Value::SingleQuotedString(value) | Value::DoubleQuotedString(value) => {
                Ok(format!("'{}'", value.replace('\'', "''")))
            }
            Value::Boolean(value) => Ok(if *value { "TRUE" } else { "FALSE" }.to_string()),
            Value::Null => Ok("NULL".to_string()),
            Value::Placeholder(marker) if marker == "?" => {
                render_context.next_parameter_ordinal()?;
                Ok("?".to_string())
            }
            _ => unsupported("SELECT literal"),
        },
        // The engine counts rows and non-null values the way MySQL does, so a
        // COUNT crosses without changing what it means.
        Expr::Function(function)
            if static_select_metadata::is_count_call(function)
                || static_select_metadata::column_aggregate_argument(function).is_some() =>
        {
            Ok(render_aggregate_call(function, render_context))
        }
        Expr::Function(function)
            if render_context.takes_a_joined_aggregate
                && static_select_metadata::qualified_aggregate_argument(function).is_some() =>
        {
            Ok(render_qualified_aggregate(function, render_context))
        }
        Expr::Function(function)
            if static_select_metadata::aggregate_over_branches(function).is_some() =>
        {
            render_aggregate_over_branches(function, render_context)
        }
        // MySQL writes a column out, reads a whole number out of it, or reads
        // the day or the moment out of it. Each is spelled here as what the
        // engine answers the same value with. Which targets those are is the
        // classifier's to say; a target it does not take is refused here.
        Expr::Cast {
            kind,
            expr,
            data_type,
            format,
            array,
        } => {
            if contains_decimal_operand(expr, render_context.decimal_columns) {
                return unsupported("SELECT CAST over DECIMAL requires exact numeric handling");
            }
            let Some(target) = static_select_metadata::checked_cast_target(
                kind,
                expr,
                data_type,
                format.as_ref(),
                *array,
            ) else {
                return unsupported("SELECT CAST target");
            };
            let column = render_select_expr(expr, render_context)?;
            Ok(render_cast_target(&column, target))
        }
        // `EXTRACT(<field> FROM col)` reads the same part of a moment the call
        // spelling of that part reads, and the engine reads it the same way.
        Expr::Extract {
            field,
            syntax,
            expr: extracted,
        } => {
            if contains_decimal_operand(extracted, render_context.decimal_columns) {
                return unsupported("SELECT EXTRACT over DECIMAL requires exact numeric handling");
            }
            if !matches!(syntax, sqlparser::ast::ExtractSyntax::From)
                || static_select_metadata::classify_static_select_expr(expr).is_none()
            {
                return unsupported("SELECT EXTRACT field");
            }
            let Some(strftime) = extract_strftime_field(field) else {
                return unsupported("SELECT EXTRACT field");
            };
            Ok(format!(
                "CAST(strftime('{strftime}', {}) AS INTEGER)",
                render_select_expr(extracted, render_context)?
            ))
        }
        // `CONVERT(col, <type>)` means what `CAST(col AS <type>)` means, so it
        // is written out the same way.
        Expr::Convert { .. } => {
            if contains_decimal_operand(expr, render_context.decimal_columns) {
                return unsupported("SELECT CONVERT over DECIMAL requires exact numeric handling");
            }
            let Some((inner, target)) = static_select_metadata::checked_convert_target(expr) else {
                return unsupported("SELECT CONVERT target");
            };
            let column = render_select_expr(inner, render_context)?;
            Ok(render_cast_target(&column, target))
        }
        Expr::IsNull(expr) => Ok(format!(
            "({} IS NULL)",
            render_select_expr(expr, render_context)?
        )),
        Expr::IsNotNull(expr) => Ok(format!(
            "({} IS NOT NULL)",
            render_select_expr(expr, render_context)?
        )),
        Expr::UnaryOp {
            op: UnaryOperator::Minus,
            expr,
        } if matches!(expr.as_ref(), Expr::Value(value) if matches!(&value.value, Value::Number(_, false))) =>
        {
            let Expr::Value(value) = expr.as_ref() else {
                unreachable!("guard requires a numeric literal");
            };
            let Value::Number(value, false) = &value.value else {
                unreachable!("guard requires a numeric literal");
            };
            let magnitude = value.parse::<u64>().map_err(|_| ParseError::Unsupported {
                feature: "SELECT numeric literal outside signed 64-bit integer range",
            })?;
            if magnitude > (i64::MAX as u64) + 1 {
                return unsupported("SELECT numeric literal outside signed 64-bit integer range");
            }
            Ok(format!("(-{magnitude})"))
        }
        Expr::UnaryOp {
            op: UnaryOperator::Plus,
            expr,
        } if matches!(expr.as_ref(), Expr::Value(value) if matches!(&value.value, Value::Number(_, false))) => {
            Ok(format!("(+{})", render_select_expr(expr, render_context)?))
        }
        // `-col` over a whole number. The column's type is the frontend's to
        // check, which refuses the one kind whose smallest value has no
        // negative.
        Expr::UnaryOp {
            op: UnaryOperator::Minus,
            expr: negated,
        } if matches!(
            static_select_metadata::classify_static_select_expr(expr),
            Some(StaticSelectMetadata::ScalarCall {
                function: static_select_metadata::ScalarFunction::Negates,
                ..
            })
        ) =>
        {
            if contains_decimal_operand(negated, render_context.decimal_columns) {
                return unsupported("SELECT negation over DECIMAL requires exact numeric handling");
            }
            render_context.checks_type_sensitive_expression = true;
            Ok(format!(
                "(-{})",
                render_select_expr(negated, render_context)?
            ))
        }
        Expr::Nested(expr) => Ok(format!("({})", render_select_expr(expr, render_context)?)),
        // `CASE JSON_EXTRACT(...) WHEN 'null'` asks whether the reading found
        // the JSON null, which the text the reading answers says: the JSON
        // string `"null"` is written with its quotes.
        Expr::Case {
            operand: Some(operand),
            ..
        } if static_select_metadata::json_member_unless_null(expr).is_some() => {
            let Some(StaticSelectMetadata::ScalarCall { function, .. }) =
                static_select_metadata::json_member_unless_null(expr)
            else {
                unreachable!("the guard read a member of a JSON column");
            };
            let reading = render_select_expr(operand, render_context)?;
            Ok(format!(
                "(CASE WHEN {reading} = 'null' THEN NULL ELSE {} END)",
                read_json_member_as(function, &reading)
            ))
        }
        Expr::Case {
            operand,
            conditions,
            else_result,
            ..
        } if static_select_metadata::classify_static_select_expr(expr).is_some() => {
            let answer = conditional_answer(expr, render_context);
            let mut arms = Vec::with_capacity(conditions.len());
            for when in conditions {
                // `CASE col WHEN v` is `CASE WHEN col = v`, and is written that
                // way so the comparison is checked and collated as one in a
                // `WHERE` is.
                let condition = match operand {
                    Some(operand) => render_select_predicate(
                        &Expr::BinaryOp {
                            left: operand.clone(),
                            op: BinaryOperator::Eq,
                            right: Box::new(when.condition.clone()),
                        },
                        render_context,
                    )?,
                    None => render_select_predicate(&when.condition, render_context)?,
                };
                arms.push((condition, &when.result));
            }
            render_picked_branches(arms, else_result.as_deref(), answer, render_context)
        }
        // The engine's `substr` reads a place of 0, or one before the start,
        // as the start, where MySQL answers nothing: measured on 8.4.11,
        // `SUBSTR('apple', 0, 3)` and `SUBSTR('apple', -6)` are both empty.
        Expr::Substring {
            expr: target,
            substring_from: Some(substring_from),
            substring_for,
            ..
        } if static_select_metadata::classify_static_select_expr(expr).is_some() => {
            if contains_decimal_operand(expr, render_context.decimal_columns) {
                return unsupported("SELECT SUBSTRING over DECIMAL requires text conversion");
            }
            let target = render_select_expr(target, render_context)?;
            let from = render_select_expr(substring_from, render_context)?;
            match substring_for {
                Some(count) => Ok(format!(
                    "mysql_substring({target}, {from}, {})",
                    render_select_expr(count, render_context)?
                )),
                None => Ok(format!("mysql_substring({target}, {from})")),
            }
        }
        // The engine's three names are what MySQL's one name with a side is.
        // What to trim is one character, which is the only width where
        // removing whole copies and removing any of the characters agree.
        Expr::Trim {
            trim_where,
            trim_what,
            expr: target,
            ..
        } if static_select_metadata::classify_static_select_expr(expr).is_some() => {
            if contains_decimal_operand(expr, render_context.decimal_columns) {
                return unsupported("SELECT TRIM over DECIMAL requires text conversion");
            }
            use sqlparser::ast::TrimWhereField;
            let name = match trim_where {
                Some(TrimWhereField::Leading) => "ltrim",
                Some(TrimWhereField::Trailing) => "rtrim",
                Some(TrimWhereField::Both) | None => "trim",
            };
            let target = render_select_expr(target, render_context)?;
            match trim_what {
                Some(trim_what) => {
                    let removed = render_select_expr(trim_what, render_context)?;
                    Ok(format!("{name}({target}, {removed})"))
                }
                None => Ok(format!("{name}({target})")),
            }
        }
        Expr::Position { expr: needle, r#in }
            if static_select_metadata::classify_static_select_expr(expr).is_some() =>
        {
            let Expr::Identifier(column) = r#in.as_ref() else {
                unreachable!("a checked POSITION was checked to look in a column");
            };
            render_context
                .collation_sensitive_call_columns
                .push(column.value.clone());
            render_context.checks_type_sensitive_expression = true;
            Ok(format!(
                "mysql_locate({}, {})",
                render_select_expr(needle, render_context)?,
                render_ident(column)
            ))
        }
        Expr::Floor { expr: inner, field }
            if static_select_metadata::classify_static_select_expr(expr).is_some() =>
        {
            let sqlparser::ast::CeilFloorKind::DateTimeField(
                sqlparser::ast::DateTimeField::NoDateTime,
            ) = field
            else {
                return unsupported("FLOOR option");
            };
            if decimal_operand_scale(inner, render_context.decimal_columns).is_some() {
                let inner = render_select_expr(inner, render_context)?;
                return Ok(render_decimal_to_whole(&inner, false));
            }
            // The engine's `floor` keeps a whole number whole and a real number
            // real, which is the kind MySQL answers for each.
            let inner = render_select_expr(inner, render_context)?;
            Ok(format!("floor({inner})"))
        }
        Expr::Ceil { expr: inner, field }
            if static_select_metadata::classify_static_select_expr(expr).is_some() =>
        {
            let sqlparser::ast::CeilFloorKind::DateTimeField(
                sqlparser::ast::DateTimeField::NoDateTime,
            ) = field
            else {
                return unsupported("CEIL option");
            };
            if decimal_operand_scale(inner, render_context.decimal_columns).is_some() {
                let inner = render_select_expr(inner, render_context)?;
                return Ok(render_decimal_to_whole(&inner, true));
            }
            let inner = render_select_expr(inner, render_context)?;
            Ok(format!("ceil({inner})"))
        }
        // MySQL's own spelling of a JSON reading, read as `JSON_EXTRACT` is.
        Expr::BinaryOp {
            left,
            op: op @ (BinaryOperator::Arrow | BinaryOperator::LongArrow),
            right,
        } => {
            if contains_decimal_operand(left, render_context.decimal_columns)
                || contains_decimal_operand(right, render_context.decimal_columns)
            {
                return unsupported("SELECT JSON path over DECIMAL requires text conversion");
            }
            // Only a plain path is one the engine reads as MySQL does, and only
            // over one is the reading's column known: measured on 8.4.11,
            // `doc->'$.a.*'` answers `[1]` over `{"a": {"b": 1}}` where the
            // engine answers nothing, and either answered a column of no type.
            if static_select_metadata::classify_static_select_expr(expr).is_none() {
                return unsupported("SELECT JSON reading over a path this does not read");
            }
            let left = render_select_expr(left, render_context)?;
            let right = render_select_expr(right, render_context)?;
            if matches!(op, BinaryOperator::LongArrow) {
                return Ok(format!(
                    "mysql_json_unquote(mysql_json_extract({left}, {right}))"
                ));
            }
            Ok(format!("mysql_json_extract({left}, {right})"))
        }
        // `created_at + INTERVAL 1 DAY` is the operator spelling of a
        // `DATE_ADD`, and is written out as one.
        Expr::BinaryOp { .. }
            if static_select_metadata::interval_shift_as_call(expr)
                .is_some_and(|call| static_select_metadata::scalar_call(&call).is_some()) =>
        {
            let call = static_select_metadata::interval_shift_as_call(expr)
                .expect("the guard read the operator as a shift");
            render_scalar_call(&call, render_context)
        }
        // `NOT col` and `col IS TRUE` and its three kin over a column of
        // numbers, which the frontend checks it is. The engine reads a number
        // as true where it is not zero, as MySQL does.
        Expr::UnaryOp {
            op: UnaryOperator::Not,
            expr: negated,
        } if static_select_metadata::classify_static_select_expr(expr).is_some() => {
            render_context.checks_type_sensitive_expression = true;
            Ok(format!(
                "(NOT {})",
                render_select_expr(negated, render_context)?
            ))
        }
        Expr::IsTrue(tested)
        | Expr::IsFalse(tested)
        | Expr::IsNotTrue(tested)
        | Expr::IsNotFalse(tested)
            if static_select_metadata::classify_static_select_expr(expr).is_some() =>
        {
            render_context.checks_type_sensitive_expression = true;
            let tested = render_select_expr(tested, render_context)?;
            Ok(render_truth_test(expr, &tested))
        }
        // A comparison standing as a result column is written the way a
        // `WHERE` writes it, so a column is held to its type and a word is
        // compared under the column's collation. MySQL answers it as 1, 0 or
        // NULL, and so does the engine.
        Expr::BinaryOp { left, op, right }
            if matches!(
                static_select_metadata::classify_static_select_expr(expr),
                Some(StaticSelectMetadata::ScalarCall {
                    function: static_select_metadata::ScalarFunction::Compares
                        | static_select_metadata::ScalarFunction::ComparesACallOrASubquery,
                    ..
                })
            ) =>
        {
            // A subquery's count against a written number reads no column of
            // the statement's own, so no type has to be known for it.
            if !matches!(left.as_ref(), Expr::Subquery(_))
                && !matches!(right.as_ref(), Expr::Subquery(_))
            {
                render_context.checks_type_sensitive_expression = true;
            }
            let counted = |expr: &Expr| matches!(expr, Expr::Function(function) if static_select_metadata::is_count_call(function));
            let whole = |expr: &Expr| direct_signed_integer(expr).is_some();
            if counted(left) || counted(right) || (whole(left) && whole(right)) {
                return Ok(format!(
                    "({} {} {})",
                    render_select_expr(left, render_context)?,
                    checked_select_comparison_sql_operator(op),
                    render_select_expr(right, render_context)?
                ));
            }
            render_checked_select_comparison(left, op, right, render_context)
        }
        // `col % n` and `col DIV n` over a whole number, which the engine's
        // `%` and `/` answer the same way: both keep the dividend's sign and
        // cut toward zero, and both answer NULL for a zero divisor.
        Expr::BinaryOp {
            left,
            op: op @ (BinaryOperator::Modulo | BinaryOperator::MyIntegerDivide),
            right,
        } if static_select_metadata::classify_static_select_expr(expr).is_some() => {
            if contains_decimal_operand(left, render_context.decimal_columns) {
                return unsupported(
                    "SELECT whole division over DECIMAL requires exact numeric handling",
                );
            }
            render_context.checks_type_sensitive_expression = true;
            let operator = if matches!(op, BinaryOperator::Modulo) {
                "%"
            } else {
                "/"
            };
            Ok(format!(
                "({} {operator} {})",
                render_select_expr(left, render_context)?,
                render_select_expr(right, render_context)?
            ))
        }
        Expr::BinaryOp { left, op, right }
            if static_select_metadata::classify_arithmetic(expr).is_some() =>
        {
            if !arithmetic_names_column(expr)
                && [left.as_ref(), right.as_ref()].iter().any(|operand| {
                    decimal_numeric_literal(operand).is_some_and(|(_, scale)| scale > 0)
                })
            {
                return unsupported("SELECT fractional literal arithmetic");
            }
            let left_scale = decimal_operand_scale(left, render_context.decimal_columns);
            let right_scale = decimal_operand_scale(right, render_context.decimal_columns);
            if left_scale.is_some() || right_scale.is_some() {
                let other = if left_scale.is_some() && right_scale.is_none() {
                    Some(right.as_ref())
                } else if right_scale.is_some() && left_scale.is_none() {
                    Some(left.as_ref())
                } else {
                    None
                };
                if other.is_some_and(|operand| {
                    known_typed_numeric_operand(operand, render_context.real_columns)
                }) {
                    return unsupported("DECIMAL arithmetic with an approximate column");
                }
                let literal = other.and_then(decimal_numeric_literal);
                if let Some(operand) = other {
                    if literal.is_none()
                        && !known_typed_numeric_operand(operand, render_context.integer_columns)
                        && !matches!(operand, Expr::Function(function) if static_select_metadata::is_count_call(function))
                    {
                        return unsupported("DECIMAL arithmetic operands");
                    }
                }
                if matches!(op, BinaryOperator::Divide) {
                    let Some((written, _)) = literal else {
                        return unsupported("DECIMAL division operand");
                    };
                    if left_scale.is_none()
                        || !written
                            .bytes()
                            .any(|byte| byte.is_ascii_digit() && byte != b'0')
                    {
                        return unsupported("DECIMAL division operand");
                    }
                    let decimal = render_select_expr(left, render_context)?;
                    return Ok(format!(
                        "mysql_decimal_div_round({decimal}, '{written}', {})",
                        left_scale
                            .expect("division requires decimal left operand")
                            .saturating_add(4)
                            .min(30)
                    ));
                }
                let name = match op {
                    BinaryOperator::Plus => "numeric_add",
                    BinaryOperator::Minus => "numeric_sub",
                    BinaryOperator::Multiply => "numeric_mul",
                    _ => return unsupported("DECIMAL arithmetic operator"),
                };
                let unsigned_integer_arithmetic = [left.as_ref(), right.as_ref()]
                    .iter()
                    .any(|operand| {
                        direct_typed_integer_operand(operand, render_context.integer_columns)
                            && decimal_operand_scale(operand, render_context.decimal_columns)
                                .is_some()
                    })
                    && [left.as_ref(), right.as_ref()].iter().all(|operand| {
                        direct_typed_integer_operand(operand, render_context.integer_columns)
                            || decimal_numeric_literal(operand)
                                .is_some_and(|(_, scale)| scale == 0)
                            || matches!(operand, Expr::Function(function) if static_select_metadata::is_count_call(function))
                    });
                let left = if let Some((written, _)) = decimal_numeric_literal(left) {
                    format!("'{written}'")
                } else {
                    render_select_expr(left, render_context)?
                };
                let right = if let Some((written, _)) = decimal_numeric_literal(right) {
                    format!("'{written}'")
                } else {
                    render_select_expr(right, render_context)?
                };
                let result = format!("{name}({left}, {right})");
                if unsigned_integer_arithmetic {
                    return Ok(format!("mysql_uint64_result({result})"));
                }
                if matches!(op, BinaryOperator::Multiply)
                    && left_scale
                        .unwrap_or_else(|| literal.as_ref().map_or(0, |(_, scale)| *scale))
                        .saturating_add(
                            right_scale
                                .unwrap_or_else(|| literal.as_ref().map_or(0, |(_, scale)| *scale)),
                        )
                        > 30
                {
                    return Ok(format!("mysql_decimal_round({result}, 30)"));
                }
                return Ok(result);
            }
            if contains_decimal_operand(left, render_context.decimal_columns)
                || contains_decimal_operand(right, render_context.decimal_columns)
            {
                return unsupported("DECIMAL arithmetic form");
            }
            let left = render_arithmetic_operand(left, render_context)?;
            let right = render_arithmetic_operand(right, render_context)?;
            // MySQL's `/` is decimal division and the engine's is integer
            // division, so `3/2` would answer 1 rather than 1.5 without this.
            if matches!(op, BinaryOperator::Divide) {
                return Ok(format!("(CAST({left} AS REAL) / {right})"));
            }
            Ok(format!(
                "({left} {} {right})",
                checked_arithmetic_sql_operator(op)
            ))
        }
        // A scalar subquery goes through the same reader a subquery in a
        // `WHERE` does, so the table it reads is named and authorized like any
        // other, and its own rules are the ones a bare `SELECT` is held to.
        Expr::Subquery(query)
            if static_select_metadata::classify_static_select_expr(expr).is_some() =>
        {
            let (rendered, _) = render_subquery(query, render_context)?;
            Ok(format!("({rendered})"))
        }
        // Laravel asks whether a table is there with `SELECT EXISTS (SELECT 1
        // FROM information_schema.tables ...)`, which reads the subquery the
        // way a `WHERE EXISTS` does.
        Expr::Exists { subquery, negated } => {
            let (rendered, _) = render_subquery(subquery, render_context)?;
            Ok(format!(
                "({}EXISTS ({rendered}))",
                if *negated { "NOT " } else { "" }
            ))
        }
        Expr::Function(function) if static_select_metadata::scalar_call(function).is_some() => {
            render_scalar_call(function, render_context)
        }
        Expr::Function(function)
            if defaulted_derived_count(function).is_some_and(|(table, column)| {
                render_context
                    .derived_counts
                    .iter()
                    .any(|(known_table, known_column)| {
                        known_table.eq_ignore_ascii_case(&table.value)
                            && known_column.eq_ignore_ascii_case(&column.value)
                    })
            }) =>
        {
            let (table, column) =
                defaulted_derived_count(function).expect("the guard read the call");
            let fallback = direct_signed_integer(defaulted_fallback(function))
                .expect("the call falls back on a whole number");
            Ok(format!(
                "coalesce({}.{}, {fallback})",
                render_ident(table),
                render_ident(column)
            ))
        }
        Expr::Function(function)
            if static_select_metadata::classify_window_call(function).is_some() =>
        {
            render_window_call(function, render_context)
        }
        Expr::Function(function)
            if matches!(function.name.0.as_slice(), [ObjectNamePart::Identifier(name)] if name.value.eq_ignore_ascii_case("LAST_INSERT_ID"))
                && !function.uses_odbc_syntax
                && matches!(&function.parameters, FunctionArguments::None)
                && matches!(&function.args, FunctionArguments::List(arguments) if arguments.args.is_empty() && arguments.duplicate_treatment.is_none() && arguments.clauses.is_empty())
                && function.filter.is_none()
                && function.null_treatment.is_none()
                && function.over.is_none()
                && function.within_group.is_empty() =>
        {
            Ok("last_insert_id()".to_string())
        }
        _ => unsupported("SELECT expression"),
    }
}

/// Renders what SQLAlchemy reads a JSON member as, over the rendered reading
/// of the member.
pub(crate) fn read_json_member_as(
    function: static_select_metadata::ScalarFunction,
    reading: &str,
) -> String {
    match function {
        static_select_metadata::ScalarFunction::ReadsJsonText => {
            format!("mysql_json_unquote({reading})")
        }
        static_select_metadata::ScalarFunction::ReadsJsonAsWholeNumber => {
            format!("mysql_json_as_whole_number({reading})")
        }
        static_select_metadata::ScalarFunction::ReadsJsonAsDouble => {
            format!("mysql_json_as_double({reading})")
        }
        _ => unreachable!("a JSON member is read as text, a whole number or a double"),
    }
}

/// Writes `FLOOR` or `CEIL` of a `DECIMAL` as the whole number it lands on.
///
/// The engine's own `floor` reads a `DECIMAL` as a double and would lose its
/// digits, so the fraction is cut off exactly and the whole number moved one
/// down, or one up, where a fraction was cut. The frontend holds the column to
/// the widths MySQL answers a whole number for.
fn render_decimal_to_whole(decimal: &str, up: bool) -> String {
    let whole = format!("mysql_decimal_truncate({decimal}, 0)");
    let (past, step) = if up {
        (format!("numeric_lt({whole}, {decimal})"), "numeric_add")
    } else {
        (format!("numeric_lt({decimal}, {whole})"), "numeric_sub")
    };
    format!("CAST(CASE WHEN {past} THEN {step}({whole}, '1') ELSE {whole} END AS INTEGER)")
}

/// Writes `IS TRUE`, `IS FALSE`, `IS NOT TRUE` or `IS NOT FALSE` over what
/// has already been written for the thing tested.
///
/// Measured on MySQL 8.4.11: each answers 1 or 0 and never NULL, a NULL being
/// neither true nor false, so `NULL IS NOT TRUE` is 1.
fn render_truth_test(test: &Expr, tested: &str) -> String {
    match test {
        Expr::IsTrue(_) => format!("COALESCE(({tested}) <> 0, 0)"),
        Expr::IsFalse(_) => format!("COALESCE(({tested}) = 0, 0)"),
        Expr::IsNotTrue(_) => format!("COALESCE(({tested}) = 0, 1)"),
        Expr::IsNotFalse(_) => format!("COALESCE(({tested}) <> 0, 1)"),
        _ => unreachable!("a truth test was checked to be one of the four"),
    }
}

fn decimal_operand_scale(expr: &Expr, columns: &[(String, u32)]) -> Option<u32> {
    let name = match expr {
        Expr::Identifier(name) => name,
        Expr::CompoundIdentifier(parts) if parts.len() == 2 => &parts[1],
        Expr::Function(function) => {
            let (kind, column) = static_select_metadata::column_aggregate_argument(function)?;
            let scale = columns
                .iter()
                .find(|(name, _)| name.eq_ignore_ascii_case(&column.value))?
                .1;
            return match kind {
                static_select_metadata::ColumnAggregateKind::MinMax
                | static_select_metadata::ColumnAggregateKind::Sum => Some(scale),
                static_select_metadata::ColumnAggregateKind::Avg => Some((scale + 4).min(30)),
                _ => None,
            };
        }
        Expr::Nested(inner) => return decimal_operand_scale(inner, columns),
        _ => return None,
    };
    columns
        .iter()
        .find(|(column, _)| column.eq_ignore_ascii_case(&name.value))
        .map(|(_, scale)| *scale)
}

fn known_typed_numeric_operand(expr: &Expr, columns: &[String]) -> bool {
    let name = match expr {
        Expr::Identifier(name) => name,
        Expr::CompoundIdentifier(parts) if parts.len() == 2 => &parts[1],
        Expr::Function(function) => {
            let Some((_, column)) = static_select_metadata::column_aggregate_argument(function)
            else {
                return false;
            };
            column
        }
        Expr::Nested(inner) => return known_typed_numeric_operand(inner, columns),
        _ => return false,
    };
    columns
        .iter()
        .any(|column| column.eq_ignore_ascii_case(&name.value))
}

fn direct_typed_integer_operand(expr: &Expr, columns: &[String]) -> bool {
    let name = match expr {
        Expr::Identifier(name) => name,
        Expr::CompoundIdentifier(parts) if parts.len() == 2 => &parts[1],
        Expr::Nested(inner) => return direct_typed_integer_operand(inner, columns),
        _ => return false,
    };
    columns
        .iter()
        .any(|column| column.eq_ignore_ascii_case(&name.value))
}

fn contains_decimal_operand(expr: &Expr, columns: &[(String, u32)]) -> bool {
    if decimal_operand_scale(expr, columns).is_some() {
        return true;
    }
    match expr {
        Expr::BinaryOp { left, right, .. } => {
            contains_decimal_operand(left, columns) || contains_decimal_operand(right, columns)
        }
        Expr::Nested(inner)
        | Expr::UnaryOp { expr: inner, .. }
        | Expr::Cast { expr: inner, .. }
        | Expr::Convert { expr: inner, .. }
        | Expr::Collate { expr: inner, .. }
        | Expr::IsNull(inner)
        | Expr::IsNotNull(inner)
        | Expr::Floor { expr: inner, .. }
        | Expr::Ceil { expr: inner, .. } => contains_decimal_operand(inner, columns),
        Expr::Function(function) => match &function.args {
            FunctionArguments::List(arguments) => arguments.args.iter().any(|argument| {
                matches!(argument,
                    sqlparser::ast::FunctionArg::Unnamed(sqlparser::ast::FunctionArgExpr::Expr(inner))
                        if contains_decimal_operand(inner, columns))
            }),
            _ => false,
        },
        Expr::Case {
            operand,
            conditions,
            else_result,
            ..
        } => {
            operand
                .as_ref()
                .is_some_and(|operand| contains_decimal_operand(operand, columns))
                || conditions.iter().any(|arm| {
                    contains_decimal_operand(&arm.condition, columns)
                        || contains_decimal_operand(&arm.result, columns)
                })
                || else_result
                    .as_ref()
                    .is_some_and(|result| contains_decimal_operand(result, columns))
        }
        Expr::Substring {
            expr,
            substring_from,
            substring_for,
            ..
        } => {
            contains_decimal_operand(expr, columns)
                || substring_from
                    .as_ref()
                    .is_some_and(|from| contains_decimal_operand(from, columns))
                || substring_for
                    .as_ref()
                    .is_some_and(|count| contains_decimal_operand(count, columns))
        }
        Expr::Trim {
            expr, trim_what, ..
        } => {
            contains_decimal_operand(expr, columns)
                || trim_what
                    .as_ref()
                    .is_some_and(|what| contains_decimal_operand(what, columns))
        }
        _ => false,
    }
}

fn decimal_numeric_literal(expr: &Expr) -> Option<(String, u32)> {
    let (sign, literal) = match expr {
        Expr::UnaryOp {
            op: UnaryOperator::Minus,
            expr,
        } => ("-", expr.as_ref()),
        Expr::UnaryOp {
            op: UnaryOperator::Plus,
            expr,
        } => ("", expr.as_ref()),
        _ => ("", expr),
    };
    let Expr::Value(value) = literal else {
        return None;
    };
    let Value::Number(written, false) = &value.value else {
        return None;
    };
    let (whole, scale) = if let Some((whole, fraction)) = written.split_once('.') {
        if fraction.is_empty() || !fraction.bytes().all(|byte| byte.is_ascii_digit()) {
            return None;
        }
        (whole, u32::try_from(fraction.len()).ok()?)
    } else {
        (written.as_str(), 0)
    };
    if whole.is_empty() || !whole.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    Some((format!("{sign}{written}"), scale))
}

fn render_arithmetic_operand(
    expr: &Expr,
    render_context: &mut SelectRenderContext<'_>,
) -> Result<String, ParseError> {
    if let Some((written, _)) = decimal_numeric_literal(expr) {
        return Ok(written);
    }
    render_select_expr(expr, render_context)
}

fn arithmetic_names_column(expr: &Expr) -> bool {
    match expr {
        Expr::Identifier(_) | Expr::CompoundIdentifier(_) => true,
        Expr::Function(function) => {
            static_select_metadata::column_aggregate_argument(function).is_some()
        }
        Expr::BinaryOp { left, right, .. } => {
            arithmetic_names_column(left) || arithmetic_names_column(right)
        }
        Expr::Nested(inner) => arithmetic_names_column(inner),
        _ => false,
    }
}

/// Renders `ROW_NUMBER()`, `RANK()` or `DENSE_RANK()` over its window.
///
/// Both engines spell the three the same way, so only the window is rewritten:
/// a text column is partitioned and ordered under the default UCA 9 collation
/// MySQL's default gives it, the same treatment an outer `ORDER BY` gets.
fn render_window_call(
    function: &sqlparser::ast::Function,
    render_context: &mut SelectRenderContext<'_>,
) -> Result<String, ParseError> {
    let [ObjectNamePart::Identifier(name)] = function.name.0.as_slice() else {
        unreachable!("a checked window call was checked to have one name");
    };
    let Some(over) = function.over.as_ref() else {
        unreachable!("a checked window call was checked to have a window");
    };
    let Some(spec) = static_select_metadata::checked_window_spec(
        over,
        static_select_metadata::window_answers_over_the_whole_set(&name.value),
    ) else {
        unreachable!("a checked window call was checked to have a checked window");
    };
    let FunctionArguments::List(arguments) = &function.args else {
        unreachable!("a checked window call was checked to have an argument list");
    };
    if arguments.args.iter().any(|argument| {
        matches!(argument,
            sqlparser::ast::FunctionArg::Unnamed(sqlparser::ast::FunctionArgExpr::Expr(expr))
                if contains_decimal_operand(expr, render_context.decimal_columns))
    }) {
        return unsupported("SELECT window function over DECIMAL requires exact numeric handling");
    }
    // `NTILE` carries a count and `LAG` and `LEAD` a column, an offset and a
    // default; the rest carry nothing, and each spelling is the engine's own.
    let arguments = arguments
        .args
        .iter()
        .map(|argument| match argument {
            sqlparser::ast::FunctionArg::Unnamed(sqlparser::ast::FunctionArgExpr::Expr(expr)) => {
                render_select_expr(expr, render_context)
            }
            sqlparser::ast::FunctionArg::Unnamed(sqlparser::ast::FunctionArgExpr::Wildcard) => {
                Ok("*".to_owned())
            }
            _ => unreachable!("a checked window call was checked to have plain arguments"),
        })
        .collect::<Result<Vec<_>, _>>()?
        .join(", ");
    let mut window = String::new();
    if !spec.partition_by.is_empty() {
        window.push_str("PARTITION BY ");
        let terms = spec
            .partition_by
            .iter()
            .map(|expr| render_window_column(expr, render_context))
            .collect::<Result<Vec<_>, _>>()?;
        window.push_str(&terms.join(", "));
    }
    if !spec.order_by.is_empty() {
        if !window.is_empty() {
            window.push(' ');
        }
        window.push_str("ORDER BY ");
        let terms = spec
            .order_by
            .iter()
            .map(|term| {
                let direction = if term.options.asc == Some(false) {
                    "DESC"
                } else {
                    "ASC"
                };
                Ok(format!(
                    "{} {direction}",
                    render_window_column(&term.expr, render_context)?
                ))
            })
            .collect::<Result<Vec<_>, _>>()?;
        window.push_str(&terms.join(", "));
    }
    if let Some(frame) = spec.window_frame.as_ref() {
        if !window.is_empty() {
            window.push(' ');
        }
        // The shorthand `ROWS <bound>` means `BETWEEN <bound> AND CURRENT ROW`,
        // so it is written out — measured on MySQL 8.4.11, the two answer the
        // same rows, and so do they in the engine.
        window.push_str(&format!(
            "{} BETWEEN {} AND {}",
            frame.units,
            frame.start_bound,
            frame
                .end_bound
                .as_ref()
                .map_or_else(|| "CURRENT ROW".to_owned(), ToString::to_string)
        ));
    }
    Ok(format!(
        "{}({arguments}) OVER ({window})",
        name.value.to_ascii_lowercase()
    ))
}

/// Renders the position a member holds among the ones its column declares.
///
/// Measured on MySQL 8.4.11: an `ENUM` orders by that position rather than by
/// the member text, so `small, medium, large` come back in the order they were
/// declared in, and the empty error member sorts in front of all of them where
/// a NULL sorts in front of it.
fn member_position(column: &str, members: &[String]) -> String {
    let mut rendered = format!("CASE {column} WHEN '' THEN 0");
    for (index, member) in members.iter().enumerate() {
        rendered.push_str(&format!(" WHEN '{member}' THEN {}", index + 1));
    }
    rendered.push_str(" END");
    rendered
}

/// A SET's numeric value has one bit per declared member. Comparing those
/// bits from highest to lowest also works for all 64 members without relying
/// on signed or floating-point arithmetic in the engine.
fn set_member_order(
    column: &str,
    members: &[String],
    direction: &str,
) -> Result<String, ParseError> {
    if members.is_empty()
        || members.len() > 64
        || members
            .iter()
            .any(|member| member.is_empty() || member.contains(','))
    {
        return unsupported("SELECT ORDER BY SET members");
    }
    Ok(members
        .iter()
        .rev()
        .map(|member| {
            let member = member.replace('\'', "''");
            format!(
                "CASE WHEN {column} IS NULL THEN NULL ELSE instr(',' || {column} || ',', ',{member},') > 0 END {direction}"
            )
        })
        .collect::<Vec<_>>()
        .join(", "))
}

/// Renders one column a window partitions or orders by.
fn render_window_column(
    expr: &Expr,
    render_context: &mut SelectRenderContext<'_>,
) -> Result<String, ParseError> {
    let Expr::Identifier(column) = expr else {
        unreachable!("a checked window was checked to name plain columns");
    };
    render_context.orders_a_bare_column = true;
    Ok(render_ident(column))
}

/// Renders a checked scalar call as the engine's own spelling of it.
///
/// MySQL's `LENGTH` counts bytes and its `CHAR_LENGTH` counts characters, which
/// the engine spells `octet_length` and `length`; the rest carry over by name.
/// `NOW()` reads the clock in UTC, which is the zone this server runs in.
fn render_scalar_call(
    function: &sqlparser::ast::Function,
    render_context: &mut SelectRenderContext<'_>,
) -> Result<String, ParseError> {
    let [ObjectNamePart::Identifier(name)] = function.name.0.as_slice() else {
        unreachable!("a checked scalar call was checked to have one name");
    };
    if matches!(
        static_select_metadata::scalar_call(function),
        Some(StaticSelectMetadata::ScalarCall { columns, .. }) if !columns.is_empty()
    ) {
        render_context.checks_type_sensitive_expression = true;
    }
    if let Some(branches @ StaticSelectMetadata::Branches { .. }) =
        static_select_metadata::scalar_call(function)
    {
        return render_branches_call(name, function, &branches, render_context);
    }
    let decimal_argument_scales = match &function.args {
        FunctionArguments::List(arguments) => arguments
            .args
            .iter()
            .filter_map(|argument| match argument {
                sqlparser::ast::FunctionArg::Unnamed(sqlparser::ast::FunctionArgExpr::Expr(
                    expr,
                )) => decimal_operand_scale(expr, render_context.decimal_columns),
                _ => None,
            })
            .collect::<Vec<_>>(),
        _ => Vec::new(),
    };
    let has_decimal_argument = !decimal_argument_scales.is_empty();
    // The engine reads a `BIGINT UNSIGNED` and a `DECIMAL` with no places out
    // as the digits MySQL writes it with, so a call writing its argument out
    // as text answers what MySQL answers over one.
    let writes_whole_decimals_out = decimal_argument_scales.iter().all(|scale| *scale == 0)
        && ["CONCAT", "CONCAT_WS", "LPAD", "RPAD", "LEFT", "RIGHT"]
            .iter()
            .any(|call| name.value.eq_ignore_ascii_case(call));
    if has_decimal_argument
        && !writes_whole_decimals_out
        && ![
            "ABS",
            "TRUNCATE",
            "ROUND",
            "FORMAT",
            "CEILING",
            "JSON_ARRAY",
            "JSON_OBJECT",
            "JSON_SET",
            "JSON_INSERT",
            "JSON_REPLACE",
        ]
        .iter()
        .any(|call| name.value.eq_ignore_ascii_case(call))
    {
        return unsupported("SELECT function over DECIMAL requires exact numeric handling");
    }
    let engine = if name.value.eq_ignore_ascii_case("LENGTH")
        || name.value.eq_ignore_ascii_case("OCTET_LENGTH")
    {
        "octet_length"
    } else if name.value.eq_ignore_ascii_case("CHAR_LENGTH")
        || name.value.eq_ignore_ascii_case("CHARACTER_LENGTH")
    {
        "length"
    } else if let Some(StaticSelectMetadata::ScalarCall {
        function: static_select_metadata::ScalarFunction::NowToAFraction { places },
        ..
    }) = static_select_metadata::scalar_call(function)
    {
        return Ok(render_clock_to_a_fraction("%Y-%m-%d %H:%M:%f", 20, places));
    } else if let Some(StaticSelectMetadata::ScalarCall {
        function: static_select_metadata::ScalarFunction::TimeOfDayToAFraction { places },
        ..
    }) = static_select_metadata::scalar_call(function)
    {
        return Ok(render_clock_to_a_fraction("%H:%M:%f", 9, places));
    } else if name.value.eq_ignore_ascii_case("NOW")
        || name.value.eq_ignore_ascii_case("CURRENT_TIMESTAMP")
        || name.value.eq_ignore_ascii_case("UTC_TIMESTAMP")
        || name.value.eq_ignore_ascii_case("SYSDATE")
        || name.value.eq_ignore_ascii_case("LOCALTIME")
        || name.value.eq_ignore_ascii_case("LOCALTIMESTAMP")
    {
        return Ok("datetime('now')".to_owned());
    } else if name.value.eq_ignore_ascii_case("CURDATE")
        || name.value.eq_ignore_ascii_case("CURRENT_DATE")
        || name.value.eq_ignore_ascii_case("UTC_DATE")
    {
        // The engine writes `date('now')` as `YYYY-MM-DD`, which is the form
        // MySQL answers and the form a DATE column holds.
        return Ok("date('now')".to_owned());
    } else if name.value.eq_ignore_ascii_case("DATE") {
        // The engine reads the day out with the same call `CAST(col AS DATE)`
        // is written as, because the two are one thing.
        return Ok(format!("date({})", single_column_argument(function)));
    } else if name.value.eq_ignore_ascii_case("CURTIME")
        || name.value.eq_ignore_ascii_case("CURRENT_TIME")
        || name.value.eq_ignore_ascii_case("UTC_TIME")
    {
        return Ok("time('now')".to_owned());
    } else if name.value.eq_ignore_ascii_case("DAYNAME")
        || name.value.eq_ignore_ascii_case("MONTHNAME")
    {
        // `DATE_FORMAT` writes the same two names with `%W` and `%M`.
        let specifier = if name.value.eq_ignore_ascii_case("DAYNAME") {
            "%W"
        } else {
            "%M"
        };
        return Ok(format!(
            "mysql_date_format({}, '{specifier}')",
            moment_argument(function, render_context)?
        ));
    } else if name.value.eq_ignore_ascii_case("WEEK") {
        // Measured on MySQL 8.4.11: a `WEEK` with no mode counts by mode 0,
        // the default `default_week_format` names.
        let sqlparser::ast::FunctionArguments::List(arguments) = &function.args else {
            unreachable!("a checked scalar call was checked to have an argument list");
        };
        let mode = match arguments.args.get(1) {
            Some(sqlparser::ast::FunctionArg::Unnamed(sqlparser::ast::FunctionArgExpr::Expr(
                mode,
            ))) => static_select_metadata::week_mode(mode)
                .expect("a checked WEEK was checked to name a mode from 0 through 7"),
            _ => 0,
        };
        return Ok(format!(
            "mysql_week({}, {mode})",
            moment_argument(function, render_context)?
        ));
    } else if name.value.eq_ignore_ascii_case("TIME_TO_SEC")
        || name.value.eq_ignore_ascii_case("SEC_TO_TIME")
    {
        let sqlparser::ast::FunctionArguments::List(arguments) = &function.args else {
            unreachable!("a checked scalar call was checked to have an argument list");
        };
        // A written time is read the way a TIME column stores it, which is
        // the form the dialect reads.
        let read = match arguments.args.as_slice() {
            [sqlparser::ast::FunctionArg::Unnamed(sqlparser::ast::FunctionArgExpr::Expr(
                Expr::Value(value),
            ))] => match &value.value {
                Value::SingleQuotedString(word) | Value::DoubleQuotedString(word) => {
                    let stored = crate::normalize_time(word)
                        .expect("a checked TIME_TO_SEC was checked to read a time");
                    format!("'{stored}'")
                }
                _ => scalar_argument(function, 0)?,
            },
            _ => scalar_argument(function, 0)?,
        };
        return Ok(format!("mysql_{}({read})", name.value.to_ascii_lowercase()));
    } else if name.value.eq_ignore_ascii_case("INET_ATON")
        || name.value.eq_ignore_ascii_case("INET_NTOA")
        || name.value.eq_ignore_ascii_case("IS_IPV4")
    {
        return Ok(format!(
            "mysql_{}({})",
            name.value.to_ascii_lowercase(),
            scalar_argument(function, 0)?
        ));
    } else if name.value.eq_ignore_ascii_case("ASCII")
        || name.value.eq_ignore_ascii_case("ORD")
        || name.value.eq_ignore_ascii_case("CRC32")
        || name.value.eq_ignore_ascii_case("QUOTE")
        || name.value.eq_ignore_ascii_case("TO_BASE64")
    {
        // The engine has none of these, so each is answered by the dialect
        // over the value's bytes.
        return Ok(format!(
            "mysql_{}({})",
            name.value.to_ascii_lowercase(),
            single_column_argument(function)
        ));
    } else if name.value.eq_ignore_ascii_case("ADDDATE")
        || name.value.eq_ignore_ascii_case("SUBDATE")
    {
        let spelled = static_select_metadata::date_shift_spelled_out(function)
            .expect("a checked ADDDATE was checked to spell out as a DATE_ADD");
        return render_scalar_call(&spelled, render_context);
    } else if name.value.eq_ignore_ascii_case("TO_DAYS") {
        return Ok(format!(
            "mysql_to_days({})",
            moment_argument(function, render_context)?
        ));
    } else if name.value.eq_ignore_ascii_case("YEARWEEK") {
        // Measured on MySQL 8.4.11: a `YEARWEEK` with no mode counts by mode
        // 0, as `WEEK` does.
        let sqlparser::ast::FunctionArguments::List(arguments) = &function.args else {
            unreachable!("a checked scalar call was checked to have an argument list");
        };
        let mode = match arguments.args.get(1) {
            Some(sqlparser::ast::FunctionArg::Unnamed(sqlparser::ast::FunctionArgExpr::Expr(
                mode,
            ))) => static_select_metadata::week_mode(mode)
                .expect("a checked YEARWEEK was checked to name a mode from 0 through 7"),
            _ => 0,
        };
        return Ok(format!(
            "mysql_yearweek({}, {mode})",
            moment_argument(function, render_context)?
        ));
    } else if name.value.eq_ignore_ascii_case("QUARTER") {
        // The engine has no quarter of its own, so it is counted off the
        // month: January through March answer 1, and December answers 4.
        return Ok(format!(
            "((CAST(strftime('%m', {}) AS INTEGER) + 2) / 3)",
            single_column_argument(function)
        ));
    } else if name.value.eq_ignore_ascii_case("WEEKDAY") {
        // MySQL counts the week from Monday as 0 and the engine from Sunday
        // as 0, so the engine's answer is shifted round by one day.
        return Ok(format!(
            "((CAST(strftime('%w', {}) AS INTEGER) + 6) % 7)",
            single_column_argument(function)
        ));
    } else if name.value.eq_ignore_ascii_case("DAYOFWEEK") {
        // This one counts from Sunday as 1, which is the engine's numbering
        // with one added.
        return Ok(format!(
            "(CAST(strftime('%w', {}) AS INTEGER) + 1)",
            single_column_argument(function)
        ));
    } else if name.value.eq_ignore_ascii_case("LAST_DAY") {
        // The engine names the day the month ends on by walking to the start
        // of the next month and back one day.
        return Ok(format!(
            "date({}, 'start of month', '+1 month', '-1 day')",
            single_column_argument(function)
        ));
    } else if let Some(field) = strftime_field(&name.value) {
        // The engine reads a part of a moment out as text, where MySQL
        // answers a number, so the cast is what keeps the two agreeing.
        return Ok(format!(
            "CAST(strftime('{field}', {}) AS INTEGER)",
            single_column_argument(function)
        ));
    } else if name.value.eq_ignore_ascii_case("DATE_ADD")
        || name.value.eq_ignore_ascii_case("DATE_SUB")
    {
        let sqlparser::ast::FunctionArguments::List(arguments) = &function.args else {
            unreachable!("a checked call was checked to have an argument list");
        };
        let [sqlparser::ast::FunctionArg::Unnamed(sqlparser::ast::FunctionArgExpr::Expr(shifted)), sqlparser::ast::FunctionArg::Unnamed(sqlparser::ast::FunctionArgExpr::Expr(
            Expr::Interval(interval),
        ))] = arguments.args.as_slice()
        else {
            unreachable!("a checked shift was checked to take a column and an interval");
        };
        let Some(count) = shifted_moment_count(name, interval) else {
            unreachable!("a checked shift was checked to count a written number of a unit");
        };
        // A reading of the moment says which kind it is, so which reader to
        // ask is known here rather than worked out from what is stored.
        if let Some((rendered, _)) = render_shifted_clock_reading(function) {
            return Ok(rendered);
        }
        if let Expr::Value(value) = shifted {
            let (Value::SingleQuotedString(written) | Value::DoubleQuotedString(written)) =
                &value.value
            else {
                unreachable!("a checked shift was checked to take a written moment");
            };
            let written = crate::temporal_value::written_moment_to_shift(written)
                .expect("a checked shift was checked to take a written moment");
            return Ok(render_shifted_moment(&format!("'{written}'"), count));
        }
        let Expr::Identifier(column) = shifted else {
            unreachable!("a checked shift was checked to take a column or a reading");
        };
        return Ok(render_shifted_moment(&render_ident(column), count));
    } else if name.value.eq_ignore_ascii_case("TIMESTAMPDIFF") {
        // The engine has no calendar month and its `unixepoch` drops the
        // fraction of a second, so the whole count is the frontend's.
        let sqlparser::ast::FunctionArguments::List(arguments) = &function.args else {
            unreachable!("a checked scalar call was checked to have an argument list");
        };
        let [sqlparser::ast::FunctionArg::Unnamed(sqlparser::ast::FunctionArgExpr::Expr(unit)), sqlparser::ast::FunctionArg::Unnamed(sqlparser::ast::FunctionArgExpr::Expr(from)), sqlparser::ast::FunctionArg::Unnamed(sqlparser::ast::FunctionArgExpr::Expr(to))] =
            arguments.args.as_slice()
        else {
            unreachable!("a checked TIMESTAMPDIFF was checked to take a unit and two moments");
        };
        let unit = static_select_metadata::timestampdiff_unit(unit)
            .expect("a checked TIMESTAMPDIFF was checked to name a unit");
        return Ok(format!(
            "mysql_timestampdiff('{unit}', {}, {})",
            render_select_expr(from, render_context)?,
            render_select_expr(to, render_context)?
        ));
    } else if name.value.eq_ignore_ascii_case("DATEDIFF") {
        let sqlparser::ast::FunctionArguments::List(arguments) = &function.args else {
            unreachable!("a checked scalar call was checked to have an argument list");
        };
        let [sqlparser::ast::FunctionArg::Unnamed(sqlparser::ast::FunctionArgExpr::Expr(later)), sqlparser::ast::FunctionArg::Unnamed(sqlparser::ast::FunctionArgExpr::Expr(earlier))] =
            arguments.args.as_slice()
        else {
            unreachable!("a checked DATEDIFF was checked to take two moments");
        };
        return Ok(format!(
            "mysql_datediff({}, {})",
            render_select_expr(later, render_context)?,
            render_select_expr(earlier, render_context)?
        ));
    } else if name.value.eq_ignore_ascii_case("LOWER") {
        "mysql_lower"
    } else if name.value.eq_ignore_ascii_case("UPPER") {
        "mysql_upper"
    } else if name.value.eq_ignore_ascii_case("REVERSE") {
        "string_reverse"
    } else if name.value.eq_ignore_ascii_case("HEX") {
        // Measured on MySQL 8.4.11: HEX writes a number in hexadecimal and
        // text as its bytes, so which it is has to be asked at the row rather
        // than worked out from the column. A fractional number is rounded
        // first — `HEX(1234.56)` is 4D3 — and NULL is NULL, where the
        // engine's own `hex` answers an empty word for it.
        let value = scalar_argument(function, 0)?;
        return Ok(format!(
            "CASE WHEN {value} IS NULL THEN NULL WHEN typeof({value}) IN ('integer', 'real') THEN printf('%X', CAST(round({value}) AS INTEGER)) ELSE hex({value}) END"
        ));
    } else if name.value.eq_ignore_ascii_case("ABS") {
        if has_decimal_argument {
            let value = single_column_argument(function);
            return Ok(format!(
                "CASE WHEN numeric_lt({value}, '0') THEN numeric_sub('0', {value}) ELSE {value} END"
            ));
        }
        "abs"
    } else if name.value.eq_ignore_ascii_case("SIGN") {
        "sign"
    } else if name.value.eq_ignore_ascii_case("BIN") || name.value.eq_ignore_ascii_case("OCT") {
        return Ok(format!(
            "mysql_{}({})",
            name.value.to_lowercase(),
            single_column_argument(function)
        ));
    } else if name.value.eq_ignore_ascii_case("FIELD") || name.value.eq_ignore_ascii_case("ELT") {
        if name.value.eq_ignore_ascii_case("FIELD") {
            record_collation_sensitive_call_column(function, 0, render_context);
        }
        return Ok(format!(
            "mysql_{}({})",
            name.value.to_lowercase(),
            render_scalar_arguments(function, render_context)?
        ));
    } else if name.value.eq_ignore_ascii_case("PI") {
        // Measured: MySQL answers 3.141593, six places rather than the whole
        // of the number, which is what its reported six decimals say.
        return Ok("round(pi(), 6)".to_owned());
    } else if let Some(engine) = engine_math_reading(&name.value) {
        engine
    } else if name.value.eq_ignore_ascii_case("CEILING") {
        if has_decimal_argument {
            return Ok(render_decimal_to_whole(
                &single_column_argument(function),
                true,
            ));
        }
        return Ok(format!("ceil({})", single_column_argument(function)));
    } else if name.value.eq_ignore_ascii_case("ROUND") {
        if let Some(StaticSelectMetadata::RoundedAggregate {
            column_name,
            kind,
            places,
        }) = static_select_metadata::scalar_call(function)
        {
            return render_rounded_aggregate(&column_name, kind, places, render_context);
        }
        let Some(StaticSelectMetadata::ScalarCall {
            function: static_select_metadata::ScalarFunction::RoundsToPlaces { places },
            ..
        }) = static_select_metadata::scalar_call(function)
        else {
            unreachable!("a checked ROUND was checked to name its places");
        };
        let value = scalar_argument(function, 0)?;
        // MySQL rounds a DECIMAL half away from zero at the places it names,
        // held to the column's own scale, which is what the engine's decimal
        // rounding does. A place left of the point is not taken for one.
        if let Some(scale) = match &function.args {
            FunctionArguments::List(arguments) => {
                arguments.args.first().and_then(|argument| match argument {
                    sqlparser::ast::FunctionArg::Unnamed(
                        sqlparser::ast::FunctionArgExpr::Expr(expr),
                    ) => decimal_operand_scale(expr, render_context.decimal_columns),
                    _ => None,
                })
            }
            _ => None,
        } {
            let Ok(places) = u32::try_from(places) else {
                return unsupported("SELECT ROUND of a DECIMAL left of the point");
            };
            return Ok(format!(
                "mysql_decimal_round({value}, {})",
                places.min(scale)
            ));
        }
        return Ok(format!("mysql_round({value}, {places})"));
    } else if name.value.eq_ignore_ascii_case("IF") {
        // MySQL's `IF` is the call spelling of a two-branch `CASE`, which is
        // the shape the engine reads.
        let sqlparser::ast::FunctionArguments::List(arguments) = &function.args else {
            unreachable!("a checked scalar call was checked to have an argument list");
        };
        let [sqlparser::ast::FunctionArg::Unnamed(sqlparser::ast::FunctionArgExpr::Expr(condition)), ..] =
            arguments.args.as_slice()
        else {
            unreachable!("IF was checked to take three arguments");
        };
        return Ok(format!(
            "CASE WHEN {} THEN {} ELSE {} END",
            render_select_predicate(condition, render_context)?,
            scalar_argument(function, 1)?,
            scalar_argument(function, 2)?
        ));
    } else if name.value.eq_ignore_ascii_case("CONCAT") {
        // The engine's own `concat` skips a NULL argument where MySQL answers
        // NULL for the whole call; `||` is the operator that agrees.
        return Ok(format!(
            "({})",
            rendered_scalar_arguments(function, render_context)?.join(" || ")
        ));
    } else if name.value.eq_ignore_ascii_case("LEFT") {
        return Ok(format!(
            "substr({}, 1, {})",
            scalar_argument(function, 0)?,
            scalar_argument(function, 1)?
        ));
    } else if name.value.eq_ignore_ascii_case("RIGHT") {
        return Ok(format!(
            "substr({}, -{})",
            scalar_argument(function, 0)?,
            scalar_argument(function, 1)?
        ));
    } else if name.value.eq_ignore_ascii_case("POW") || name.value.eq_ignore_ascii_case("POWER") {
        return Ok(format!(
            "pow({}, {})",
            scalar_argument(function, 0)?,
            scalar_argument(function, 1)?
        ));
    } else if name.value.eq_ignore_ascii_case("MOD") {
        return Ok(format!(
            "CAST(mod({}, {}) AS INTEGER)",
            scalar_argument(function, 0)?,
            scalar_argument(function, 1)?
        ));
    } else if name.value.eq_ignore_ascii_case("JSON_EXTRACT") {
        // The engine's `->` reads `$[0]` over something that is not an array
        // as nothing where MySQL reads the value itself, and writes a document
        // without MySQL's spacing, so the dialect reads it.
        return Ok(format!(
            "mysql_json_extract({}, {})",
            scalar_argument(function, 0)?,
            scalar_argument(function, 1)?
        ));
    } else if name.value.eq_ignore_ascii_case("JSON_UNQUOTE") {
        // The only shape taken is `JSON_UNQUOTE(JSON_EXTRACT(col, path))`. The
        // engine's `->>` answers the JSON null as no value, where MySQL
        // answers the word `null`.
        let sqlparser::ast::FunctionArguments::List(outer) = &function.args else {
            unreachable!("a checked scalar call was already recognized");
        };
        let [sqlparser::ast::FunctionArg::Unnamed(sqlparser::ast::FunctionArgExpr::Expr(
            Expr::Function(inner),
        ))] = outer.args.as_slice()
        else {
            unreachable!("a checked JSON_UNQUOTE was checked to wrap a JSON_EXTRACT");
        };
        return Ok(format!(
            "mysql_json_unquote(mysql_json_extract({}, {}))",
            scalar_argument(inner, 0)?,
            scalar_argument(inner, 1)?
        ));
    } else if name.value.eq_ignore_ascii_case("STR_TO_DATE") {
        return Ok(format!(
            "mysql_str_to_date({}, {})",
            moment_argument(function, render_context)?,
            scalar_argument(function, 1)?
        ));
    } else if name.value.eq_ignore_ascii_case("DATE_FORMAT") {
        // The engine's strftime answers a few of MySQL's specifiers and none
        // of the rest, so the whole of it is written by the dialect instead.
        return Ok(format!(
            "mysql_date_format({}, {})",
            moment_argument(function, render_context)?,
            scalar_argument(function, 1)?
        ));
    } else if name.value.eq_ignore_ascii_case("JSON_ARRAY")
        || name.value.eq_ignore_ascii_case("JSON_OBJECT")
    {
        // The engine builds the same document and writes it without the space
        // MySQL puts after a comma or a colon, and it keeps an object's keys
        // in the order they were written where MySQL sorts them and keeps the
        // last of a repeated key. Writing it again is what settles all three.
        return Ok(format!(
            "mysql_json_document({}({}))",
            name.value.to_lowercase(),
            render_json_builder_arguments(function, render_context)?
        ));
    } else if name.value.eq_ignore_ascii_case("JSON_ARRAYAGG") {
        // Each row's document is read back as a document, so the array holds
        // it rather than its text. Over no rows MySQL answers NULL where the
        // engine answers an empty array.
        let sqlparser::ast::FunctionArguments::List(arguments) = &function.args else {
            unreachable!("a checked JSON_ARRAYAGG was checked to have an argument list");
        };
        let [sqlparser::ast::FunctionArg::Unnamed(sqlparser::ast::FunctionArgExpr::Expr(
            Expr::Function(built),
        ))] = arguments.args.as_slice()
        else {
            unreachable!("a checked JSON_ARRAYAGG was checked to take a built document");
        };
        return Ok(format!(
            "CASE WHEN count(*) = 0 THEN NULL ELSE mysql_json_document(json_group_array(json({}))) END",
            render_scalar_call(built, render_context)?
        ));
    } else if name.value.eq_ignore_ascii_case("JSON_SET")
        || name.value.eq_ignore_ascii_case("JSON_INSERT")
        || name.value.eq_ignore_ascii_case("JSON_REPLACE")
        || name.value.eq_ignore_ascii_case("JSON_REMOVE")
    {
        // The engine changes the same member the same way — the paths this
        // takes are the ones the two agree on — and writes the answer without
        // MySQL's spacing, so it is written again. A value put into the
        // document is written into it the way a built document writes one.
        let sqlparser::ast::FunctionArguments::List(arguments) = &function.args else {
            unreachable!("a checked scalar call was checked to have an argument list");
        };
        let removes = name.value.eq_ignore_ascii_case("JSON_REMOVE");
        let rendered = arguments
            .args
            .iter()
            .enumerate()
            .map(|(position, argument)| {
                let sqlparser::ast::FunctionArg::Unnamed(
                    sqlparser::ast::FunctionArgExpr::Expr(expr),
                ) = argument
                else {
                    return unsupported("SELECT call argument");
                };
                if !removes && position > 0 && position % 2 == 0 {
                    // Measured on MySQL 8.4.11 with a statement prepared once:
                    // a bound word goes in as a JSON string, whatever it
                    // holds, and NULL as the JSON null. Once a number has been
                    // bound, MySQL reads every later word as a number and
                    // refuses it with 1292, so a number is refused here.
                    if is_a_bare_placeholder(expr) {
                        render_context.next_parameter_ordinal()?;
                        return Ok("mysql_json_bound_word(?)".to_owned());
                    }
                    return render_json_value_argument(expr, render_context);
                }
                // The document changed is a JSON column or text; MySQL refuses
                // a number or a moment there with 3146.
                if position == 0
                    && (decimal_operand_scale(expr, render_context.decimal_columns).is_some()
                        || matches!(expr, Expr::Identifier(column) if render_context.is_moment_column(&column.value)))
                {
                    return unsupported("JSON document changed that is not a document");
                }
                scalar_argument(function, position)
            })
            .collect::<Result<Vec<_>, _>>()?
            .join(", ");
        return Ok(format!(
            "mysql_json_document({}({rendered}))",
            name.value.to_lowercase()
        ));
    } else if name.value.eq_ignore_ascii_case("JSON_CONTAINS_PATH") {
        // The engine's json_type answers the kind at a path and nothing at all
        // where the path is not there, which tells a member holding the JSON
        // null from a member that is not there — MySQL counts the first as
        // being there. A NULL document answers NULL rather than 0.
        let sqlparser::ast::FunctionArguments::List(arguments) = &function.args else {
            unreachable!("a checked scalar call was checked to have an argument list");
        };
        let document = scalar_argument(function, 0)?;
        let keyword = scalar_argument(function, 1)?;
        let every = keyword.trim_matches('\'').eq_ignore_ascii_case("all");
        let joiner = if every { " AND " } else { " OR " };
        let found = (2..arguments.args.len())
            .map(|index| {
                Ok(format!(
                    "json_type({document}, {}) IS NOT NULL",
                    scalar_argument(function, index)?
                ))
            })
            .collect::<Result<Vec<_>, ParseError>>()?
            .join(joiner);
        return Ok(format!(
            "CASE WHEN {document} IS NULL THEN NULL ELSE CAST(({found}) AS INTEGER) END"
        ));
    } else if name.value.eq_ignore_ascii_case("JSON_CONTAINS") {
        // The engine has no containment of its own, so the whole of it is
        // answered by the dialect. A path names the part of the target to look
        // in, and the arrow reads it as a document rather than as its text.
        let sqlparser::ast::FunctionArguments::List(arguments) = &function.args else {
            unreachable!("a checked scalar call was checked to have an argument list");
        };
        let target = scalar_argument(function, 0)?;
        let candidate = scalar_argument(function, 1)?;
        let looked_in = match arguments.args.len() {
            3 => format!("({target} -> {})", scalar_argument(function, 2)?),
            _ => target,
        };
        return Ok(format!("mysql_json_contains({looked_in}, {candidate})"));
    } else if name.value.eq_ignore_ascii_case("UNIX_TIMESTAMP") {
        let sqlparser::ast::FunctionArguments::List(arguments) = &function.args else {
            unreachable!("a checked scalar call was checked to have an argument list");
        };
        if arguments.args.is_empty() {
            return Ok("unixepoch()".to_owned());
        }
        // Measured on MySQL 8.4.11: a moment before the epoch answers 0 rather
        // than a negative count, where the engine counts backwards.
        return Ok(format!(
            "max(unixepoch({}), 0)",
            scalar_argument(function, 0)?
        ));
    } else if name.value.eq_ignore_ascii_case("FROM_UNIXTIME") {
        // Measured: a negative count answers no moment at all, where the
        // engine reads one before the epoch, and so does a count past
        // 32536771199, `3001-01-18 23:59:59`, the last moment MySQL reads.
        let seconds = scalar_argument(function, 0)?;
        let moment = format!("datetime({seconds}, 'unixepoch')");
        let sqlparser::ast::FunctionArguments::List(arguments) = &function.args else {
            unreachable!("a checked scalar call was checked to have an argument list");
        };
        let written = if arguments.args.len() == 2 {
            format!(
                "mysql_date_format({moment}, {})",
                scalar_argument(function, 1)?
            )
        } else {
            moment
        };
        return Ok(format!(
            "CASE WHEN {seconds} < 0 OR {seconds} > 32536771199 THEN NULL ELSE {written} END"
        ));
    } else if name.value.eq_ignore_ascii_case("JSON_SEARCH") {
        let sqlparser::ast::FunctionArguments::List(arguments) = &function.args else {
            unreachable!("a checked scalar call was checked to have an argument list");
        };
        // The escape is MySQL's fourth argument and a backslash where it is not
        // written, which is what the dialect is given either way.
        let escape = match arguments.args.len() {
            4 => scalar_argument(function, 3)?,
            _ => "'\\'".to_owned(),
        };
        return Ok(format!(
            "mysql_json_search({}, {}, {}, {escape})",
            scalar_argument(function, 0)?,
            scalar_argument(function, 1)?,
            scalar_argument(function, 2)?
        ));
    } else if name.value.eq_ignore_ascii_case("JSON_OVERLAPS") {
        return Ok(format!(
            "mysql_json_overlaps({}, {})",
            scalar_argument(function, 0)?,
            scalar_argument(function, 1)?
        ));
    } else if name.value.eq_ignore_ascii_case("JSON_MERGE_PATCH")
        || name.value.eq_ignore_ascii_case("JSON_MERGE_PRESERVE")
        || name.value.eq_ignore_ascii_case("JSON_MERGE")
    {
        // MySQL takes as many documents as it is given and the merge is
        // written for two, so the rest are folded into the first — measured,
        // folding two at a time answers what MySQL answers for three.
        let sqlparser::ast::FunctionArguments::List(arguments) = &function.args else {
            unreachable!("a checked scalar call was checked to have an argument list");
        };
        // `JSON_MERGE` is MySQL's deprecated spelling of `JSON_MERGE_PRESERVE`.
        let reading = if name.value.eq_ignore_ascii_case("JSON_MERGE_PATCH") {
            "mysql_json_merge_patch"
        } else {
            "mysql_json_merge_preserve"
        };
        let mut merged = scalar_argument(function, 0)?;
        for index in 1..arguments.args.len() {
            merged = format!("{reading}({merged}, {})", scalar_argument(function, index)?);
        }
        return Ok(merged);
    } else if name.value.eq_ignore_ascii_case("JSON_VALID") {
        return Ok(format!("json_valid({})", scalar_argument(function, 0)?));
    } else if let Some(reading) = mysql_json_reading(&name.value) {
        // Each of these reads what the engine's own JSON functions read
        // differently: a different vocabulary of type names, a length for
        // arrays alone, and no keys at all. The argument may itself be a
        // reading, which only the select renderer knows how to write.
        let sqlparser::ast::FunctionArguments::List(arguments) = &function.args else {
            unreachable!("a checked scalar call was checked to have an argument list");
        };
        let Some(sqlparser::ast::FunctionArg::Unnamed(sqlparser::ast::FunctionArgExpr::Expr(read))) =
            arguments.args.first()
        else {
            unreachable!("a checked JSON reading was checked to take one argument");
        };
        return Ok(format!(
            "{reading}({})",
            render_select_expr(read, render_context)?
        ));
    } else if name.value.eq_ignore_ascii_case("INSTR") {
        record_collation_sensitive_call_column(function, 0, render_context);
        return Ok(format!(
            "mysql_instr({}, {})",
            scalar_argument(function, 0)?,
            scalar_argument(function, 1)?
        ));
    } else if name.value.eq_ignore_ascii_case("LOCATE") {
        record_collation_sensitive_call_column(function, 1, render_context);
        let sqlparser::ast::FunctionArguments::List(arguments) = &function.args else {
            unreachable!("a checked scalar call was checked to have an argument list");
        };
        if arguments.args.len() == 3 {
            let (needle, haystack, start) = (
                scalar_argument(function, 0)?,
                scalar_argument(function, 1)?,
                scalar_argument(function, 2)?,
            );
            return Ok(format!("mysql_locate({needle}, {haystack}, {start})"));
        }
        return Ok(format!(
            "mysql_locate({}, {})",
            scalar_argument(function, 0)?,
            scalar_argument(function, 1)?
        ));
    } else if name.value.eq_ignore_ascii_case("RAND") {
        // Measured on MySQL 8.4.11: a double between zero and one. The engine
        // answers a whole random number, so it is scaled into that range.
        // The cast is what makes the engine name the answer a real rather
        // than the numeric it calls a division of an integer by one.
        return Ok("CAST(abs(random()) / 9223372036854775808.0 AS REAL)".to_owned());
    } else if name.value.eq_ignore_ascii_case("UUID") {
        return Ok("uuid4_str()".to_owned());
    } else if name.value.eq_ignore_ascii_case("TRUNCATE") {
        let sqlparser::ast::FunctionArguments::List(arguments) = &function.args else {
            unreachable!("a checked TRUNCATE call has an argument list");
        };
        let decimal = matches!(arguments.args.first(), Some(sqlparser::ast::FunctionArg::Unnamed(sqlparser::ast::FunctionArgExpr::Expr(expr))) if decimal_operand_scale(expr, render_context.decimal_columns).is_some());
        return Ok(format!(
            "{}({}, {})",
            if decimal {
                "mysql_decimal_truncate"
            } else {
                "mysql_truncate"
            },
            scalar_argument(function, 0)?,
            scalar_argument(function, 1)?
        ));
    } else if name.value.eq_ignore_ascii_case("FORMAT") {
        // A DECIMAL is rounded half away from zero and written out in full
        // first, as MySQL rounds one, so its digits are grouped as they stand
        // rather than through a double.
        if has_decimal_argument {
            let sqlparser::ast::FunctionArguments::List(arguments) = &function.args else {
                unreachable!("a checked FORMAT was checked to have an argument list");
            };
            let Some(places) = arguments.args.get(1).and_then(|argument| match argument {
                sqlparser::ast::FunctionArg::Unnamed(sqlparser::ast::FunctionArgExpr::Expr(
                    expr,
                )) => direct_signed_integer(expr),
                _ => None,
            }) else {
                return unsupported("SELECT FORMAT of a DECIMAL to places not written whole");
            };
            // Measured on MySQL 8.4.11: a negative count writes no fraction
            // and a count past thirty writes thirty.
            let places = places.clamp(0, 30);
            return Ok(format!(
                "mysql_format(mysql_decimal_round({}, {places}), {places})",
                scalar_argument(function, 0)?
            ));
        }
        return Ok(format!(
            "mysql_format({}, {})",
            scalar_argument(function, 0)?,
            scalar_argument(function, 1)?
        ));
    } else if name.value.eq_ignore_ascii_case("MD5") {
        return Ok(format!("mysql_md5({})", scalar_argument(function, 0)?));
    } else if name.value.eq_ignore_ascii_case("SHA1") || name.value.eq_ignore_ascii_case("SHA") {
        return Ok(format!("mysql_sha1({})", scalar_argument(function, 0)?));
    } else if name.value.eq_ignore_ascii_case("SHA2") {
        return Ok(format!(
            "mysql_sha2({}, {})",
            scalar_argument(function, 0)?,
            scalar_argument(function, 1)?
        ));
    } else if name.value.eq_ignore_ascii_case("CONCAT_WS") {
        // The engine's `concat_ws` skips a NULL part and answers NULL for a
        // NULL separator, as MySQL does, and writes a whole number the way
        // MySQL does.
        return Ok(format!(
            "concat_ws({})",
            render_scalar_arguments(function, render_context)?
        ));
    } else if name.value.eq_ignore_ascii_case("SUBSTRING_INDEX") {
        let sqlparser::ast::FunctionArguments::List(arguments) = &function.args else {
            unreachable!("a checked scalar call was checked to have an argument list");
        };
        let Some(sqlparser::ast::FunctionArg::Unnamed(sqlparser::ast::FunctionArgExpr::Expr(
            delimiter,
        ))) = arguments.args.get(1)
        else {
            unreachable!("a checked SUBSTRING_INDEX was checked to take a delimiter");
        };
        return Ok(format!(
            "mysql_substring_index({}, {}, {})",
            scalar_argument(function, 0)?,
            render_select_expr(delimiter, render_context)?,
            scalar_argument(function, 2)?
        ));
    } else if name.value.eq_ignore_ascii_case("REPLACE") {
        return Ok(format!(
            "replace({}, {}, {})",
            scalar_argument(function, 0)?,
            scalar_argument(function, 1)?,
            scalar_argument(function, 2)?
        ));
    } else if name.value.eq_ignore_ascii_case("REPEAT") {
        return Ok(format!(
            "repeat({}, {})",
            scalar_argument(function, 0)?,
            scalar_argument(function, 1)?
        ));
    } else if name.value.eq_ignore_ascii_case("LPAD") || name.value.eq_ignore_ascii_case("RPAD") {
        return Ok(format!(
            "{}({}, {}, {})",
            name.value.to_lowercase(),
            scalar_argument(function, 0)?,
            scalar_argument(function, 1)?,
            scalar_argument(function, 2)?
        ));
    } else if name.value.eq_ignore_ascii_case("NULLIF") {
        record_collation_sensitive_call_column(function, 0, render_context);
        let first = scalar_argument(function, 0)?;
        let second = scalar_argument(function, 1)?;
        return Ok(format!(
            "CASE WHEN typeof({first}) = 'text' THEN mysql_text_nullif({first}, {second}) ELSE nullif({first}, {second}) END"
        ));
    } else if name.value.eq_ignore_ascii_case("IFNULL")
        || name.value.eq_ignore_ascii_case("COALESCE")
    {
        let rendered = format!(
            "{}({})",
            name.value.to_lowercase(),
            render_scalar_arguments(function, render_context)?
        );
        // Measured on MySQL 8.4.11, `IFNULL(score, 0)` over a `DOUBLE` or a
        // `FLOAT` answers the column's kind for the fallback row too, which
        // the engine would answer as the whole number it was written as.
        return Ok(if falls_back_from_a_real_column(function, render_context) {
            format!("CAST({rendered} AS REAL)")
        } else {
            rendered
        });
    } else if name.value.eq_ignore_ascii_case("GREATEST") {
        record_all_collation_sensitive_call_columns(function, render_context);
        let values = scalar_arguments(function)?;
        let has_text = values
            .iter()
            .map(|value| format!("typeof({value}) = 'text'"))
            .collect::<Vec<_>>()
            .join(" OR ");
        let values = values.join(", ");
        return Ok(format!(
            "CASE WHEN {has_text} THEN mysql_text_greatest({values}) ELSE max({values}) END"
        ));
    } else if name.value.eq_ignore_ascii_case("LEAST") {
        record_all_collation_sensitive_call_columns(function, render_context);
        let values = scalar_arguments(function)?;
        let has_text = values
            .iter()
            .map(|value| format!("typeof({value}) = 'text'"))
            .collect::<Vec<_>>()
            .join(" OR ");
        let values = values.join(", ");
        return Ok(format!(
            "CASE WHEN {has_text} THEN mysql_text_least({values}) ELSE min({values}) END"
        ));
    } else {
        unreachable!("a checked scalar call was already recognized");
    };
    Ok(format!("{engine}({})", single_column_argument(function)))
}

fn falls_back_from_a_real_column(
    function: &sqlparser::ast::Function,
    render_context: &SelectRenderContext<'_>,
) -> bool {
    let Some(StaticSelectMetadata::ScalarCall {
        function: static_select_metadata::ScalarFunction::Defaulted,
        columns,
        ..
    }) = static_select_metadata::scalar_call(function)
    else {
        return false;
    };
    columns.iter().any(|column| {
        render_context
            .real_columns
            .iter()
            .any(|real| real.eq_ignore_ascii_case(column))
    })
}

/// How the answer of a `CASE`, `IF`, `IFNULL` or `COALESCE` is written for the
/// engine, which depends on the kind of number its columns hold.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ConditionalAnswer {
    /// Whole numbers or words, which the engine answers as MySQL does.
    AsWritten,
    /// A `DOUBLE` column among the branches makes the answer a `DOUBLE`, so a
    /// whole number branch has to come back as one too.
    Double,
    /// A `DECIMAL` column among the branches makes the answer a `DECIMAL`
    /// with the largest scale among them.
    Decimal { scale: u32 },
}

/// Works out how a conditional's answer is written, from the columns its
/// branches name.
///
/// Only a second rendering knows what the columns hold. The first one answers
/// as written and says so, and a statement whose final rendering still did not
/// know is refused.
fn conditional_answer(
    expr: &Expr,
    render_context: &mut SelectRenderContext<'_>,
) -> ConditionalAnswer {
    let Some(StaticSelectMetadata::Branches { branches, .. }) =
        static_select_metadata::classify_static_select_expr(expr)
    else {
        return ConditionalAnswer::AsWritten;
    };
    let columns = branches
        .iter()
        .filter_map(|branch| match branch {
            static_select_metadata::Branch::Column { column_name } => Some(column_name.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>();
    if columns.is_empty() {
        return ConditionalAnswer::AsWritten;
    }
    render_context.checks_type_sensitive_expression = true;
    if render_context.table_columns.is_empty() {
        render_context.renders_a_condition_without_column_types = true;
        return ConditionalAnswer::AsWritten;
    }
    if columns.iter().any(|column| {
        render_context
            .real_columns
            .iter()
            .any(|real| real.eq_ignore_ascii_case(column))
    }) {
        return ConditionalAnswer::Double;
    }
    columns
        .iter()
        .filter_map(|column| {
            render_context
                .decimal_columns
                .iter()
                .find(|(decimal, _)| decimal.eq_ignore_ascii_case(column))
                .map(|(_, scale)| *scale)
        })
        .max()
        .map_or(ConditionalAnswer::AsWritten, |scale| {
            ConditionalAnswer::Decimal { scale }
        })
}

/// Writes a `CASE` whose conditions are already written.
///
/// Measured on MySQL 8.4.11, a `CASE` answering a `DECIMAL` answers each
/// branch at its own scale rather than at the answer's: `THEN balance ELSE 0`
/// over a `DECIMAL(10,2)` answers `10.50` and `0`, in both protocols, and an
/// `INT` branch beside it answers `30`. Each branch is written out as the text
/// it is, which is what the engine's `DECIMAL` already reads as.
fn render_picked_branches(
    arms: Vec<(String, &Expr)>,
    else_result: Option<&Expr>,
    answer: ConditionalAnswer,
    render_context: &mut SelectRenderContext<'_>,
) -> Result<String, ParseError> {
    let mut rendered = "CASE".to_owned();
    for (condition, result) in arms {
        rendered.push_str(" WHEN ");
        rendered.push_str(&condition);
        rendered.push_str(" THEN ");
        rendered.push_str(&render_branch(result, answer, render_context)?);
    }
    // Both engines answer NULL for a row that matches nothing, so a missing
    // ELSE is written as a missing ELSE.
    if let Some(else_result) = else_result {
        rendered.push_str(" ELSE ");
        rendered.push_str(&render_branch(else_result, answer, render_context)?);
    }
    rendered.push_str(" END");
    Ok(match answer {
        ConditionalAnswer::Double => format!("CAST({rendered} AS REAL)"),
        ConditionalAnswer::AsWritten | ConditionalAnswer::Decimal { .. } => rendered,
    })
}

fn render_branch(
    expr: &Expr,
    answer: ConditionalAnswer,
    render_context: &mut SelectRenderContext<'_>,
) -> Result<String, ParseError> {
    let rendered = render_select_expr(expr, render_context)?;
    if matches!(answer, ConditionalAnswer::Decimal { .. })
        && decimal_operand_scale(expr, render_context.decimal_columns).is_none()
    {
        return Ok(format!("CAST({rendered} AS TEXT)"));
    }
    Ok(rendered)
}

/// Writes an `IF`, an `IFNULL` or a `COALESCE` that names a column or answers
/// whole numbers.
///
/// Measured on MySQL 8.4.11, `IFNULL` and `COALESCE` answering a `DECIMAL`
/// answer every value at the answer's scale — `IFNULL(age, balance)` over an
/// `INT` and a `DECIMAL(10,2)` answers `30.00` — where a `CASE` does not.
fn render_branches_call(
    name: &Ident,
    function: &sqlparser::ast::Function,
    branches: &StaticSelectMetadata,
    render_context: &mut SelectRenderContext<'_>,
) -> Result<String, ParseError> {
    let answer = conditional_answer(&Expr::Function(function.clone()), render_context);
    let FunctionArguments::List(arguments) = &function.args else {
        unreachable!("a checked scalar call was checked to have an argument list");
    };
    let arguments = arguments
        .args
        .iter()
        .map(|argument| match argument {
            sqlparser::ast::FunctionArg::Unnamed(sqlparser::ast::FunctionArgExpr::Expr(expr)) => {
                expr
            }
            _ => unreachable!("a checked conditional was checked to take plain arguments"),
        })
        .collect::<Vec<_>>();
    if name.value.eq_ignore_ascii_case("IF") {
        let [condition, then_result, else_result] = arguments.as_slice() else {
            unreachable!("IF was checked to take three arguments");
        };
        let condition = render_select_predicate(condition, render_context)?;
        return render_picked_branches(
            vec![(condition, then_result)],
            Some(else_result),
            answer,
            render_context,
        );
    }
    assert!(
        matches!(
            branches,
            StaticSelectMetadata::Branches {
                falls_back: true,
                ..
            }
        ),
        "only IF picks a branch among the checked calls"
    );
    let values = arguments
        .into_iter()
        .map(|argument| render_select_expr(argument, render_context))
        .collect::<Result<Vec<_>, _>>()?
        .join(", ");
    Ok(match answer {
        ConditionalAnswer::AsWritten => format!("coalesce({values})"),
        ConditionalAnswer::Double => format!("CAST(coalesce({values}) AS REAL)"),
        ConditionalAnswer::Decimal { scale } => {
            format!("mysql_decimal_round(coalesce({values}), {scale})")
        }
    })
}

/// Writes `COUNT`, `SUM`, `AVG`, `MIN` or `MAX` over a `CASE` or `IF`.
///
/// Measured on MySQL 8.4.11, `SUM` and `AVG` over a `CASE` answering a
/// `DECIMAL` answer at the `CASE`'s scale whatever the rows held —
/// `SUM(CASE WHEN id > 100 THEN balance ELSE 0 END)` over no matching row is
/// `0.00` — so every value is brought to that scale before it is added. `AVG`
/// over whole numbers answers four places exactly, which the engine's
/// `DECIMAL` average does and its own average does not. `MIN` and `MAX` over
/// a `DECIMAL` are refused: the engine would compare the written values as
/// words.
fn render_aggregate_over_branches(
    function: &sqlparser::ast::Function,
    render_context: &mut SelectRenderContext<'_>,
) -> Result<String, ParseError> {
    let (kind, argument) = static_select_metadata::aggregated_branches(function)
        .expect("the caller checked the aggregate reads a CASE");
    let answer = conditional_answer(argument, render_context);
    let values = render_select_expr(argument, render_context)?;
    let Some(kind) = kind else {
        return Ok(format!("count({values})"));
    };
    Ok(match (kind, answer) {
        (ColumnAggregateKind::Sum, ConditionalAnswer::Decimal { scale }) => {
            format!("mysql_decimal_sum(mysql_decimal_round({values}, {scale}))")
        }
        (ColumnAggregateKind::Sum, _) => format!("sum({values})"),
        (ColumnAggregateKind::Avg, ConditionalAnswer::Decimal { scale }) => {
            format!("mysql_decimal_avg(mysql_decimal_round({values}, {scale}))")
        }
        (ColumnAggregateKind::Avg, ConditionalAnswer::Double) => format!("avg({values})"),
        (ColumnAggregateKind::Avg, ConditionalAnswer::AsWritten) => {
            format!("mysql_decimal_avg({values})")
        }
        (ColumnAggregateKind::MinMax, ConditionalAnswer::Decimal { .. }) => {
            return unsupported("SELECT MIN or MAX over a CASE answering a DECIMAL");
        }
        (ColumnAggregateKind::MinMax, _) => {
            format!("{}({values})", function.name.to_string().to_lowercase())
        }
        (
            ColumnAggregateKind::Concatenated
            | ColumnAggregateKind::DeviatesBySample
            | ColumnAggregateKind::CollectsIntoJson,
            _,
        ) => unreachable!("only COUNT, SUM, AVG, MIN and MAX are read over a CASE"),
    })
}

/// Renders what `JSON_ARRAY` and `JSON_OBJECT` are given, each the way MySQL
/// writes it into a document.
///
/// Measured on MySQL 8.4.11: a `JSON` column is written in as the document it
/// holds rather than as its text, a `DATETIME` as a string carrying six places
/// of a second — `"2026-01-02 03:04:05.000000"` — and a `BIGINT UNSIGNED` or a
/// `DECIMAL` with no places as the number it holds, which the engine reads out
/// of either as text. A `DECIMAL` with places is written with every place it
/// carries — `1.50` — which a document read back through the engine loses, so
/// it is refused, and so is a number written with a place the engine would
/// not write back.
fn render_json_builder_arguments(
    function: &sqlparser::ast::Function,
    render_context: &mut SelectRenderContext<'_>,
) -> Result<String, ParseError> {
    let sqlparser::ast::FunctionArguments::List(arguments) = &function.args else {
        unreachable!("a checked scalar call was checked to have an argument list");
    };
    arguments
        .args
        .iter()
        .map(|argument| {
            let sqlparser::ast::FunctionArg::Unnamed(sqlparser::ast::FunctionArgExpr::Expr(expr)) =
                argument
            else {
                return unsupported("SELECT call argument");
            };
            render_json_value_argument(expr, render_context)
        })
        .collect::<Result<Vec<_>, _>>()
        .map(|arguments| arguments.join(", "))
}

fn render_json_value_argument(
    expr: &Expr,
    render_context: &SelectRenderContext<'_>,
) -> Result<String, ParseError> {
    let Expr::Identifier(column) = expr else {
        if names_a_number_a_double_writes_differently(expr) {
            return unsupported("JSON document built from a number written with a trailing zero");
        }
        return render_scalar_argument_expr(expr);
    };
    let rendered = render_ident(column);
    if render_context.is_json_column(&column.value) {
        return Ok(format!("json({rendered})"));
    }
    if render_context.is_moment_column(&column.value) {
        return Ok(format!(
            "substr({rendered} || CASE WHEN instr({rendered}, '.') > 0 THEN '000000' ELSE '.000000' END, 1, 26)"
        ));
    }
    match decimal_operand_scale(expr, render_context.decimal_columns) {
        Some(0) => Ok(format!("json({rendered})")),
        Some(_) => unsupported("JSON document built from a DECIMAL with places"),
        None => Ok(rendered),
    }
}

/// Reports whether a number written with a point goes into a document with
/// digits the engine's double would not write back.
///
/// Measured on MySQL 8.4.11: `JSON_ARRAY(1.5, 1.0, 10.00, 1.10)` is
/// `[1.5, 1.0, 10.00, 1.10]` — every place written is kept — where the engine
/// reads each as a double and writes `10.0` and `1.1`. A double writes back
/// the shortest digits that read as it, which are the written ones while no
/// place past the first is a trailing zero and fifteen digits hold them.
fn names_a_number_a_double_writes_differently(expr: &Expr) -> bool {
    let written = match expr {
        Expr::UnaryOp {
            op: UnaryOperator::Minus | UnaryOperator::Plus,
            expr: inner,
        } => inner.as_ref(),
        other => other,
    };
    let Expr::Value(value) = written else {
        return false;
    };
    let Value::Number(number, false) = &value.value else {
        return false;
    };
    let Some((whole, fraction)) = number.split_once('.') else {
        return false;
    };
    if number.contains(['e', 'E']) {
        return false;
    }
    (fraction.len() > 1 && fraction.ends_with('0'))
        || whole.trim_start_matches('0').len() + fraction.len() > 15
}

/// Reads the clock to a count of places of a second.
///
/// The engine's clock reads to the millisecond, so the places past the third
/// are always zero — a reading MySQL's clock could have taken, at a coarser
/// grain. Cutting rather than rounding is what MySQL does: measured,
/// `UTC_TIMESTAMP(2)` beside `UTC_TIME(3)` of `.137` answers `.13`.
fn render_clock_to_a_fraction(format: &str, whole_length: u32, places: u32) -> String {
    let read = format!(
        "substr(strftime('{format}', 'now'), 1, {})",
        whole_length + places.min(3)
    );
    match places.checked_sub(3) {
        Some(padding) if padding > 0 => format!("({read} || '{}')", "0".repeat(padding as usize)),
        _ => read,
    }
}

/// Renders `ROUND(SUM(col), n)` and the same over `AVG` as MySQL works them
/// out: the aggregate as an exact decimal, then rounded half away from zero.
///
/// MySQL's `AVG` answers four more places than the column has and rounds
/// there before `ROUND` rounds again, so the average is taken as the exact
/// decimal the engine's own `mysql_decimal_avg` answers, which is the same
/// number, rather than as a float. A place beyond the aggregate's own scale
/// adds no digits: measured on 8.4.11, `ROUND(AVG(views), 6)` over an `INT`
/// answers `5.0000`.
///
/// The column's scale is the frontend's to know, so a first reading assumes a
/// whole-number column and the second, told which columns are decimals,
/// renders the scale they carry. A column holding a float is refused where the
/// result's shape is worked out.
fn render_rounded_aggregate(
    column_name: &str,
    kind: ColumnAggregateKind,
    places: u32,
    render_context: &mut SelectRenderContext<'_>,
) -> Result<String, ParseError> {
    render_context.checks_type_sensitive_expression = true;
    let column_scale = render_context
        .decimal_columns
        .iter()
        .find(|(known, _)| known.eq_ignore_ascii_case(column_name))
        .map_or(0, |(_, scale)| *scale);
    let (aggregate, scale) = match kind {
        ColumnAggregateKind::Sum => ("mysql_decimal_sum", column_scale),
        ColumnAggregateKind::Avg => ("mysql_decimal_avg", (column_scale + 4).min(30)),
        _ => unreachable!("only SUM and AVG are rounded aggregates"),
    };
    Ok(format!(
        "mysql_decimal_round({aggregate}({}), {})",
        render_ident_str(column_name),
        places.min(scale)
    ))
}

/// Names the reading that answers a MySQL JSON call the engine has none for.
fn mysql_json_reading(name: &str) -> Option<&'static str> {
    for (call, reading) in [
        ("JSON_TYPE", "mysql_json_type"),
        ("JSON_LENGTH", "mysql_json_length"),
        ("JSON_KEYS", "mysql_json_keys"),
        ("JSON_QUOTE", "mysql_json_quote"),
    ] {
        if name.eq_ignore_ascii_case(call) {
            return Some(reading);
        }
    }
    None
}

/// Names the engine's spelling of a MySQL reading that answers a double.
///
/// Each of these is spelled the same in both, and each is one the two work out
/// the same way — which is not true of the readings a maths library rounds for
/// itself, and those are refused rather than listed here.
fn engine_math_reading(name: &str) -> Option<&'static str> {
    ["sqrt", "degrees", "radians"]
        .into_iter()
        .find(|reading| name.eq_ignore_ascii_case(reading))
}

/// Names the strftime field a MySQL reading call asks for.
fn strftime_field(name: &str) -> Option<&'static str> {
    for (call, field) in [
        ("YEAR", "%Y"),
        ("MONTH", "%m"),
        ("DAY", "%d"),
        ("DAYOFMONTH", "%d"),
        ("DAYOFYEAR", "%j"),
        ("HOUR", "%H"),
        ("MINUTE", "%M"),
        ("SECOND", "%S"),
    ] {
        if name.eq_ignore_ascii_case(call) {
            return Some(field);
        }
    }
    None
}

/// Names the strftime field an `EXTRACT` asks for.
fn extract_strftime_field(field: &sqlparser::ast::DateTimeField) -> Option<&'static str> {
    match field {
        sqlparser::ast::DateTimeField::Year => Some("%Y"),
        sqlparser::ast::DateTimeField::Month => Some("%m"),
        sqlparser::ast::DateTimeField::Day => Some("%d"),
        sqlparser::ast::DateTimeField::Hour => Some("%H"),
        sqlparser::ast::DateTimeField::Minute => Some("%M"),
        sqlparser::ast::DateTimeField::Second => Some("%S"),
        _ => None,
    }
}

fn single_column_argument(function: &sqlparser::ast::Function) -> String {
    let sqlparser::ast::FunctionArguments::List(arguments) = &function.args else {
        unreachable!("a checked call was checked to have an argument list");
    };
    match arguments.args.as_slice() {
        [sqlparser::ast::FunctionArg::Unnamed(sqlparser::ast::FunctionArgExpr::Expr(
            Expr::Identifier(column),
        ))] => render_ident(column),
        _ => unreachable!("a checked call was checked to take one column"),
    }
}

/// Renders one argument of a checked call by position.
fn scalar_argument(
    function: &sqlparser::ast::Function,
    index: usize,
) -> Result<String, ParseError> {
    let sqlparser::ast::FunctionArguments::List(arguments) = &function.args else {
        unreachable!("a checked scalar call was checked to have an argument list");
    };
    let Some(sqlparser::ast::FunctionArg::Unnamed(sqlparser::ast::FunctionArgExpr::Expr(expr))) =
        arguments.args.get(index)
    else {
        return unsupported("SELECT call argument");
    };
    match expr {
        Expr::Identifier(column) => Ok(render_ident(column)),
        _ => render_scalar_argument_expr(expr),
    }
}

fn render_scalar_argument_expr(expr: &Expr) -> Result<String, ParseError> {
    match expr {
        Expr::Value(value) if matches!(&value.value, Value::Number(_, false)) => {
            let Value::Number(written, false) = &value.value else {
                unreachable!("guard requires a numeric literal");
            };
            if written.parse::<f64>().is_ok_and(f64::is_finite) {
                Ok(written.clone())
            } else {
                unsupported("SELECT call numeric argument")
            }
        }
        Expr::UnaryOp {
            op: UnaryOperator::Minus | UnaryOperator::Plus,
            expr: inner,
        } if matches!(inner.as_ref(), Expr::Value(value) if matches!(&value.value, Value::Number(_, false))) =>
        {
            let sign = if matches!(
                expr,
                Expr::UnaryOp {
                    op: UnaryOperator::Minus,
                    ..
                }
            ) {
                "-"
            } else {
                "+"
            };
            let Expr::Value(value) = inner.as_ref() else {
                unreachable!("guard requires a numeric literal");
            };
            let Value::Number(written, false) = &value.value else {
                unreachable!("guard requires a numeric literal");
            };
            if written.parse::<f64>().is_ok_and(f64::is_finite) {
                Ok(format!("{sign}{written}"))
            } else {
                unsupported("SELECT call numeric argument")
            }
        }
        _ => render_dml_expr(expr),
    }
}

fn scalar_arguments(function: &sqlparser::ast::Function) -> Result<Vec<String>, ParseError> {
    let sqlparser::ast::FunctionArguments::List(arguments) = &function.args else {
        unreachable!("a checked scalar call was checked to have an argument list");
    };
    (0..arguments.args.len())
        .map(|index| scalar_argument(function, index))
        .collect()
}

fn record_collation_sensitive_call_column(
    function: &sqlparser::ast::Function,
    index: usize,
    render_context: &mut SelectRenderContext<'_>,
) {
    let sqlparser::ast::FunctionArguments::List(arguments) = &function.args else {
        unreachable!("a checked scalar call has an argument list");
    };
    if let Some(sqlparser::ast::FunctionArg::Unnamed(sqlparser::ast::FunctionArgExpr::Expr(
        Expr::Identifier(column),
    ))) = arguments.args.get(index)
    {
        render_context
            .collation_sensitive_call_columns
            .push(column.value.clone());
    }
}

/// Records the columns a call reads, where its answer is ordered or compared
/// under `utf8mb4_0900_ai_ci`'s weights and may be text. MySQL orders and
/// compares a text answer under the collation of the column it came from, so
/// the frontend refuses the call over a column declared with another.
fn record_the_columns_a_text_call_reads(call: &Expr, render_context: &mut SelectRenderContext<'_>) {
    if matches!(
        static_select_metadata::comparison_answer(call),
        Some(answer) if answer != crate::CheckedComparisonAnswer::Text
    ) {
        return;
    }
    if let Some(StaticSelectMetadata::ScalarCall { columns, .. }) =
        static_select_metadata::classify_static_select_expr(call)
    {
        render_context
            .collation_sensitive_call_columns
            .extend(columns);
    }
}

fn record_all_collation_sensitive_call_columns(
    function: &sqlparser::ast::Function,
    render_context: &mut SelectRenderContext<'_>,
) {
    let sqlparser::ast::FunctionArguments::List(arguments) = &function.args else {
        unreachable!("a checked scalar call has an argument list");
    };
    for index in 0..arguments.args.len() {
        record_collation_sensitive_call_column(function, index, render_context);
    }
}

/// Renders the moment a `DATE_FORMAT` writes out or a `STR_TO_DATE` reads.
///
/// It is a column as often as not, but a clock reading and a moment written
/// out as a word are moments too, and the classifier says which of the three
/// this is. Each is spelled here the way the engine spells it.
fn moment_argument(
    function: &sqlparser::ast::Function,
    render_context: &mut SelectRenderContext<'_>,
) -> Result<String, ParseError> {
    let sqlparser::ast::FunctionArguments::List(arguments) = &function.args else {
        unreachable!("a checked scalar call was checked to have an argument list");
    };
    let Some(sqlparser::ast::FunctionArg::Unnamed(sqlparser::ast::FunctionArgExpr::Expr(moment))) =
        arguments.args.first()
    else {
        return unsupported("SELECT call argument");
    };
    render_select_expr(moment, render_context)
}

/// Renders every argument of a checked call, which only the two-argument
/// forms need.
fn render_scalar_arguments(
    function: &sqlparser::ast::Function,
    render_context: &mut SelectRenderContext<'_>,
) -> Result<String, ParseError> {
    Ok(rendered_scalar_arguments(function, render_context)?.join(", "))
}

fn rendered_scalar_arguments(
    function: &sqlparser::ast::Function,
    render_context: &mut SelectRenderContext<'_>,
) -> Result<Vec<String>, ParseError> {
    let sqlparser::ast::FunctionArguments::List(arguments) = &function.args else {
        unreachable!("a checked scalar call was checked to have an argument list");
    };
    arguments
        .args
        .iter()
        .map(|argument| {
            let sqlparser::ast::FunctionArg::Unnamed(sqlparser::ast::FunctionArgExpr::Expr(expr)) =
                argument
            else {
                return unsupported("SELECT call argument");
            };
            match expr {
                Expr::Identifier(column) => Ok(render_ident(column)),
                // `IFNULL(SUM(n), 0)` puts an aggregate inside a call, and
                // only the classifier says which calls take one.
                Expr::Function(inner)
                    if static_select_metadata::is_count_call(inner)
                        || static_select_metadata::column_aggregate_argument(inner).is_some() =>
                {
                    Ok(render_aggregate_call(inner, render_context))
                }
                _ => render_scalar_argument_expr(expr),
            }
        })
        .collect()
}

fn checked_arithmetic_sql_operator(operator: &BinaryOperator) -> &'static str {
    match operator {
        BinaryOperator::Plus => "+",
        BinaryOperator::Minus => "-",
        BinaryOperator::Multiply => "*",
        _ => unreachable!("a checked arithmetic operator was already recognized"),
    }
}

/// Walks forward from where a call starts to the parenthesis that closes it,
/// answering the offset just past it. Parentheses inside a quoted string are
/// not parentheses.
/// Returns the call an expression ends with, when it ends with one.
///
/// Arithmetic is the shape that reaches this: sqlparser gives a `SUM(n) +
/// SUM(m)` a span that stops where its last call's span stops, which is before
/// that call's closing parenthesis.
fn trailing_call(expr: &Expr) -> Option<&Expr> {
    match expr {
        Expr::BinaryOp { right, .. } => match right.as_ref() {
            right @ Expr::Function(function)
                if !matches!(function.args, sqlparser::ast::FunctionArguments::None) =>
            {
                Some(right)
            }
            right => trailing_call(right),
        },
        _ => None,
    }
}

fn closing_parenthesis(source: &str, start: usize) -> Option<usize> {
    let mut depth = 0usize;
    let mut quote = None;
    for (offset, character) in source.get(start..)?.char_indices() {
        match (quote, character) {
            (Some(mark), character) if character == mark => quote = None,
            (Some(_), _) => {}
            (None, '\'' | '"' | '`') => quote = Some(character),
            (None, '(') => depth += 1,
            (None, ')') => {
                depth = depth.checked_sub(1)?;
                if depth == 0 {
                    return Some(start + offset + character.len_utf8());
                }
            }
            (None, _) => {}
        }
    }
    None
}

/// Returns the statement text one expression was written with.
///
/// sqlparser reports a span in 1-based line and column numbers, and drops the
/// parentheses around a nested expression, so both have to be undone here to
/// recover what the client actually typed.
fn source_text(source: &str, expr: &Expr) -> Option<String> {
    use sqlparser::ast::Spanned;
    let span = expr.span();
    let start = byte_offset(source, span.start)?;
    let end = byte_offset(source, span.end)?;
    if start > end || end > source.len() {
        return None;
    }
    let (mut start, mut end) = (start, end);
    let bytes = source.as_bytes();
    // An interval's span covers its count alone, and MySQL's name for the
    // column carries the `INTERVAL` before the count and the unit after it.
    let mut unnested = expr;
    while let Expr::Nested(inner) = unnested {
        unnested = inner;
    }
    if let Expr::BinaryOp { left, right, .. } = unnested {
        if matches!(right.as_ref(), Expr::Interval(_)) {
            end = end_of_the_unit_after(source, end)?;
        }
        if matches!(left.as_ref(), Expr::Interval(_)) {
            start = start_of_the_interval_keyword_before(source, start)?;
        }
        // A subquery's span covers its `SELECT` and not the parentheses around
        // it, which MySQL names the column with.
        if matches!(left.as_ref(), Expr::Subquery(_)) {
            start = bytes[..start].iter().rposition(|byte| *byte == b'(')?;
        }
        if let Expr::Subquery(subquery) = right.as_ref() {
            let subquery_start = byte_offset(source, subquery.span().start)?;
            let opening = bytes[..subquery_start]
                .iter()
                .rposition(|byte| *byte == b'(')?;
            end = end.max(closing_parenthesis(source, opening)?);
        }
    }
    // A call's span covers its name and arguments but not its closing
    // parenthesis, and a CASE's stops before its END; MySQL's own name for the
    // column includes both.
    if matches!(
        expr,
        Expr::Substring { .. }
            | Expr::Trim { .. }
            | Expr::Floor { .. }
            | Expr::Ceil { .. }
            | Expr::Extract { .. }
            | Expr::Cast { .. }
            | Expr::Convert { .. }
            | Expr::Position { .. }
    ) {
        let open_paren = bytes[..start].iter().rposition(|byte| *byte == b'(')?;
        let name_end = bytes[..open_paren]
            .iter()
            .rposition(|byte| !byte.is_ascii_whitespace())?;
        let name_start = bytes[..name_end]
            .iter()
            .rposition(|byte| !(byte.is_ascii_alphanumeric() || *byte == b'_'))
            .map_or(0, |pos| pos + 1);
        start = name_start;
    }
    // A negation's span covers what it negates and not its sign or its NOT,
    // and a truth test's covers what it tests and not the words after it.
    if let Expr::UnaryOp { op, .. } = expr {
        let operator = match op {
            sqlparser::ast::UnaryOperator::Minus => "-",
            sqlparser::ast::UnaryOperator::Not => "NOT",
            _ => "",
        };
        let before = source.get(..start)?.trim_end();
        let operator_start = before.len().checked_sub(operator.len());
        if let Some(operator_start) = operator_start.filter(|_| !operator.is_empty()) {
            if before
                .get(operator_start..)
                .is_some_and(|written| written.eq_ignore_ascii_case(operator))
            {
                start = operator_start;
            }
        }
    }
    if matches!(
        expr,
        Expr::IsTrue(_) | Expr::IsFalse(_) | Expr::IsNotTrue(_) | Expr::IsNotFalse(_)
    ) {
        end += truth_test_words_len(source.get(end..)?)?;
    }
    // A bare `CURRENT_DATE` is a call with no parentheses at all, so there is
    // no closing one to reach for and its span is already the whole name.
    let closes_with_a_paren = match expr {
        Expr::Function(function) => {
            !matches!(function.args, sqlparser::ast::FunctionArguments::None)
        }
        Expr::Substring { .. }
        | Expr::Trim { .. }
        | Expr::Floor { .. }
        | Expr::Ceil { .. }
        | Expr::Extract { .. }
        | Expr::Cast { .. }
        | Expr::Convert { .. }
        | Expr::Position { .. } => true,
        _ => false,
    };
    if closes_with_a_paren {
        // The span stops before the parenthesis that closes the call, and a
        // call nested inside it closes one of its own first, so the end is
        // where the call's own parenthesis closes rather than the first one
        // after the span.
        end = end.max(closing_parenthesis(source, start)?);
    }
    // A call at the end of an expression stops before its own closing
    // parenthesis the way a call standing alone does, and the expression's
    // span stops with it: `SUM(n) + SUM(m)` would be named `SUM(n) + SUM(m`.
    if let Some(trailing) = trailing_call(expr) {
        let trailing_start = byte_offset(source, trailing.span().start)?;
        end = end.max(closing_parenthesis(source, trailing_start)?);
    }
    // A windowed call's span stops at its arguments, and MySQL's name for the
    // column carries the whole `OVER (...)` after them.
    if matches!(expr, Expr::Function(function) if function.over.is_some()) {
        end += window_clause_len(source.get(end..)?)?;
    }
    if matches!(expr, Expr::Case { .. })
        && !source
            .get(start..end)?
            .trim_end()
            .to_ascii_uppercase()
            .ends_with("END")
    {
        let tail = source.get(end..)?;
        let offset = tail.to_ascii_uppercase().find("END")?;
        end += offset + "END".len();
    }
    let mut depth = nested_depth(expr);
    while depth > 0 {
        let opening = bytes[..start]
            .iter()
            .rposition(|byte| !byte.is_ascii_whitespace())?;
        let closing = bytes[end..]
            .iter()
            .position(|byte| !byte.is_ascii_whitespace())?
            + end;
        if bytes[opening] != b'(' || bytes[closing] != b')' {
            return None;
        }
        start = opening;
        end = closing + 1;
        depth -= 1;
    }
    source.get(start..end).map(str::to_owned)
}

/// Where the unit word written after an interval's count ends.
fn end_of_the_unit_after(source: &str, count_end: usize) -> Option<usize> {
    let rest = source.get(count_end..)?;
    let unit_start = count_end + (rest.len() - rest.trim_start().len());
    let unit_length = source
        .get(unit_start..)?
        .bytes()
        .take_while(|byte| byte.is_ascii_alphabetic() || *byte == b'_')
        .count();
    (unit_length > 0).then_some(unit_start + unit_length)
}

/// Where the `INTERVAL` written before an interval's count starts.
fn start_of_the_interval_keyword_before(source: &str, count_start: usize) -> Option<usize> {
    let keyword_end = source.get(..count_start)?.trim_end().len();
    let keyword_start = keyword_end.checked_sub("INTERVAL".len())?;
    source
        .get(keyword_start..keyword_end)?
        .eq_ignore_ascii_case("INTERVAL")
        .then_some(keyword_start)
}

/// Counts the bytes of `IS TRUE`, `IS FALSE`, `IS NOT TRUE` or `IS NOT FALSE`
/// at the front of `tail`, whitespace before each word included.
fn truth_test_words_len(tail: &str) -> Option<usize> {
    let mut read = 0;
    for words in [
        &["IS"][..],
        &["NOT", "TRUE", "FALSE"][..],
        &["TRUE", "FALSE"][..],
    ] {
        let rest = &tail[read..];
        let skipped = rest.len() - rest.trim_start().len();
        let word = words.iter().find(|word| {
            rest[skipped..]
                .get(..word.len())
                .is_some_and(|written| written.eq_ignore_ascii_case(word))
        });
        match word {
            Some(word) => {
                read += skipped + word.len();
                if *word != "NOT" && *word != "IS" {
                    return Some(read);
                }
            }
            None => return None,
        }
    }
    Some(read)
}

fn nested_depth(expr: &Expr) -> usize {
    match expr {
        Expr::Nested(inner) => 1 + nested_depth(inner),
        // A subquery's span covers the `SELECT` and not the parentheses around
        // it, and MySQL names the column after both.
        Expr::Subquery(_) => 1,
        _ => 0,
    }
}

fn byte_offset(source: &str, location: sqlparser::tokenizer::Location) -> Option<usize> {
    let mut line = 1;
    let mut column = 1;
    for (offset, character) in source.char_indices() {
        if line == location.line && column == location.column {
            return Some(offset);
        }
        if character == '\n' {
            line += 1;
            column = 1;
        } else {
            column += 1;
        }
    }
    (line == location.line && column == location.column).then_some(source.len())
}

fn render_select_predicate(
    expr: &Expr,
    render_context: &mut SelectRenderContext<'_>,
) -> Result<String, ParseError> {
    match expr {
        Expr::IsNull(inner) | Expr::IsNotNull(inner)
            if json_condition::reads_a_json_column(inner) =>
        {
            json_condition::render_json_null_test(
                inner,
                matches!(expr, Expr::IsNotNull(_)),
                render_context,
            )
        }
        expr @ Expr::Function(_) if json_condition::reads_a_json_column(expr) => {
            json_condition::render_json_test(expr, render_context)
        }
        Expr::IsNull(expr) => Ok(format!(
            "({} IS NULL)",
            render_select_expr(expr, render_context)?
        )),
        Expr::IsNotNull(expr) => Ok(format!(
            "({} IS NOT NULL)",
            render_select_expr(expr, render_context)?
        )),
        // `WHERE BINARY name = 'alpha'` is how a statement asks for the row
        // spelled exactly so. MySQL reads the `BINARY` as a cast of the column
        // alone, and sqlparser reads it as a cast of everything after it, so
        // the condition is read back out with the first column compared by
        // its bytes. Measured on 8.4.11 over 'alpha', 'Alpha', 'ALPHA' and
        // 'alpha ': it finds the first alone, and `'alpha '` the last alone —
        // a binary string keeps its trailing spaces, where `utf8mb4_bin`
        // pads them away.
        Expr::Cast {
            kind: sqlparser::ast::CastKind::Cast,
            expr: cast,
            data_type: sqlparser::ast::DataType::Binary(None),
            format: None,
            array: false,
        } if written_as_a_binary_prefix(cast, render_context.source) => {
            let Some(condition) = compared_by_bytes_first(cast) else {
                return unsupported("SELECT BINARY over anything but a column compared");
            };
            render_select_predicate(&condition, render_context)
        }
        Expr::BinaryOp { left, op, right }
            if matches!(op, BinaryOperator::And | BinaryOperator::Or) =>
        {
            let op = if matches!(op, BinaryOperator::And) {
                "AND"
            } else {
                "OR"
            };
            Ok(format!(
                "({} {op} {})",
                render_select_predicate(left, render_context)?,
                render_select_predicate(right, render_context)?
            ))
        }
        // A statement built up in pieces starts its WHERE with a comparison
        // that names no column at all — `WHERE 1 = 1 AND ...` — so the pieces
        // after it can each be written with an AND in front. Measured on MySQL
        // 8.4.11, the engine answers each of these the same way.
        Expr::BinaryOp { left, op, right }
            if is_checked_select_comparison_operator(op)
                && names_a_whole_number(left)
                && names_a_whole_number(right) =>
        {
            Ok(format!(
                "({} {} {})",
                render_dml_expr(left)?,
                checked_select_comparison_sql_operator(op),
                render_dml_expr(right)?
            ))
        }
        Expr::BinaryOp { left, op, right } if is_checked_select_comparison_operator(op) => {
            render_checked_select_comparison(left, op, right, render_context)
        }
        Expr::Like {
            negated,
            any,
            expr,
            pattern,
            escape_char,
        } => render_checked_like(
            *negated,
            *any,
            expr,
            pattern,
            escape_char.as_ref(),
            render_context,
        ),
        // `name REGEXP 'a.c'` asks whether a pattern matches anywhere in the
        // column. The engine keeps its own matching in an extension this
        // frontend does not register, and MySQL holds the match to a
        // collation rather than to the pattern, so the dialect answers it.
        Expr::RLike {
            negated,
            expr,
            pattern,
            regexp: _,
        } => render_checked_regexp(*negated, expr, pattern, render_context),
        Expr::UnaryOp {
            op: UnaryOperator::Not,
            expr,
        } => Ok(format!(
            "(NOT {})",
            render_select_predicate(expr, render_context)?
        )),
        Expr::IsTrue(tested)
        | Expr::IsFalse(tested)
        | Expr::IsNotTrue(tested)
        | Expr::IsNotFalse(tested) => {
            let tested = render_select_predicate(tested, render_context)?;
            Ok(render_truth_test(expr, &tested))
        }
        Expr::Nested(expr) => Ok(format!(
            "({})",
            render_select_predicate(expr, render_context)?
        )),
        Expr::Value(value) if matches!(&value.value, Value::Boolean(_)) => {
            render_select_expr(expr, render_context)
        }
        expr if names_a_whole_number(expr) => render_dml_expr(expr),
        // `WHERE active` is how a statement tests a column that holds a flag,
        // which MySQL reads as a comparison against zero. Measured on 8.4.11
        // over a `TINYINT(1)` and an `INT`: a value that is not zero keeps the
        // row, zero and NULL do not, and a negative number keeps it. The
        // engine reads a bare column the same way, so it is written out as it
        // stands.
        //
        // A column of words is refused. MySQL reads one as the number it
        // begins with, where the engine compares a word against a number by
        // their kinds, so the two would keep different rows.
        Expr::Identifier(column) => {
            render_context.tests_a_bare_column = true;
            if render_context
                .decimal_columns
                .iter()
                .any(|(known, _)| known.eq_ignore_ascii_case(&column.value))
            {
                return unsupported("SELECT WHERE testing a DECIMAL column");
            }
            if render_context.is_text_column(&column.value) {
                return unsupported("SELECT WHERE testing a column of words");
            }
            Ok(render_ident(column))
        }
        Expr::CompoundIdentifier(parts) if parts.len() == 2 => {
            render_context.tests_a_bare_column = true;
            if render_context
                .decimal_columns
                .iter()
                .any(|(known, _)| known.eq_ignore_ascii_case(&parts[1].value))
            {
                return unsupported("SELECT WHERE testing a DECIMAL column");
            }
            if render_context.is_text_column(&parts[1].value) {
                return unsupported("SELECT WHERE testing a column of words");
            }
            Ok(format!(
                "{}.{}",
                render_ident(&parts[0]),
                render_ident(&parts[1])
            ))
        }
        Expr::InSubquery {
            expr,
            subquery,
            negated,
        } => render_in_subquery(expr, subquery, *negated, render_context),
        Expr::InList {
            expr,
            list,
            negated,
        } => render_checked_in_list(expr, list, *negated, render_context),
        // Laravel's `whereHas` through a pivot table joins the pivot inside
        // the `EXISTS`, which answers whether a row is there, so no column of
        // it is read and its tables may be joined.
        Expr::Exists { subquery, negated } => {
            render_context.an_exists_may_join = true;
            let rendered = render_subquery(subquery, render_context);
            render_context.an_exists_may_join = false;
            Ok(format!(
                "({}EXISTS ({}))",
                if *negated { "NOT " } else { "" },
                rendered?.0
            ))
        }
        Expr::Between {
            expr,
            negated,
            low,
            high,
        } => render_checked_between(*negated, expr, low, high, render_context),
        _ => unsupported("SELECT WHERE predicate before coercion calibration"),
    }
}

/// Reports whether an expression is a whole number written out.
///
/// That is the only side a comparison naming no column takes. Measured on
/// MySQL 8.4.11, a word against a word is compared without regard to case and
/// a number against a word coerces the word to a number, neither of which the
/// engine does, so those keep the refusal every uncalibrated comparison has.
/// Reports whether a cast to `BINARY` was written as the `BINARY` prefix of
/// the column first in it rather than as `CAST(... AS BINARY)`, which sqlparser
/// reads the same way and which means a cast of the answer.
fn written_as_a_binary_prefix(cast: &Expr, source: &str) -> bool {
    use sqlparser::ast::Spanned;
    let Some(start) = byte_offset(source, first_operand(cast).span().start) else {
        return false;
    };
    let Some(before) = source.get(..start) else {
        return false;
    };
    let before = before.trim_end();
    let Some(keyword_start) = before.len().checked_sub("BINARY".len()) else {
        return false;
    };
    before
        .get(keyword_start..)
        .is_some_and(|keyword| keyword.eq_ignore_ascii_case("BINARY"))
        && !before[..keyword_start]
            .chars()
            .next_back()
            .is_some_and(|character| character.is_alphanumeric() || character == '_')
}

/// The operand written first in a chain of `AND`, `OR` and comparisons.
fn first_operand(expr: &Expr) -> &Expr {
    match expr {
        Expr::BinaryOp { left, .. } => first_operand(left),
        other => other,
    }
}

/// Reads a condition sqlparser put under a `BINARY` back out, with the
/// comparison written first comparing its column by its bytes and everything
/// after it as it was: `BINARY name = 'a' AND n > 0` is `(BINARY name) = 'a'
/// AND n > 0`, the prefix binding tighter than any operator.
fn compared_by_bytes_first(condition: &Expr) -> Option<Expr> {
    let Expr::BinaryOp { left, op, right } = condition else {
        return None;
    };
    if matches!(op, BinaryOperator::And | BinaryOperator::Or) {
        return Some(Expr::BinaryOp {
            left: Box::new(compared_by_bytes_first(left)?),
            op: op.clone(),
            right: right.clone(),
        });
    }
    if !is_checked_select_comparison_operator(op) || !matches!(left.as_ref(), Expr::Identifier(_)) {
        return None;
    }
    Some(Expr::BinaryOp {
        left: Box::new(Expr::Collate {
            expr: left.clone(),
            collation: ObjectName(vec![ObjectNamePart::Identifier(
                sqlparser::ast::Ident::new("binary"),
            )]),
        }),
        op: op.clone(),
        right: right.clone(),
    })
}

fn names_a_whole_number(expr: &Expr) -> bool {
    match expr {
        Expr::Value(value) => {
            matches!(&value.value, Value::Number(digits, false) if digits.parse::<i64>().is_ok())
        }
        Expr::UnaryOp {
            op: UnaryOperator::Minus | UnaryOperator::Plus,
            expr,
        }
        | Expr::Nested(expr) => names_a_whole_number(expr),
        _ => false,
    }
}

fn reverse_checked_comparison_operator(op: &BinaryOperator) -> Option<BinaryOperator> {
    match op {
        BinaryOperator::Eq => Some(BinaryOperator::Eq),
        BinaryOperator::NotEq => Some(BinaryOperator::NotEq),
        BinaryOperator::Lt => Some(BinaryOperator::Gt),
        BinaryOperator::LtEq => Some(BinaryOperator::GtEq),
        BinaryOperator::Gt => Some(BinaryOperator::Lt),
        BinaryOperator::GtEq => Some(BinaryOperator::LtEq),
        BinaryOperator::Spaceship => Some(BinaryOperator::Spaceship),
        _ => None,
    }
}

/// Renders `col IN (a, b)`, which MySQL answers by comparing the column
/// against each member under the column's own collation.
///
/// Measured on MySQL 8.4.11 over rows (1,'b'), (2,'A'), (3,'c'):
/// `name IN ('a','C')` answers 2 and 3, so a text list ignores case the way a
/// text `=` does. `id NOT IN (1, NULL)` answers nothing, which is ordinary
/// three-valued logic and what the engine already does.
///
/// Every member is recorded as its own checked comparison, so the frontend
/// holds each one to the column's type exactly as it holds a single `=`.
fn render_checked_in_list(
    expr: &Expr,
    list: &[Expr],
    negated: bool,
    render_context: &mut SelectRenderContext<'_>,
) -> Result<String, ParseError> {
    if let Expr::Tuple(columns) = expr {
        return render_checked_row_in_list(columns, list, negated, render_context);
    }
    if let Some(rendered) =
        json_condition::render_in_list_over_a_json_reading(expr, list, negated, render_context)?
    {
        return Ok(rendered);
    }
    let (qualifier, column) = match expr {
        Expr::Identifier(ident) => (None, ident),
        Expr::CompoundIdentifier(parts) if parts.len() == 2 => (Some(&parts[0]), &parts[1]),
        _ => return unsupported("SELECT IN requires one column"),
    };
    if list.is_empty() {
        return unsupported("SELECT IN over an empty list");
    }
    let column_name = column.value.clone();
    let decimal_column = render_context
        .decimal_columns
        .iter()
        .any(|(known, _)| known.eq_ignore_ascii_case(&column_name));
    let allow_large_integer = render_context.table_columns.is_empty() || decimal_column;
    let json_column = render_context.is_json_column(&column_name);
    let mut members = Vec::with_capacity(list.len());
    for element in list {
        let (rendered, rhs) = render_checked_select_comparison_rhs_allowing_large_integer(
            element,
            render_context,
            allow_large_integer,
        )?;
        members.push(if json_column {
            (rendered, rhs)
        } else {
            number_a_written_word_names(rendered, rhs, &column_name, render_context)
        });
    }
    // One text member collates the whole list, because MySQL compares every
    // member under the column's collation rather than each member's own. A `?`
    // carries no type until it is bound, so it is collated only once the
    // caller has said the column is text.
    // Under the single-source assumption verified in `translate_select_query`,
    // the unqualified column name is sufficient to look up the column's type.
    let collated = members.iter().any(|(_, rhs)| match rhs {
        CheckedSelectComparisonRhs::Text(_) => true,
        CheckedSelectComparisonRhs::Placeholder { .. } => {
            render_context.is_text_column(&column_name)
        }
        _ => false,
    });
    if members
        .iter()
        .any(|(_, rhs)| matches!(rhs, CheckedSelectComparisonRhs::Placeholder { .. }))
    {
        render_context.compares_a_placeholder = true;
    }
    let operator = if negated {
        CheckedSelectComparisonOperator::NotIn
    } else {
        CheckedSelectComparisonOperator::In
    };
    let rendered_column = match qualifier {
        Some(qualifier) => format!("{}.{}", render_ident(qualifier), render_ident(column)),
        None => render_ident(column),
    };
    let rendered = if json_column {
        let matches = members
            .iter()
            .map(|(rendered, rhs)| match rhs {
                CheckedSelectComparisonRhs::Text(_) => Ok(format!(
                    "CAST({rendered_column} AS BLOB) = CAST(mysql_json_quote({rendered}) AS BLOB)"
                )),
                CheckedSelectComparisonRhs::SignedInteger(_) => Ok(format!(
                    "mysql_json_equals_integer({rendered_column}, {rendered})"
                )),
                CheckedSelectComparisonRhs::Null => Ok("NULL".to_owned()),
                _ => unsupported("JSON IN requires a written string, integer, or NULL"),
            })
            .collect::<Result<Vec<_>, _>>()?;
        let matches = matches.join(" OR ");
        if negated {
            format!("(NOT ({matches}))")
        } else {
            format!("({matches})")
        }
    } else if decimal_column {
        let matches = members
            .iter()
            .map(|(member, _)| format!("numeric_eq({rendered_column}, {member})"))
            .collect::<Vec<_>>()
            .join(" OR ");
        if negated {
            format!("(NOT ({matches}))")
        } else {
            format!("({matches})")
        }
    } else {
        format!(
            "({rendered_column} {}IN ({}))",
            if negated { "NOT " } else { "" },
            members
                .iter()
                .map(|(rendered, _)| rendered.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        )
    };
    for (_, rhs) in members {
        render_context
            .checked_comparisons
            .push(CheckedSelectComparison {
                qualifier: qualifier.map(|q| q.value.clone()),
                inner_sources: Vec::new(),
                column_name: column_name.clone(),
                operator,
                rhs,
                collated,
                answers: None,
            });
    }
    Ok(rendered)
}

/// Renders `(a, b) IN ((1, 'x'), (2, 'y'))`, which asks whether the columns
/// hold one of the rows written out.
///
/// The engine has no list of rows to ask that of, so it is asked the question
/// the row list means: each row is its columns compared one by one and joined
/// by AND, and the rows are joined by OR. Measured on MySQL 8.4.11, that
/// answers what the row list answers, three-valued logic included — a row
/// holding NULL in one of the columns is left out of the `NOT IN` as well as
/// the `IN`, which `NOT (NULL AND true)` is.
fn render_checked_row_in_list(
    columns: &[Expr],
    list: &[Expr],
    negated: bool,
    render_context: &mut SelectRenderContext<'_>,
) -> Result<String, ParseError> {
    if columns.len() < 2 || list.is_empty() {
        return unsupported("SELECT IN over a row of columns");
    }
    let mut rows = Vec::with_capacity(list.len());
    for element in list {
        let Expr::Tuple(written) = element else {
            return unsupported("SELECT IN over a row requires a row for each member");
        };
        if written.len() != columns.len() {
            return unsupported("SELECT IN over a row of a different width");
        }
        let mut parts = Vec::with_capacity(written.len());
        for (column, value) in columns.iter().zip(written) {
            parts.push(render_checked_select_comparison(
                column,
                &BinaryOperator::Eq,
                value,
                render_context,
            )?);
        }
        rows.push(format!("({})", parts.join(" AND ")));
    }
    let matched = format!("({})", rows.join(" OR "));
    Ok(if negated {
        format!("(NOT {matched})")
    } else {
        matched
    })
}

fn render_checked_between(
    negated: bool,
    expr: &Expr,
    low: &Expr,
    high: &Expr,
    render_context: &mut SelectRenderContext<'_>,
) -> Result<String, ParseError> {
    let is_valid_column = match expr {
        Expr::Identifier(_) => true,
        Expr::CompoundIdentifier(parts) => parts.len() == 2,
        _ => false,
    };
    if !is_valid_column {
        return unsupported("BETWEEN requires a column as its subject");
    }
    let lower = render_checked_select_comparison(expr, &BinaryOperator::GtEq, low, render_context)?;
    let upper =
        render_checked_select_comparison(expr, &BinaryOperator::LtEq, high, render_context)?;
    let condition = format!("({lower} AND {upper})");
    if negated {
        Ok(format!("(NOT {condition})"))
    } else {
        Ok(condition)
    }
}

fn render_checked_select_comparison(
    left: &Expr,
    op: &BinaryOperator,
    right: &Expr,
    render_context: &mut SelectRenderContext<'_>,
) -> Result<String, ParseError> {
    let left = without_parentheses(left);
    let right = without_parentheses(right);
    if let Some(rendered) =
        json_condition::render_comparison_over_a_json_reading(left, op, right, render_context)?
    {
        return Ok(rendered);
    }
    // A call stands where a column stands, and says what it answers rather
    // than holding a declared type. `WHERE LOWER(email) = 'a'` and
    // `WHERE CHAR_LENGTH(name) > 3` are the shapes this is for.
    if let Some(rendered) = render_comparison_over_a_call(left, op, right, render_context)? {
        return Ok(rendered);
    }
    // A subquery that answers one value stands where a value stands, which is
    // how a statement asks for the row holding the highest of something.
    if let Some(rendered) =
        render_comparison_over_a_scalar_subquery(left, op, right, render_context)?
    {
        return Ok(rendered);
    }
    if let Some(rendered) =
        render_comparison_over_arithmetic_or_a_fallback(left, op, right, render_context)?
    {
        return Ok(rendered);
    }
    // `name = 'a' COLLATE utf8mb4_bin` compares the bytes rather than the
    // collation's own reading, and MySQL takes the collation written on either
    // side: measured on 8.4.11 over 'alpha', 'Alpha' and 'ALPHA', both
    // spellings find the one row spelled exactly so.
    let (left, right, named_collation) = match (left, right) {
        (Expr::Collate { expr, collation }, right) => (expr.as_ref(), right, Some(collation)),
        (left, Expr::Collate { expr, collation }) => (left, expr.as_ref(), Some(collation)),
        (left, right) => (left, right, None),
    };
    let compares_bytes = match named_collation {
        Some(collation) => match collation_orders_by_bytes(collation) {
            Some(by_bytes) => by_bytes,
            None => return unsupported("SELECT comparison collation"),
        },
        None => false,
    };
    let (qualifier, column, op_reversed, rhs_expr) = match (left, right) {
        (Expr::Identifier(column), _) => (None, column, op.clone(), right),
        (Expr::CompoundIdentifier(parts), _) if parts.len() == 2 => {
            (Some(&parts[0]), &parts[1], op.clone(), right)
        }
        (_, Expr::Identifier(column)) => {
            let reversed =
                reverse_checked_comparison_operator(op).ok_or(ParseError::Unsupported {
                    feature: "reversed SELECT comparison operator",
                })?;
            (None, column, reversed, left)
        }
        (_, Expr::CompoundIdentifier(parts)) if parts.len() == 2 => {
            let reversed =
                reverse_checked_comparison_operator(op).ok_or(ParseError::Unsupported {
                    feature: "reversed SELECT comparison operator",
                })?;
            (Some(&parts[0]), &parts[1], reversed, left)
        }
        _ => return unsupported("SELECT comparison requires one column"),
    };
    if let Some((other_qualifier, other_column)) = named_column(rhs_expr) {
        if named_collation.is_some() {
            return unsupported("SELECT comparison of two columns under a named collation");
        }
        return Ok(render_column_pair_comparison(
            (qualifier, column),
            &op_reversed,
            (other_qualifier, other_column),
            render_context,
        ));
    }
    if let Some(answers) = answer_of_a_call_reading_a_column(rhs_expr) {
        if named_collation.is_some() {
            return unsupported("SELECT comparison of a column and a call under a named collation");
        }
        return render_column_against_a_call(
            (qualifier, column),
            &op_reversed,
            (rhs_expr, answers),
            render_context,
        );
    }
    let column_name = column.value.clone();
    let json_column = render_context.is_json_column(&column_name);
    if json_column && named_collation.is_some() {
        return unsupported("JSON comparison with explicit collation");
    }
    let decimal_column = render_context
        .decimal_columns
        .iter()
        .any(|(known, _)| known.eq_ignore_ascii_case(&column_name));
    let allow_large_decimal_integer = render_context.table_columns.is_empty() || decimal_column;
    let (rendered_rhs, rhs) = render_checked_select_comparison_rhs_allowing_large_integer(
        rhs_expr,
        render_context,
        allow_large_decimal_integer,
    )?;
    let (rendered_rhs, rhs) =
        midnight_of_a_written_day(rendered_rhs, rhs, &column_name, render_context);
    let (rendered_rhs, rhs) = if named_collation.is_none() && !json_column {
        number_a_written_word_names(rendered_rhs, rhs, &column_name, render_context)
    } else {
        (rendered_rhs, rhs)
    };
    let operator =
        checked_select_comparison_operator(&op_reversed).expect("comparison operator guard");
    // A bare column already has its declared collation in the stored schema.
    // Keep it for implicit comparisons; an explicit COLLATE overrides it.
    // A `?` carries no type of its own, so the metadata still records whether
    // it is compared with a text column.
    // Under the single-source assumption verified in `translate_select_query`,
    // the unqualified column name is sufficient to look up the column's type.
    // A collation is named over text and nothing else. A bound value carries
    // no text until it binds, and what a written one is checked for here could
    // not be checked there.
    if compares_bytes && !matches!(rhs, CheckedSelectComparisonRhs::Text(_)) {
        return unsupported("SELECT comparison collation over a value that is not written text");
    }
    let collated = match rhs {
        CheckedSelectComparisonRhs::Text(_) => !compares_bytes,
        CheckedSelectComparisonRhs::Placeholder { .. } => {
            render_context.compares_a_placeholder = true;
            render_context.is_text_column(&column_name)
        }
        _ => false,
    };
    // An explicit collation is the only one that belongs in rendered SQL.
    let collation = match (collated, compares_bytes, named_collation) {
        (true, _, Some(_)) => " COLLATE MYSQL_UCA9_AI_CI",
        (true, _, None) => "",
        (false, true, Some(collation)) if collation_is_utf8mb4_bin(collation) => {
            " COLLATE MYSQL_UTF8MB4_BIN"
        }
        (false, true, _) => " COLLATE BINARY",
        (false, false, _) => "",
    };
    let rendered_column = match qualifier {
        Some(qualifier) => format!("{}.{}", render_ident(qualifier), render_ident(column)),
        None => render_ident(column),
    };
    let rendered = if json_column
        && matches!(rhs, CheckedSelectComparisonRhs::SignedInteger(_))
        && matches!(
            operator,
            CheckedSelectComparisonOperator::Equal
                | CheckedSelectComparisonOperator::NotEqual
                | CheckedSelectComparisonOperator::NullSafeEqual
        ) {
        let equal = format!("mysql_json_equals_integer({rendered_column}, {rendered_rhs})");
        match operator {
            CheckedSelectComparisonOperator::Equal => format!("({equal})"),
            CheckedSelectComparisonOperator::NotEqual => format!("(NOT {equal})"),
            CheckedSelectComparisonOperator::NullSafeEqual => format!("(coalesce({equal}, 0))"),
            _ => unreachable!("JSON integer comparison was restricted to equality"),
        }
    } else if json_column
        && matches!(rhs, CheckedSelectComparisonRhs::Text(_))
        && matches!(
            operator,
            CheckedSelectComparisonOperator::Equal
                | CheckedSelectComparisonOperator::NotEqual
                | CheckedSelectComparisonOperator::NullSafeEqual
        )
    {
        format!(
            "(CAST({rendered_column} AS BLOB) {} CAST(mysql_json_quote({rendered_rhs}) AS BLOB))",
            checked_select_comparison_sql_operator(&op_reversed)
        )
    } else if json_column
        && matches!(
            operator,
            CheckedSelectComparisonOperator::LessThan
                | CheckedSelectComparisonOperator::LessThanOrEqual
                | CheckedSelectComparisonOperator::GreaterThan
                | CheckedSelectComparisonOperator::GreaterThanOrEqual
        )
        && matches!(
            rhs,
            CheckedSelectComparisonRhs::SignedInteger(_) | CheckedSelectComparisonRhs::Text(_)
        )
    {
        let compare = match rhs {
            CheckedSelectComparisonRhs::SignedInteger(_) => "mysql_json_compare_integer",
            CheckedSelectComparisonRhs::Text(_) => "mysql_json_compare_string",
            _ => unreachable!("JSON ordering was restricted to an integer or string"),
        };
        format!(
            "({compare}({rendered_column}, {rendered_rhs}) {} 0)",
            checked_select_comparison_sql_operator(&op_reversed)
        )
    } else if decimal_column
        && (matches!(rhs, CheckedSelectComparisonRhs::Placeholder { .. })
            || operator == CheckedSelectComparisonOperator::NullSafeEqual)
    {
        let comparison = match operator {
            CheckedSelectComparisonOperator::Equal => {
                format!("numeric_eq({rendered_column}, {rendered_rhs})")
            }
            CheckedSelectComparisonOperator::NotEqual => {
                format!("NOT numeric_eq({rendered_column}, {rendered_rhs})")
            }
            CheckedSelectComparisonOperator::LessThan => {
                format!("numeric_lt({rendered_column}, {rendered_rhs})")
            }
            CheckedSelectComparisonOperator::LessThanOrEqual => {
                format!("NOT numeric_lt({rendered_rhs}, {rendered_column})")
            }
            CheckedSelectComparisonOperator::GreaterThan => {
                format!("numeric_lt({rendered_rhs}, {rendered_column})")
            }
            CheckedSelectComparisonOperator::GreaterThanOrEqual => {
                format!("NOT numeric_lt({rendered_column}, {rendered_rhs})")
            }
            CheckedSelectComparisonOperator::NullSafeEqual => {
                format!("numeric_nullsafe_eq({rendered_column}, {rendered_rhs})")
            }
            _ => unreachable!("checked numeric comparison operator"),
        };
        format!("({comparison})")
    } else {
        format!(
            "({rendered_column}{collation} {} {rendered_rhs})",
            checked_select_comparison_sql_operator(&op_reversed)
        )
    };
    render_context
        .checked_comparisons
        .push(CheckedSelectComparison {
            qualifier: qualifier.map(|q| q.value.clone()),
            inner_sources: Vec::new(),
            column_name,
            operator,
            rhs,
            collated,
            answers: None,
        });
    Ok(rendered)
}

/// A column, a value or a `?` in parentheses is the thing itself: MySQL reads
/// `(a) = (b)` as `a = b`, which is how Prisma writes every join and every
/// key it compares. Parentheses around anything larger are kept.
fn without_parentheses(expr: &Expr) -> &Expr {
    let mut inner = expr;
    while let Expr::Nested(nested) = inner {
        inner = nested;
    }
    match inner {
        Expr::Identifier(_) | Expr::CompoundIdentifier(_) | Expr::Value(_) => inner,
        _ => expr,
    }
}

/// Renders a comparison whose one side reads a column through `+`, `-` or `*`
/// — `WHERE age + 1 > 10` — or through a fallback — `WHERE COALESCE(age, 0) >
/// 10` — or nothing when neither side does.
fn render_comparison_over_arithmetic_or_a_fallback(
    left: &Expr,
    op: &BinaryOperator,
    right: &Expr,
    render_context: &mut SelectRenderContext<'_>,
) -> Result<Option<String>, ParseError> {
    let (expression, op, other) = if reads_a_column_through_an_expression(left) {
        (left, op.clone(), right)
    } else if reads_a_column_through_an_expression(right) {
        let Some(reversed) = reverse_checked_comparison_operator(op) else {
            return Ok(None);
        };
        (right, reversed, left)
    } else {
        return Ok(None);
    };
    let Some(operator) = checked_select_comparison_operator(&op) else {
        return Ok(None);
    };
    if let Some((column, fallback)) = fallback_over_a_column(expression) {
        return render_comparison_over_a_fallback(
            column,
            fallback,
            (&op, operator),
            other,
            render_context,
        )
        .map(Some);
    }
    render_comparison_over_arithmetic(expression, (&op, operator), other, render_context).map(Some)
}

fn reads_a_column_through_an_expression(expr: &Expr) -> bool {
    fallback_over_a_column(expr).is_some() || arithmetic_over_columns(expr).is_some()
}

/// Reads `COALESCE(col, value)` or `IFNULL(col, value)`.
fn fallback_over_a_column(expr: &Expr) -> Option<(&Expr, &Expr)> {
    let Expr::Function(function) = expr else {
        return None;
    };
    let [ObjectNamePart::Identifier(name)] = function.name.0.as_slice() else {
        return None;
    };
    if name.quote_style.is_some()
        || !["COALESCE", "IFNULL"]
            .iter()
            .any(|candidate| name.value.eq_ignore_ascii_case(candidate))
        || function.over.is_some()
        || function.filter.is_some()
        || !function.within_group.is_empty()
    {
        return None;
    }
    let sqlparser::ast::FunctionArguments::List(arguments) = &function.args else {
        return None;
    };
    if arguments.duplicate_treatment.is_some() || !arguments.clauses.is_empty() {
        return None;
    }
    let [sqlparser::ast::FunctionArg::Unnamed(sqlparser::ast::FunctionArgExpr::Expr(column)), sqlparser::ast::FunctionArg::Unnamed(sqlparser::ast::FunctionArgExpr::Expr(fallback))] =
        arguments.args.as_slice()
    else {
        return None;
    };
    named_column(column)?;
    Some((column, fallback))
}

/// Reads one `+`, `-` or `*` between two columns, or a column and a whole
/// number no wider than an `INT`, and answers the columns it reads.
///
/// The width is what keeps the answer inside a `BIGINT` whatever the column
/// holds, which the frontend holds the column itself to.
fn arithmetic_over_columns(expr: &Expr) -> Option<Vec<(Option<&Ident>, &Ident)>> {
    let Expr::BinaryOp { left, op, right } = expr else {
        return None;
    };
    if !matches!(
        op,
        BinaryOperator::Plus | BinaryOperator::Minus | BinaryOperator::Multiply
    ) {
        return None;
    }
    let columns = [arithmetic_operand(left)?, arithmetic_operand(right)?]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>();
    (!columns.is_empty()).then_some(columns)
}

/// Reads one side of the arithmetic: a column, or a whole number no wider
/// than an `INT`, which reads no column.
fn arithmetic_operand(expr: &Expr) -> Option<Option<(Option<&Ident>, &Ident)>> {
    if let Some(column) = named_column(expr) {
        return Some(Some(column));
    }
    let Expr::Value(value) = expr else {
        return None;
    };
    let Value::Number(number, false) = &value.value else {
        return None;
    };
    (is_written_as_a_whole_number(number) && number.parse::<i32>().is_ok()).then_some(None)
}

/// Renders `COALESCE(col, value) op other`.
///
/// Both the fallback and the value compared with are held to the column the
/// way a comparison against the column is, and the column to the kinds whose
/// fallback the engine answers in the column's own form. Two words compare
/// under `utf8mb4_0900_ai_ci`, the collation the frontend holds the column to:
/// measured on 8.4.11, MySQL takes the column's collation over a written
/// word's.
fn render_comparison_over_a_fallback(
    column: &Expr,
    fallback: &Expr,
    (op, operator): (&BinaryOperator, CheckedSelectComparisonOperator),
    other: &Expr,
    render_context: &mut SelectRenderContext<'_>,
) -> Result<String, ParseError> {
    let (qualifier, column) = named_column(column).expect("the fallback was read over a column");
    let (rendered_fallback, fallback_value) =
        render_checked_select_comparison_rhs(fallback, render_context)?;
    if !matches!(
        fallback_value,
        CheckedSelectComparisonRhs::SignedInteger(_)
            | CheckedSelectComparisonRhs::Decimal(_)
            | CheckedSelectComparisonRhs::Text(_)
    ) {
        return unsupported("COALESCE comparison falling back on something not written out");
    }
    let rendered_fallback = number_without_quotes(rendered_fallback, &fallback_value);
    let (rendered_other, value) = render_checked_select_comparison_rhs(other, render_context)?;
    let rendered_other = number_without_quotes(rendered_other, &value);
    let words = matches!(fallback_value, CheckedSelectComparisonRhs::Text(_));
    if words
        && !matches!(
            value,
            CheckedSelectComparisonRhs::Text(_) | CheckedSelectComparisonRhs::Null
        )
    {
        return unsupported("COALESCE comparison of a word with something not written out");
    }
    let collation = if words {
        render_context
            .collation_sensitive_call_columns
            .push(column.value.clone());
        " COLLATE MYSQL_UCA9_AI_CI"
    } else {
        ""
    };
    let rendered_column = match qualifier {
        Some(qualifier) => format!("{}.{}", render_ident(qualifier), render_ident(column)),
        None => render_ident(column),
    };
    let held = |operator, rhs| CheckedSelectComparison {
        qualifier: qualifier.map(|qualifier| qualifier.value.clone()),
        inner_sources: Vec::new(),
        column_name: column.value.clone(),
        operator,
        rhs,
        collated: words,
        answers: None,
    };
    render_context.checked_comparisons.extend([
        held(
            operator,
            CheckedSelectComparisonRhs::Operand(crate::CheckedComparisonOperand::Fallback),
        ),
        held(CheckedSelectComparisonOperator::Equal, fallback_value),
        held(operator, value),
    ]);
    Ok(format!(
        "(coalesce({rendered_column}, {rendered_fallback}){collation} {} {rendered_other})",
        checked_select_comparison_sql_operator(op)
    ))
}

/// Renders `col + n op other`, and the other arithmetic
/// `arithmetic_over_columns` reads.
///
/// The answer is a whole number, compared with a number written out: measured
/// on 8.4.11, `age + 1 > 10` over an `INT` finds the rows past nine.
fn render_comparison_over_arithmetic(
    expression: &Expr,
    (op, operator): (&BinaryOperator, CheckedSelectComparisonOperator),
    other: &Expr,
    render_context: &mut SelectRenderContext<'_>,
) -> Result<String, ParseError> {
    let columns = arithmetic_over_columns(expression).expect("the side was read as arithmetic");
    let (rendered_other, value) = render_checked_select_comparison_rhs(other, render_context)?;
    if !matches!(
        value,
        CheckedSelectComparisonRhs::SignedInteger(_)
            | CheckedSelectComparisonRhs::Decimal(_)
            | CheckedSelectComparisonRhs::Null
    ) {
        return unsupported("arithmetic comparison with something other than a written number");
    }
    let rendered_other = number_without_quotes(rendered_other, &value);
    let Expr::BinaryOp {
        left,
        op: arithmetic,
        right,
    } = expression
    else {
        unreachable!("arithmetic is one binary operator");
    };
    let rendered = format!(
        "({} {arithmetic} {})",
        render_compared_arithmetic_operand(left),
        render_compared_arithmetic_operand(right)
    );
    for (qualifier, column) in columns {
        render_context
            .checked_comparisons
            .push(CheckedSelectComparison {
                qualifier: qualifier.map(|qualifier| qualifier.value.clone()),
                inner_sources: Vec::new(),
                column_name: column.value.clone(),
                operator,
                rhs: CheckedSelectComparisonRhs::Operand(
                    crate::CheckedComparisonOperand::Arithmetic,
                ),
                collated: false,
                answers: None,
            });
    }
    render_context
        .checked_comparisons
        .push(CheckedSelectComparison {
            qualifier: None,
            inner_sources: Vec::new(),
            column_name: String::new(),
            operator,
            rhs: value,
            collated: false,
            answers: Some(crate::CheckedComparisonAnswer::WholeNumber),
        });
    Ok(format!(
        "({rendered} {} {rendered_other})",
        checked_select_comparison_sql_operator(op)
    ))
}

fn render_compared_arithmetic_operand(expr: &Expr) -> String {
    match named_column(expr) {
        Some((Some(qualifier), column)) => {
            format!("{}.{}", render_ident(qualifier), render_ident(column))
        }
        Some((None, column)) => render_ident(column),
        None => expr.to_string(),
    }
}

/// Writes a number with a fraction out bare where the value reader quotes it.
///
/// The reader quotes one so that a column's own affinity reads it as the
/// number it names. A call or an expression has no affinity, and the engine
/// would compare a number with the text.
fn number_without_quotes(rendered: String, value: &CheckedSelectComparisonRhs) -> String {
    match value {
        CheckedSelectComparisonRhs::Decimal(written) => format!("({written})"),
        _ => rendered,
    }
}

/// Reads the column an expression names, qualified or not.
fn named_column(expr: &Expr) -> Option<(Option<&Ident>, &Ident)> {
    match expr {
        Expr::Identifier(column) => Some((None, column)),
        Expr::CompoundIdentifier(parts) if parts.len() == 2 => Some((Some(&parts[0]), &parts[1])),
        _ => None,
    }
}

/// Renders one column compared with another — `WHERE age > score`.
///
/// Nothing is written beside either column. The frontend takes only two
/// columns that MySQL and the engine compare the same way as they stand: two
/// words under one collation, which the engine reads off the left column as
/// MySQL reads it off both, or two numbers or two moments of one kind.
fn render_column_pair_comparison(
    (qualifier, column): (Option<&Ident>, &Ident),
    op: &BinaryOperator,
    (other_qualifier, other_column): (Option<&Ident>, &Ident),
    render_context: &mut SelectRenderContext<'_>,
) -> String {
    let operator = checked_select_comparison_operator(op).expect("comparison operator guard");
    let rendered = |qualifier: Option<&Ident>, column: &Ident| match qualifier {
        Some(qualifier) => format!("{}.{}", render_ident(qualifier), render_ident(column)),
        None => render_ident(column),
    };
    let comparison = format!(
        "({} {} {})",
        rendered(qualifier, column),
        checked_select_comparison_sql_operator(op),
        rendered(other_qualifier, other_column)
    );
    render_context
        .checked_comparisons
        .push(CheckedSelectComparison {
            qualifier: qualifier.map(|qualifier| qualifier.value.clone()),
            inner_sources: Vec::new(),
            column_name: column.value.clone(),
            operator,
            rhs: CheckedSelectComparisonRhs::Column {
                qualifier: other_qualifier.map(|qualifier| qualifier.value.clone()),
                name: other_column.value.clone(),
            },
            collated: false,
            answers: None,
        });
    comparison
}

/// What a call reading a column answers, when it is one a comparison takes.
///
/// A call reading nothing — `CURDATE()`, `NOW() - INTERVAL 1 DAY` — is a value
/// the value reader takes, and is left to it.
fn answer_of_a_call_reading_a_column(expr: &Expr) -> Option<crate::CheckedComparisonAnswer> {
    let Some(StaticSelectMetadata::ScalarCall { columns, .. }) =
        static_select_metadata::classify_static_select_expr(expr)
    else {
        return None;
    };
    if columns.is_empty() {
        return None;
    }
    static_select_metadata::comparison_answer(expr).filter(|answers| {
        matches!(
            answers,
            crate::CheckedComparisonAnswer::Text
                | crate::CheckedComparisonAnswer::WholeNumber
                | crate::CheckedComparisonAnswer::Day
                | crate::CheckedComparisonAnswer::Moment
        )
    })
}

/// Renders a column compared with a call — `WHERE email = LOWER(name)`.
///
/// The column is held to the kind the call answers. Two words compare under
/// the collation both carry, which the frontend holds to `utf8mb4_0900_ai_ci`
/// for the column and for every column the call reads, as it does for a call
/// against a written word.
fn render_column_against_a_call(
    (qualifier, column): (Option<&Ident>, &Ident),
    op: &BinaryOperator,
    (call, answers): (&Expr, crate::CheckedComparisonAnswer),
    render_context: &mut SelectRenderContext<'_>,
) -> Result<String, ParseError> {
    let operator = checked_select_comparison_operator(op).expect("comparison operator guard");
    let rendered_call = render_select_expr(call, render_context)?;
    let collated = answers == crate::CheckedComparisonAnswer::Text;
    let collation = if collated {
        record_the_columns_a_text_call_reads(call, render_context);
        render_context
            .collation_sensitive_call_columns
            .push(column.value.clone());
        " COLLATE MYSQL_UCA9_AI_CI"
    } else {
        ""
    };
    let rendered_column = match qualifier {
        Some(qualifier) => format!("{}.{}", render_ident(qualifier), render_ident(column)),
        None => render_ident(column),
    };
    render_context
        .checked_comparisons
        .push(CheckedSelectComparison {
            qualifier: qualifier.map(|qualifier| qualifier.value.clone()),
            inner_sources: Vec::new(),
            column_name: column.value.clone(),
            operator,
            rhs: CheckedSelectComparisonRhs::Call(answers),
            collated,
            answers: None,
        });
    Ok(format!(
        "({rendered_column} {} {rendered_call}{collation})",
        checked_select_comparison_sql_operator(op)
    ))
}

/// Reads a written day against a column holding a moment as that day's
/// midnight, which is what MySQL reads it as.
///
/// A `DATETIME` is held as `2026-01-05 10:00:00` and a `DATE` as `2026-01-05`,
/// so `at > '2026-01-01'` means one thing over the first and another over the
/// second — measured on 8.4.11, `at > '2026-01-01'` over a `DATETIME` holding
/// exactly `2026-01-01 00:00:00` answers no row, where reading the two as text
/// would answer one. Only the frontend can see which kind the column is, so a
/// statement writing a day says so and is rendered a second time knowing.
///
/// The written day is left alone over every other column, a `DATE` among them,
/// where it is already the form the column holds.
fn midnight_of_a_written_day(
    rendered: String,
    rhs: CheckedSelectComparisonRhs,
    column_name: &str,
    render_context: &mut SelectRenderContext<'_>,
) -> (String, CheckedSelectComparisonRhs) {
    let CheckedSelectComparisonRhs::Text(written) = &rhs else {
        return (rendered, rhs);
    };
    if crate::normalize_date(written).as_deref() != Some(written.as_str()) {
        return (rendered, rhs);
    }
    render_context.compares_a_written_day = true;
    if !render_context.is_moment_column(column_name) {
        return (rendered, rhs);
    }
    let Some(midnight) = crate::normalize_datetime(written) else {
        return (rendered, rhs);
    };
    (
        format!("'{}'", midnight.replace('\'', "''")),
        CheckedSelectComparisonRhs::Text(midnight),
    )
}

/// Reads a word against a column holding numbers as the number it names, which
/// is what MySQL reads it as.
///
/// Measured on MySQL 8.4.11: against a whole-number column MySQL reads a word
/// naming a whole number as exactly that number — `big = '9007199254740993'`
/// finds that row and not its neighbour, which a comparison between doubles
/// would — and against a `DECIMAL` it reads a word naming a decimal exactly
/// too. Only the frontend can see which kind the column is, so a statement
/// writing such a word says so and is rendered a second time knowing. Against
/// any other column, or a whole-number column with a word carrying a point,
/// the word is left alone, and the frontend holds it to the column as ever.
fn number_a_written_word_names(
    rendered: String,
    rhs: CheckedSelectComparisonRhs,
    column_name: &str,
    render_context: &mut SelectRenderContext<'_>,
) -> (String, CheckedSelectComparisonRhs) {
    let CheckedSelectComparisonRhs::Text(written) = &rhs else {
        return (rendered, rhs);
    };
    let Some(number) = crate::read_written_number(written) else {
        return (rendered, rhs);
    };
    render_context.compares_a_written_number = true;
    let whole_number_column = render_context
        .integer_columns
        .iter()
        .any(|column| column.eq_ignore_ascii_case(column_name));
    let decimal_column = !whole_number_column
        && render_context
            .decimal_columns
            .iter()
            .any(|(column, _)| column.eq_ignore_ascii_case(column_name));
    match number {
        crate::WrittenNumber::Whole(value) if whole_number_column || decimal_column => (
            value.to_string(),
            CheckedSelectComparisonRhs::SignedInteger(value),
        ),
        crate::WrittenNumber::Decimal(number) if decimal_column => (
            format!("'{number}'"),
            CheckedSelectComparisonRhs::Decimal(number),
        ),
        _ => (rendered, rhs),
    }
}

/// Reads a word against a call answering a whole number as the number it
/// names.
///
/// MySQL compares a call's answer with a word as two doubles — measured on
/// 8.4.11, `YEAR(created_at) = '2026'` finds the rows of 2026 — which is the
/// exact comparison for every whole number a double holds exactly. So only a
/// word naming a whole number nearer zero than 2^53 is read.
fn whole_number_a_written_word_names(
    rendered: String,
    rhs: CheckedSelectComparisonRhs,
) -> (String, CheckedSelectComparisonRhs) {
    const EXACT_IN_A_DOUBLE: u64 = 1 << 53;
    let CheckedSelectComparisonRhs::Text(written) = &rhs else {
        return (rendered, rhs);
    };
    match crate::read_written_number(written) {
        Some(crate::WrittenNumber::Whole(value)) if value.unsigned_abs() < EXACT_IN_A_DOUBLE => (
            value.to_string(),
            CheckedSelectComparisonRhs::SignedInteger(value),
        ),
        _ => (rendered, rhs),
    }
}

/// Renders a comparison against a subquery answering one value, or nothing
/// when the comparison is not that shape.
///
/// `WHERE n = (SELECT MAX(n) FROM t)` is how a statement asks for the row
/// holding the highest of something, and `WHERE (SELECT COUNT(*) FROM c) > 0`
/// how it asks whether another table holds anything at all. Both are answered
/// the same way by MySQL and the engine.
///
/// What makes them safe is that an aggregate over one implicit group answers
/// exactly one row. A plain column does not: measured on MySQL 8.4.11,
/// `id = (SELECT parent_id FROM child)` over two child rows answers 1242 where
/// the engine takes the first row it finds, so that shape is refused — unless
/// the subquery picks its row by a key, as Prisma's cursor does with
/// `posts.id >= (SELECT posts.id FROM posts WHERE posts.id = ?)`, which never
/// finds two.
fn render_comparison_over_a_scalar_subquery(
    left: &Expr,
    op: &BinaryOperator,
    right: &Expr,
    render_context: &mut SelectRenderContext<'_>,
) -> Result<Option<String>, ParseError> {
    let (query, other) = match (left, right) {
        (Expr::Subquery(query), other) | (other, Expr::Subquery(query)) => (query, other),
        _ => return Ok(None),
    };
    let SetExpr::Select(select) = query.body.as_ref() else {
        return Ok(None);
    };
    let Some(answered) = subquery_answering_one_value(select) else {
        return Ok(None);
    };
    if let ScalarSubqueryAnswer::AnExactAverage(inner_column_name) = &answered {
        return render_comparison_against_an_average(
            left,
            op,
            query,
            other,
            inner_column_name,
            render_context,
        );
    }
    let rendered_other = match (&answered, other) {
        // MIN and MAX answer the column's own kind, and so does a column read
        // out of the one row a key picks, so the two columns are held to the
        // rule `column IN (SELECT column ...)` holds them to.
        (
            ScalarSubqueryAnswer::TheColumnsOwnKind(_) | ScalarSubqueryAnswer::OneRowsColumn { .. },
            Expr::Identifier(column),
        ) => render_ident(column),
        (ScalarSubqueryAnswer::OneRowsColumn { .. }, Expr::CompoundIdentifier(parts))
            if parts.len() == 2 =>
        {
            format!("{}.{}", render_ident(&parts[0]), render_ident(&parts[1]))
        }
        // A COUNT answers a whole number whatever it counts, so it meets a
        // whole number written out and nothing else.
        (ScalarSubqueryAnswer::AWholeNumber, other) if names_a_whole_number(other) => {
            render_dml_expr(other)?
        }
        _ => return Ok(None),
    };
    let Some(inner_table) = subquery_source_table(select) else {
        return Ok(None);
    };
    let (rendered_subquery, _) = render_subquery(query, render_context)?;
    let (inner_column_name, fixed_columns) = match &answered {
        ScalarSubqueryAnswer::TheColumnsOwnKind(inner_column_name) => {
            (Some(inner_column_name), Vec::new())
        }
        ScalarSubqueryAnswer::OneRowsColumn { column, fixed } => (Some(column), fixed.clone()),
        _ => (None, Vec::new()),
    };
    let outer_column = match other {
        Expr::Identifier(column) => Some((None, column)),
        Expr::CompoundIdentifier(parts) if parts.len() == 2 => {
            Some((Some(parts[0].value.clone()), &parts[1]))
        }
        _ => None,
    };
    if let (Some(inner_column_name), Some((qualifier, column))) = (inner_column_name, outer_column)
    {
        render_context
            .checked_subquery_comparisons
            .push(CheckedSubqueryComparison {
                qualifier,
                column_name: column.value.clone(),
                inner_table: inner_table.as_str().to_owned(),
                inner_column_name: inner_column_name.clone(),
                fixed_columns,
            });
    }
    let rendered_subquery = format!("({rendered_subquery})");
    let (rendered_left, rendered_right) = if matches!(left, Expr::Subquery(_)) {
        (rendered_subquery, rendered_other)
    } else {
        (rendered_other, rendered_subquery)
    };
    Ok(Some(format!(
        "({rendered_left} {} {rendered_right})",
        checked_select_comparison_sql_operator(op)
    )))
}

/// Renders a column compared against `(SELECT AVG(col) FROM ...)`, which is
/// how a statement asks for the rows above average.
///
/// MySQL answers `AVG` over a whole number as a decimal rounded to four
/// places, and compares the column against that decimal. The engine's float
/// average keeps the whole fraction, so a row could land on the other side of
/// it; the engine's `mysql_decimal_avg` answers MySQL's decimal, and
/// `numeric_lt` and `numeric_eq` compare the two as the exact numbers they
/// are, answering NULL where either is NULL. Both columns are held to whole
/// numbers, which is all this has measured.
fn render_comparison_against_an_average(
    left: &Expr,
    op: &BinaryOperator,
    query: &sqlparser::ast::Query,
    other: &Expr,
    inner_column_name: &str,
    render_context: &mut SelectRenderContext<'_>,
) -> Result<Option<String>, ParseError> {
    let (qualifier, column) = match other {
        Expr::Identifier(column) => (None, column),
        Expr::CompoundIdentifier(parts) if parts.len() == 2 => {
            (Some(parts[0].value.clone()), &parts[1])
        }
        _ => return Ok(None),
    };
    let Some(inner_table) = subquery_source_table(match query.body.as_ref() {
        SetExpr::Select(select) => select,
        _ => return Ok(None),
    }) else {
        return Ok(None);
    };
    let averages_exactly = std::mem::replace(&mut render_context.averages_exactly, true);
    let rendered = render_subquery(query, render_context);
    render_context.averages_exactly = averages_exactly;
    let (rendered_subquery, _) = rendered?;
    let inner_reference = render_context
        .subquery_tables
        .iter()
        .rev()
        .find(|source| source.table == inner_table)
        .map(|source| source.reference.clone())
        .ok_or(ParseError::Unsupported {
            feature: "SELECT comparison against an average of an unknown table",
        })?;
    let whole_number = |qualifier, column_name: &str| CheckedSelectComparison {
        qualifier,
        inner_sources: Vec::new(),
        column_name: column_name.to_owned(),
        operator: CheckedSelectComparisonOperator::Equal,
        rhs: CheckedSelectComparisonRhs::SignedInteger(1),
        collated: false,
        answers: None,
    };
    render_context
        .checked_comparisons
        .push(whole_number(qualifier, &column.value));
    render_context
        .checked_comparisons
        .push(whole_number(Some(inner_reference), inner_column_name));
    let rendered_column = render_select_expr(other, render_context)?;
    let average = format!("({rendered_subquery})");
    // The operator reads left to right, whichever side the subquery is on.
    let (lhs, rhs) = if matches!(left, Expr::Subquery(_)) {
        (average, rendered_column)
    } else {
        (rendered_column, average)
    };
    Ok(Some(match op {
        BinaryOperator::Lt => format!("numeric_lt({lhs}, {rhs})"),
        BinaryOperator::Gt => format!("numeric_lt({rhs}, {lhs})"),
        BinaryOperator::LtEq => format!("(NOT numeric_lt({rhs}, {lhs}))"),
        BinaryOperator::GtEq => format!("(NOT numeric_lt({lhs}, {rhs}))"),
        BinaryOperator::Eq => format!("numeric_eq({lhs}, {rhs})"),
        BinaryOperator::NotEq => format!("(NOT numeric_eq({lhs}, {rhs}))"),
        _ => return unsupported("SELECT comparison against an average with this operator"),
    }))
}

/// What a subquery standing where a value stands answers.
enum ScalarSubqueryAnswer {
    /// `MIN(c)` or `MAX(c)`, which answer `c`'s own kind.
    TheColumnsOwnKind(String),
    /// `COUNT(...)`, which answers a whole number whatever it counts.
    AWholeNumber,
    /// `AVG(c)`, which MySQL answers as a decimal four places past `c`'s own.
    AnExactAverage(String),
    /// `c` itself, read out of the rows whose `fixed` columns the `WHERE`
    /// fixes to one value each. It is one row at most only when those columns
    /// cover a key, which the frontend checks.
    OneRowsColumn { column: String, fixed: Vec<String> },
}

/// Reads what a subquery answers, when it answers one value at most.
///
/// An aggregate over one implicit group answers exactly one. `SUM` is left
/// out: it has not been measured against a column. A plain column answers one
/// when its `WHERE` fixes a key of the table to one value.
fn subquery_answering_one_value(select: &sqlparser::ast::Select) -> Option<ScalarSubqueryAnswer> {
    let sqlparser::ast::GroupByExpr::Expressions(group_by, _) = &select.group_by else {
        return None;
    };
    if !group_by.is_empty() || select.having.is_some() || select.distinct.is_some() {
        return None;
    }
    let [SelectItem::UnnamedExpr(projected)] = select.projection.as_slice() else {
        return None;
    };
    if let Some(column) = column_of_the_subquerys_table(projected, select) {
        let fixed = columns_fixed_to_one_value(select.selection.as_ref()?, select);
        return (!fixed.is_empty())
            .then_some(ScalarSubqueryAnswer::OneRowsColumn { column, fixed });
    }
    let Expr::Function(function) = projected else {
        return None;
    };
    if static_select_metadata::is_count_call(function) {
        return Some(ScalarSubqueryAnswer::AWholeNumber);
    }
    let (kind, column) = static_select_metadata::column_aggregate_argument(function)?;
    match kind {
        ColumnAggregateKind::MinMax => Some(ScalarSubqueryAnswer::TheColumnsOwnKind(
            column.value.clone(),
        )),
        ColumnAggregateKind::Avg => {
            Some(ScalarSubqueryAnswer::AnExactAverage(column.value.clone()))
        }
        _ => None,
    }
}

/// Names the column `expr` reads when it is one of the subquery's own table,
/// written bare or through the name the subquery reads its table under.
fn column_of_the_subquerys_table(expr: &Expr, select: &sqlparser::ast::Select) -> Option<String> {
    match expr {
        Expr::Identifier(column) => Some(column.value.clone()),
        Expr::CompoundIdentifier(parts) => match parts.as_slice() {
            [table, column]
                if subquery_table_reference(select)
                    .is_some_and(|reference| reference.eq_ignore_ascii_case(&table.value)) =>
            {
                Some(column.value.clone())
            }
            _ => None,
        },
        Expr::Nested(inner) => column_of_the_subquerys_table(inner, select),
        _ => None,
    }
}

/// The columns a `WHERE` fixes to one written or bound value each, through
/// `AND`s alone: `a = 1 AND (b) = (?)` fixes `a` and `b`, and anything else
/// beside them only narrows the rows further. An `OR` fixes nothing.
fn columns_fixed_to_one_value(condition: &Expr, select: &sqlparser::ast::Select) -> Vec<String> {
    let mut fixed = Vec::new();
    let mut pending = vec![condition];
    while let Some(condition) = pending.pop() {
        match condition {
            Expr::Nested(inner) => pending.push(inner),
            Expr::BinaryOp {
                left,
                op: BinaryOperator::And,
                right,
            } => {
                pending.push(left);
                pending.push(right);
            }
            Expr::BinaryOp {
                left,
                op: BinaryOperator::Eq,
                right,
            } => {
                let column = [(left, right), (right, left)]
                    .into_iter()
                    .find(|(_, value)| is_one_written_or_bound_value(value))
                    .and_then(|(column, _)| column_of_the_subquerys_table(column, select));
                fixed.extend(column);
            }
            _ => {}
        }
    }
    fixed
}

fn is_one_written_or_bound_value(expr: &Expr) -> bool {
    match expr {
        Expr::Nested(inner) => is_one_written_or_bound_value(inner),
        Expr::Value(value) => matches!(
            value.value,
            Value::Number(..) | Value::SingleQuotedString(_) | Value::Placeholder(_)
        ),
        _ => false,
    }
}

/// The name a subquery reads its one table under: its alias, or the table's
/// own name.
fn subquery_table_reference(select: &sqlparser::ast::Select) -> Option<&str> {
    let [source] = select.from.as_slice() else {
        return None;
    };
    if !source.joins.is_empty() {
        return None;
    }
    let TableFactor::Table { name, alias, .. } = &source.relation else {
        return None;
    };
    if let Some(alias) = alias {
        return Some(alias.name.value.as_str());
    }
    let [ObjectNamePart::Identifier(name)] = name.0.as_slice() else {
        return None;
    };
    Some(name.value.as_str())
}

/// Names the one table a subquery reads.
fn subquery_source_table(select: &sqlparser::ast::Select) -> Option<MySqlTableName> {
    let [source] = select.from.as_slice() else {
        return None;
    };
    if !source.joins.is_empty() {
        return None;
    }
    let TableFactor::Table { name, .. } = &source.relation else {
        return None;
    };
    let [ObjectNamePart::Identifier(name)] = name.0.as_slice() else {
        return None;
    };
    MySqlTableName::parse(&name.value).ok()
}

/// Renders a comparison whose left side is a call, or nothing when it is not.
///
/// The call says what it answers, so the value it meets is held to that rather
/// than to a column's declared type. A call answering a word is compared
/// without regard to case, which is what MySQL's collation does after the call
/// has answered: measured on 8.4.11, `LOWER(name) = 'ADA'` finds the row
/// holding `Ada`.
fn render_comparison_over_a_call(
    left: &Expr,
    op: &BinaryOperator,
    right: &Expr,
    render_context: &mut SelectRenderContext<'_>,
) -> Result<Option<String>, ParseError> {
    // A column on either side is the other path's: a column says what it holds
    // through its declared type, and `WHERE d = CURDATE()` is already read
    // there with the reading on the right.
    let names_a_column = |expr: &Expr| {
        matches!(expr, Expr::Identifier(_))
            || matches!(expr, Expr::CompoundIdentifier(parts) if parts.len() == 2)
    };
    if names_a_column(left) || names_a_column(right) {
        return Ok(None);
    }
    let (call, op_reversed, rhs_expr) = match (
        static_select_metadata::comparison_answer(left),
        static_select_metadata::comparison_answer(right),
    ) {
        (Some(_), _) => (left, op.clone(), right),
        (None, Some(_)) => {
            let Some(reversed) = reverse_checked_comparison_operator(op) else {
                return Ok(None);
            };
            (right, reversed, left)
        }
        (None, None) => return Ok(None),
    };
    let answers = static_select_metadata::comparison_answer(call)
        .expect("the call was read to answer something");
    let Some(operator) = checked_select_comparison_operator(&op_reversed) else {
        return Ok(None);
    };
    if let Some(other_answers) = static_select_metadata::comparison_answer(rhs_expr) {
        return render_comparison_of_two_calls(
            (call, answers),
            &op_reversed,
            (rhs_expr, other_answers),
            render_context,
        )
        .map(Some);
    }
    let rendered_call = render_select_expr(call, render_context)?;
    let (rendered_rhs, rhs) = render_checked_select_comparison_rhs(rhs_expr, render_context)?;
    let (rendered_rhs, rhs) = if answers == crate::CheckedComparisonAnswer::WholeNumber {
        whole_number_a_written_word_names(rendered_rhs, rhs)
    } else {
        (rendered_rhs, rhs)
    };
    let collated = answers == crate::CheckedComparisonAnswer::Text
        && matches!(rhs, CheckedSelectComparisonRhs::Text(_));
    let collation = if collated {
        record_the_columns_a_text_call_reads(call, render_context);
        " COLLATE MYSQL_UCA9_AI_CI"
    } else {
        ""
    };
    let rendered = format!(
        "({rendered_call}{collation} {} {rendered_rhs})",
        checked_select_comparison_sql_operator(&op_reversed)
    );
    render_context
        .checked_comparisons
        .push(CheckedSelectComparison {
            qualifier: None,
            inner_sources: Vec::new(),
            // The call reads a column, and naming it is what an error message
            // needs; what the value is held to is the answer beside it.
            column_name: String::new(),
            operator,
            rhs,
            collated,
            answers: Some(answers),
        });
    Ok(Some(rendered))
}

/// Renders one call compared with another — `LOWER(name) = LOWER('ANN')`.
///
/// The two have to answer one kind. Two words are compared without regard to
/// case, as a word against a written word is: measured on 8.4.11, MySQL takes
/// the collation of the column a call reads over a written word's, and one
/// column's over another's when the two are the same, and the frontend holds
/// every column either call reads to that one collation.
fn render_comparison_of_two_calls(
    (call, answers): (&Expr, crate::CheckedComparisonAnswer),
    op: &BinaryOperator,
    (other, other_answers): (&Expr, crate::CheckedComparisonAnswer),
    render_context: &mut SelectRenderContext<'_>,
) -> Result<String, ParseError> {
    use crate::CheckedComparisonAnswer;

    if answers != other_answers
        || !matches!(
            answers,
            CheckedComparisonAnswer::Text
                | CheckedComparisonAnswer::WholeNumber
                | CheckedComparisonAnswer::Day
                | CheckedComparisonAnswer::Moment
        )
    {
        return unsupported("SELECT comparison of two calls answering different kinds");
    }
    let operator = checked_select_comparison_operator(op).expect("comparison operator guard");
    let rendered_call = render_select_expr(call, render_context)?;
    let rendered_other = render_select_expr(other, render_context)?;
    let collated = answers == CheckedComparisonAnswer::Text;
    let collation = if collated {
        record_the_columns_a_text_call_reads(call, render_context);
        record_the_columns_a_text_call_reads(other, render_context);
        " COLLATE MYSQL_UCA9_AI_CI"
    } else {
        ""
    };
    render_context
        .checked_comparisons
        .push(CheckedSelectComparison {
            qualifier: None,
            inner_sources: Vec::new(),
            column_name: String::new(),
            operator,
            rhs: CheckedSelectComparisonRhs::Call(other_answers),
            collated,
            answers: Some(answers),
        });
    Ok(format!(
        "({rendered_call}{collation} {} {rendered_other})",
        checked_select_comparison_sql_operator(op)
    ))
}

/// Renders a `LIKE` against one text column using MySQL's Unicode 9 weights.
fn render_checked_like(
    negated: bool,
    any: bool,
    expr: &Expr,
    pattern: &Expr,
    escape_char: Option<&sqlparser::ast::ValueWithSpan>,
    render_context: &mut SelectRenderContext<'_>,
) -> Result<String, ParseError> {
    if any {
        return unsupported("SELECT LIKE option");
    }
    // MySQL takes a character to escape `%` and `_`. NO_BACKSLASH_ESCAPES
    // removes the usual implicit backslash escape.
    let escape = match escape_char {
        Some(named) => {
            let Value::SingleQuotedString(named) = &named.value else {
                return unsupported("SELECT LIKE ESCAPE requires a written character");
            };
            let [character] = named.chars().collect::<Vec<_>>()[..] else {
                return unsupported("SELECT LIKE ESCAPE requires one character");
            };
            Some(character)
        }
        None if render_context.no_backslash_escapes => None,
        None => Some('\\'),
    };
    let escape_argument = match escape {
        Some(character) => format!("'{}'", character.to_string().replace('\'', "''")),
        None => "''".to_owned(),
    };
    if let Some(rendered) = json_condition::render_like_over_a_json_reading(
        negated,
        expr,
        pattern,
        &escape_argument,
        render_context,
    )? {
        return Ok(rendered);
    }
    let (qualifier, column) = match expr {
        Expr::Identifier(ident) => (None, ident),
        Expr::CompoundIdentifier(parts) if parts.len() == 2 => (Some(&parts[0]), &parts[1]),
        _ => return unsupported("SELECT LIKE requires one column"),
    };
    render_context.checks_type_sensitive_expression = true;
    if render_context
        .decimal_columns
        .iter()
        .any(|(known, _)| known.eq_ignore_ascii_case(&column.value))
    {
        return unsupported("SELECT LIKE over DECIMAL requires decimal text conversion");
    }
    let (rendered_pattern, rhs) = render_like_pattern(pattern, render_context)?;
    let rendered_column = match qualifier {
        Some(qualifier) => format!("{}.{}", render_ident(qualifier), render_ident(column)),
        None => render_ident(column),
    };
    let matched =
        format!("mysql_uca9_like({rendered_column}, {rendered_pattern}, {escape_argument})");
    let rendered = if negated {
        format!("(NOT {matched})")
    } else {
        format!("({matched})")
    };
    render_context
        .checked_comparisons
        .push(CheckedSelectComparison {
            qualifier: qualifier.map(|q| q.value.clone()),
            inner_sources: Vec::new(),
            column_name: column.value.clone(),
            operator: if negated {
                CheckedSelectComparisonOperator::NotLike
            } else {
                CheckedSelectComparisonOperator::Like
            },
            rhs,
            collated: false,
            answers: None,
        });
    Ok(rendered)
}

/// Renders a `LIKE` pattern, written or bound, and says which it was.
pub(super) fn render_like_pattern(
    pattern: &Expr,
    render_context: &mut SelectRenderContext<'_>,
) -> Result<(String, CheckedSelectComparisonRhs), ParseError> {
    let Some(pieces) = like_pattern_pieces(pattern) else {
        return unsupported("SELECT LIKE requires a string pattern");
    };
    // A pattern is written or it is bound, and either may come in pieces. A
    // bound one carries no text until it binds, so what a written one is
    // checked for here is checked there instead.
    let mut written = String::new();
    let mut rendered_pieces = Vec::with_capacity(pieces.len());
    let mut bound = None;
    for piece in &pieces {
        match piece {
            LikePatternPiece::Written(text) => {
                written.push_str(text);
                rendered_pieces.push(format!("'{}'", text.replace('\'', "''")));
            }
            LikePatternPiece::Bound => {
                if bound.is_some() {
                    return unsupported("SELECT LIKE pattern binding more than one value");
                }
                bound = Some(render_context.next_parameter_ordinal()?);
                rendered_pieces.push("?".to_owned());
            }
        }
    }
    let rhs = match bound {
        Some(ordinal) => CheckedSelectComparisonRhs::Placeholder { ordinal },
        None => CheckedSelectComparisonRhs::Text(written.clone()),
    };
    // Pieces that are all written join into the one pattern they spell, which
    // is the pattern MySQL matches. A bound piece has to stay a piece.
    let rendered_pattern = match (bound.is_some(), rendered_pieces.len()) {
        (false, _) => format!("'{}'", written.replace('\'', "''")),
        (true, 1) => "?".to_owned(),
        (true, _) => format!("({})", rendered_pieces.join(" || ")),
    };
    Ok((rendered_pattern, rhs))
}

/// One piece of a `LIKE` pattern.
enum LikePatternPiece {
    Written(String),
    Bound,
}

/// Reads a `LIKE` pattern into the pieces it is written in.
///
/// `CONCAT('%', ?, '%')` is how a statement wraps a value it binds in
/// wildcards, and it spells the same pattern the pieces spell joined up:
/// measured on MySQL 8.4.11, `LIKE CONCAT('%', 'lph', '%')` and `LIKE '%lph%'`
/// answer the same rows. A piece naming a column is refused — the pattern
/// would then be a different one for every row, which nothing here measures.
fn like_pattern_pieces(pattern: &Expr) -> Option<Vec<LikePatternPiece>> {
    match pattern {
        Expr::Value(value) => like_pattern_piece(&value.value).map(|piece| vec![piece]),
        Expr::Function(function) if names_concat(function) => {
            let sqlparser::ast::FunctionArguments::List(arguments) = &function.args else {
                return None;
            };
            if arguments.args.is_empty() {
                return None;
            }
            arguments
                .args
                .iter()
                .map(|argument| {
                    let sqlparser::ast::FunctionArg::Unnamed(
                        sqlparser::ast::FunctionArgExpr::Expr(Expr::Value(value)),
                    ) = argument
                    else {
                        return None;
                    };
                    like_pattern_piece(&value.value)
                })
                .collect()
        }
        _ => None,
    }
}

fn like_pattern_piece(value: &Value) -> Option<LikePatternPiece> {
    match value {
        Value::SingleQuotedString(text) | Value::DoubleQuotedString(text) => {
            Some(LikePatternPiece::Written(text.clone()))
        }
        Value::Placeholder(marker) if marker == "?" => Some(LikePatternPiece::Bound),
        _ => None,
    }
}

/// Reports whether a call is a plain `CONCAT`, which is the only call a
/// pattern is built with here.
fn names_concat(function: &sqlparser::ast::Function) -> bool {
    let [ObjectNamePart::Identifier(name)] = function.name.0.as_slice() else {
        return false;
    };
    name.quote_style.is_none()
        && name.value.eq_ignore_ascii_case("CONCAT")
        && function.over.is_none()
        && function.filter.is_none()
        && function.null_treatment.is_none()
        && function.within_group.is_empty()
}

/// Renders `column REGEXP 'pattern'`, which the dialect answers.
///
/// The column has to be one holding text: measured on MySQL 8.4.11 the match
/// follows the column's collation, and text is what carries one here. The
/// pattern has to be written out, because what it spells is the whole of what
/// the match answers and a bound one carries nothing until it binds.
fn render_checked_regexp(
    negated: bool,
    expr: &Expr,
    pattern: &Expr,
    render_context: &mut SelectRenderContext<'_>,
) -> Result<String, ParseError> {
    let (qualifier, column) = match expr {
        Expr::Identifier(ident) => (None, ident),
        Expr::CompoundIdentifier(parts) if parts.len() == 2 => (Some(&parts[0]), &parts[1]),
        _ => return unsupported("SELECT REGEXP requires one column"),
    };
    render_context.checks_type_sensitive_expression = true;
    if render_context
        .decimal_columns
        .iter()
        .any(|(known, _)| known.eq_ignore_ascii_case(&column.value))
    {
        return unsupported("SELECT REGEXP over DECIMAL requires decimal text conversion");
    }
    let Expr::Value(value) = pattern else {
        return unsupported("SELECT REGEXP requires a written pattern");
    };
    let (Value::SingleQuotedString(written) | Value::DoubleQuotedString(written)) = &value.value
    else {
        return unsupported("SELECT REGEXP requires a written pattern");
    };
    let rendered_column = match qualifier {
        Some(qualifier) => format!("{}.{}", render_ident(qualifier), render_ident(column)),
        None => render_ident(column),
    };
    render_context
        .checked_comparisons
        .push(CheckedSelectComparison {
            qualifier: qualifier.map(|qualifier| qualifier.value.clone()),
            inner_sources: Vec::new(),
            column_name: column.value.clone(),
            operator: CheckedSelectComparisonOperator::Like,
            rhs: CheckedSelectComparisonRhs::Text(written.clone()),
            collated: false,
            answers: None,
        });
    let matched = format!(
        "mysql_regexp({rendered_column}, '{}')",
        written.replace('\'', "''")
    );
    Ok(if negated {
        format!("(NOT {matched})")
    } else {
        format!("({matched})")
    })
}

fn render_checked_select_comparison_rhs(
    expr: &Expr,
    render_context: &mut SelectRenderContext<'_>,
) -> Result<(String, CheckedSelectComparisonRhs), ParseError> {
    render_checked_select_comparison_rhs_allowing_large_integer(expr, render_context, false)
}

fn render_checked_select_comparison_rhs_allowing_large_integer(
    expr: &Expr,
    render_context: &mut SelectRenderContext<'_>,
    allow_large_integer: bool,
) -> Result<(String, CheckedSelectComparisonRhs), ParseError> {
    match expr {
        _ if crate::written_value::written_byte_string(expr).is_some() => {
            let rendered = crate::written_value::written_byte_string(expr)
                .expect("the guard read a string of bytes")
                .ok_or(ParseError::Unsupported {
                    feature: "a string of bytes spelled some other way",
                })?;
            Ok((rendered, CheckedSelectComparisonRhs::Bytes))
        }
        Expr::Nested(expr) => {
            let (rendered, rhs) = render_checked_select_comparison_rhs_allowing_large_integer(
                expr,
                render_context,
                allow_large_integer,
            )?;
            Ok((format!("({rendered})"), rhs))
        }
        Expr::Value(value) => match &value.value {
            Value::Number(number, false) if is_written_as_a_whole_number(number) => {
                if let Ok(value) = number.parse::<i64>() {
                    return Ok((
                        value.to_string(),
                        CheckedSelectComparisonRhs::SignedInteger(value),
                    ));
                }
                large_integer_comparison(number, "", render_context, allow_large_integer)
            }
            Value::Number(number, false) => Ok((
                format!("'{number}'"),
                CheckedSelectComparisonRhs::Decimal(checked_decimal_literal(number)?),
            )),
            Value::SingleQuotedString(text) | Value::DoubleQuotedString(text) => Ok((
                format!("'{}'", text.replace('\'', "''")),
                CheckedSelectComparisonRhs::Text(text.clone()),
            )),
            Value::Null => Ok(("NULL".to_string(), CheckedSelectComparisonRhs::Null)),
            // MySQL's `TRUE` and `FALSE` are the integers 1 and 0, which is
            // how Rails writes every boolean it compares.
            Value::Boolean(value) => Ok((
                u8::from(*value).to_string(),
                CheckedSelectComparisonRhs::SignedInteger(i64::from(*value)),
            )),
            Value::Placeholder(marker) if marker == "?" => {
                let ordinal = render_context.next_parameter_ordinal()?;
                Ok((
                    "?".to_string(),
                    CheckedSelectComparisonRhs::Placeholder { ordinal },
                ))
            }
            _ => unsupported(
                "SELECT comparison requires an exact signed integer, a string, NULL, or ?",
            ),
        },
        // `LOWER('ANN')` is the word it answers, written out: the value is
        // then held to the column it meets the way that word would be.
        Expr::Function(function) if case_of_a_written_word(function).is_some() => {
            let word = case_of_a_written_word(function)
                .expect("the guard requires a written word in another case");
            Ok((
                format!("'{}'", word.replace('\'', "''")),
                CheckedSelectComparisonRhs::Text(word),
            ))
        }
        Expr::Function(function) if CheckedComparisonNow::read(function).is_some() => {
            let now = CheckedComparisonNow::read(function)
                .expect("the guard requires a call answering the moment");
            Ok((
                now.engine_call().to_owned(),
                CheckedSelectComparisonRhs::Now(now),
            ))
        }
        // A shift of one of those readings is read the same way: what matters
        // to the column it meets is which kind the shift answers, not that it
        // was shifted.
        Expr::Function(function) if render_shifted_clock_reading(function).is_some() => {
            let (rendered, answers) = render_shifted_clock_reading(function)
                .expect("the guard requires a shift of a clock reading");
            Ok((rendered, CheckedSelectComparisonRhs::Now(answers)))
        }
        // `NOW() - INTERVAL 1 DAY` is the same shift, spelled as an operator.
        Expr::BinaryOp { .. } if shifted_clock_reading_operator(expr).is_some() => {
            let (rendered, answers) = shifted_clock_reading_operator(expr)
                .expect("the guard requires a shift of a clock reading");
            Ok((rendered, CheckedSelectComparisonRhs::Now(answers)))
        }
        Expr::UnaryOp { op, expr }
            if matches!(op, UnaryOperator::Minus | UnaryOperator::Plus)
                && matches!(expr.as_ref(), Expr::Value(value) if matches!(&value.value, Value::Number(_, false))) =>
        {
            let Expr::Value(value) = expr.as_ref() else {
                unreachable!("guard requires a numeric literal");
            };
            let Value::Number(number, false) = &value.value else {
                unreachable!("guard requires a numeric literal");
            };
            if !is_written_as_a_whole_number(number) {
                let written = checked_decimal_literal(number)?;
                let sign = if matches!(op, UnaryOperator::Minus) {
                    "-"
                } else {
                    "+"
                };
                return Ok((
                    format!("'{sign}{written}'"),
                    CheckedSelectComparisonRhs::Decimal(format!("{sign}{written}")),
                ));
            }
            let magnitude = match number.parse::<u64>() {
                Ok(magnitude) => magnitude,
                Err(_) => {
                    return large_integer_comparison(
                        number,
                        if matches!(op, UnaryOperator::Minus) {
                            "-"
                        } else {
                            ""
                        },
                        render_context,
                        allow_large_integer,
                    );
                }
            };
            let value = if matches!(op, UnaryOperator::Minus) {
                if magnitude > (i64::MAX as u64) + 1 {
                    return large_integer_comparison(
                        number,
                        "-",
                        render_context,
                        allow_large_integer,
                    );
                }
                if magnitude == (i64::MAX as u64) + 1 {
                    i64::MIN
                } else {
                    -(magnitude as i64)
                }
            } else {
                match i64::try_from(magnitude) {
                    Ok(value) => value,
                    Err(_) => {
                        return large_integer_comparison(
                            number,
                            "",
                            render_context,
                            allow_large_integer,
                        );
                    }
                }
            };
            Ok((
                if matches!(op, UnaryOperator::Minus) {
                    format!("(-{magnitude})")
                } else {
                    format!("(+{magnitude})")
                },
                CheckedSelectComparisonRhs::SignedInteger(value),
            ))
        }
        _ => {
            unsupported("SELECT comparison requires an exact signed integer, a string, NULL, or ?")
        }
    }
}

/// Reads `LOWER('...')` or `UPPER('...')` over a written word as the word it
/// answers.
///
/// Only a word written in ASCII is read: MySQL changes the case of the rest
/// by Unicode rules this does not reproduce.
fn case_of_a_written_word(function: &sqlparser::ast::Function) -> Option<String> {
    let [ObjectNamePart::Identifier(name)] = function.name.0.as_slice() else {
        return None;
    };
    let lower = ["LOWER", "LCASE"]
        .iter()
        .any(|candidate| name.value.eq_ignore_ascii_case(candidate));
    let upper = ["UPPER", "UCASE"]
        .iter()
        .any(|candidate| name.value.eq_ignore_ascii_case(candidate));
    if name.quote_style.is_some() || !(lower || upper) || function.over.is_some() {
        return None;
    }
    let sqlparser::ast::FunctionArguments::List(arguments) = &function.args else {
        return None;
    };
    if arguments.duplicate_treatment.is_some() || !arguments.clauses.is_empty() {
        return None;
    }
    let [sqlparser::ast::FunctionArg::Unnamed(sqlparser::ast::FunctionArgExpr::Expr(Expr::Value(
        value,
    )))] = arguments.args.as_slice()
    else {
        return None;
    };
    let (Value::SingleQuotedString(word) | Value::DoubleQuotedString(word)) = &value.value else {
        return None;
    };
    if !word.is_ascii() {
        return None;
    }
    Some(if lower {
        word.to_ascii_lowercase()
    } else {
        word.to_ascii_uppercase()
    })
}

fn large_integer_comparison(
    number: &str,
    sign: &str,
    render_context: &mut SelectRenderContext<'_>,
    allowed: bool,
) -> Result<(String, CheckedSelectComparisonRhs), ParseError> {
    if !allowed || number.len() > 65 || !is_written_as_a_whole_number(number) {
        return unsupported("SELECT comparison literal outside signed 64-bit integer range");
    }
    render_context.compares_a_large_decimal_integer = true;
    let written = format!("{sign}{number}");
    Ok((
        format!("'{written}'"),
        CheckedSelectComparisonRhs::Decimal(written),
    ))
}

/// Reports whether a number was written as a run of digits and nothing else.
///
/// One that was is read as a signed integer, which is exact; one written with
/// a fraction or an exponent is read as the number it names, which is not.
fn is_written_as_a_whole_number(number: &str) -> bool {
    !number.is_empty() && number.bytes().all(|byte| byte.is_ascii_digit())
}

/// Holds a number written with a fraction or an exponent to one the engine
/// reads as the same number.
fn checked_decimal_literal(number: &str) -> Result<String, ParseError> {
    if !number.parse::<f64>().is_ok_and(f64::is_finite) {
        return unsupported("SELECT comparison literal outside the range of a binary64 number");
    }
    Ok(number.to_owned())
}

/// Renders `DATE_ADD` or `DATE_SUB` over a reading of the moment, and says
/// what the shift answers.
///
/// Measured on MySQL 8.4.11: shifting `NOW()` answers a moment whatever the
/// interval named, and shifting `CURDATE()` answers a day for an interval of
/// whole days, months or years and a moment for one carrying a time. The
/// reading says which kind it is, so which of the engine's two readers to ask
/// is known here rather than worked out from what a column stored.
/// How many units one `DATE_ADD` or `DATE_SUB` counts, and which unit.
///
/// `DATE_SUB` counts the other way, and a negative count already does, so the
/// two are folded into one signed number here. A week and a quarter are
/// counted in the unit each is made of.
fn shifted_moment_count(
    name: &Ident,
    interval: &sqlparser::ast::Interval,
) -> Option<(i64, &'static str)> {
    let (unit, _, of_each) = static_select_metadata::checked_interval_unit(interval)?;
    let written = static_select_metadata::checked_interval_count(interval)?;
    let counted = written.checked_mul(of_each)?;
    let counted = if name.value.eq_ignore_ascii_case("DATE_SUB") {
        counted.checked_neg()?
    } else {
        counted
    };
    Some((counted, unit))
}

/// Renders one shift as the call that does MySQL's own month arithmetic.
///
/// The engine shifts by months the way SQLite does, which overflows a day the
/// target month has not got — measured on MySQL 8.4.11, `2026-01-31` a month
/// on is `2026-02-28` where the engine answers `2026-03-03` — so the whole
/// shift is worked out by the frontend instead. The reader also decides there
/// whether the answer is a day or a moment, which the stored text says: a
/// `DATE` is exactly the ten characters of `YYYY-MM-DD`.
fn render_shifted_moment(moment: &str, (count, unit): (i64, &'static str)) -> String {
    format!("mysql_shift_moment({moment}, {count}, '{unit}')")
}

/// Reports whether a call is `DATE_ADD` or `DATE_SUB` over a reading of the
/// clock, which a comparison reads the way a `WHERE` does.
pub(crate) fn shifts_a_reading_of_the_clock(function: &sqlparser::ast::Function) -> bool {
    render_shifted_clock_reading(function).is_some()
}

fn render_shifted_clock_reading(
    function: &sqlparser::ast::Function,
) -> Option<(String, CheckedComparisonNow)> {
    let [ObjectNamePart::Identifier(name)] = function.name.0.as_slice() else {
        return None;
    };
    let subtracts = name.value.eq_ignore_ascii_case("DATE_SUB");
    if !subtracts && !name.value.eq_ignore_ascii_case("DATE_ADD") {
        return None;
    }
    let sqlparser::ast::FunctionArguments::List(arguments) = &function.args else {
        return None;
    };
    let [sqlparser::ast::FunctionArg::Unnamed(sqlparser::ast::FunctionArgExpr::Expr(
        Expr::Function(reading),
    )), sqlparser::ast::FunctionArg::Unnamed(sqlparser::ast::FunctionArgExpr::Expr(
        Expr::Interval(interval),
    ))] = arguments.args.as_slice()
    else {
        return None;
    };
    let now = CheckedComparisonNow::read(reading)?;
    // A time of day holds a span rather than a moment, and shifting a span by
    // a month names nothing.
    if now == CheckedComparisonNow::TimeOfDay {
        return None;
    }
    let count = shifted_moment_count(name, interval)?;
    let (_, whole_days, _) = static_select_metadata::checked_interval_unit(interval)?;
    let rendered = render_shifted_moment(now.engine_call(), count);
    if whole_days && now == CheckedComparisonNow::Day {
        return Some((rendered, CheckedComparisonNow::Day));
    }
    Some((rendered, CheckedComparisonNow::Moment))
}

/// Renders `NOW() - INTERVAL 1 DAY` as the `DATE_SUB(NOW(), INTERVAL 1 DAY)`
/// it is, and says what the shift answers.
fn shifted_clock_reading_operator(expr: &Expr) -> Option<(String, CheckedComparisonNow)> {
    render_shifted_clock_reading(&static_select_metadata::interval_shift_as_call(expr)?)
}

/// Writes a column out the way one of the four cast targets answers it.
fn render_cast_target(column: &str, target: static_select_metadata::ScalarFunction) -> String {
    match target {
        static_select_metadata::ScalarFunction::CastsToText => format!("CAST({column} AS TEXT)"),
        // Measured on MySQL 8.4.11: `CAST(1.5 AS SIGNED)` answers 2 and
        // `CAST(-1.5 AS SIGNED)` answers -2, so it rounds away from zero where
        // the engine's own cast cuts the fraction off. Rounding first is what
        // makes the two answer one number.
        static_select_metadata::ScalarFunction::CastsToWholeNumber => {
            format!("CAST(round({column}) AS INTEGER)")
        }
        static_select_metadata::ScalarFunction::CastsToDay => format!("date({column})"),
        _ => format!("datetime({column})"),
    }
}

fn is_checked_select_comparison_operator(operator: &BinaryOperator) -> bool {
    matches!(
        operator,
        BinaryOperator::Eq
            | BinaryOperator::NotEq
            | BinaryOperator::Lt
            | BinaryOperator::LtEq
            | BinaryOperator::Gt
            | BinaryOperator::GtEq
            | BinaryOperator::Spaceship
    )
}

fn checked_select_comparison_operator(
    operator: &BinaryOperator,
) -> Option<CheckedSelectComparisonOperator> {
    Some(match operator {
        BinaryOperator::Eq => CheckedSelectComparisonOperator::Equal,
        BinaryOperator::NotEq => CheckedSelectComparisonOperator::NotEqual,
        BinaryOperator::Lt => CheckedSelectComparisonOperator::LessThan,
        BinaryOperator::LtEq => CheckedSelectComparisonOperator::LessThanOrEqual,
        BinaryOperator::Gt => CheckedSelectComparisonOperator::GreaterThan,
        BinaryOperator::GtEq => CheckedSelectComparisonOperator::GreaterThanOrEqual,
        BinaryOperator::Spaceship => CheckedSelectComparisonOperator::NullSafeEqual,
        _ => return None,
    })
}

fn checked_select_comparison_sql_operator(operator: &BinaryOperator) -> &'static str {
    match operator {
        BinaryOperator::Eq => "=",
        BinaryOperator::NotEq => "<>",
        BinaryOperator::Lt => "<",
        BinaryOperator::LtEq => "<=",
        BinaryOperator::Gt => ">",
        BinaryOperator::GtEq => ">=",
        BinaryOperator::Spaceship => "IS",
        _ => unreachable!("comparison operator guard"),
    }
}

pub(crate) fn render_simple_view_query(
    query: &sqlparser::ast::Query,
) -> Result<String, ParseError> {
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
        return unsupported("CREATE VIEW query clause");
    }
    let SetExpr::Select(select) = query.body.as_ref() else {
        return unsupported("CREATE VIEW query");
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
        || select.selection.is_some()
        || !select.connect_by.is_empty()
        || !matches!(&select.group_by, sqlparser::ast::GroupByExpr::Expressions(exprs, _) if exprs.is_empty())
        || !select.cluster_by.is_empty()
        || !select.distribute_by.is_empty()
        || !select.sort_by.is_empty()
        || select.having.is_some()
        || !select.named_window.is_empty()
        || select.qualify.is_some()
        || select.window_before_qualify
        || select.value_table_mode.is_some()
    {
        return unsupported("CREATE VIEW SELECT feature");
    }
    if select.from.is_empty() {
        let columns = select
            .projection
            .iter()
            .map(|item| match item {
                SelectItem::ExprWithAlias {
                    expr: Expr::Value(value),
                    alias,
                } if matches!(&value.value, Value::Number(number, false) if number == "1") => {
                    Ok(format!("1 AS {}", render_ident(alias)))
                }
                _ => unsupported("CREATE VIEW constant projection"),
            })
            .collect::<Result<Vec<_>, _>>()?;
        if columns.is_empty() {
            return unsupported("CREATE VIEW without projections");
        }
        return Ok(format!("SELECT {}", columns.join(", ")));
    }
    let [from] = select.from.as_slice() else {
        return unsupported("CREATE VIEW FROM clause");
    };
    if !from.joins.is_empty() {
        return unsupported("CREATE VIEW JOIN");
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
        return unsupported("CREATE VIEW table source");
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
        return unsupported("CREATE VIEW table option");
    }
    let table_name = render_unqualified_name(name)?;
    let columns = select
        .projection
        .iter()
        .map(|item| match item {
            SelectItem::UnnamedExpr(Expr::Identifier(column)) => Ok(render_ident(column)),
            SelectItem::ExprWithAlias {
                expr: Expr::CompoundIdentifier(parts),
                alias,
            } if parts.len() == 2
                && render_ident(&parts[0]).eq_ignore_ascii_case(&table_name)
                && parts[1].value.eq_ignore_ascii_case(&alias.value) =>
            {
                Ok(render_ident(&parts[1]))
            }
            _ => unsupported("CREATE VIEW projection"),
        })
        .collect::<Result<Vec<_>, _>>()?;
    if columns.is_empty() {
        return unsupported("CREATE VIEW without projections");
    }
    Ok(format!("SELECT {} FROM {table_name}", columns.join(", ")))
}
