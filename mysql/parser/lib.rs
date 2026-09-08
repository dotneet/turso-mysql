//! Conservative MySQL parsing for the SQLite-compatible path.

mod admin_command;
mod alter_table_indexes;
mod analyze_table;
mod checked_primary_key;
mod create_table_as_select;
mod date_format;
mod drop_table;
mod drop_view;
mod flush_tables;
mod information_schema;
mod insert_select;
mod json_value;
mod like_pattern;
mod lock_tables;
mod mysql_ddl;
mod number_format;
mod session_queries;
mod session_settings;
mod session_variables;
mod shift_moment;
mod show_engines;
mod show_full_tables;
mod show_table_status;
mod static_select_metadata;
mod str_to_date;
mod temporal_value;
mod translate;
mod truncate_table;

use admin_command::{
    admin_command_ends, consume_admin_database_name, consume_admin_qualified_table_name,
    consume_admin_table_name,
    consume_admin_u64, consume_admin_word, savepoint_command, skip_admin_comments,
    tokenize_admin_command, transaction_token_kind, AdminToken, TransactionTokenKind,
};
use information_schema::{
    contains_information_schema_object, contains_information_schema_tables,
    reject_information_schema_columns_query_tokens, reject_information_schema_query_tokens,
    reject_information_schema_schemata_query_tokens, tokenize_information_schema_query,
    validate_information_schema_columns_query, validate_information_schema_schemata_query,
    validate_information_schema_tables_query,
};
use mysql_ddl::render_mysql_column;
use static_select_metadata::classify_static_select_expr;
use translate::{
    columns_given_their_default, delete_source_table, direct_signed_integer,
    names_the_columns_default, render_simple_view_query, select_static_result_metadata,
    translate_delete, translate_insert, translate_select_query, translate_update, RenderedSelect,
    SelectRenderContext,
};

pub use admin_command::{parse_admin_command, parse_optional_admin_command};
pub use alter_table_indexes::rename_table_spelled_as_alter_table;
pub use alter_table_indexes::{
    parse_optional_alter_table_indexes, MySqlAlterTableIndexOperation, MySqlAlterTableIndexes,
};
pub use analyze_table::{
    parse_analyze_table, parse_check_table, parse_optional_analyze_table,
    parse_optional_check_table, MySqlAnalyzeTableCommand, MySqlCheckTableCommand,
};
pub use checked_primary_key::{
    parse_checked_primary_key_create_table, CheckedPrimaryKeyCreateTable,
    CheckedPrimaryKeyIntegerType,
};
pub use create_table_as_select::{
    parse_optional_create_table_as_select, MySqlCreateTableAsSelect,
    MySqlCreateTableAsSelectColumn, MySqlCreateTableAsSelectSource,
};
pub use date_format::{format_moment, format_width};
pub use drop_table::{parse_optional_drop_table, MySqlDropTableCommand};
pub use drop_view::parse_optional_drop_view;
pub use flush_tables::{parse_flush_tables, parse_optional_flush_tables, MySqlFlushTablesCommand};
pub use insert_select::{
    parse_optional_insert_select_without_columns, parse_optional_insert_set_as_values,
    MySqlInsertSelectWithoutColumns,
};
pub use json_value::{
    json_contains, json_keys, json_length, json_merge_patch, json_merge_preserve, json_overlaps,
    json_quote, json_search, json_type, normalize_json, JsonError,
};
pub use like_pattern::MySqlLikePattern;
pub use lock_tables::{parse_optional_lock_tables, MySqlLockTablesCommand};
pub use mysql_ddl::{
    render_counted_create_table_mysql_with_mode, render_create_index_mysql,
    render_create_index_mysql_with_mode, render_create_table_mysql,
    render_create_table_mysql_with_mode, render_create_trigger_mysql,
    render_create_trigger_mysql_with_mode, render_create_view_mysql,
    render_create_view_mysql_with_mode, stored_character_length,
};
pub use number_format::{format_number, truncate_number};
pub use session_queries::{
    parse_optional_select_database, parse_optional_system_variable_query,
    parse_optional_user_variable_query, MySqlSelectDatabaseQuery, MySqlSystemVariableQuery,
    MySqlUserVariableQuery, MySqlUserVariableRead,
};
pub use session_settings::{
    parse_optional_session_setting, parse_optional_user_variable_assignment, MySqlSessionSetting,
    MySqlUserVariableAssignment, MySqlUserVariableValue,
};
pub use session_variables::{parse_optional_session_sql_notes, MySqlSessionSqlNotes};
pub use shift_moment::shifted_moment;
pub use show_engines::{parse_optional_show_engines, parse_show_engines, MySqlShowEnginesCommand};
pub use show_table_status::{
    parse_optional_show_table_status, parse_show_table_status, MySqlShowTableStatusCommand,
};
pub use show_full_tables::{
    parse_optional_show_full_tables, parse_show_full_tables, MySqlShowFullTablesCommand,
};
pub use static_select_metadata::{
    ArithmeticOperand, ArithmeticOperator, ArithmeticShape, ColumnAggregateKind, ScalarFunction,
    StaticIntegerSign, StaticSelectMetadata, StaticSelectProjectionMetadata,
};
pub use str_to_date::{format_reads, read_by_format, FormatShape};
pub use temporal_value::{
    normalize_date, normalize_datetime, normalize_time, normalize_year, year_from_number,
};
pub use translate::{MySqlCatalogTable, MySqlSelectSource};
pub use truncate_table::{parse_optional_truncate_table, MySqlTruncateTableCommand};

/// Longest `VARCHAR` this server takes, in characters.
///
/// MySQL bounds a row's VARCHAR at 65535 bytes, which is 16383 characters at
/// the four bytes per character utf8mb4 reserves.
const MAX_VARCHAR_CHARACTERS: u64 = 16_383;

/// Widest `DECIMAL` MySQL takes, and the widest scale inside it.
const MAX_DECIMAL_PRECISION: u64 = 65;
const MAX_DECIMAL_SCALE: u64 = 30;

use std::any::TypeId;
use std::{fmt, num::NonZeroUsize};

use sqlparser::{
    ast::{
        AlterTable, AlterTableOperation, BinaryOperator, CharLengthUnits, CharacterLength,
        ColumnDef, ColumnOption, ColumnOptionDef, CreateIndex, CreateTable, CreateTableOptions,
        CreateTrigger, CreateView, DataType, Delete, ExactNumberInfo, Expr, FromTable,
        FunctionArguments, HiveDistributionStyle, Ident, IndexColumn, Insert, NullsDistinctOption,
        ObjectName, ObjectNamePart, PrimaryKeyConstraint, RenameTableNameKind, SelectFlavor,
        SelectItem, SetExpr, SqlOption, Statement, TableConstraint, TableFactor, TableObject,
        TriggerEvent as SqlTriggerEvent, TriggerObject, TriggerObjectKind, TriggerPeriod,
        UnaryOperator, Update, Value,
    },
    dialect::{Dialect, MySqlDialect},
    keywords::Keyword,
    parser::{Parser, ParserError},
    tokenizer::{Token, Tokenizer, Whitespace},
};
use turso_parser::{
    ast::{
        Cmd as TursoCmd, ColumnConstraint as TursoColumnConstraint,
        CreateTableBody as TursoCreateTableBody, Expr as TursoExpr, Literal as TursoLiteral,
        Name as TursoName, NamedColumnConstraint, NamedTableConstraint, OneSelect,
        Operator as TursoOperator, QualifiedName, RefAct, RefArg, ResultColumn, SelectTable, Stmt,
        TableConstraint as TursoTableConstraint, Type as TursoType, TypeSize as TursoTypeSize,
        UnaryOperator as TursoUnaryOperator,
    },
    parser::Parser as TursoParser,
};

/// Session SQL modes that change how MySQL tokenizes DDL.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SessionSqlMode {
    pub ansi_quotes: bool,
    pub no_backslash_escapes: bool,
}

/// A MySQL dialect that applies the session lexer modes relevant to this slice.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SessionMySqlDialect {
    mode: SessionSqlMode,
    expand_executable_comments: bool,
}

impl SessionMySqlDialect {
    /// Creates a dialect for one MySQL session.
    pub const fn new(mode: SessionSqlMode) -> Self {
        Self {
            mode,
            expand_executable_comments: true,
        }
    }

    const fn without_executable_comments(mode: SessionSqlMode) -> Self {
        Self {
            mode,
            expand_executable_comments: false,
        }
    }
}

impl Default for SessionMySqlDialect {
    fn default() -> Self {
        Self::new(SessionSqlMode::default())
    }
}

macro_rules! delegate_mysql_bool {
    ($name:ident) => {
        fn $name(&self) -> bool {
            MySqlDialect {}.$name()
        }
    };
}

impl Dialect for SessionMySqlDialect {
    fn dialect(&self) -> TypeId {
        TypeId::of::<MySqlDialect>()
    }

    fn is_identifier_start(&self, ch: char) -> bool {
        MySqlDialect {}.is_identifier_start(ch)
    }

    fn is_identifier_part(&self, ch: char) -> bool {
        MySqlDialect {}.is_identifier_part(ch)
    }

    fn is_delimited_identifier_start(&self, ch: char) -> bool {
        ch == '`' || (self.mode.ansi_quotes && ch == '"')
    }

    fn identifier_quote_style(&self, identifier: &str) -> Option<char> {
        MySqlDialect {}.identifier_quote_style(identifier)
    }

    fn supports_string_literal_backslash_escape(&self) -> bool {
        !self.mode.no_backslash_escapes
    }

    delegate_mysql_bool!(supports_string_literal_concatenation);
    delegate_mysql_bool!(ignores_wildcard_escapes);
    delegate_mysql_bool!(supports_numeric_prefix);
    delegate_mysql_bool!(supports_bitwise_shift_operators);
    fn supports_multiline_comment_hints(&self) -> bool {
        self.expand_executable_comments && MySqlDialect {}.supports_multiline_comment_hints()
    }

    fn parse_infix(
        &self,
        parser: &mut Parser,
        expr: &Expr,
        precedence: u8,
    ) -> Option<Result<Expr, ParserError>> {
        MySqlDialect {}.parse_infix(parser, expr, precedence)
    }

    fn parse_statement(&self, parser: &mut Parser) -> Option<Result<Statement, ParserError>> {
        MySqlDialect {}.parse_statement(parser)
    }

    delegate_mysql_bool!(require_interval_qualifier);
    delegate_mysql_bool!(supports_limit_comma);
    delegate_mysql_bool!(supports_create_table_select);
    delegate_mysql_bool!(supports_insert_set);
    delegate_mysql_bool!(supports_user_host_grantee);

    fn is_table_factor_alias(
        &self,
        explicit: bool,
        keyword: &Keyword,
        parser: &mut Parser,
    ) -> bool {
        MySqlDialect {}.is_table_factor_alias(explicit, keyword, parser)
    }

    delegate_mysql_bool!(supports_table_hints);
    delegate_mysql_bool!(requires_single_line_comment_whitespace);
    delegate_mysql_bool!(supports_match_against);
    delegate_mysql_bool!(supports_select_modifiers);
    delegate_mysql_bool!(supports_set_names);
    delegate_mysql_bool!(supports_comma_separated_set_assignments);
    delegate_mysql_bool!(supports_update_order_by);
    delegate_mysql_bool!(supports_data_type_signed_suffix);
    delegate_mysql_bool!(supports_cross_join_constraint);
    delegate_mysql_bool!(supports_double_ampersand_operator);
    delegate_mysql_bool!(supports_binary_kw_as_cast);
    delegate_mysql_bool!(supports_comment_optimizer_hint);
    delegate_mysql_bool!(supports_constraint_keyword_without_name);
    delegate_mysql_bool!(supports_key_column_option);
}

/// SQLite DDL produced from one checked MySQL `CREATE TABLE` statement.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TranslatedCreateTable {
    pub sqlite_sql: String,
}

impl TranslatedCreateTable {
    /// Returns the SQLite statement without a trailing semicolon.
    pub fn as_sql(&self) -> &str {
        &self.sqlite_sql
    }
}

/// One checked MySQL `AUTO_INCREMENT` table ready for later allocator wiring.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckedAutoIncrementCreateTable {
    /// The decoded unqualified table name.
    pub table_name: String,
    /// The zero-based stored-column position owned by the allocator.
    pub allocator_column_ordinal: usize,
    /// The decoded name of the column owned by the allocator.
    pub allocator_column_name: String,
    /// The column's own integer type, which sets how high the numbering runs.
    /// `INT` stops at 2147483647 and `INT UNSIGNED` at 4294967295.
    pub allocator_column_type: MySqlIntegerType,
    /// Canonical MySQL DDL, including the checked `AUTO_INCREMENT` declaration.
    pub normalized_mysql_ddl: String,
    /// SQLite-compatible table definition with an `INTEGER PRIMARY KEY` rowid alias.
    pub sqlite_statement: Stmt,
}

/// One checked MySQL `INSERT ... VALUES` statement that is eligible for
/// AUTO_INCREMENT range injection.
///
/// The allocator column is deliberately not part of this value yet. The
/// parser cannot know which table columns are allocator-owned without looking
/// at the durable table definition, so callers must bind that name before a
/// statement can be materialized for execution.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckedAutoIncrementInsert {
    table_name: TursoName,
    columns: Vec<TursoName>,
    row_count: NonZeroUsize,
    sqlite_statement: Stmt,
}

impl CheckedAutoIncrementInsert {
    /// Returns the unqualified target table name.
    pub fn table_name(&self) -> &TursoName {
        &self.table_name
    }

    /// Returns the explicit target columns, which do not include the
    /// allocator column until [`Self::bind_allocator_table`] succeeds.
    pub fn columns(&self) -> &[TursoName] {
        &self.columns
    }

    /// Returns the statically known number of VALUES rows.
    pub const fn row_count(&self) -> NonZeroUsize {
        self.row_count
    }

    /// Returns the checked SQLite AST before allocator range injection.
    pub fn sqlite_statement(&self) -> &Stmt {
        &self.sqlite_statement
    }

    /// Binds the allocator column after the frontend has validated the target
    /// table's durable AUTO_INCREMENT definition.
    pub fn bind_allocator_table(
        self,
        table: &CheckedAutoIncrementCreateTable,
    ) -> Result<BoundAutoIncrementInsert, ParseError> {
        if !self
            .table_name
            .as_str()
            .eq_ignore_ascii_case(&table.table_name)
        {
            return unsupported("AUTO_INCREMENT INSERT table does not match its definition");
        }
        let allocator_column = TursoName::exact(table.allocator_column_name.clone());
        if self.columns.iter().any(|column| {
            column
                .as_str()
                .eq_ignore_ascii_case(allocator_column.as_str())
        }) {
            return unsupported("INSERT explicitly names the AUTO_INCREMENT column");
        }
        Ok(BoundAutoIncrementInsert {
            insert: self,
            allocator_column,
        })
    }
}

/// A checked INSERT whose allocator column has been verified to be omitted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BoundAutoIncrementInsert {
    insert: CheckedAutoIncrementInsert,
    allocator_column: TursoName,
}

impl BoundAutoIncrementInsert {
    /// Returns the unqualified target table name.
    pub fn table_name(&self) -> &TursoName {
        self.insert.table_name()
    }

    /// Returns the explicit non-allocator target columns.
    pub fn columns(&self) -> &[TursoName] {
        self.insert.columns()
    }

    /// Returns the decoded allocator column name.
    pub fn allocator_column(&self) -> &TursoName {
        &self.allocator_column
    }

    /// Returns the statically known number of VALUES rows.
    pub const fn row_count(&self) -> NonZeroUsize {
        self.insert.row_count()
    }

    /// Injects one contiguous, already-reserved positive range.
    ///
    /// The returned statement owns the allocator values as typed Turso AST
    /// literals. No SQL text is rebuilt or reparsed after the range is known.
    ///
    /// The bound here is the widest number the engine holds. How high the
    /// numbering may actually run is the column's own type's to say, and the
    /// caller holds the range to it before this is reached.
    pub fn inject_reserved_range(&self, first_id: u64) -> Result<Stmt, ParseError> {
        let count = u64::try_from(self.row_count().get()).map_err(|_| ParseError::Unsupported {
            feature: "AUTO_INCREMENT range count outside unsigned 64-bit range",
        })?;
        if first_id == 0 {
            return unsupported("AUTO_INCREMENT range must be positive");
        }
        let last_id = first_id
            .checked_add(count - 1)
            .ok_or(ParseError::Unsupported {
                feature: "AUTO_INCREMENT range outside what the engine holds",
            })?;
        if last_id > i64::MAX as u64 {
            return unsupported("AUTO_INCREMENT range outside what the engine holds");
        }

        let mut statement = self.insert.sqlite_statement.clone();
        let Stmt::Insert { columns, body, .. } = &mut statement else {
            return Err(ParseError::TursoParser(
                "checked AUTO_INCREMENT INSERT did not produce an INSERT AST".to_string(),
            ));
        };
        let turso_parser::ast::InsertBody::Select(select, upsert) = body else {
            return Err(ParseError::TursoParser(
                "checked AUTO_INCREMENT INSERT did not produce a VALUES body".to_string(),
            ));
        };
        if upsert.is_some()
            || !select.order_by.is_empty()
            || select.limit.is_some()
            || !select.body.compounds.is_empty()
        {
            return Err(ParseError::TursoParser(
                "checked AUTO_INCREMENT INSERT contains unsupported query clauses".to_string(),
            ));
        }
        let turso_parser::ast::OneSelect::Values(rows) = &mut select.body.select else {
            return Err(ParseError::TursoParser(
                "checked AUTO_INCREMENT INSERT did not produce VALUES rows".to_string(),
            ));
        };
        if rows.len() != self.row_count().get() || columns.len() != self.columns().len() {
            return Err(ParseError::TursoParser(
                "checked AUTO_INCREMENT INSERT changed shape before range injection".to_string(),
            ));
        }
        columns.insert(0, self.allocator_column.clone());
        for (offset, row) in rows.iter_mut().enumerate() {
            if row.len() != self.columns().len() {
                return Err(ParseError::TursoParser(
                    "checked AUTO_INCREMENT INSERT row changed shape before range injection"
                        .to_string(),
                ));
            }
            let id = first_id
                .checked_add(offset as u64)
                .ok_or(ParseError::Unsupported {
                    feature: "AUTO_INCREMENT range outside what the engine holds",
                })?;
            row.insert(
                0,
                Box::new(TursoExpr::Literal(TursoLiteral::Numeric(id.to_string()))),
            );
        }
        Ok(statement)
    }
}

/// SQLite SQL produced from one checked MySQL `SELECT` statement.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TranslatedSelect {
    pub sqlite_sql: String,
    reads_table: bool,
    orders_a_bare_column: bool,
    orders_wildcard_ordinal: bool,
    compares_a_placeholder: bool,
    counts_distinct_column: bool,
    tests_a_bare_column: bool,
    compares_a_written_day: bool,
    checked_subquery_comparisons: Vec<CheckedSubqueryComparison>,
    source_table: Option<MySqlTableName>,
    source_tables: Vec<MySqlSelectSource>,
    static_result_metadata: Vec<StaticSelectProjectionMetadata>,
    checked_comparisons: Vec<CheckedSelectComparison>,
    locks_rows: bool,
    row_count_parameters: Vec<usize>,
    parameter_count: usize,
}

/// The exact right-hand side forms checked for a SELECT comparison.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CheckedSelectComparisonRhs {
    /// One signed integer literal represented without loss in an `i64`.
    SignedInteger(i64),
    /// One number written with a fraction or an exponent, kept as it was
    /// written so the rendered SQL asks for the same number.
    Decimal(String),
    /// One string literal, compared without regard to case.
    Text(String),
    /// One call answering the moment the statement runs, which needs no
    /// argument and answers a value in the form the column it meets holds.
    Now(CheckedComparisonNow),
    /// A SQL NULL literal, which retains ordinary SQL three-valued logic.
    Null,
    /// One binary-protocol parameter at the zero-based statement ordinal.
    Placeholder { ordinal: usize },
}

/// What a call answering the moment the statement runs answers.
///
/// Each answers it in the form the column it meets holds, which is what makes
/// the comparison the one MySQL makes: measured on 8.4.11, `CURDATE()` answers
/// `2026-09-08` and a `DATE` holds a day written that way.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckedComparisonNow {
    /// `CURDATE()` and `CURRENT_DATE`, which answer today.
    Day,
    /// `NOW()` and `CURRENT_TIMESTAMP`, which answer this moment.
    Moment,
    /// `CURTIME()` and `CURRENT_TIME`, which answer the time of day.
    TimeOfDay,
}

impl CheckedComparisonNow {
    /// Reads which of these a call is, or nothing when it is another call.
    ///
    /// MySQL spells each of them with and without its parentheses, and
    /// sqlparser gives the bare form no argument list at all rather than an
    /// empty one, so both shapes count as taking nothing.
    pub(crate) fn read(function: &sqlparser::ast::Function) -> Option<Self> {
        let [sqlparser::ast::ObjectNamePart::Identifier(name)] = function.name.0.as_slice() else {
            return None;
        };
        let takes_nothing = match &function.args {
            sqlparser::ast::FunctionArguments::None => true,
            sqlparser::ast::FunctionArguments::List(arguments) => arguments.args.is_empty(),
            sqlparser::ast::FunctionArguments::Subquery(_) => false,
        };
        if !takes_nothing || function.over.is_some() {
            return None;
        }
        let named = |candidates: &[&str]| {
            candidates
                .iter()
                .any(|candidate| name.value.eq_ignore_ascii_case(candidate))
        };
        if named(&["CURDATE", "CURRENT_DATE"]) {
            return Some(Self::Day);
        }
        if named(&["NOW", "CURRENT_TIMESTAMP"]) {
            return Some(Self::Moment);
        }
        named(&["CURTIME", "CURRENT_TIME"]).then_some(Self::TimeOfDay)
    }

    /// The engine call answering the same value in the same form.
    pub(crate) const fn engine_call(self) -> &'static str {
        match self {
            Self::Day => "date('now')",
            Self::Moment => "datetime('now')",
            Self::TimeOfDay => "time('now')",
        }
    }
}

/// What the left side of a comparison answers, when it is a call rather than a
/// column.
///
/// A column says what it holds through its declared type; a call says it
/// through what it is. Either way the value compared against it has to fit,
/// and this is what it has to fit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckedComparisonAnswer {
    /// A word, which MySQL compares without regard to case.
    Text,
    /// A number counted in whole ones.
    WholeNumber,
    /// A day, written the way a `DATE` column holds one.
    Day,
    /// A moment, written the way a `DATETIME` column holds one.
    Moment,
}

/// The comparison operators accepted by the strict integer SELECT subset.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckedSelectComparisonOperator {
    /// Equal to (`=`).
    Equal,
    /// Not equal to (`<>` or `!=`).
    NotEqual,
    /// Less than (`<`).
    LessThan,
    /// Less than or equal to (`<=`).
    LessThanOrEqual,
    /// Greater than (`>`).
    GreaterThan,
    /// Greater than or equal to (`>=`).
    GreaterThanOrEqual,
    /// Null-safe equal to (`<=>`).
    NullSafeEqual,
    /// Matches a pattern (`LIKE`).
    Like,
    /// Does not match a pattern (`NOT LIKE`).
    NotLike,
    /// Equal to one member of a list (`IN`). Each member is recorded on its
    /// own, so a list of three carries three comparisons.
    In,
    /// Equal to no member of a list (`NOT IN`).
    NotIn,
}

/// One `IN (SELECT ...)` found while rendering a checked SELECT.
///
/// The two columns have to be the same kind, which only the frontend can see,
/// so the pair is recorded here and checked there — the same arrangement a
/// literal comparison uses.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckedSubqueryComparison {
    column_name: String,
    inner_table: String,
    inner_column_name: String,
}

impl CheckedSubqueryComparison {
    /// Returns the outer column tested for membership.
    pub fn column_name(&self) -> &str {
        &self.column_name
    }

    /// Returns the table the subquery reads.
    pub fn inner_table(&self) -> &str {
        &self.inner_table
    }

    /// Returns the column the subquery projects.
    pub fn inner_column_name(&self) -> &str {
        &self.inner_column_name
    }
}

/// One strict integer comparison found while rendering a checked SELECT.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckedSelectComparison {
    qualifier: Option<String>,
    /// The subquery this comparison was written inside, when it was written
    /// inside one. An unqualified name looks there before it looks at the
    /// statement's own table, which is how MySQL reads one.
    inner_source: Option<String>,
    column_name: String,
    operator: CheckedSelectComparisonOperator,
    rhs: CheckedSelectComparisonRhs,
    collated: bool,
    /// What the left side answers, when it is a call. A column leaves this
    /// empty and is held to its declared type instead.
    answers: Option<CheckedComparisonAnswer>,
}

impl CheckedSelectComparison {
    /// Records the subquery this comparison was written inside.
    pub(crate) fn name_the_inner_source(&mut self, reference: &str) {
        if self.inner_source.is_none() {
            self.inner_source = Some(reference.to_owned());
        }
    }

    /// Returns the subquery this comparison was written inside, if any.
    ///
    /// Measured on MySQL 8.4.11: an unqualified name inside a subquery is the
    /// subquery's column when it has one, and the outer statement's when it
    /// does not — `EXISTS (SELECT 1 FROM b WHERE name = 'one')` reads `a.name`
    /// where `b` carries no `name`.
    pub fn inner_source(&self) -> Option<&str> {
        self.inner_source.as_deref()
    }

    /// Returns the qualifier (table name or alias) used on the column, if any.
    pub fn qualifier(&self) -> Option<&str> {
        self.qualifier.as_deref()
    }

    /// Returns the unqualified column name used on the left side.
    pub fn column_name(&self) -> &str {
        &self.column_name
    }

    /// Returns the checked comparison operator.
    pub const fn operator(&self) -> CheckedSelectComparisonOperator {
        self.operator
    }

    /// Returns what the left side answers, when it is a call rather than a
    /// column.
    pub const fn answers(&self) -> Option<CheckedComparisonAnswer> {
        self.answers
    }

    /// Returns the exact checked right-hand side form.
    pub const fn rhs(&self) -> &CheckedSelectComparisonRhs {
        &self.rhs
    }

    /// Reports whether this comparison was rendered with MySQL's collation.
    ///
    /// A comparison against a string literal always is. One against a `?` is
    /// only when the parser was told the column is text, which is what makes a
    /// string parameter safe to bind to it.
    pub const fn collated(&self) -> bool {
        self.collated
    }
}

/// One assignment in a checked MySQL `UPDATE` statement.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckedUpdateAssignment {
    column_name: String,
    value: CheckedUpdateAssignmentValue,
}

/// The forms that callers may safely distinguish in a checked UPDATE assignment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckedUpdateAssignmentValue {
    /// The right side is the same unqualified column identifier.
    SelfAssignment,
    /// The right side is one direct signed integer literal.
    SignedInteger(i64),
    /// Any other expression accepted by the conservative UPDATE grammar.
    Other,
}

impl CheckedUpdateAssignment {
    /// Returns the unqualified assignment target as written by the client.
    pub fn column_name(&self) -> &str {
        &self.column_name
    }

    /// Returns the statically checked form of the right side.
    pub const fn value(&self) -> CheckedUpdateAssignmentValue {
        self.value
    }

    /// Returns whether the assignment is exactly `column = column`.
    pub const fn assigns_column_to_itself(&self) -> bool {
        matches!(self.value, CheckedUpdateAssignmentValue::SelfAssignment)
    }
}

/// The target and assignments of one checked MySQL `UPDATE` statement.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckedUpdate {
    table_name: String,
    assignments: Vec<CheckedUpdateAssignment>,
}

impl CheckedUpdate {
    /// Returns the unqualified target table name.
    pub fn table_name(&self) -> &str {
        &self.table_name
    }

    /// Returns the checked assignments in source order.
    pub fn assignments(&self) -> &[CheckedUpdateAssignment] {
        &self.assignments
    }
}

/// SQLite SQL produced from one checked MySQL `INSERT`, `UPDATE`, or `DELETE` statement.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TranslatedDml {
    sqlite_sql: String,
    checked_update: Option<CheckedUpdate>,
    checked_comparisons: Vec<CheckedSelectComparison>,
    source_table: Option<String>,
    /// Every table the statement reads, which for an `INSERT ... SELECT` is the
    /// SELECT's own. The caller authorizes these the way it authorizes a
    /// `SELECT`'s; a statement that reads a table has to say so, or the table
    /// goes unchecked.
    read_tables: Vec<MySqlSelectSource>,
    checked_subquery_comparisons: Vec<CheckedSubqueryComparison>,
    ordered_columns: Vec<String>,
}

impl TranslatedDml {
    /// Returns the normalized statement without a trailing semicolon.
    pub fn as_sql(&self) -> &str {
        &self.sqlite_sql
    }

    /// Parses the already-checked normalized SQL into Turso's AST.
    pub fn parse_ast(&self) -> Result<Stmt, ParseError> {
        parse_normalized_dml(self.as_sql())
    }

    /// Returns the comparisons the `WHERE` made, for the frontend to check
    /// against the columns they name.
    pub fn checked_comparisons(&self) -> &[CheckedSelectComparison] {
        &self.checked_comparisons
    }

    /// Returns the table an `UPDATE` or `DELETE` names, which is the table the
    /// comparisons have to be checked against.
    /// Returns every table the statement reads.
    pub fn read_tables(&self) -> &[MySqlSelectSource] {
        &self.read_tables
    }

    /// Returns each `IN (SELECT ...)` this statement's `WHERE` makes.
    pub fn checked_subquery_comparisons(&self) -> &[CheckedSubqueryComparison] {
        &self.checked_subquery_comparisons
    }

    pub fn source_table(&self) -> Option<&str> {
        self.source_table.as_deref()
    }

    /// Returns checked UPDATE target information when this is an UPDATE.
    pub fn checked_update(&self) -> Option<&CheckedUpdate> {
        self.checked_update.as_ref()
    }

    /// Returns the columns named in an `ORDER BY` clause, for the frontend to check.
    pub fn ordered_columns(&self) -> &[String] {
        &self.ordered_columns
    }
}

/// The integer range associated with one MySQL table column.
///
/// `BIGINT UNSIGNED` is the one type here whose range is narrower than MySQL's.
/// Its top value, 18446744073709551615, is more than twice `i64::MAX`, and the
/// engine holds an integer as an `i64`, so the column takes 0 to `i64::MAX` and
/// answers 1264 above that — the same answer MySQL gives one past its own top
/// value, at a lower place.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MySqlIntegerType {
    TinyInt,
    SmallInt,
    MediumInt,
    Int,
    BigInt,
    TinyIntUnsigned,
    SmallIntUnsigned,
    MediumIntUnsigned,
    IntUnsigned,
    BigIntUnsigned,
}

impl MySqlIntegerType {
    /// Returns the inclusive i64 bounds used by the strict assignment slice.
    pub const fn bounds(self) -> (i64, i64) {
        match self {
            Self::TinyInt => (-128, 127),
            Self::SmallInt => (-32_768, 32_767),
            Self::MediumInt => (-8_388_608, 8_388_607),
            Self::Int => (-2_147_483_648, 2_147_483_647),
            Self::BigInt => (i64::MIN, i64::MAX),
            // Measured on MySQL 8.4.11: these are the top values each unsigned
            // type accepts, and one past any of them answers 1264. All four fit
            // an i64, which is why they are here and BIGINT UNSIGNED is not.
            Self::TinyIntUnsigned => (0, 255),
            Self::SmallIntUnsigned => (0, 65_535),
            Self::MediumIntUnsigned => (0, 16_777_215),
            Self::IntUnsigned => (0, 4_294_967_295),
            // MySQL takes 18446744073709551615 here and the engine cannot hold
            // it, so this is the top of what can be stored honestly.
            Self::BigIntUnsigned => (0, i64::MAX),
        }
    }

    /// Answers whether the column refuses a negative value.
    pub const fn is_unsigned(self) -> bool {
        matches!(
            self,
            Self::TinyIntUnsigned
                | Self::SmallIntUnsigned
                | Self::MediumIntUnsigned
                | Self::IntUnsigned
                | Self::BigIntUnsigned
        )
    }
}

/// Private MySQL numeric metadata rebuilt from durable normalized table DDL.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MySqlNumericSpec {
    columns: Vec<Option<MySqlIntegerType>>,
    character_lengths: Vec<Option<u32>>,
    fixed_widths: Vec<bool>,
    binary_lengths: Vec<Option<u32>>,
    datetimes: Vec<bool>,
    dates: Vec<bool>,
    times: Vec<bool>,
    years: Vec<bool>,
    enums: Vec<Option<Vec<String>>>,
    sets: Vec<Option<Vec<String>>>,
    jsons: Vec<bool>,
    unsigned_reals: Vec<bool>,
}

impl MySqlNumericSpec {
    /// Returns the signed range for a stored column position, if this slice owns it.
    pub fn column(&self, index: usize) -> Option<MySqlIntegerType> {
        self.columns.get(index).copied().flatten()
    }

    /// Returns the declared character count for a stored column position.
    pub fn character_length(&self, index: usize) -> Option<u32> {
        self.character_lengths.get(index).copied().flatten()
    }

    /// Reports whether a stored column position holds a fixed-width `CHAR`,
    /// which stores what MySQL calls a padded value rather than what it was
    /// written with.
    pub fn is_fixed_width(&self, index: usize) -> bool {
        self.fixed_widths.get(index).copied().unwrap_or(false)
    }

    /// Returns the declared byte count of a `VARBINARY` column.
    pub fn binary_length(&self, index: usize) -> Option<u32> {
        self.binary_lengths.get(index).copied().flatten()
    }

    /// Reports whether a stored column position holds a `DATETIME`.
    pub fn is_datetime(&self, index: usize) -> bool {
        self.datetimes.get(index).copied().unwrap_or(false)
    }

    /// Reports whether a stored column position holds a `DATE`.
    pub fn is_date(&self, index: usize) -> bool {
        self.dates.get(index).copied().unwrap_or(false)
    }

    /// Reports whether a stored column position holds a `TIME`.
    pub fn is_time(&self, index: usize) -> bool {
        self.times.get(index).copied().unwrap_or(false)
    }

    /// Reports whether a stored column position holds a `YEAR`.
    pub fn is_year(&self, index: usize) -> bool {
        self.years.get(index).copied().unwrap_or(false)
    }

    /// Returns the members an `ENUM` column lists, if the position holds one.
    pub fn enum_members(&self, index: usize) -> Option<&[String]> {
        self.enums.get(index)?.as_deref()
    }

    /// Returns the members a `SET` column lists, if the position holds one.
    pub fn set_members(&self, index: usize) -> Option<&[String]> {
        self.sets.get(index)?.as_deref()
    }

    /// Reports whether a stored column position holds a `JSON` document.
    pub fn is_json(&self, index: usize) -> bool {
        self.jsons.get(index).copied().unwrap_or(false)
    }

    /// Reports whether a stored column position holds an unsigned `DOUBLE` or
    /// `FLOAT`, which takes no negative value.
    pub fn is_unsigned_real(&self, index: usize) -> bool {
        self.unsigned_reals.get(index).copied().unwrap_or(false)
    }

    /// Returns the number of columns represented by the durable table DDL.
    pub fn len(&self) -> usize {
        self.columns.len()
    }

    /// Returns whether no column needs checking before storage.
    pub fn is_empty(&self) -> bool {
        self.columns.iter().all(Option::is_none)
            && self.character_lengths.iter().all(Option::is_none)
            && !self.datetimes.iter().any(|is_datetime| *is_datetime)
            && !self.dates.iter().any(|is_date| *is_date)
            && !self.times.iter().any(|is_time| *is_time)
            && !self.years.iter().any(|is_year| *is_year)
            && !self.enums.iter().any(Option::is_some)
            && !self.sets.iter().any(Option::is_some)
            && !self.jsons.iter().any(|is_json| *is_json)
    }
}

impl TranslatedSelect {
    /// Returns the normalized statement without a trailing semicolon.
    pub fn as_sql(&self) -> &str {
        &self.sqlite_sql
    }

    /// Parses the already-checked normalized SQL into Turso's AST.
    pub fn parse_ast(&self) -> Result<Stmt, ParseError> {
        parse_normalized_select(self.as_sql())
    }

    /// Returns whether this SELECT reads from a table.
    pub const fn reads_table(&self) -> bool {
        self.reads_table
    }

    /// Returns the canonical unqualified table read by this SELECT.
    pub fn source_table(&self) -> Option<&str> {
        self.source_table.as_ref().map(MySqlTableName::as_str)
    }

    /// Reports whether this statement's rendering depends on a column's type.
    ///
    /// An `ORDER BY` over a bare column and a comparison against a `?` are the
    /// two places it does, and both want the same answer: whether the column is
    /// text, and so wants MySQL's collation.
    pub const fn needs_column_types(&self) -> bool {
        self.orders_a_bare_column
            || self.compares_a_placeholder
            || self.counts_distinct_column
            || self.tests_a_bare_column
            || self.compares_a_written_day
            || self.orders_wildcard_ordinal
    }

    /// Returns whether this SELECT contains an ORDER BY ordinal over a wildcard projection.
    pub const fn orders_wildcard_ordinal(&self) -> bool {
        self.orders_wildcard_ordinal
    }

    /// Returns each `IN (SELECT ...)` this statement makes.
    pub fn checked_subquery_comparisons(&self) -> &[CheckedSubqueryComparison] {
        &self.checked_subquery_comparisons
    }

    /// Returns every table this statement reads, in the order it names them.
    ///
    /// `source_table` is the one of these that a single-table statement has;
    /// a join has several and none of them is "the" table.
    pub fn source_tables(&self) -> &[MySqlSelectSource] {
        &self.source_tables
    }

    /// Returns source metadata parallel to checked projection items.
    pub fn static_result_metadata(&self) -> &[StaticSelectProjectionMetadata] {
        &self.static_result_metadata
    }

    /// Returns strict integer comparisons collected from the SELECT predicate.
    pub fn checked_comparisons(&self) -> &[CheckedSelectComparison] {
        &self.checked_comparisons
    }

    /// Reports whether the statement asked to read the rows it is about to
    /// change, which `FOR UPDATE` and `LOCK IN SHARE MODE` are the spellings
    /// of.
    pub const fn locks_rows(&self) -> bool {
        self.locks_rows
    }

    /// Returns which parameters stand where a row count is written.
    ///
    /// A `LIMIT ?` binds a whole number that is not negative, which only the
    /// frontend can hold the bound value to.
    pub fn row_count_parameters(&self) -> &[usize] {
        &self.row_count_parameters
    }

    /// Returns the total number of `?` parameters in projection and predicate order.
    pub const fn parameter_count(&self) -> usize {
        self.parameter_count
    }
}

/// Errors produced while parsing or checking the supported DDL subset.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParseError {
    Sqlparser(String),
    TursoParser(String),
    ExpectedOneStatement { actual: usize },
    ExpectedAdminCommand,
    ExpectedTransactionCommand,
    TrailingAdminCommandTokens,
    InvalidDatabaseName { reason: &'static str },
    InvalidTableName { reason: &'static str },
    InvalidSavepointName { reason: &'static str },
    ExpectedCreateTable,
    ExpectedCreateIndex,
    ExpectedCreateView,
    ExpectedCreateTrigger,
    ExpectedAlterTable,
    ExpectedSelect,
    ExpectedDml,
    Unsupported { feature: &'static str },
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Sqlparser(error) => write!(f, "MySQL parse error: {error}"),
            Self::TursoParser(error) => write!(f, "normalized SQLite parse error: {error}"),
            Self::ExpectedOneStatement { actual } => {
                write!(f, "expected exactly one statement, found {actual}")
            }
            Self::ExpectedAdminCommand => {
                f.write_str("expected CREATE DATABASE, DROP DATABASE, or USE")
            }
            Self::ExpectedTransactionCommand => {
                f.write_str("expected BEGIN, START TRANSACTION, COMMIT, or ROLLBACK")
            }
            Self::TrailingAdminCommandTokens => {
                f.write_str("unexpected token after database-management command")
            }
            Self::InvalidDatabaseName { reason } => {
                write!(f, "invalid MySQL database name: {reason}")
            }
            Self::InvalidTableName { reason } => write!(f, "invalid MySQL table name: {reason}"),
            Self::InvalidSavepointName { reason } => {
                write!(f, "invalid MySQL savepoint name: {reason}")
            }
            Self::ExpectedCreateTable => f.write_str("expected a CREATE TABLE statement"),
            Self::ExpectedCreateIndex => f.write_str("expected a CREATE INDEX statement"),
            Self::ExpectedCreateView => f.write_str("expected a CREATE VIEW statement"),
            Self::ExpectedCreateTrigger => f.write_str("expected a CREATE TRIGGER statement"),
            Self::ExpectedAlterTable => f.write_str("expected an ALTER TABLE statement"),
            Self::ExpectedSelect => f.write_str("expected a SELECT statement"),
            Self::ExpectedDml => f.write_str("expected an INSERT, UPDATE, or DELETE statement"),
            Self::Unsupported { feature } => {
                write!(f, "unsupported MySQL schema feature: {feature}")
            }
        }
    }
}

impl std::error::Error for ParseError {}

/// A logical database name accepted by the MySQL compatibility registry.
///
/// The registry deliberately has a smaller name space than MySQL itself. Keeping
/// this checked value in the parser prevents a protocol or SQL caller from
/// turning a logical name into a path, a dot-qualified name, or a hidden file.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct MySqlDatabaseName(String);

impl MySqlDatabaseName {
    /// Validates and canonicalizes one logical database name.
    pub fn parse(name: &str) -> Result<Self, ParseError> {
        if name.is_empty() {
            return Err(ParseError::InvalidDatabaseName { reason: "empty" });
        }
        if name.len() > 64 {
            return Err(ParseError::InvalidDatabaseName {
                reason: "longer than 64 bytes",
            });
        }

        let mut canonical = String::with_capacity(name.len());
        for byte in name.bytes() {
            let byte = match byte {
                b'A'..=b'Z' => byte.to_ascii_lowercase(),
                b'a'..=b'z' | b'0'..=b'9' | b'_' | b'$' => byte,
                0 => {
                    return Err(ParseError::InvalidDatabaseName { reason: "NUL byte" });
                }
                b'/' | b'\\' => {
                    return Err(ParseError::InvalidDatabaseName {
                        reason: "path separator",
                    });
                }
                0x80..=u8::MAX => {
                    return Err(ParseError::InvalidDatabaseName {
                        reason: "non-ASCII character",
                    });
                }
                _ => {
                    return Err(ParseError::InvalidDatabaseName {
                        reason: "character outside [A-Za-z0-9_$]",
                    });
                }
            };
            canonical.push(char::from(byte));
        }

        if matches!(canonical.as_str(), "." | "..")
            || matches!(
                canonical.as_str(),
                "information_schema"
                    | "mysql"
                    | "performance_schema"
                    | "sys"
                    | "main"
                    | "temp"
                    | "sqlite_master"
                    | "sqlite_schema"
            )
        {
            return Err(ParseError::InvalidDatabaseName {
                reason: "reserved database name",
            });
        }

        Ok(Self(canonical))
    }

    /// Returns the canonical ASCII-lowercase name.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Returns the canonical name as an owned string.
    pub fn into_string(self) -> String {
        self.0
    }
}

impl AsRef<str> for MySqlDatabaseName {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

/// A table name accepted by the initial MySQL metadata surface.
///
/// This is deliberately constrained to the same ASCII-lowercase name policy
/// recorded for MySQL-owned databases. It prevents the catalog provider from
/// accepting a table name that the current frontend cannot address reliably.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct MySqlTableName(String);

impl MySqlTableName {
    /// Validates and canonicalizes one unqualified table name.
    pub fn parse(name: &str) -> Result<Self, ParseError> {
        if name.is_empty() {
            return Err(ParseError::InvalidTableName { reason: "empty" });
        }
        if name.len() > 64 {
            return Err(ParseError::InvalidTableName {
                reason: "longer than 64 bytes",
            });
        }

        let mut canonical = String::with_capacity(name.len());
        for byte in name.bytes() {
            let byte = match byte {
                b'A'..=b'Z' => byte.to_ascii_lowercase(),
                b'a'..=b'z' | b'0'..=b'9' | b'_' | b'$' => byte,
                0 => return Err(ParseError::InvalidTableName { reason: "NUL byte" }),
                0x80..=u8::MAX => {
                    return Err(ParseError::InvalidTableName {
                        reason: "non-ASCII character",
                    });
                }
                _ => {
                    return Err(ParseError::InvalidTableName {
                        reason: "character outside [A-Za-z0-9_$]",
                    });
                }
            };
            canonical.push(char::from(byte));
        }
        Ok(Self(canonical))
    }

    /// Returns the canonical ASCII-lowercase name.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl AsRef<str> for MySqlTableName {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

/// A checked MySQL database-management command.
///
/// These commands are intentionally kept outside the shared SQLite AST. The
/// frontend must perform the corresponding registry operation before any Core
/// connection is selected or changed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MySqlAdminCommand {
    /// Create one logical database.
    CreateDatabase { name: MySqlDatabaseName },
    /// Drop one logical database.
    DropDatabase { name: MySqlDatabaseName },
    /// Select one logical database for the current session.
    Use { name: MySqlDatabaseName },
    /// List the logical databases visible to this session.
    ListDatabases,
}

impl MySqlAdminCommand {
    /// Returns the command's logical database name, when it has one.
    pub fn name(&self) -> Option<&MySqlDatabaseName> {
        match self {
            Self::CreateDatabase { name } | Self::DropDatabase { name } | Self::Use { name } => {
                Some(name)
            }
            Self::ListDatabases => None,
        }
    }
}

/// A checked read-only MySQL `SHOW TABLES` command that operates on the
/// selected database rather than on the logical-database registry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MySqlShowCommand {
    pattern: Option<MySqlLikePattern>,
}

impl MySqlShowCommand {
    /// Returns the pattern the command names its tables with, if any.
    ///
    /// A table name keeps its case here, unlike every other `SHOW ... LIKE`
    /// subject, and the pattern text goes into the column name.
    pub fn pattern(&self) -> Option<&MySqlLikePattern> {
        self.pattern.as_ref()
    }

    /// Reports whether the command asks for the table called `name`.
    pub fn covers(&self, name: &str) -> bool {
        self.pattern
            .as_ref()
            .is_none_or(|pattern| pattern.matches_keeping_case(name))
    }
}

/// One column of `information_schema.TABLES` this answers.
///
/// MySQL's table has twenty-one and these are the three whose values this
/// server holds. A query naming any other column is refused rather than
/// answered with a value that would be made up.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MySqlInformationSchemaTablesColumn {
    TableSchema,
    TableName,
    TableType,
}

impl MySqlInformationSchemaTablesColumn {
    /// Reads one column by the name a query wrote, whatever its case.
    fn named(name: &str) -> Option<Self> {
        Some(match () {
            () if name.eq_ignore_ascii_case("TABLE_SCHEMA") => Self::TableSchema,
            () if name.eq_ignore_ascii_case("TABLE_NAME") => Self::TableName,
            () if name.eq_ignore_ascii_case("TABLE_TYPE") => Self::TableType,
            () => return None,
        })
    }
}

/// A checked `information_schema.TABLES` query over the selected database.
///
/// The columns come back in the order the query named them, which is the order
/// MySQL answers in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MySqlInformationSchemaTablesQuery {
    columns: Vec<MySqlInformationSchemaTablesColumn>,
}

impl MySqlInformationSchemaTablesQuery {
    /// Returns the columns the query named, in the order it named them.
    pub fn columns(&self) -> &[MySqlInformationSchemaTablesColumn] {
        &self.columns
    }
}

/// The one `information_schema.SCHEMATA` query supported by the catalog surface.
///
/// The value carries no user input because the query always lists the logical
/// databases visible to the session and always returns one catalog column.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MySqlInformationSchemaSchemataQuery;

/// One column of `information_schema.COLUMNS` this answers.
///
/// MySQL's table has twenty-two and these are the seven whose values this
/// server holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MySqlInformationSchemaColumnsColumn {
    ColumnName,
    OrdinalPosition,
    ColumnDefault,
    IsNullable,
    ColumnType,
    ColumnKey,
    Extra,
}

impl MySqlInformationSchemaColumnsColumn {
    /// Reads one column by the name a query wrote, whatever its case.
    fn named(name: &str) -> Option<Self> {
        Some(match () {
            () if name.eq_ignore_ascii_case("COLUMN_NAME") => Self::ColumnName,
            () if name.eq_ignore_ascii_case("ORDINAL_POSITION") => Self::OrdinalPosition,
            () if name.eq_ignore_ascii_case("COLUMN_DEFAULT") => Self::ColumnDefault,
            () if name.eq_ignore_ascii_case("IS_NULLABLE") => Self::IsNullable,
            () if name.eq_ignore_ascii_case("COLUMN_TYPE") => Self::ColumnType,
            () if name.eq_ignore_ascii_case("COLUMN_KEY") => Self::ColumnKey,
            () if name.eq_ignore_ascii_case("EXTRA") => Self::Extra,
            () => return None,
        })
    }
}

/// A checked `information_schema.COLUMNS` query for one selected-database table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MySqlInformationSchemaColumnsQuery {
    schema: Option<String>,
    table: MySqlTableName,
    columns: Vec<MySqlInformationSchemaColumnsColumn>,
}

impl MySqlInformationSchemaColumnsQuery {
    /// Returns the database the query named, when it wrote one out rather than
    /// asking for the selected one with `DATABASE()`.
    ///
    /// A client writes it either way — a migration tool that knows which
    /// database it is working on writes the name — and only the caller can
    /// tell whether the name is the one selected.
    pub fn schema(&self) -> Option<&str> {
        self.schema.as_deref()
    }

    /// Returns the canonical table identifier selected by the query.
    pub fn table(&self) -> &MySqlTableName {
        &self.table
    }

    /// Returns the columns the query named, in the order it named them.
    pub fn columns(&self) -> &[MySqlInformationSchemaColumnsColumn] {
        &self.columns
    }
}

/// A checked read-only MySQL `SHOW COLUMNS` command for one table in the
/// selected database.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MySqlShowColumnsCommand {
    database: Option<MySqlDatabaseName>,
    table: MySqlTableName,
    pattern: Option<MySqlLikePattern>,
    full: bool,
}

impl MySqlShowColumnsCommand {
    /// Returns the table identifier selected by the command.
    pub fn table(&self) -> &MySqlTableName {
        &self.table
    }

    /// Returns the `database.` qualifier the command was written with, if any.
    pub fn database(&self) -> Option<&MySqlDatabaseName> {
        self.database.as_ref()
    }

    /// Returns the pattern the command names its columns with, if any.
    ///
    /// Measured on MySQL 8.4.11: `SHOW COLUMNS FROM t LIKE 'n%'` and
    /// `DESCRIBE t 'n%'` answer the same rows, and a pattern nothing matches
    /// answers no rows rather than an error.
    pub fn pattern(&self) -> Option<&MySqlLikePattern> {
        self.pattern.as_ref()
    }

    /// Reports whether the command asked for the `FULL` columns.
    ///
    /// Measured on MySQL 8.4.11: `FULL` puts `Collation` third and appends
    /// `Privileges` and `Comment`.
    pub const fn full(&self) -> bool {
        self.full
    }
}

/// A checked read-only MySQL `SHOW INDEX` command for one base table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MySqlShowIndexCommand {
    database: Option<MySqlDatabaseName>,
    table: MySqlTableName,
}

impl MySqlShowIndexCommand {
    /// Returns the table identifier selected by the command.
    pub fn table(&self) -> &MySqlTableName {
        &self.table
    }

    /// Returns the `database.` qualifier the command was written with, if any.
    pub fn database(&self) -> Option<&MySqlDatabaseName> {
        self.database.as_ref()
    }
}

/// A checked read-only MySQL `SHOW CREATE TABLE` command for one base table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MySqlShowCreateTableCommand {
    database: Option<MySqlDatabaseName>,
    table: MySqlTableName,
}

impl MySqlShowCreateTableCommand {
    /// Returns the table identifier selected by the command.
    pub fn table(&self) -> &MySqlTableName {
        &self.table
    }

    /// Returns the `database.` qualifier the command was written with, if any.
    pub fn database(&self) -> Option<&MySqlDatabaseName> {
        self.database.as_ref()
    }
}

/// The scope a `SHOW VARIABLES` command reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MySqlVariableScope {
    /// The values this session is using.
    Session,
    /// The values the server started every session from.
    Global,
}

/// A checked read-only MySQL `SHOW VARIABLES` command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MySqlShowVariablesCommand {
    scope: MySqlVariableScope,
    pattern: Option<MySqlLikePattern>,
}

impl MySqlShowVariablesCommand {
    /// Returns the scope the command was written with.
    pub fn scope(&self) -> MySqlVariableScope {
        self.scope
    }

    /// Reports whether the command asks for the variable called `name`.
    pub fn selects(&self, name: &str) -> bool {
        self.pattern
            .as_ref()
            .is_none_or(|pattern| pattern.matches(name))
    }
}

/// A checked read-only MySQL `SHOW WARNINGS` command.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MySqlShowWarningsCommand {
    offset: u64,
    row_count: Option<u64>,
    count: bool,
}

impl MySqlShowWarningsCommand {
    /// Creates a new `SHOW WARNINGS` command with the given offset and limit.
    pub const fn new(offset: u64, row_count: Option<u64>) -> Self {
        Self {
            offset,
            row_count,
            count: false,
        }
    }

    /// Creates a new `SHOW COUNT(*) WARNINGS` command.
    pub const fn count() -> Self {
        Self {
            offset: 0,
            row_count: None,
            count: true,
        }
    }

    /// Returns whether this command only asks for the count of warnings.
    pub const fn is_count(&self) -> bool {
        self.count
    }

    /// Returns the number of warnings to skip from the beginning.
    pub const fn offset(&self) -> u64 {
        self.offset
    }

    /// Returns the maximum number of warnings to report, if limited.
    pub const fn row_count(&self) -> Option<u64> {
        self.row_count
    }
}

/// A checked read-only MySQL `SHOW ERRORS` command.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MySqlShowErrorsCommand {
    offset: u64,
    row_count: Option<u64>,
    count: bool,
}

impl MySqlShowErrorsCommand {
    /// Creates a new `SHOW ERRORS` command with the given offset and limit.
    pub const fn new(offset: u64, row_count: Option<u64>) -> Self {
        Self {
            offset,
            row_count,
            count: false,
        }
    }

    /// Creates a new `SHOW COUNT(*) ERRORS` command.
    pub const fn count() -> Self {
        Self {
            offset: 0,
            row_count: None,
            count: true,
        }
    }

    /// Returns whether this command only asks for the count of errors.
    pub const fn is_count(&self) -> bool {
        self.count
    }

    /// Returns the number of errors to skip from the beginning.
    pub const fn offset(&self) -> u64 {
        self.offset
    }

    /// Returns the maximum number of errors to report, if limited.
    pub const fn row_count(&self) -> Option<u64> {
        self.row_count
    }
}

/// One `KEY name (columns)` or `UNIQUE KEY name (columns)` lifted out of a
/// `CREATE TABLE`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MySqlInlineIndex {
    name: String,
    columns: Vec<String>,
    unique: bool,
}

impl MySqlInlineIndex {
    /// Returns the index name as the statement wrote it.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Returns the indexed columns, in order.
    pub fn columns(&self) -> &[String] {
        &self.columns
    }

    /// Whether the key lets one set of values stand in the table only once.
    pub const fn is_unique(&self) -> bool {
        self.unique
    }
}

/// A `CREATE TABLE` that declares plain indexes inline, split into the two
/// kinds of statement the engine takes.
///
/// The engine has no inline non-unique index, so one MySQL statement becomes a
/// `CREATE TABLE` and one `CREATE INDEX` per key. They have to apply together,
/// which is the caller's job.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MySqlCreateTableWithKeys {
    table: MySqlTableName,
    table_sql: String,
    indexes: Vec<MySqlInlineIndex>,
}

impl MySqlCreateTableWithKeys {
    /// Returns the table the statement creates.
    pub fn table(&self) -> &MySqlTableName {
        &self.table
    }

    /// Returns the `CREATE TABLE` with its key clauses removed.
    pub fn table_sql(&self) -> &str {
        &self.table_sql
    }

    /// Returns the keys the statement declared, in the order it wrote them.
    pub fn indexes(&self) -> &[MySqlInlineIndex] {
        &self.indexes
    }
}

/// Splits a `CREATE TABLE` that carries `KEY`, `INDEX` or `UNIQUE` clauses.
///
/// Returns `None` for a `CREATE TABLE` with no such clause, and for anything
/// that is not a `CREATE TABLE`, so the ordinary path keeps those. The index
/// options MySQL takes here are refused, since none of them could be printed
/// back.
pub fn parse_optional_create_table_with_keys(
    sql: &str,
    mode: SessionSqlMode,
) -> Result<Option<MySqlCreateTableWithKeys>, ParseError> {
    let Ok(Statement::CreateTable(table)) = parse_one_statement(sql, mode) else {
        return Ok(None);
    };
    if !table.constraints.iter().any(|constraint| {
        matches!(
            constraint,
            TableConstraint::Index(_) | TableConstraint::Unique(_)
        )
    }) {
        return Ok(None);
    }
    let [ObjectNamePart::Identifier(table_ident)] = table.name.0.as_slice() else {
        return unsupported("schema-qualified CREATE TABLE name");
    };
    let table_name =
        MySqlTableName::parse(&table_ident.value).map_err(|_| ParseError::Unsupported {
            feature: "CREATE TABLE name",
        })?;
    let mut indexes = Vec::new();
    let mut remaining = table.clone();
    remaining.constraints.clear();
    for constraint in &table.constraints {
        let (unique, written_name, index_type, index_options, index_columns) = match constraint {
            TableConstraint::Index(index) => (
                false,
                index.name.as_ref(),
                index.index_type.as_ref(),
                index.index_options.as_slice(),
                index.columns.as_slice(),
            ),
            TableConstraint::Unique(key) => {
                // `DEFERRABLE` and `NULLS [NOT] DISTINCT` are not MySQL's
                // spelling and each says something this cannot keep.
                if key.characteristics.is_some()
                    || !matches!(key.nulls_distinct, NullsDistinctOption::None)
                {
                    return unsupported("UNIQUE key option");
                }
                (
                    true,
                    // Measured on MySQL 8.4.11: `UNIQUE KEY uq (e)` and
                    // `CONSTRAINT uq UNIQUE (e)` both make an index called
                    // `uq`, so whichever of the two names the statement wrote
                    // is the index's.
                    key.index_name.as_ref().or(key.name.as_ref()),
                    key.index_type.as_ref(),
                    key.index_options.as_slice(),
                    key.columns.as_slice(),
                )
            }
            _ => {
                remaining.constraints.push(constraint.clone());
                continue;
            }
        };
        if index_type.is_some() || !index_options.is_empty() {
            return unsupported("index option");
        }
        let columns = inline_index_columns(index_columns)?;
        // Measured on MySQL 8.4.11: an unnamed key is named after its first
        // column, and where that is taken it gains `_2`, `_3` and so on. The
        // names it counts as taken are the ones written before it and the ones
        // named before it, in the order the statement wrote them — `KEY (a),
        // KEY a_2 (b), KEY (a)` names the three `a`, `a_2` and `a_3`.
        let name = match written_name {
            Some(index_name) => MySqlTableName::parse(&index_name.value)
                .map_err(|_| ParseError::Unsupported {
                    feature: "inline KEY name",
                })?
                .as_str()
                .to_owned(),
            None => inline_index_name(&indexes, &columns).ok_or(ParseError::Unsupported {
                feature: "inline KEY name",
            })?,
        };
        indexes.push(MySqlInlineIndex {
            name,
            columns,
            unique,
        });
    }
    Ok(Some(MySqlCreateTableWithKeys {
        table: table_name,
        table_sql: Statement::CreateTable(remaining).to_string(),
        indexes,
    }))
}

/// Names an inline key the statement left unnamed.
///
/// The name is the first column's, and where an earlier key in the same
/// statement already carries it, it gains `_2`, `_3` and so on until one is
/// free.
fn inline_index_name(named: &[MySqlInlineIndex], columns: &[String]) -> Option<String> {
    let first = columns.first()?;
    let taken = |candidate: &str| {
        named
            .iter()
            .any(|index| index.name.eq_ignore_ascii_case(candidate))
    };
    if !taken(first) {
        return Some(first.clone());
    }
    (2..=u32::MAX)
        .map(|suffix| format!("{first}_{suffix}"))
        .find(|candidate| !taken(candidate))
}

/// Reads the plain column names an inline key covers.
fn inline_index_columns(columns: &[IndexColumn]) -> Result<Vec<String>, ParseError> {
    let mut names = Vec::with_capacity(columns.len());
    for column in columns {
        if column.operator_class.is_some()
            || column.column.options.asc.is_some()
            || column.column.options.nulls_first.is_some()
        {
            return unsupported("indexed column ordering");
        }
        let Expr::Identifier(name) = &column.column.expr else {
            return unsupported("indexed column expression");
        };
        names.push(name.value.clone());
    }
    if names.is_empty() {
        return unsupported("inline KEY without columns");
    }
    Ok(names)
}

/// Parses the strict `SHOW TABLES` catalog command.
pub fn parse_show_tables(sql: &str, mode: SessionSqlMode) -> Result<MySqlShowCommand, ParseError> {
    parse_optional_show_tables(sql, mode)?.ok_or(ParseError::Unsupported {
        feature: "SHOW TABLES statement",
    })
}

/// Parses `SHOW TABLES [LIKE 'pattern']` when the statement belongs to the
/// catalog surface.
///
/// Other `SHOW` forms return `None` so that their own parser can handle them.
/// Once `SHOW TABLES` is recognized, an optional single semicolon is allowed;
/// comments, other clauses, and additional statements are rejected.
pub fn parse_optional_show_tables(
    sql: &str,
    mode: SessionSqlMode,
) -> Result<Option<MySqlShowCommand>, ParseError> {
    let tokens = tokenize_admin_command(sql, mode)?;
    let mut cursor = skip_admin_comments(&tokens, 0);
    if !consume_admin_word(&tokens, &mut cursor, "SHOW") {
        return Ok(None);
    }
    cursor = skip_admin_comments(&tokens, cursor);
    if !consume_admin_word(&tokens, &mut cursor, "TABLES") {
        return Ok(None);
    }
    let pattern = consume_admin_like_pattern(&tokens, &mut cursor, mode)?;
    if !admin_command_ends(&tokens, cursor) {
        return Err(ParseError::TrailingAdminCommandTokens);
    }
    Ok(Some(MySqlShowCommand { pattern }))
}

/// Reads an optional `LIKE 'pattern'` that follows an admin command.
pub(crate) fn consume_admin_like_pattern(
    tokens: &[AdminToken],
    cursor: &mut usize,
    mode: SessionSqlMode,
) -> Result<Option<MySqlLikePattern>, ParseError> {
    if !consume_admin_word(tokens, cursor, "LIKE") {
        return Ok(None);
    }
    *cursor = skip_admin_comments(tokens, *cursor);
    let Some(AdminToken::StringLiteral(pattern)) = tokens.get(*cursor) else {
        return Err(ParseError::ExpectedAdminCommand);
    };
    *cursor += 1;
    Ok(Some(MySqlLikePattern::new(pattern, mode)))
}

/// Parses the strict `information_schema.TABLES` catalog query.
pub fn parse_information_schema_tables(
    sql: &str,
    mode: SessionSqlMode,
) -> Result<MySqlInformationSchemaTablesQuery, ParseError> {
    parse_optional_information_schema_tables(sql, mode)?.ok_or(ParseError::Unsupported {
        feature: "information_schema.TABLES query",
    })
}

/// Parses the supported `information_schema.TABLES` query when it is present.
///
/// Other SELECT statements return `None` so that the ordinary SELECT parser can
/// handle them. Once a query names `information_schema.TABLES`, every clause is
/// checked against the one supported shape and unsupported variants fail closed.
pub fn parse_optional_information_schema_tables(
    sql: &str,
    mode: SessionSqlMode,
) -> Result<Option<MySqlInformationSchemaTablesQuery>, ParseError> {
    let tokens = tokenize_information_schema_query(sql, mode)?;
    let first = tokens
        .iter()
        .find(|token| !matches!(token, Token::Whitespace(_)));
    if !matches!(first, Some(token) if is_unquoted_word(token, "SELECT")) {
        return Ok(None);
    }
    if !contains_information_schema_tables(&tokens) {
        return Ok(None);
    }
    reject_information_schema_query_tokens(&tokens)?;

    let statement = parse_one_statement(sql, mode)?;
    let Statement::Query(query) = statement else {
        return Err(ParseError::ExpectedSelect);
    };
    let columns = validate_information_schema_tables_query(&query)?;
    Ok(Some(MySqlInformationSchemaTablesQuery { columns }))
}

/// Parses the strict `information_schema.SCHEMATA` catalog query.
pub fn parse_information_schema_schemata(
    sql: &str,
    mode: SessionSqlMode,
) -> Result<MySqlInformationSchemaSchemataQuery, ParseError> {
    parse_optional_information_schema_schemata(sql, mode)?.ok_or(ParseError::Unsupported {
        feature: "information_schema.SCHEMATA query",
    })
}

/// Parses the supported `information_schema.SCHEMATA` query when it is present.
///
/// Other SELECT statements return `None` so that the ordinary SELECT parser can
/// handle them. Once a query names `information_schema.SCHEMATA`, every clause
/// is checked against the one supported shape and unsupported variants fail
/// closed.
pub fn parse_optional_information_schema_schemata(
    sql: &str,
    mode: SessionSqlMode,
) -> Result<Option<MySqlInformationSchemaSchemataQuery>, ParseError> {
    let tokens = tokenize_information_schema_query(sql, mode)?;
    let first = tokens
        .iter()
        .find(|token| !matches!(token, Token::Whitespace(_)));
    if !matches!(first, Some(token) if is_unquoted_word(token, "SELECT")) {
        return Ok(None);
    }
    if !contains_information_schema_object(&tokens, "SCHEMATA") {
        return Ok(None);
    }
    reject_information_schema_schemata_query_tokens(&tokens)?;

    let statement = parse_one_statement(sql, mode)?;
    let Statement::Query(query) = statement else {
        return Err(ParseError::ExpectedSelect);
    };
    validate_information_schema_schemata_query(&query)?;
    Ok(Some(MySqlInformationSchemaSchemataQuery))
}

/// Parses the strict `information_schema.COLUMNS` catalog query.
pub fn parse_information_schema_columns(
    sql: &str,
    mode: SessionSqlMode,
) -> Result<MySqlInformationSchemaColumnsQuery, ParseError> {
    parse_optional_information_schema_columns(sql, mode)?.ok_or(ParseError::Unsupported {
        feature: "information_schema.COLUMNS query",
    })
}

/// Parses the supported `information_schema.COLUMNS` query when it is present.
///
/// Other SELECT statements return `None` so that the ordinary SELECT parser can
/// handle them. Once a query names `information_schema.COLUMNS`, every clause is
/// checked against the one supported shape and unsupported variants fail closed.
pub fn parse_optional_information_schema_columns(
    sql: &str,
    mode: SessionSqlMode,
) -> Result<Option<MySqlInformationSchemaColumnsQuery>, ParseError> {
    let tokens = tokenize_information_schema_query(sql, mode)?;
    let first = tokens
        .iter()
        .find(|token| !matches!(token, Token::Whitespace(_)));
    if !matches!(first, Some(token) if is_unquoted_word(token, "SELECT")) {
        return Ok(None);
    }
    if !contains_information_schema_object(&tokens, "COLUMNS") {
        return Ok(None);
    }
    reject_information_schema_columns_query_tokens(&tokens)?;

    let statement = parse_one_statement(sql, mode)?;
    let Statement::Query(query) = statement else {
        return Err(ParseError::ExpectedSelect);
    };
    let (schema, table, columns) = validate_information_schema_columns_query(&query)?;
    Ok(Some(MySqlInformationSchemaColumnsQuery {
        schema,
        table,
        columns,
    }))
}

/// Parses the strict `SHOW COLUMNS FROM table` catalog command.
pub fn parse_show_columns(
    sql: &str,
    mode: SessionSqlMode,
) -> Result<MySqlShowColumnsCommand, ParseError> {
    parse_optional_show_columns(sql, mode)?.ok_or(ParseError::Unsupported {
        feature: "SHOW COLUMNS statement",
    })
}

/// Parses `SHOW COLUMNS FROM table` when the statement belongs to the catalog
/// surface.
///
/// Other `SHOW` forms return `None` so that their own parser can handle them.
/// Once `SHOW COLUMNS` is recognized, this accepts one unqualified identifier
/// and an optional single semicolon. Comments, clauses, database qualifiers,
/// and additional statements are rejected.
pub fn parse_optional_show_columns(
    sql: &str,
    mode: SessionSqlMode,
) -> Result<Option<MySqlShowColumnsCommand>, ParseError> {
    let tokens = tokenize_admin_command(sql, mode)?;
    let mut cursor = skip_admin_comments(&tokens, 0);
    if !consume_admin_word(&tokens, &mut cursor, "SHOW") {
        return Ok(None);
    }
    let full = consume_admin_word(&tokens, &mut cursor, "FULL");
    if !consume_admin_word(&tokens, &mut cursor, "COLUMNS") {
        return Ok(None);
    }
    if !consume_admin_word(&tokens, &mut cursor, "FROM") {
        return Err(ParseError::ExpectedAdminCommand);
    }
    let (database, table) = consume_admin_qualified_table_name(&tokens, &mut cursor)?;
    let pattern = if consume_admin_word(&tokens, &mut cursor, "LIKE") {
        let Some(AdminToken::StringLiteral(pattern)) = tokens.get(cursor) else {
            return Err(ParseError::ExpectedAdminCommand);
        };
        cursor += 1;
        Some(MySqlLikePattern::new(pattern, mode))
    } else {
        None
    };
    if !admin_command_ends(&tokens, cursor) {
        return Err(ParseError::TrailingAdminCommandTokens);
    }
    Ok(Some(MySqlShowColumnsCommand {
        database,
        table,
        pattern,
        full,
    }))
}

/// Parses the strict `SHOW INDEX FROM table` catalog command.
pub fn parse_show_index(
    sql: &str,
    mode: SessionSqlMode,
) -> Result<MySqlShowIndexCommand, ParseError> {
    parse_optional_show_index(sql, mode)?.ok_or(ParseError::Unsupported {
        feature: "SHOW INDEX statement",
    })
}

/// Parses `SHOW INDEX FROM table` when the statement belongs to the catalog
/// surface.
///
/// MySQL spells this three ways and takes either `FROM` or `IN`, so all six
/// spellings are read. Other `SHOW` forms return `None` for their own parser.
pub fn parse_optional_show_index(
    sql: &str,
    mode: SessionSqlMode,
) -> Result<Option<MySqlShowIndexCommand>, ParseError> {
    let tokens = tokenize_admin_command(sql, mode)?;
    let mut cursor = skip_admin_comments(&tokens, 0);
    if !consume_admin_word(&tokens, &mut cursor, "SHOW") {
        return Ok(None);
    }
    if !["INDEX", "INDEXES", "KEYS"]
        .iter()
        .any(|word| consume_admin_word(&tokens, &mut cursor, word))
    {
        return Ok(None);
    }
    if !["FROM", "IN"]
        .iter()
        .any(|word| consume_admin_word(&tokens, &mut cursor, word))
    {
        return Err(ParseError::ExpectedAdminCommand);
    }
    let (database, table) = consume_admin_qualified_table_name(&tokens, &mut cursor)?;
    if !admin_command_ends(&tokens, cursor) {
        return Err(ParseError::TrailingAdminCommandTokens);
    }
    Ok(Some(MySqlShowIndexCommand { database, table }))
}

/// Parses the strict `SHOW [SESSION|GLOBAL] VARIABLES [LIKE 'pattern']` command.
pub fn parse_show_variables(
    sql: &str,
    mode: SessionSqlMode,
) -> Result<MySqlShowVariablesCommand, ParseError> {
    parse_optional_show_variables(sql, mode)?.ok_or(ParseError::Unsupported {
        feature: "SHOW VARIABLES statement",
    })
}

/// Parses `SHOW WARNINGS` or `SHOW COUNT(*) WARNINGS`, which report what the last statement warned about.
pub fn parse_optional_show_warnings(
    sql: &str,
    mode: SessionSqlMode,
) -> Result<Option<MySqlShowWarningsCommand>, ParseError> {
    parse_optional_show_diagnostics(sql, mode, "WARNINGS").map(|opt| {
        opt.map(|diag| {
            if diag.count {
                MySqlShowWarningsCommand::count()
            } else {
                MySqlShowWarningsCommand::new(diag.offset, diag.row_count)
            }
        })
    })
}

/// Parses `SHOW ERRORS` or `SHOW COUNT(*) ERRORS`, which report the errors the last statement raised.
pub fn parse_optional_show_errors(
    sql: &str,
    mode: SessionSqlMode,
) -> Result<Option<MySqlShowErrorsCommand>, ParseError> {
    parse_optional_show_diagnostics(sql, mode, "ERRORS").map(|opt| {
        opt.map(|diag| {
            if diag.count {
                MySqlShowErrorsCommand::count()
            } else {
                MySqlShowErrorsCommand::new(diag.offset, diag.row_count)
            }
        })
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ParsedDiagnostics {
    count: bool,
    offset: u64,
    row_count: Option<u64>,
}

fn parse_optional_show_diagnostics(
    sql: &str,
    mode: SessionSqlMode,
    target: &str,
) -> Result<Option<ParsedDiagnostics>, ParseError> {
    let Ok(tokens) = tokenize_admin_command(sql, mode) else {
        return Ok(None);
    };
    let mut cursor = skip_admin_comments(&tokens, 0);
    if !consume_admin_word(&tokens, &mut cursor, "SHOW") {
        return Ok(None);
    }
    cursor = skip_admin_comments(&tokens, cursor);
    if consume_admin_word(&tokens, &mut cursor, "COUNT") {
        cursor = skip_admin_comments(&tokens, cursor);
        if !matches!(tokens.get(cursor), Some(AdminToken::LeftParen)) {
            return Err(ParseError::ExpectedAdminCommand);
        }
        cursor += 1;
        cursor = skip_admin_comments(&tokens, cursor);
        if !matches!(tokens.get(cursor), Some(AdminToken::Star)) {
            return Err(ParseError::ExpectedAdminCommand);
        }
        cursor += 1;
        cursor = skip_admin_comments(&tokens, cursor);
        if !matches!(tokens.get(cursor), Some(AdminToken::RightParen)) {
            return Err(ParseError::ExpectedAdminCommand);
        }
        cursor += 1;
        cursor = skip_admin_comments(&tokens, cursor);
        if !consume_admin_word(&tokens, &mut cursor, target) {
            if (target.eq_ignore_ascii_case("WARNINGS")
                && is_diagnostic_word(&tokens, cursor, "ERRORS"))
                || (target.eq_ignore_ascii_case("ERRORS")
                    && is_diagnostic_word(&tokens, cursor, "WARNINGS"))
            {
                return Ok(None);
            }
            return Err(ParseError::ExpectedAdminCommand);
        }
        if !admin_command_ends(&tokens, cursor) {
            return Err(ParseError::TrailingAdminCommandTokens);
        }
        return Ok(Some(ParsedDiagnostics {
            count: true,
            offset: 0,
            row_count: None,
        }));
    }
    if !consume_admin_word(&tokens, &mut cursor, target) {
        return Ok(None);
    }
    let mut offset = 0;
    let mut row_count = None;
    if consume_admin_word(&tokens, &mut cursor, "LIMIT") {
        let first =
            consume_admin_u64(&tokens, &mut cursor).ok_or(ParseError::ExpectedAdminCommand)?;
        if matches!(tokens.get(cursor), Some(AdminToken::Comma)) {
            cursor += 1;
            let second =
                consume_admin_u64(&tokens, &mut cursor).ok_or(ParseError::ExpectedAdminCommand)?;
            offset = first;
            row_count = Some(second);
        } else if consume_admin_word(&tokens, &mut cursor, "OFFSET") {
            let second =
                consume_admin_u64(&tokens, &mut cursor).ok_or(ParseError::ExpectedAdminCommand)?;
            offset = second;
            row_count = Some(first);
        } else {
            offset = 0;
            row_count = Some(first);
        }
    }
    if !admin_command_ends(&tokens, cursor) {
        return Err(ParseError::TrailingAdminCommandTokens);
    }
    Ok(Some(ParsedDiagnostics {
        count: false,
        offset,
        row_count,
    }))
}

fn is_diagnostic_word(tokens: &[AdminToken], cursor: usize, expected: &str) -> bool {
    matches!(tokens.get(cursor), Some(AdminToken::Word(word)) if word.eq_ignore_ascii_case(expected))
}

pub fn parse_optional_show_variables(
    sql: &str,
    mode: SessionSqlMode,
) -> Result<Option<MySqlShowVariablesCommand>, ParseError> {
    // This parser runs before every other statement surface, so text it cannot
    // even split into tokens belongs to whichever parser comes next.
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
        // Measured on MySQL 8.4.11: `LOCAL` names the session scope too.
        let _ = consume_admin_word(&tokens, &mut cursor, "SESSION")
            || consume_admin_word(&tokens, &mut cursor, "LOCAL");
        MySqlVariableScope::Session
    };
    if !consume_admin_word(&tokens, &mut cursor, "VARIABLES") {
        return Ok(None);
    }
    let pattern = if consume_admin_word(&tokens, &mut cursor, "LIKE") {
        let Some(AdminToken::StringLiteral(pattern)) = tokens.get(cursor) else {
            return Err(ParseError::ExpectedAdminCommand);
        };
        cursor += 1;
        Some(MySqlLikePattern::new(pattern, mode))
    } else {
        None
    };
    if !admin_command_ends(&tokens, cursor) {
        return Err(ParseError::TrailingAdminCommandTokens);
    }
    Ok(Some(MySqlShowVariablesCommand { scope, pattern }))
}

/// Parses the strict `SHOW CREATE TABLE table` catalog command.
pub fn parse_show_create_table(
    sql: &str,
    mode: SessionSqlMode,
) -> Result<MySqlShowCreateTableCommand, ParseError> {
    parse_optional_show_create_table(sql, mode)?.ok_or(ParseError::Unsupported {
        feature: "SHOW CREATE TABLE statement",
    })
}

/// Parses `SHOW CREATE TABLE table` when the statement belongs to the catalog
/// surface.
///
/// Other `SHOW` forms return `None` so that their own parser can handle them.
/// Once `SHOW CREATE TABLE` is recognized, this accepts one unqualified
/// identifier and an optional single semicolon. Comments, clauses, database
/// qualifiers, and additional statements are rejected, matching the other
/// catalog commands. MySQL is looser: it also takes a leading comment, a
/// second semicolon, and a `db.table` qualifier.
pub fn parse_optional_show_create_table(
    sql: &str,
    mode: SessionSqlMode,
) -> Result<Option<MySqlShowCreateTableCommand>, ParseError> {
    let tokens = tokenize_admin_command(sql, mode)?;
    let mut cursor = skip_admin_comments(&tokens, 0);
    if !consume_admin_word(&tokens, &mut cursor, "SHOW") {
        return Ok(None);
    }
    if !consume_admin_word(&tokens, &mut cursor, "CREATE") {
        return Ok(None);
    }
    if !consume_admin_word(&tokens, &mut cursor, "TABLE") {
        return Ok(None);
    }
    let (database, table) = consume_admin_qualified_table_name(&tokens, &mut cursor)?;
    if !admin_command_ends(&tokens, cursor) {
        return Err(ParseError::TrailingAdminCommandTokens);
    }
    Ok(Some(MySqlShowCreateTableCommand { database, table }))
}

/// Parses the strict `DESCRIBE table` catalog command.
pub fn parse_describe(
    sql: &str,
    mode: SessionSqlMode,
) -> Result<MySqlShowColumnsCommand, ParseError> {
    parse_optional_describe(sql, mode)?.ok_or(ParseError::Unsupported {
        feature: "DESCRIBE statement",
    })
}

/// Parses `DESCRIBE table`, its minimal `DESC table` alias, or `EXPLAIN table`.
///
/// Other commands return `None` so their own parser can handle them. Once one
/// of the keywords is recognized, this accepts one unqualified identifier and
/// an optional single semicolon. Comments, clauses, database qualifiers, and
/// additional statements are rejected.
///
/// Measured on MySQL 8.4.11: `EXPLAIN t` prints exactly what `DESCRIBE t`
/// prints. `EXPLAIN <statement>` is the optimizer's plan instead, so anything
/// after `EXPLAIN` that is not one lone name is left for the ordinary path,
/// which refuses it.
pub fn parse_optional_describe(
    sql: &str,
    mode: SessionSqlMode,
) -> Result<Option<MySqlShowColumnsCommand>, ParseError> {
    let tokens = tokenize_admin_command(sql, mode)?;
    let mut cursor = skip_admin_comments(&tokens, 0);
    let describes = consume_admin_word(&tokens, &mut cursor, "DESCRIBE")
        || consume_admin_word(&tokens, &mut cursor, "DESC");
    if !describes && !consume_admin_word(&tokens, &mut cursor, "EXPLAIN") {
        return Ok(None);
    }
    // MySQL reads an unquoted `TABLE` here as a keyword rather than a name:
    // measured, `DESCRIBE TABLE reports` answers 1146 for `reports`, where a
    // plain reading would name `TABLE`. That form is refused rather than
    // answered about the wrong table.
    if matches!(tokens.get(cursor), Some(AdminToken::Word(word)) if word.eq_ignore_ascii_case("TABLE"))
    {
        return if describes {
            Err(ParseError::ExpectedAdminCommand)
        } else {
            Ok(None)
        };
    }
    let Ok((database, table)) = consume_admin_qualified_table_name(&tokens, &mut cursor) else {
        return if describes {
            Err(ParseError::ExpectedAdminCommand)
        } else {
            Ok(None)
        };
    };
    // Measured on MySQL 8.4.11: `DESCRIBE t <name>` names the columns the way
    // `SHOW COLUMNS FROM t LIKE <name>` does, quoted or not. It is read only
    // for the `DESCRIBE` and `DESC` spellings: after `EXPLAIN`, a second word
    // is as likely to be the statement whose plan was asked for, and no shape
    // tells `EXPLAIN t 1` from `EXPLAIN SELECT 1`.
    let pattern = match tokens.get(cursor) {
        Some(
            AdminToken::StringLiteral(pattern)
            | AdminToken::QuotedIdentifier(pattern)
            | AdminToken::Word(pattern),
        ) if describes => {
            let pattern = MySqlLikePattern::new(pattern, mode);
            cursor += 1;
            Some(pattern)
        }
        _ => None,
    };
    if !admin_command_ends(&tokens, cursor) {
        return if describes {
            Err(ParseError::TrailingAdminCommandTokens)
        } else {
            Ok(None)
        };
    }
    Ok(Some(MySqlShowColumnsCommand {
        database,
        table,
        pattern,
        full: false,
    }))
}

/// One checked MySQL transaction-control command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MySqlTransactionCommand {
    Begin,
    /// `START TRANSACTION READ ONLY`. MySQL answers 1792 to a write inside one.
    BeginReadOnly,
    Commit,
    Rollback,
    /// `COMMIT AND CHAIN`, which commits and begins another transaction at
    /// once. Measured on MySQL 8.4.11: it leaves the session in a transaction
    /// even when autocommit is on.
    CommitAndChain,
    /// `ROLLBACK AND CHAIN`, the same for a rollback.
    RollbackAndChain,
    /// `SAVEPOINT name`, which marks a point inside a transaction.
    Savepoint(String),
    /// `ROLLBACK TO [SAVEPOINT] name`, which undoes the work since that point
    /// and, unlike a plain `ROLLBACK`, leaves the transaction open.
    RollbackToSavepoint(String),
    /// `RELEASE SAVEPOINT name`, which forgets the point without undoing
    /// anything.
    ReleaseSavepoint(String),
}

/// One checked change to the MySQL session's autocommit mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MySqlAutocommitSetting {
    pub enabled: bool,
}

/// One MySQL driver bootstrap query recognized by this parser.
///
/// This intentionally identifies the complete wire query, not general MySQL
/// `SELECT` syntax. It keeps the bootstrap response contract separate from
/// queries that happen to read system variables.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MySqlDriverBootstrapQuery {
    MaxAllowedPacketAndWaitTimeout,
}

/// Parses the exact settings query sent by the pinned `mysql_async` driver.
///
/// The driver constructs this query as `SELECT @@max_allowed_packet,@@wait_timeout`.
/// It does not send a semicolon, aliases, qualifiers, or extra whitespace.
/// Accepting only those bytes prevents this bootstrap path from becoming a
/// general system-variable SELECT parser.
pub fn parse_driver_bootstrap_query(sql: &str) -> Result<MySqlDriverBootstrapQuery, ParseError> {
    if sql == "SELECT @@max_allowed_packet,@@wait_timeout" {
        Ok(MySqlDriverBootstrapQuery::MaxAllowedPacketAndWaitTimeout)
    } else {
        unsupported("mysql_async driver bootstrap query")
    }
}

/// Parses a strict `SET [SESSION] autocommit = 0|1` statement.
pub fn parse_autocommit_setting(
    sql: &str,
    mode: SessionSqlMode,
) -> Result<MySqlAutocommitSetting, ParseError> {
    parse_optional_autocommit_setting(sql, mode)?.ok_or(ParseError::Unsupported {
        feature: "SET autocommit statement",
    })
}

/// Parses a supported autocommit assignment when the statement starts with `SET`.
pub fn parse_optional_autocommit_setting(
    sql: &str,
    mode: SessionSqlMode,
) -> Result<Option<MySqlAutocommitSetting>, ParseError> {
    let dialect = SessionMySqlDialect::without_executable_comments(mode);
    let tokens = Tokenizer::new(&dialect, sql)
        .tokenize()
        .map_err(|error| ParseError::Sqlparser(error.to_string()))?;
    let tokens = tokens
        .iter()
        .filter(|token| {
            !matches!(
                token,
                Token::Whitespace(Whitespace::Space | Whitespace::Newline | Whitespace::Tab)
            )
        })
        .collect::<Vec<_>>();
    let Some(first_significant) = tokens
        .iter()
        .find(|token| !matches!(token, Token::Whitespace(_)))
    else {
        return Ok(None);
    };
    if !is_unquoted_word(first_significant, "SET") {
        return Ok(None);
    }
    if tokens
        .iter()
        .any(|token| matches!(token, Token::Whitespace(_)))
    {
        return unsupported("comments in SET autocommit");
    }
    let tokens = tokens.strip_suffix(&[&Token::SemiColon]).unwrap_or(&tokens);
    let assignment = match tokens {
        [set, name, equals, value]
            if is_unquoted_word(set, "SET")
                && is_unquoted_word(name, "AUTOCOMMIT")
                && matches!(equals, Token::Eq) =>
        {
            value
        }
        [set, session, name, equals, value]
            if is_unquoted_word(set, "SET")
                && is_unquoted_word(session, "SESSION")
                && is_unquoted_word(name, "AUTOCOMMIT")
                && matches!(equals, Token::Eq) =>
        {
            value
        }
        _ => return unsupported("SET autocommit syntax"),
    };
    let enabled = match assignment {
        Token::Number(value, false) if value == "0" => false,
        Token::Number(value, false) if value == "1" => true,
        _ => return unsupported("SET autocommit value; expected 0 or 1"),
    };
    Ok(Some(MySqlAutocommitSetting { enabled }))
}

/// Parses exactly one transaction-control command without options.
pub fn parse_transaction_command(
    sql: &str,
    mode: SessionSqlMode,
) -> Result<MySqlTransactionCommand, ParseError> {
    parse_optional_transaction_command(sql, mode)?.ok_or(ParseError::ExpectedTransactionCommand)
}

/// Parses a transaction-control command when the statement belongs to that surface.
///
/// `BEGIN` and `START TRANSACTION` both return [`MySqlTransactionCommand::Begin`].
/// The three savepoint statements are read here too. Transaction modes,
/// comments, and additional statements are rejected.
pub fn parse_optional_transaction_command(
    sql: &str,
    mode: SessionSqlMode,
) -> Result<Option<MySqlTransactionCommand>, ParseError> {
    if let Some(command) = savepoint_command(sql, mode)? {
        return Ok(Some(command));
    }
    let token_kind = transaction_token_kind(sql, mode)?;
    if token_kind == TransactionTokenKind::Other {
        return Ok(None);
    }
    if token_kind == TransactionTokenKind::Invalid {
        return unsupported("transaction options");
    }
    // sqlparser cannot parse this one, so the token check is where it is read.
    // It begins a transaction, which is what the engine does with it; MySQL
    // takes the read view at the statement and the engine takes it at the
    // first read, which COMPAT.md records.
    if token_kind == TransactionTokenKind::ConsistentSnapshot {
        return Ok(Some(MySqlTransactionCommand::Begin));
    }
    let statement = parse_one_statement(sql, mode)?;
    let command = match statement {
        Statement::StartTransaction {
            modes,
            begin,
            transaction,
            modifier,
            statements,
            exception,
            has_end_keyword,
        } => {
            if modifier.is_some()
                || !statements.is_empty()
                || exception.is_some()
                || has_end_keyword
                || (!begin && transaction.is_none())
            {
                return unsupported("transaction options");
            }
            // `READ WRITE` is the default spelled out, so it changes nothing.
            // `READ ONLY` is a promise MySQL keeps with 1792, and this keeps it
            // too rather than accepting the words and ignoring them.
            match modes.as_slice() {
                [] => MySqlTransactionCommand::Begin,
                [sqlparser::ast::TransactionMode::AccessMode(
                    sqlparser::ast::TransactionAccessMode::ReadWrite,
                )] => MySqlTransactionCommand::Begin,
                [sqlparser::ast::TransactionMode::AccessMode(
                    sqlparser::ast::TransactionAccessMode::ReadOnly,
                )] => MySqlTransactionCommand::BeginReadOnly,
                _ => return unsupported("transaction options"),
            }
        }
        Statement::Commit {
            chain,
            end,
            modifier,
        } => {
            if end || modifier.is_some() {
                return unsupported("COMMIT options");
            }
            if chain {
                MySqlTransactionCommand::CommitAndChain
            } else {
                MySqlTransactionCommand::Commit
            }
        }
        Statement::Rollback { chain, savepoint } => {
            if savepoint.is_some() {
                return unsupported("ROLLBACK options");
            }
            if chain {
                MySqlTransactionCommand::RollbackAndChain
            } else {
                MySqlTransactionCommand::Rollback
            }
        }
        _ => return Ok(None),
    };
    Ok(Some(command))
}

/// Parses exactly one MySQL `CREATE TABLE` statement and translates the supported subset to SQLite.
pub fn parse_create_table(
    sql: &str,
    mode: SessionSqlMode,
) -> Result<TranslatedCreateTable, ParseError> {
    reject_unsupported_mysql_string_escapes(sql, mode)?;
    let statement = parse_one_statement(sql, mode)?;
    let Statement::CreateTable(table) = statement else {
        return Err(ParseError::ExpectedCreateTable);
    };
    translate_create_table(&table)
}

/// Parses the deliberately narrow MySQL `AUTO_INCREMENT` table shape.
///
/// This is separate from [`parse_create_table`] while the frontend has no
/// allocator-backed execution path. It accepts exactly one inline signed `INT`
/// or `INTEGER NOT NULL AUTO_INCREMENT PRIMARY KEY` column in that token order.
pub fn parse_auto_increment_create_table(
    sql: &str,
    mode: SessionSqlMode,
) -> Result<CheckedAutoIncrementCreateTable, ParseError> {
    reject_unsupported_mysql_string_escapes(sql, mode)?;
    validate_auto_increment_token_shape(sql, mode)?;
    let statement = parse_one_statement(sql, mode)?;
    let Statement::CreateTable(table) = statement else {
        return Err(ParseError::ExpectedCreateTable);
    };
    translate_auto_increment_create_table(&table, mode)
}

fn validate_auto_increment_token_shape(sql: &str, mode: SessionSqlMode) -> Result<(), ParseError> {
    let dialect = SessionMySqlDialect::without_executable_comments(mode);
    let tokens = Tokenizer::new(&dialect, sql)
        .tokenize()
        .map_err(|error| ParseError::Sqlparser(error.to_string()))?;
    if tokens.iter().any(|token| {
        matches!(
            token,
            Token::Whitespace(Whitespace::MultiLineComment(comment)) if comment.starts_with('!')
        )
    }) {
        return unsupported("executable comment in AUTO_INCREMENT definition");
    }
    let tokens = tokens
        .iter()
        .filter(|token| !matches!(token, Token::Whitespace(_)))
        .collect::<Vec<_>>();
    let positions = tokens
        .iter()
        .enumerate()
        .filter_map(|(index, token)| is_unquoted_word(token, "AUTO_INCREMENT").then_some(index))
        .collect::<Vec<_>>();
    let [position] = positions.as_slice() else {
        return unsupported("exactly one AUTO_INCREMENT token");
    };
    let position = *position;
    if position < 2
        || position + 1 >= tokens.len()
        || !is_unquoted_word(tokens[position - 2], "NOT")
        || !is_unquoted_word(tokens[position - 1], "NULL")
    {
        return unsupported("AUTO_INCREMENT token order; expected NOT NULL AUTO_INCREMENT");
    }
    // The column may declare the key itself, or the table may write it as a
    // clause after the columns — the spelling MySQL prints and every dumped
    // schema carries. Where it is written as a clause the words are moved onto
    // the column before the checks below run, so what follows the marker here
    // is the end of the column instead.
    let declares_the_key = position + 3 < tokens.len()
        && is_unquoted_word(tokens[position + 1], "PRIMARY")
        && is_unquoted_word(tokens[position + 2], "KEY")
        && matches!(tokens[position + 3], Token::Comma | Token::RParen);
    let ends_the_column = matches!(tokens[position + 1], Token::Comma | Token::RParen);
    if !declares_the_key && !ends_the_column {
        return unsupported(
            "AUTO_INCREMENT token order; expected NOT NULL AUTO_INCREMENT PRIMARY KEY",
        );
    }
    Ok(())
}

fn is_unquoted_word(token: &Token, expected: &str) -> bool {
    matches!(
        token,
        Token::Word(word)
            if word.quote_style.is_none() && word.value.eq_ignore_ascii_case(expected)
    )
}

/// Parses exactly one MySQL `SELECT` statement and translates the supported
/// semantics-preserving subset to SQLite SQL.
pub fn parse_select(sql: &str, mode: SessionSqlMode) -> Result<TranslatedSelect, ParseError> {
    parse_select_with_column_types(sql, mode, &[], &[], &[])
}

/// Parses a checked `SELECT`, told which columns hold text, which hold a
/// moment, and what columns the table has.
///
/// This is the whole of what the frontend knows and the parser cannot see.
/// [`parse_select_with_column_types`] is the same thing for a caller that has
/// no moment columns to name.
pub fn parse_select_knowing_the_columns(
    sql: &str,
    mode: SessionSqlMode,
    text_columns: &[String],
    table_columns: &[String],
    member_columns: &[(String, Vec<String>)],
    moment_columns: &[String],
) -> Result<TranslatedSelect, ParseError> {
    parse_select_inner(
        sql,
        mode,
        text_columns,
        table_columns,
        member_columns,
        moment_columns,
    )
}

/// Parses a checked `SELECT`, told which of the table's columns are text and
/// what columns the table contains in declaration order.
///
/// Only the frontend can see a column's type and table schema, so an ordinary
/// parse renders without that and this one renders with it. A statement whose
/// rendering depends on it says so through `needs_column_types`, and the frontend
/// parses it a second time; everything else is rendered once.
pub fn parse_select_with_column_types(
    sql: &str,
    mode: SessionSqlMode,
    text_columns: &[String],
    table_columns: &[String],
    member_columns: &[(String, Vec<String>)],
) -> Result<TranslatedSelect, ParseError> {
    parse_select_inner(sql, mode, text_columns, table_columns, member_columns, &[])
}

fn parse_select_inner(
    sql: &str,
    mode: SessionSqlMode,
    text_columns: &[String],
    table_columns: &[String],
    member_columns: &[(String, Vec<String>)],
    moment_columns: &[String],
) -> Result<TranslatedSelect, ParseError> {
    let statement = parse_one_statement(sql, mode)?;
    let Statement::Query(query) = statement else {
        return Err(ParseError::ExpectedSelect);
    };
    let tokens = Tokenizer::new(&SessionMySqlDialect::new(mode), sql)
        .tokenize()
        .map_err(|error| ParseError::Sqlparser(error.to_string()))?;
    let significant = tokens
        .iter()
        .filter(|token| !matches!(token, Token::Whitespace(_)))
        .collect::<Vec<_>>();
    // sqlparser drops LIMIT ALL from the query AST.
    if significant
        .windows(2)
        .any(|tokens| is_unquoted_word(tokens[0], "LIMIT") && is_unquoted_word(tokens[1], "ALL"))
    {
        return unsupported("SELECT LIMIT ALL");
    }
    let static_result_metadata = select_static_result_metadata(&query);
    let RenderedSelect {
        sqlite_sql,
        source_table,
        source_tables,
        checked_comparisons,
        locks_rows,
        row_count_parameters,
        parameter_count,
        orders_a_bare_column,
        orders_wildcard_ordinal,
        compares_a_placeholder,
        counts_distinct_column,
        tests_a_bare_column,
        compares_a_written_day,
        checked_subquery_comparisons,
    } = translate_select_query(
        &query,
        sql,
        mode,
        text_columns,
        table_columns,
        member_columns,
        moment_columns,
    )?;
    Ok(TranslatedSelect {
        reads_table: !source_tables.is_empty(),
        orders_a_bare_column,
        orders_wildcard_ordinal,
        compares_a_placeholder,
        counts_distinct_column,
        tests_a_bare_column,
        compares_a_written_day,
        checked_subquery_comparisons,
        sqlite_sql,
        source_table,
        source_tables,
        static_result_metadata,
        checked_comparisons,
        locks_rows,
        row_count_parameters,
        parameter_count,
    })
}

/// Parses one checked MySQL `SELECT` into Turso's SQLite AST.
pub fn parse_select_ast(sql: &str, mode: SessionSqlMode) -> Result<Stmt, ParseError> {
    let translated = parse_select(sql, mode)?;
    translated.parse_ast()
}

/// Parses exactly one MySQL `INSERT`, `UPDATE`, or `DELETE` statement in the checked DML subset.
pub fn parse_dml(sql: &str, mode: SessionSqlMode) -> Result<TranslatedDml, ParseError> {
    let statement = parse_one_statement(sql, mode)?;
    let mut render_context = SelectRenderContext::new(sql, mode, &[], &[], &[], &[]);
    let read_tables;
    let mut inherited_comparisons = Vec::new();
    let (sqlite_sql, checked_update, source_table) = match statement {
        Statement::Insert(insert) => {
            let rendered = translate_insert(&insert, sql, mode)?;
            read_tables = rendered.read_tables;
            inherited_comparisons = rendered.checked_comparisons;
            // An INSERT ... SELECT compares against the table the SELECT reads,
            // not the one it writes, so that is the table the comparisons are
            // checked against.
            (rendered.sqlite_sql, None, rendered.compared_table)
        }
        Statement::Update(update) => {
            let (rendered, tables, checked) = translate_update(&update, &mut render_context)?;
            read_tables = tables;
            let table = checked.table_name().to_owned();
            (rendered, Some(checked), Some(table))
        }
        Statement::Delete(delete) => {
            let (rendered, tables) = translate_delete(&delete, &mut render_context)?;
            read_tables = tables;
            (rendered, None, delete_source_table(&delete))
        }
        _ => return Err(ParseError::ExpectedDml),
    };
    let mut ordered_columns = Vec::new();
    if let Some(source_table) = &source_table {
        for comparison in &render_context.checked_comparisons {
            if let Some(qualifier) = comparison.qualifier() {
                // A joined DELETE reads several tables, and the qualifier is
                // what says which of them a comparison's column belongs to.
                let names_a_read_table = read_tables
                    .iter()
                    .any(|source| qualifier.eq_ignore_ascii_case(source.reference()));
                if !qualifier.eq_ignore_ascii_case(source_table) && !names_a_read_table {
                    return Err(ParseError::Unsupported {
                        feature: "DML comparison qualifier must match the table name",
                    });
                }
            }
        }
        for (qualifier, column_name) in &render_context.ordered_columns {
            if let Some(qualifier) = qualifier {
                if !qualifier.eq_ignore_ascii_case(source_table) {
                    return Err(ParseError::Unsupported {
                        feature: "DML ORDER BY qualifier must match the table name",
                    });
                }
            }
            ordered_columns.push(column_name.clone());
        }
    }
    let mut checked_comparisons = render_context.checked_comparisons;
    checked_comparisons.extend(inherited_comparisons);
    Ok(TranslatedDml {
        sqlite_sql,
        checked_update,
        checked_comparisons,
        source_table,
        read_tables,
        checked_subquery_comparisons: render_context.checked_subquery_comparisons,
        ordered_columns,
    })
}

/// Parses one checked MySQL `INSERT`, `UPDATE`, or `DELETE` into Turso's SQLite AST.
pub fn parse_dml_ast(sql: &str, mode: SessionSqlMode) -> Result<Stmt, ParseError> {
    let translated = parse_dml(sql, mode)?;
    translated.parse_ast()
}

/// Parses the first literal-only executable AUTO_INCREMENT INSERT slice.
///
/// This accepts one unqualified table, an explicit unique column list, and a
/// statically known nonempty `VALUES` batch whose expressions are direct
/// literals. The allocator column is checked separately by
/// [`CheckedAutoIncrementInsert::bind_allocator_table`] because only the
/// frontend has the durable table definition.
pub fn parse_auto_increment_insert(
    sql: &str,
    mode: SessionSqlMode,
) -> Result<CheckedAutoIncrementInsert, ParseError> {
    parse_checked_auto_increment_insert(sql, mode, is_direct_insert_literal)
}

/// Parses one AUTO_INCREMENT INSERT that can be executed through a prepared
/// statement.
///
/// This accepts the literal-only direct-execution subset plus bare `?` values.
/// The fixed VALUES shape lets the frontend reserve one ID per row before it
/// injects those IDs as literals, without changing user parameter positions.
pub fn parse_prepared_auto_increment_insert(
    sql: &str,
    mode: SessionSqlMode,
) -> Result<CheckedAutoIncrementInsert, ParseError> {
    parse_checked_auto_increment_insert(sql, mode, is_prepared_insert_value)
}

fn parse_checked_auto_increment_insert(
    sql: &str,
    mode: SessionSqlMode,
    accepts_value: fn(&Expr) -> bool,
) -> Result<CheckedAutoIncrementInsert, ParseError> {
    validate_auto_increment_insert_token_shape(sql, mode)?;
    let statement = parse_one_statement(sql, mode)?;
    let Statement::Insert(insert) = &statement else {
        return Err(ParseError::ExpectedDml);
    };

    let sqlparser::ast::TableObject::TableName(table) = &insert.table else {
        return unsupported("INSERT table source");
    };
    // An ordinary INSERT takes IGNORE, but not this one. The allocator reserves
    // its range before the rows are written, so a row IGNORE skips has already
    // taken a number, and what MySQL reports as the last insert id for a
    // statement whose rows were all skipped has not been measured.
    if insert.ignore {
        return unsupported("AUTO_INCREMENT INSERT IGNORE");
    }
    // Same reason: the range is reserved before the rows are written, so a row
    // the upsert turns into an update has already taken a number, and what
    // MySQL reports as the last insert id for it has not been measured.
    if insert.on.is_some() {
        return unsupported("AUTO_INCREMENT INSERT ON DUPLICATE KEY UPDATE");
    }
    let table_name = insert_name(table)?;
    if insert.columns.is_empty() {
        return unsupported("INSERT without an explicit column list");
    }
    let columns = insert
        .columns
        .iter()
        .map(insert_name)
        .collect::<Result<Vec<_>, _>>()?;
    if columns.iter().enumerate().any(|(index, column)| {
        columns[..index]
            .iter()
            .any(|previous| previous.as_str().eq_ignore_ascii_case(column.as_str()))
    }) {
        return unsupported("duplicate INSERT column");
    }

    let source = insert.source.as_deref().ok_or(ParseError::Unsupported {
        feature: "INSERT without VALUES",
    })?;
    let sqlparser::ast::SetExpr::Values(values) = source.body.as_ref() else {
        return unsupported("INSERT source");
    };
    if values.explicit_row || values.value_keyword || values.rows.is_empty() {
        return unsupported("INSERT VALUES option");
    }
    for row in &values.rows {
        if row.is_empty() || row.len() != columns.len() {
            return unsupported("INSERT VALUES column count");
        }
    }
    // A column given `DEFAULT` is left out of the rendered statement, so the
    // allocator has to see the column list the engine will run rather than the
    // one that was written.
    let names = columns.iter().map(TursoName::as_str).collect::<Vec<_>>();
    let defaulted = columns_given_their_default(&names, values)?;
    for row in &values.rows {
        if !row
            .iter()
            .enumerate()
            .filter(|(at, _)| !defaulted[*at])
            .all(|(_, value)| accepts_value(value))
        {
            return unsupported("INSERT VALUES expression");
        }
    }
    let columns = columns
        .into_iter()
        .enumerate()
        .filter(|(at, _)| !defaulted[*at])
        .map(|(_, column)| column)
        .collect::<Vec<_>>();
    // Every column defaulted renders as `DEFAULT VALUES`, which has no row for
    // the allocator to write its number into.
    if columns.is_empty() {
        return unsupported("INSERT without an explicit column list");
    }

    // Reuse the existing checked SQL normalizer only after the stricter shape
    // checks above. The executable path exposes the typed AST, not this SQL.
    let normalized = translate_insert(insert, sql, mode)?;
    let sqlite_statement = parse_normalized_dml(&normalized.sqlite_sql)?;
    let row_count = NonZeroUsize::new(values.rows.len()).ok_or(ParseError::Unsupported {
        feature: "INSERT without VALUES rows",
    })?;
    Ok(CheckedAutoIncrementInsert {
        table_name,
        columns,
        row_count,
        sqlite_statement,
    })
}

/// What one row of an `INSERT` writes into a column.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckedInsertValue {
    /// A written whole number.
    SignedInteger(i64),
    /// A written NULL.
    Null,
    /// A written `DEFAULT`, which asks for the column's own default.
    Default,
    /// Anything else, a value bound at execution time included.
    Other,
}

/// Returns what each row of one MySQL `INSERT` writes into `column`, or `None`
/// when the statement never names it.
///
/// A fixture writes its own ids — `INSERT INTO t (id, name) VALUES (1, 'a')` —
/// and a counted table has to see those before the row is written, so its own
/// counter never hands the same number out again.
pub fn parse_insert_values_written_into(
    sql: &str,
    mode: SessionSqlMode,
    column: &str,
) -> Result<Option<Vec<CheckedInsertValue>>, ParseError> {
    let names_the_column = |name: &ObjectName| {
        matches!(
            name.0.as_slice(),
            [ObjectNamePart::Identifier(ident)] if ident.value.eq_ignore_ascii_case(column)
        )
    };
    let statement = parse_one_statement(sql, mode)?;
    let Statement::Insert(insert) = &statement else {
        return Ok(None);
    };
    // MySQL's `INSERT ... SET id = 1` names its columns and values in one place
    // and writes the row the column-list form writes.
    if !insert.assignments.is_empty() {
        return Ok(insert
            .assignments
            .iter()
            .find(|assignment| match &assignment.target {
                sqlparser::ast::AssignmentTarget::ColumnName(name) => names_the_column(name),
                _ => false,
            })
            .map(|assignment| vec![written_insert_value(&assignment.value, column)]));
    }
    let Some(at) = insert.columns.iter().position(names_the_column) else {
        return Ok(None);
    };
    let Some(source) = insert.source.as_deref() else {
        return Ok(None);
    };
    let sqlparser::ast::SetExpr::Values(values) = source.body.as_ref() else {
        return Ok(None);
    };
    values
        .rows
        .iter()
        .map(|row| {
            row.get(at)
                .map(|value| written_insert_value(value, column))
                .ok_or(ParseError::Unsupported {
                    feature: "INSERT VALUES column count",
                })
        })
        .collect::<Result<Vec<_>, _>>()
        .map(Some)
}

fn written_insert_value(value: &Expr, column: &str) -> CheckedInsertValue {
    if matches!(value, Expr::Value(literal) if matches!(literal.value, Value::Null)) {
        return CheckedInsertValue::Null;
    }
    if names_the_columns_default(value, column) {
        return CheckedInsertValue::Default;
    }
    direct_signed_integer(value)
        .map(CheckedInsertValue::SignedInteger)
        .unwrap_or(CheckedInsertValue::Other)
}

/// Returns the unqualified target of one MySQL `INSERT`, without accepting it
/// for AUTO_INCREMENT range injection.
///
/// The frontend uses this after a narrower executable parse rejects an INSERT
/// so a marked table cannot fall through to generic execution.
pub fn parse_auto_increment_insert_target(
    sql: &str,
    mode: SessionSqlMode,
) -> Result<Option<String>, ParseError> {
    let statement = parse_one_statement(sql, mode)?;
    let Statement::Insert(insert) = statement else {
        return Ok(None);
    };
    let sqlparser::ast::TableObject::TableName(table) = insert.table else {
        return unsupported("INSERT table source");
    };
    Ok(Some(insert_name(&table)?.as_str().to_owned()))
}

fn validate_auto_increment_insert_token_shape(
    sql: &str,
    mode: SessionSqlMode,
) -> Result<(), ParseError> {
    let dialect = SessionMySqlDialect::without_executable_comments(mode);
    let tokens = Tokenizer::new(&dialect, sql)
        .tokenize()
        .map_err(|error| ParseError::Sqlparser(error.to_string()))?;
    if tokens.iter().any(|token| {
        matches!(
            token,
            Token::Whitespace(
                Whitespace::SingleLineComment { .. } | Whitespace::MultiLineComment(_)
            )
        )
    }) {
        return unsupported("comment in AUTO_INCREMENT INSERT");
    }
    Ok(())
}

fn insert_name(name: &ObjectName) -> Result<TursoName, ParseError> {
    let [ObjectNamePart::Identifier(ident)] = name.0.as_slice() else {
        return unsupported("qualified or dynamic INSERT name");
    };
    Ok(TursoName::exact(ident.value.clone()))
}

fn is_direct_insert_literal(expr: &Expr) -> bool {
    match expr {
        Expr::Value(value) => match &value.value {
            Value::Number(value, false) => {
                value.parse::<i64>().is_ok() || value.parse::<f64>().is_ok_and(f64::is_finite)
            }
            Value::SingleQuotedString(_) | Value::DoubleQuotedString(_) => true,
            Value::Boolean(_) | Value::Null => true,
            _ => false,
        },
        Expr::UnaryOp {
            op: UnaryOperator::Minus | UnaryOperator::Plus,
            expr,
        } => {
            let Expr::Value(value) = expr.as_ref() else {
                return false;
            };
            let Value::Number(value, false) = &value.value else {
                return false;
            };
            let Ok(magnitude) = value.parse::<u64>() else {
                return false;
            };
            magnitude <= (i64::MAX as u64) + 1
        }
        _ => false,
    }
}

fn is_prepared_insert_value(expr: &Expr) -> bool {
    is_direct_insert_literal(expr)
        || matches!(
            expr,
            Expr::Value(value) if matches!(&value.value, Value::Placeholder(marker) if marker == "?")
        )
}

/// Rebuilds strict signed-width metadata from normalized MySQL table DDL.
///
/// This deliberately reparses the durable MySQL statement instead of looking
/// at SQLite affinity names. `TINYINT` and `INT` share i64 storage but have
/// different MySQL assignment ranges.
pub fn parse_mysql_numeric_spec(
    sql: &str,
    mode: SessionSqlMode,
) -> Result<MySqlNumericSpec, ParseError> {
    reject_unsupported_mysql_string_escapes(sql, mode)?;
    let statement = parse_one_statement(sql, mode)?;
    let Statement::CreateTable(table) = statement else {
        return Err(ParseError::ExpectedCreateTable);
    };
    if let Err(error) = translate_create_table(&table) {
        if parse_auto_increment_create_table(sql, mode).is_err() {
            parse_checked_primary_key_create_table(sql, mode).map_err(|_| error)?;
        }
    }
    Ok(MySqlNumericSpec {
        columns: table
            .columns
            .iter()
            .map(|column| match column.data_type {
                DataType::TinyInt(None) => Some(MySqlIntegerType::TinyInt),
                DataType::SmallInt(None) => Some(MySqlIntegerType::SmallInt),
                DataType::MediumInt(None) => Some(MySqlIntegerType::MediumInt),
                DataType::Int(None) | DataType::Integer(None) => Some(MySqlIntegerType::Int),
                DataType::BigInt(None) => Some(MySqlIntegerType::BigInt),
                DataType::TinyIntUnsigned(None) => Some(MySqlIntegerType::TinyIntUnsigned),
                DataType::SmallIntUnsigned(None) => Some(MySqlIntegerType::SmallIntUnsigned),
                DataType::MediumIntUnsigned(None) => Some(MySqlIntegerType::MediumIntUnsigned),
                DataType::IntUnsigned(None) | DataType::IntegerUnsigned(None) => {
                    Some(MySqlIntegerType::IntUnsigned)
                }
                DataType::BigIntUnsigned(None) => Some(MySqlIntegerType::BigIntUnsigned),
                // MySQL's BOOLEAN is a TINYINT, so it takes the same range.
                DataType::Boolean | DataType::Bool => Some(MySqlIntegerType::TinyInt),
                _ => None,
            })
            .collect(),
        character_lengths: table
            .columns
            .iter()
            .map(|column| match column.data_type {
                DataType::Varchar(length) | DataType::Char(length) => {
                    declared_character_length(length).ok()
                }
                _ => None,
            })
            .collect(),
        fixed_widths: table
            .columns
            .iter()
            .map(|column| matches!(column.data_type, DataType::Char(_)))
            .collect(),
        binary_lengths: table
            .columns
            .iter()
            .map(|column| match column.data_type {
                DataType::Varbinary(length) => declared_binary_length(length).ok(),
                _ => None,
            })
            .collect(),
        datetimes: table
            .columns
            .iter()
            .map(|column| {
                matches!(
                    column.data_type,
                    DataType::Datetime(None)
                        | DataType::Timestamp(None, sqlparser::ast::TimezoneInfo::None)
                )
            })
            .collect(),
        dates: table
            .columns
            .iter()
            .map(|column| matches!(column.data_type, DataType::Date))
            .collect(),
        times: table
            .columns
            .iter()
            .map(|column| {
                matches!(
                    column.data_type,
                    DataType::Time(None, sqlparser::ast::TimezoneInfo::None)
                )
            })
            .collect(),
        years: table
            .columns
            .iter()
            .map(|column| {
                matches!(&column.data_type, DataType::Custom(name, arguments)
                    if arguments.is_empty() && names_the_year_type(name))
            })
            .collect(),
        enums: table
            .columns
            .iter()
            .map(|column| match &column.data_type {
                DataType::Enum(members, None) => Some(
                    members
                        .iter()
                        .filter_map(|member| match member {
                            sqlparser::ast::EnumMember::Name(name) => Some(name.clone()),
                            sqlparser::ast::EnumMember::NamedValue(..) => None,
                        })
                        .collect(),
                ),
                _ => None,
            })
            .collect(),
        sets: table
            .columns
            .iter()
            .map(|column| match &column.data_type {
                DataType::Set(members) => Some(members.clone()),
                _ => None,
            })
            .collect(),
        jsons: table
            .columns
            .iter()
            .map(|column| matches!(column.data_type, DataType::JSON))
            .collect(),
        unsigned_reals: table
            .columns
            .iter()
            .map(|column| {
                matches!(
                    column.data_type,
                    DataType::DoubleUnsigned(sqlparser::ast::ExactNumberInfo::None)
                        | DataType::FloatUnsigned(sqlparser::ast::ExactNumberInfo::None)
                        | DataType::DecimalUnsigned(_)
                        | DataType::DecUnsigned(_)
                )
            })
            .collect(),
    })
}

/// Parses exactly one checked MySQL `CREATE TABLE` statement into Turso's SQLite AST.
///
/// The MySQL AST is deliberately kept private. The checked normalizer is the boundary
/// between the two parser representations: it rejects unsupported MySQL syntax before
/// the normalized SQLite statement is parsed into the public Turso AST.
pub fn parse_create_table_ast(sql: &str, mode: SessionSqlMode) -> Result<Stmt, ParseError> {
    let translated = match parse_create_table(sql, mode) {
        Ok(translated) => translated,
        Err(error) => {
            if let Ok(checked) = parse_checked_primary_key_create_table(sql, mode) {
                return Ok(checked.sqlite_statement);
            }
            return Err(error);
        }
    };
    let mut parser = TursoParser::new(translated.as_sql().as_bytes());
    let command = parser
        .next_cmd()
        .map_err(|error| ParseError::TursoParser(error.to_string()))?;
    let Some(TursoCmd::Stmt(statement @ Stmt::CreateTable { .. })) = command else {
        return Err(ParseError::TursoParser(
            "normalized CREATE TABLE did not produce a CREATE TABLE AST".to_string(),
        ));
    };
    if parser
        .next_cmd()
        .map_err(|error| ParseError::TursoParser(error.to_string()))?
        .is_some()
    {
        return Err(ParseError::ExpectedOneStatement { actual: 2 });
    }
    Ok(statement)
}

/// Parses exactly one safe MySQL `ALTER TABLE` statement into Turso's SQLite AST.
///
/// A statement naming more than one operation has more than one AST, so this
/// refuses it; [`split_alter_table_operations`] is the one that answers those.
pub fn parse_alter_table_ast(sql: &str, mode: SessionSqlMode) -> Result<Stmt, ParseError> {
    reject_unsupported_mysql_string_escapes(sql, mode)?;
    let statement = parse_one_statement(sql, mode)?;
    let Statement::AlterTable(alter) = statement else {
        return Err(ParseError::ExpectedAlterTable);
    };
    let normalized = translate_alter_table(&alter)?;
    let [normalized] = normalized.as_slice() else {
        return unsupported("multiple ALTER TABLE operations");
    };
    parse_normalized_alter_table(normalized)
}

/// Splits one MySQL `ALTER TABLE` into one MySQL statement per operation.
///
/// MySQL takes several operations in one statement and the engine takes one.
/// The pieces come back as MySQL rather than as SQLite ASTs so each can go
/// through the ordinary schema path, which is what carries the durable DDL a
/// table is remembered by and the checks an `ALTER` has to pass. The caller
/// runs them inside one transaction, because MySQL applies the whole statement
/// or none of it.
pub fn split_alter_table_operations(
    sql: &str,
    mode: SessionSqlMode,
) -> Result<Vec<String>, ParseError> {
    reject_unsupported_mysql_string_escapes(sql, mode)?;
    let statement = parse_one_statement(sql, mode)?;
    let Statement::AlterTable(alter) = statement else {
        return Err(ParseError::ExpectedAlterTable);
    };
    // Rendering each operation proves it is one of the checked shapes before
    // any of them runs, so a statement this cannot take fails whole.
    translate_alter_table(&alter)?;
    let table_name = render_mysql_object_name(&alter.name)?;
    alter
        .operations
        .iter()
        .map(|operation| render_mysql_alter_table_operation(&table_name, operation, mode))
        .collect()
}

/// Renders one `ALTER TABLE` operation as a MySQL statement of its own.
fn render_mysql_alter_table_operation(
    table_name: &str,
    operation: &AlterTableOperation,
    mode: SessionSqlMode,
) -> Result<String, ParseError> {
    match operation {
        AlterTableOperation::AddColumn { column_def, .. } => Ok(format!(
            "ALTER TABLE {table_name} ADD COLUMN {}",
            render_mysql_checked_column(column_def, mode)?
        )),
        AlterTableOperation::DropColumn { column_names, .. } => {
            let [column_name] = column_names.as_slice() else {
                return unsupported("multiple DROP COLUMN names");
            };
            Ok(format!(
                "ALTER TABLE {table_name} DROP COLUMN {}",
                render_mysql_sqlparser_ident(column_name)
            ))
        }
        AlterTableOperation::RenameColumn {
            old_column_name,
            new_column_name,
        } => Ok(format!(
            "ALTER TABLE {table_name} RENAME COLUMN {} TO {}",
            render_mysql_sqlparser_ident(old_column_name),
            render_mysql_sqlparser_ident(new_column_name)
        )),
        AlterTableOperation::RenameTable {
            table_name: RenameTableNameKind::To(new_table_name),
        } => Ok(format!(
            "ALTER TABLE {table_name} RENAME TO {}",
            render_mysql_object_name(new_table_name)?
        )),
        AlterTableOperation::AddConstraint {
            constraint: constraint @ TableConstraint::ForeignKey(_),
            ..
        } => Ok(format!("ALTER TABLE {table_name} ADD {constraint}")),
        AlterTableOperation::DropForeignKey { name, .. }
        | AlterTableOperation::DropConstraint { name, .. } => Ok(format!(
            "ALTER TABLE {table_name} DROP FOREIGN KEY {}",
            render_mysql_sqlparser_ident(name)
        )),
        AlterTableOperation::ModifyColumn {
            col_name,
            data_type,
            options,
            ..
        } => Ok(format!(
            "ALTER TABLE {table_name} MODIFY COLUMN {}",
            render_mysql_checked_column(&restated_column(col_name, data_type, options), mode)?
        )),
        AlterTableOperation::ChangeColumn {
            old_name,
            new_name,
            data_type,
            options,
            ..
        } => Ok(format!(
            "ALTER TABLE {table_name} CHANGE COLUMN {} {}",
            render_mysql_sqlparser_ident(old_name),
            render_mysql_checked_column(&restated_column(new_name, data_type, options), mode)?
        )),
        _ => unsupported("ALTER TABLE operation"),
    }
}

/// Parses exactly one checked MySQL `CREATE INDEX` statement into Turso's SQLite AST.
pub fn parse_create_index_ast(sql: &str, mode: SessionSqlMode) -> Result<Stmt, ParseError> {
    let statement = parse_one_statement(sql, mode)?;
    let Statement::CreateIndex(index) = statement else {
        return Err(ParseError::ExpectedCreateIndex);
    };
    let normalized = translate_create_index(&index)?;
    parse_normalized_create_index(&normalized)
}

/// Parses exactly one checked MySQL `CREATE VIEW` statement into Turso's SQLite AST.
pub fn parse_create_view_ast(sql: &str, mode: SessionSqlMode) -> Result<Stmt, ParseError> {
    let statement = parse_one_statement(sql, mode)?;
    let Statement::CreateView(view) = statement else {
        return Err(ParseError::ExpectedCreateView);
    };
    let normalized = translate_create_view(&view)?;
    parse_normalized_create_view(&normalized)
}

/// Parses exactly one checked MySQL `CREATE TRIGGER` statement into Turso's SQLite AST.
pub fn parse_create_trigger_ast(sql: &str, mode: SessionSqlMode) -> Result<Stmt, ParseError> {
    let statement = parse_one_statement(sql, mode)?;
    let Statement::CreateTrigger(trigger) = statement else {
        return Err(ParseError::ExpectedCreateTrigger);
    };
    let normalized = translate_create_trigger(&trigger)?;
    parse_normalized_create_trigger(&normalized)
}

/// Parses exactly one supported MySQL schema DDL statement into Turso's SQLite AST.
pub fn parse_schema_ddl_ast(sql: &str, mode: SessionSqlMode) -> Result<Stmt, ParseError> {
    reject_unsupported_mysql_string_escapes(sql, mode)?;
    let statement = parse_one_statement(sql, mode)?;
    match statement {
        Statement::CreateTable(table) => match translate_create_table(&table) {
            Ok(translated) => parse_normalized_create_table(translated.as_sql()),
            Err(error) => parse_checked_primary_key_create_table(sql, mode)
                .map(|checked| checked.sqlite_statement)
                .map_err(|_| error),
        },
        Statement::CreateIndex(index) => {
            let normalized = translate_create_index(&index)?;
            parse_normalized_create_index(&normalized)
        }
        Statement::CreateView(view) => {
            let normalized = translate_create_view(&view)?;
            parse_normalized_create_view(&normalized)
        }
        Statement::CreateTrigger(trigger) => {
            let normalized = translate_create_trigger(&trigger)?;
            parse_normalized_create_trigger(&normalized)
        }
        Statement::AlterTable(alter) => {
            let normalized = translate_alter_table(&alter)?;
            let [normalized] = normalized.as_slice() else {
                return unsupported("multiple ALTER TABLE operations");
            };
            parse_normalized_alter_table(normalized)
        }
        _ => Err(ParseError::Unsupported {
            feature: "schema statement",
        }),
    }
}

fn parse_one_statement(sql: &str, mode: SessionSqlMode) -> Result<Statement, ParseError> {
    let dialect = SessionMySqlDialect::new(mode);
    let statements = Parser::parse_sql(&dialect, sql)
        .map_err(|error| ParseError::Sqlparser(error.to_string()))?;
    let [statement] = statements.as_slice() else {
        return Err(ParseError::ExpectedOneStatement {
            actual: statements.len(),
        });
    };
    Ok(statement.clone())
}

fn parse_normalized_create_table(sql: &str) -> Result<Stmt, ParseError> {
    let mut parser = TursoParser::new(sql.as_bytes());
    let command = parser
        .next_cmd()
        .map_err(|error| ParseError::TursoParser(error.to_string()))?;
    let Some(TursoCmd::Stmt(statement @ Stmt::CreateTable { .. })) = command else {
        return Err(ParseError::TursoParser(
            "normalized CREATE TABLE did not produce a CREATE TABLE AST".to_string(),
        ));
    };
    if parser
        .next_cmd()
        .map_err(|error| ParseError::TursoParser(error.to_string()))?
        .is_some()
    {
        return Err(ParseError::ExpectedOneStatement { actual: 2 });
    }
    Ok(statement)
}

fn parse_normalized_select(sql: &str) -> Result<Stmt, ParseError> {
    let mut parser = TursoParser::new(sql.as_bytes());
    let command = parser
        .next_cmd()
        .map_err(|error| ParseError::TursoParser(error.to_string()))?;
    let Some(TursoCmd::Stmt(statement @ Stmt::Select(_))) = command else {
        return Err(ParseError::TursoParser(
            "normalized SELECT did not produce a SELECT AST".to_string(),
        ));
    };
    if parser
        .next_cmd()
        .map_err(|error| ParseError::TursoParser(error.to_string()))?
        .is_some()
    {
        return Err(ParseError::ExpectedOneStatement { actual: 2 });
    }
    Ok(statement)
}

fn parse_normalized_dml(sql: &str) -> Result<Stmt, ParseError> {
    let mut parser = TursoParser::new(sql.as_bytes());
    let command = parser
        .next_cmd()
        .map_err(|error| ParseError::TursoParser(error.to_string()))?;
    let Some(TursoCmd::Stmt(
        statement @ (Stmt::Insert { .. } | Stmt::Update(_) | Stmt::Delete { .. }),
    )) = command
    else {
        return Err(ParseError::TursoParser(
            "normalized DML did not produce an INSERT, UPDATE, or DELETE AST".to_string(),
        ));
    };
    if parser
        .next_cmd()
        .map_err(|error| ParseError::TursoParser(error.to_string()))?
        .is_some()
    {
        return Err(ParseError::ExpectedOneStatement { actual: 2 });
    }
    Ok(statement)
}

fn parse_normalized_alter_table(sql: &str) -> Result<Stmt, ParseError> {
    let mut parser = TursoParser::new(sql.as_bytes());
    let command = parser
        .next_cmd()
        .map_err(|error| ParseError::TursoParser(error.to_string()))?;
    let Some(TursoCmd::Stmt(statement @ Stmt::AlterTable(_))) = command else {
        return Err(ParseError::TursoParser(
            "normalized ALTER TABLE did not produce an ALTER TABLE AST".to_string(),
        ));
    };
    if parser
        .next_cmd()
        .map_err(|error| ParseError::TursoParser(error.to_string()))?
        .is_some()
    {
        return Err(ParseError::ExpectedOneStatement { actual: 2 });
    }
    Ok(statement)
}

fn parse_normalized_create_index(sql: &str) -> Result<Stmt, ParseError> {
    let mut parser = TursoParser::new(sql.as_bytes());
    let command = parser
        .next_cmd()
        .map_err(|error| ParseError::TursoParser(error.to_string()))?;
    let Some(TursoCmd::Stmt(statement @ Stmt::CreateIndex { .. })) = command else {
        return Err(ParseError::TursoParser(
            "normalized CREATE INDEX did not produce a CREATE INDEX AST".to_string(),
        ));
    };
    if parser
        .next_cmd()
        .map_err(|error| ParseError::TursoParser(error.to_string()))?
        .is_some()
    {
        return Err(ParseError::ExpectedOneStatement { actual: 2 });
    }
    Ok(statement)
}

fn parse_normalized_create_view(sql: &str) -> Result<Stmt, ParseError> {
    let mut parser = TursoParser::new(sql.as_bytes());
    let command = parser
        .next_cmd()
        .map_err(|error| ParseError::TursoParser(error.to_string()))?;
    let Some(TursoCmd::Stmt(statement @ Stmt::CreateView { .. })) = command else {
        return Err(ParseError::TursoParser(
            "normalized CREATE VIEW did not produce a CREATE VIEW AST".to_string(),
        ));
    };
    if parser
        .next_cmd()
        .map_err(|error| ParseError::TursoParser(error.to_string()))?
        .is_some()
    {
        return Err(ParseError::ExpectedOneStatement { actual: 2 });
    }
    Ok(statement)
}

fn parse_normalized_create_trigger(sql: &str) -> Result<Stmt, ParseError> {
    let mut parser = TursoParser::new(sql.as_bytes());
    let command = parser
        .next_cmd()
        .map_err(|error| ParseError::TursoParser(error.to_string()))?;
    let Some(TursoCmd::Stmt(statement @ Stmt::CreateTrigger { .. })) = command else {
        return Err(ParseError::TursoParser(
            "normalized CREATE TRIGGER did not produce a CREATE TRIGGER AST".to_string(),
        ));
    };
    if parser
        .next_cmd()
        .map_err(|error| ParseError::TursoParser(error.to_string()))?
        .is_some()
    {
        return Err(ParseError::ExpectedOneStatement { actual: 2 });
    }
    Ok(statement)
}

fn translate_create_table(table: &CreateTable) -> Result<TranslatedCreateTable, ParseError> {
    reject_attributes_and_check_options(table)?;
    let name = render_name(&table.name)?;
    let columns = table
        .columns
        .iter()
        .map(render_column)
        .collect::<Result<Vec<_>, _>>()?;
    let constraints = table
        .constraints
        .iter()
        .map(render_table_constraint)
        .collect::<Result<Vec<_>, _>>()?;
    if columns.is_empty() && constraints.is_empty() {
        return unsupported("CREATE TABLE without columns or constraints");
    }

    let mut definitions = columns;
    definitions.extend(constraints);
    let temporary = if table.temporary { "TEMPORARY " } else { "" };
    let if_not_exists = if table.if_not_exists {
        "IF NOT EXISTS "
    } else {
        ""
    };
    Ok(TranslatedCreateTable {
        sqlite_sql: format!(
            "CREATE {temporary}TABLE {if_not_exists}{name} ({})",
            definitions.join(", ")
        ),
    })
}

fn translate_auto_increment_create_table(
    table: &CreateTable,
    mode: SessionSqlMode,
) -> Result<CheckedAutoIncrementCreateTable, ParseError> {
    if table.temporary {
        return unsupported("TEMPORARY AUTO_INCREMENT table");
    }
    if table.name.0.len() != 1 {
        return unsupported("qualified AUTO_INCREMENT table name");
    }
    reject_attributes_and_check_options(table)?;
    let table = &table_with_its_key_written_inline(table.clone());
    // A foreign key is the one table-level constraint a counted table takes,
    // the same as an ordinary one: both renderings below write whatever
    // constraints the table carries, and a table with a counted id is exactly
    // the table a child row points at.
    if table
        .constraints
        .iter()
        .any(|constraint| !matches!(constraint, TableConstraint::ForeignKey(_)))
    {
        return unsupported("table-level constraint in AUTO_INCREMENT table");
    }
    for (index, column) in table.columns.iter().enumerate() {
        if table.columns[..index]
            .iter()
            .any(|previous| previous.name.value.eq_ignore_ascii_case(&column.name.value))
        {
            return unsupported("duplicate column name in AUTO_INCREMENT table");
        }
    }

    let mut allocator_column_ordinal = None;
    for (ordinal, column) in table.columns.iter().enumerate() {
        if column_has_auto_increment(column) && allocator_column_ordinal.replace(ordinal).is_some()
        {
            return unsupported("multiple AUTO_INCREMENT columns");
        }
    }
    let Some(allocator_column_ordinal) = allocator_column_ordinal else {
        return unsupported("AUTO_INCREMENT column");
    };
    validate_auto_increment_column(&table.columns[allocator_column_ordinal])?;

    let sqlite_columns = table
        .columns
        .iter()
        .enumerate()
        .map(|(ordinal, column)| {
            if ordinal == allocator_column_ordinal {
                Ok(format!(
                    "{} INTEGER PRIMARY KEY",
                    render_ident(&column.name)
                ))
            } else {
                render_column(column)
            }
        })
        .collect::<Result<Vec<_>, _>>()?;
    if sqlite_columns.is_empty() {
        return unsupported("CREATE TABLE without columns");
    }
    let mut sqlite_definitions = sqlite_columns;
    sqlite_definitions.extend(
        table
            .constraints
            .iter()
            .map(render_table_constraint)
            .collect::<Result<Vec<_>, _>>()?,
    );
    let temporary = if table.temporary { "TEMPORARY " } else { "" };
    let if_not_exists = if table.if_not_exists {
        "IF NOT EXISTS "
    } else {
        ""
    };
    let sqlite_sql = format!(
        "CREATE {temporary}TABLE {if_not_exists}{} ({})",
        render_name(&table.name)?,
        sqlite_definitions.join(", ")
    );
    let sqlite_statement = parse_normalized_create_table(&sqlite_sql)?;

    Ok(CheckedAutoIncrementCreateTable {
        table_name: match table.name.0.as_slice() {
            [ObjectNamePart::Identifier(name)] => name.value.clone(),
            _ => unreachable!("AUTO_INCREMENT table name was already checked as unqualified"),
        },
        allocator_column_ordinal,
        allocator_column_name: table.columns[allocator_column_ordinal].name.value.clone(),
        allocator_column_type: match table.columns[allocator_column_ordinal].data_type {
            DataType::IntUnsigned(_) | DataType::IntegerUnsigned(_) => {
                MySqlIntegerType::IntUnsigned
            }
            DataType::BigInt(_) => MySqlIntegerType::BigInt,
            _ => MySqlIntegerType::Int,
        },
        normalized_mysql_ddl: render_auto_increment_mysql_ddl(
            table,
            allocator_column_ordinal,
            mode,
        )?,
        sqlite_statement,
    })
}

fn column_has_auto_increment(column: &ColumnDef) -> bool {
    column.options.iter().any(|option| {
        matches!(
            &option.option,
            ColumnOption::DialectSpecific(tokens) if is_auto_increment_tokens(tokens)
        )
    })
}

fn is_auto_increment_tokens(tokens: &[sqlparser::tokenizer::Token]) -> bool {
    matches!(tokens, [token] if token.to_string().eq_ignore_ascii_case("AUTO_INCREMENT"))
}

fn validate_auto_increment_column(column: &ColumnDef) -> Result<(), ParseError> {
    // `INT UNSIGNED AUTO_INCREMENT` is how a MySQL schema usually spells a
    // surrogate key, so it is taken alongside the signed spelling. Its top
    // value, 4294967295, is inside an i64, which is what the allocator counts
    // in. `BIGINT` is taken for the same reason and is the key an ORM writes by
    // default; `BIGINT UNSIGNED` is not, its top value being past what the
    // engine can hold.
    // A display width is taken and dropped here as it is on any other integer
    // column — `id INT(11) NOT NULL AUTO_INCREMENT PRIMARY KEY` is how a dump
    // spells this very column.
    if !matches!(
        column.data_type,
        DataType::Int(_)
            | DataType::Integer(_)
            | DataType::IntUnsigned(_)
            | DataType::IntegerUnsigned(_)
            | DataType::BigInt(_)
    ) {
        return unsupported("AUTO_INCREMENT column type");
    }
    let [not_null, auto_increment, primary_key] = column.options.as_slice() else {
        return unsupported("AUTO_INCREMENT column attributes");
    };
    if not_null.name.is_some()
        || !matches!(not_null.option, ColumnOption::NotNull)
        || auto_increment.name.is_some()
        || !matches!(
            &auto_increment.option,
            ColumnOption::DialectSpecific(tokens) if is_auto_increment_tokens(tokens)
        )
        || primary_key.name.is_some()
        || !is_plain_inline_primary_key(&primary_key.option)
    {
        return unsupported("AUTO_INCREMENT column attributes");
    }
    Ok(())
}

fn is_plain_inline_primary_key(option: &ColumnOption) -> bool {
    let ColumnOption::PrimaryKey(primary_key) = option else {
        return false;
    };
    primary_key.name.is_none()
        && primary_key.index_name.is_none()
        && primary_key.index_type.is_none()
        && primary_key.columns.is_empty()
        && primary_key.index_options.is_empty()
        && primary_key.characteristics.is_none()
}

fn render_auto_increment_mysql_ddl(
    table: &CreateTable,
    allocator_column_ordinal: usize,
    mode: SessionSqlMode,
) -> Result<String, ParseError> {
    let columns = table
        .columns
        .iter()
        .enumerate()
        .map(|(ordinal, column)| {
            if ordinal == allocator_column_ordinal {
                render_auto_increment_mysql_column(column)
            } else {
                render_mysql_checked_column(column, mode)
            }
        })
        .collect::<Result<Vec<_>, _>>()?;
    let mut definitions = columns;
    definitions.extend(
        table
            .constraints
            .iter()
            .map(|constraint| {
                render_table_constraint(constraint)?;
                Ok(constraint.to_string())
            })
            .collect::<Result<Vec<_>, ParseError>>()?,
    );
    let temporary = if table.temporary { "TEMPORARY " } else { "" };
    let if_not_exists = if table.if_not_exists {
        "IF NOT EXISTS "
    } else {
        ""
    };
    Ok(format!(
        "CREATE {temporary}TABLE {if_not_exists}{} ({})",
        render_mysql_object_name(&table.name)?,
        definitions.join(", ")
    ))
}

fn render_auto_increment_mysql_column(column: &ColumnDef) -> Result<String, ParseError> {
    let data_type = match column.data_type {
        DataType::Int(_) => "INT",
        DataType::Integer(_) => "INTEGER",
        DataType::IntUnsigned(_) => "INT UNSIGNED",
        DataType::IntegerUnsigned(_) => "INTEGER UNSIGNED",
        DataType::BigInt(_) => "BIGINT",
        _ => return unsupported("AUTO_INCREMENT column type"),
    };
    Ok(format!(
        "{} {data_type} NOT NULL AUTO_INCREMENT PRIMARY KEY",
        render_mysql_sqlparser_ident(&column.name)
    ))
}

fn render_mysql_checked_column(
    column: &ColumnDef,
    mode: SessionSqlMode,
) -> Result<String, ParseError> {
    let sqlite_column = render_column(column)?;
    let statement =
        parse_normalized_create_table(&format!("CREATE TABLE \"t\" ({sqlite_column})"))?;
    let Stmt::CreateTable {
        body:
            TursoCreateTableBody::ColumnsAndConstraints {
                columns,
                constraints,
                options,
            },
        ..
    } = statement
    else {
        unreachable!("normalized one-column CREATE TABLE must parse as a table");
    };
    let [column] = columns.as_slice() else {
        unreachable!("normalized one-column CREATE TABLE must keep one column");
    };
    if !constraints.is_empty() || options != turso_parser::ast::TableOptions::empty() {
        unreachable!("normalized one-column CREATE TABLE must not have table attributes");
    }
    render_mysql_column(column, mode)
}

fn render_mysql_object_name(name: &ObjectName) -> Result<String, ParseError> {
    if !(1..=2).contains(&name.0.len()) {
        return unsupported("object name with more than two parts");
    }
    let mut parts = Vec::with_capacity(name.0.len());
    for part in &name.0 {
        let ObjectNamePart::Identifier(ident) = part else {
            return unsupported("dynamic object name");
        };
        parts.push(render_mysql_sqlparser_ident(ident));
    }
    Ok(parts.join("."))
}

fn render_mysql_sqlparser_ident(ident: &Ident) -> String {
    format!("`{}`", ident.value.replace('`', "``"))
}

fn translate_create_index(index: &CreateIndex) -> Result<String, ParseError> {
    let Some(index_name) = index.name.as_ref() else {
        return unsupported("CREATE INDEX without a name");
    };
    if index.using.is_some()
        || index.concurrently
        || index.if_not_exists
        || !index.include.is_empty()
        || index.nulls_distinct.is_some()
        || !index.with.is_empty()
        || index.predicate.is_some()
        || !index.index_options.is_empty()
        || !index.alter_options.is_empty()
    {
        return unsupported("CREATE INDEX option");
    }
    let index_name = render_unqualified_name(index_name)?;
    let table_name = render_unqualified_name(&index.table_name)?;
    let columns = index
        .columns
        .iter()
        .map(render_index_column)
        .collect::<Result<Vec<_>, _>>()?;
    if columns.is_empty() {
        return unsupported("CREATE INDEX without columns");
    }
    let unique = if index.unique { "UNIQUE " } else { "" };
    Ok(format!(
        "CREATE {unique}INDEX {index_name} ON {table_name} ({})",
        columns.join(", ")
    ))
}

fn translate_create_view(view: &CreateView) -> Result<String, ParseError> {
    if view.or_alter
        || view.or_replace
        || view.materialized
        || view.secure
        || view.name_before_not_exists
        || !view.columns.is_empty()
        || !matches!(view.options, CreateTableOptions::None)
        || !view.cluster_by.is_empty()
        || view.comment.is_some()
        || view.with_no_schema_binding
        || view.if_not_exists
        || view.temporary
        || view.copy_grants
        || view.to.is_some()
        || view.params.is_some()
    {
        return unsupported("CREATE VIEW option");
    }
    let view_name = render_unqualified_name(&view.name)?;
    let query = render_simple_view_query(&view.query)?;
    Ok(format!("CREATE VIEW {view_name} AS {query}"))
}

fn translate_create_trigger(trigger: &CreateTrigger) -> Result<String, ParseError> {
    if trigger.or_alter
        || trigger.temporary
        || trigger.or_replace
        || trigger.is_constraint
        || !trigger.period_before_table
        || trigger.referenced_table_name.is_some()
        || !trigger.referencing.is_empty()
        || trigger.condition.is_some()
        || trigger.exec_body.is_some()
        || trigger.statements_as
        || trigger.characteristics.is_some()
    {
        return unsupported("CREATE TRIGGER option");
    }
    if trigger.period != Some(TriggerPeriod::After)
        || !matches!(trigger.events.as_slice(), [SqlTriggerEvent::Insert])
        || !matches!(
            trigger.trigger_object,
            Some(TriggerObjectKind::ForEach(TriggerObject::Row))
        )
    {
        return unsupported("CREATE TRIGGER timing or event");
    }

    let trigger_name = render_unqualified_name(&trigger.name)?;
    let table_name = render_unqualified_name(&trigger.table_name)?;
    let statements = trigger.statements.as_ref().ok_or(ParseError::Unsupported {
        feature: "CREATE TRIGGER body",
    })?;
    let sqlparser::ast::ConditionalStatements::BeginEnd(body) = statements else {
        return unsupported("CREATE TRIGGER body");
    };
    let [Statement::Insert(insert)] = body.statements.as_slice() else {
        return unsupported("CREATE TRIGGER body");
    };
    let TableObject::TableName(target_table) = &insert.table else {
        return unsupported("CREATE TRIGGER INSERT target");
    };
    if !insert.optimizer_hints.is_empty()
        || insert.or.is_some()
        || insert.ignore
        || !insert.into
        || insert.table_alias.is_some()
        || insert.overwrite
        || !insert.assignments.is_empty()
        || insert.partitioned.is_some()
        || !insert.after_columns.is_empty()
        || insert.has_table_keyword
        || insert.on.is_some()
        || insert.returning.is_some()
        || insert.output.is_some()
        || insert.priority.is_some()
        || insert.insert_alias.is_some()
        || insert.settings.is_some()
        || insert.format_clause.is_some()
        || insert.multi_table_insert_type.is_some()
        || !insert.multi_table_into_clauses.is_empty()
        || !insert.multi_table_when_clauses.is_empty()
        || insert.multi_table_else_clause.is_some()
    {
        return unsupported("CREATE TRIGGER INSERT option");
    }
    let target_table = render_unqualified_name(target_table)?;
    if insert.columns.is_empty() {
        return unsupported("CREATE TRIGGER INSERT without columns");
    }
    let columns = insert
        .columns
        .iter()
        .map(render_unqualified_name)
        .collect::<Result<Vec<_>, _>>()?;
    let Some(source) = &insert.source else {
        return unsupported("CREATE TRIGGER INSERT source");
    };
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
        return unsupported("CREATE TRIGGER INSERT source");
    }
    let SetExpr::Values(values) = source.body.as_ref() else {
        return unsupported("CREATE TRIGGER INSERT SELECT");
    };
    if values.explicit_row || values.value_keyword {
        return unsupported("CREATE TRIGGER INSERT VALUES option");
    }
    let [values] = values.rows.as_slice() else {
        return unsupported("CREATE TRIGGER INSERT VALUES rows");
    };
    if values.len() != columns.len() {
        return unsupported("CREATE TRIGGER INSERT value count");
    }
    let values = values
        .iter()
        .map(render_trigger_value)
        .collect::<Result<Vec<_>, _>>()?;
    Ok(format!(
        "CREATE TRIGGER {trigger_name} AFTER INSERT ON {table_name} FOR EACH ROW BEGIN INSERT INTO {target_table} ({}) VALUES ({}); END",
        columns.join(", "),
        values.join(", ")
    ))
}

fn render_trigger_value(value: &Expr) -> Result<String, ParseError> {
    match value {
        Expr::CompoundIdentifier(parts) if matches!(parts.as_slice(), [prefix, _] if prefix.value.eq_ignore_ascii_case("NEW")) => {
            Ok(format!("NEW.{}", render_ident(&parts[1])))
        }
        Expr::Value(value) => match &value.value {
            Value::Number(value, _) => Ok(value.clone()),
            Value::SingleQuotedString(value) | Value::DoubleQuotedString(value) => {
                Ok(format!("'{}'", value.replace('\'', "''")))
            }
            Value::Boolean(value) => Ok(if *value { "TRUE" } else { "FALSE" }.to_string()),
            Value::Null => Ok("NULL".to_string()),
            _ => unsupported("CREATE TRIGGER literal"),
        },
        _ => unsupported("CREATE TRIGGER value expression"),
    }
}

/// Renders one MySQL `ALTER TABLE` as the SQLite statements it means.
///
/// MySQL takes several operations in one statement and the engine takes one, so
/// a statement naming three becomes three. MySQL applies the whole statement or
/// none of it — measured on 8.4.11, `ADD COLUMN c, ADD COLUMN a` against a
/// table that already has `a` adds neither — so the caller runs them inside one
/// transaction.
fn translate_alter_table(alter: &AlterTable) -> Result<Vec<String>, ParseError> {
    if alter.if_exists
        || alter.only
        || alter.location.is_some()
        || alter.on_cluster.is_some()
        || alter.table_type.is_some()
    {
        return unsupported("ALTER TABLE option");
    }
    if alter.operations.is_empty() {
        return unsupported("ALTER TABLE without operations");
    }
    let table_name = render_name(&alter.name)?;
    alter
        .operations
        .iter()
        .map(|operation| translate_alter_table_operation(&table_name, operation))
        .collect()
}

fn translate_alter_table_operation(
    table_name: &str,
    operation: &AlterTableOperation,
) -> Result<String, ParseError> {
    match operation {
        AlterTableOperation::AddColumn {
            if_not_exists,
            column_def,
            column_position,
            ..
        } => {
            if *if_not_exists || column_position.is_some() {
                return unsupported("ADD COLUMN option");
            }
            Ok(format!(
                "ALTER TABLE {table_name} ADD COLUMN {}",
                render_column(column_def)?
            ))
        }
        AlterTableOperation::DropColumn {
            column_names,
            if_exists,
            drop_behavior,
            ..
        } => {
            let [column_name] = column_names.as_slice() else {
                return unsupported("multiple DROP COLUMN names");
            };
            if *if_exists || drop_behavior.is_some() {
                return unsupported("DROP COLUMN option");
            }
            Ok(format!(
                "ALTER TABLE {table_name} DROP COLUMN {}",
                render_ident(column_name)
            ))
        }
        AlterTableOperation::RenameColumn {
            old_column_name,
            new_column_name,
        } => Ok(format!(
            "ALTER TABLE {table_name} RENAME COLUMN {} TO {}",
            render_ident(old_column_name),
            render_ident(new_column_name)
        )),
        AlterTableOperation::RenameTable {
            table_name: RenameTableNameKind::To(new_table_name),
        } => Ok(format!(
            "ALTER TABLE {table_name} RENAME TO {}",
            render_unqualified_name(new_table_name)?
        )),
        AlterTableOperation::RenameTable { .. } => unsupported("RENAME TABLE AS"),
        // MySQL's MODIFY and CHANGE both restate one column whole, and the
        // engine's ALTER COLUMN takes the column it is to become — including
        // its name, which is what carries CHANGE's rename.
        // MySQL names the key it is adding with a `CONSTRAINT` clause, and
        // names it in the `DROP` — so the name has to survive, which the
        // engine's own `ADD CONSTRAINT` keeps for it.
        AlterTableOperation::AddConstraint {
            constraint: TableConstraint::ForeignKey(foreign_key),
            not_valid: false,
        } => Ok(format!(
            "ALTER TABLE {table_name} ADD {}",
            render_table_constraint(&TableConstraint::ForeignKey(foreign_key.clone()))?
        )),
        AlterTableOperation::DropForeignKey {
            name,
            drop_behavior: None,
        }
        | AlterTableOperation::DropConstraint {
            if_exists: false,
            name,
            drop_behavior: None,
        } => Ok(format!(
            "ALTER TABLE {table_name} DROP CONSTRAINT {}",
            render_ident(name)
        )),
        AlterTableOperation::ModifyColumn {
            col_name,
            data_type,
            options,
            column_position,
        } => {
            if column_position.is_some() {
                return unsupported("MODIFY COLUMN position");
            }
            Ok(format!(
                "ALTER TABLE {table_name} ALTER COLUMN {} TO {}",
                render_ident(col_name),
                render_column(&restated_column(col_name, data_type, options))?
            ))
        }
        AlterTableOperation::ChangeColumn {
            old_name,
            new_name,
            data_type,
            options,
            column_position,
        } => {
            if column_position.is_some() {
                return unsupported("CHANGE COLUMN position");
            }
            Ok(format!(
                "ALTER TABLE {table_name} ALTER COLUMN {} TO {}",
                render_ident(old_name),
                render_column(&restated_column(new_name, data_type, options))?
            ))
        }
        _ => unsupported("ALTER TABLE operation"),
    }
}

/// The column a `MODIFY` or a `CHANGE` restates.
///
/// MySQL reads the definition as the whole of what the column becomes:
/// measured on 8.4.11, `MODIFY COLUMN n BIGINT` over an `INT NOT NULL
/// DEFAULT 5` leaves a `bigint DEFAULT NULL`, so an attribute the statement
/// does not restate is gone.
fn restated_column(name: &Ident, data_type: &DataType, options: &[ColumnOption]) -> ColumnDef {
    ColumnDef {
        name: name.clone(),
        data_type: data_type.clone(),
        options: options
            .iter()
            .map(|option| ColumnOptionDef {
                name: None,
                option: option.clone(),
            })
            .collect(),
    }
}

fn render_index_column(column: &IndexColumn) -> Result<String, ParseError> {
    if column.operator_class.is_some()
        || column.column.options.asc.is_some()
        || column.column.options.nulls_first.is_some()
        || column.column.with_fill.is_some()
    {
        return unsupported("CREATE INDEX column option");
    }
    let Expr::Identifier(identifier) = &column.column.expr else {
        return unsupported("CREATE INDEX expression");
    };
    Ok(render_ident(identifier))
}

/// Writes a table's own `PRIMARY KEY (col)` clause onto the column it names.
///
/// MySQL's `SHOW CREATE TABLE` writes a key as a clause of its own, so every
/// dumped schema and every migration built from one spells it that way, while
/// this reads a key only where the column declares it. Moving the words onto
/// the column lets the one reader answer both spellings.
///
/// The table is left as it was wherever the move would say something the
/// statement did not: a key over several columns, one naming a column the
/// table does not have, one carrying a name or an index option, and a table
/// that already declares a key on a column. Each of those is refused below,
/// the way it always was.
pub(crate) fn table_with_its_key_written_inline(mut table: CreateTable) -> CreateTable {
    let Some((position, key)) = the_only_key_clause(&table) else {
        return table;
    };
    let Some(named) = the_one_column_a_key_names(&key) else {
        return table;
    };
    if table.columns.iter().any(|column| {
        column
            .options
            .iter()
            .any(|option| matches!(option.option, ColumnOption::PrimaryKey(_)))
    }) {
        return table;
    }
    let Some(column) = table
        .columns
        .iter_mut()
        .find(|column| column.name.value.eq_ignore_ascii_case(&named))
    else {
        return table;
    };
    column.options.push(ColumnOptionDef {
        name: None,
        option: ColumnOption::PrimaryKey(PrimaryKeyConstraint {
            name: None,
            columns: Vec::new(),
            ..key
        }),
    });
    table.constraints.remove(position);
    table
}

/// The table's one `PRIMARY KEY` clause and where it stands, or nothing where
/// the table writes none or writes more than one.
fn the_only_key_clause(table: &CreateTable) -> Option<(usize, PrimaryKeyConstraint)> {
    let mut found = None;
    for (position, constraint) in table.constraints.iter().enumerate() {
        let TableConstraint::PrimaryKey(key) = constraint else {
            continue;
        };
        if found.replace((position, key.clone())).is_some() {
            return None;
        }
    }
    found
}

/// The column a plain `PRIMARY KEY (col)` names, or nothing where the clause
/// names anything else.
fn the_one_column_a_key_names(key: &PrimaryKeyConstraint) -> Option<String> {
    // Measured on MySQL 8.4.11: a `CONSTRAINT` name on a key is dropped, the
    // key always being named PRIMARY, so the name says nothing to carry. A
    // `USING BTREE` is printed back and an index name is not, so both stay
    // where they are.
    if key.index_name.is_some()
        || key.index_type.is_some()
        || !key.index_options.is_empty()
        || key.characteristics.is_some()
    {
        return None;
    }
    let [column] = key.columns.as_slice() else {
        return None;
    };
    // Measured: an `ASC` is dropped where a `DESC` is printed back.
    if column.operator_class.is_some()
        || column.column.options.asc == Some(false)
        || column.column.options.nulls_first.is_some()
    {
        return None;
    }
    let Expr::Identifier(named) = &column.column.expr else {
        return None;
    };
    Some(named.value.clone())
}

/// Refuses every table attribute this does not answer, and every table option
/// but the ones naming what a table is written back as anyway.
pub(crate) fn reject_attributes_and_check_options(table: &CreateTable) -> Result<(), ParseError> {
    // `reject_table_attributes` refuses every option outright, so the rest of
    // the shape is checked with the options taken off and they are checked
    // below instead of being dropped.
    let mut without_options = table.clone();
    without_options.table_options = CreateTableOptions::None;
    reject_table_attributes(&without_options)?;
    check_table_options(&table.table_options)
}

/// Takes the table options that name what this writes back anyway, and refuses
/// the rest.
///
/// MySQL prints `ENGINE=InnoDB DEFAULT CHARSET=utf8mb4
/// COLLATE=utf8mb4_0900_ai_ci` after every table, so that trailer ends every
/// dumped schema and every migration built from one, while this prints those
/// same bytes whatever a table holds. An option naming exactly them says
/// nothing the table does not already say — measured on 8.4.11, a table
/// written with any of `ENGINE=InnoDB`, `DEFAULT CHARSET=utf8mb4`,
/// `CHARACTER SET utf8mb4` and `COLLATE=utf8mb4_0900_ai_ci` prints back byte
/// for byte the same as one written with none — so it is taken and left out.
///
/// Anything else is a claim about storage, ordering or case this cannot keep:
/// measured, `COLLATE=utf8mb4_bin`, `DEFAULT CHARSET=latin1` and
/// `ROW_FORMAT=DYNAMIC` are each printed back, so each is refused rather than
/// quietly dropped.
pub(crate) fn check_table_options(options: &CreateTableOptions) -> Result<(), ParseError> {
    let options = match options {
        CreateTableOptions::None => return Ok(()),
        CreateTableOptions::Plain(options) => options,
        CreateTableOptions::With(_)
        | CreateTableOptions::Options(_)
        | CreateTableOptions::TableProperties(_) => return unsupported("CREATE TABLE option"),
    };
    let mut written = Vec::with_capacity(options.len());
    for option in options {
        let named = match option {
            SqlOption::NamedParenthesizedList(engine)
                if engine.key.value.eq_ignore_ascii_case("ENGINE") =>
            {
                if !engine
                    .name
                    .as_ref()
                    .is_some_and(|name| name.value.eq_ignore_ascii_case("InnoDB"))
                    || !engine.values.is_empty()
                {
                    return unsupported("CREATE TABLE engine");
                }
                "ENGINE"
            }
            SqlOption::KeyValue { key, value } if names_a_character_set(key) => {
                if !written_word_is(value, "utf8mb4") {
                    return unsupported("CREATE TABLE character set");
                }
                "CHARACTER SET"
            }
            SqlOption::KeyValue { key, value } if names_a_collation(key) => {
                if !written_word_is(value, "utf8mb4_0900_ai_ci") {
                    return unsupported("CREATE TABLE collation");
                }
                "COLLATE"
            }
            _ => return unsupported("CREATE TABLE option"),
        };
        if written.contains(&named) {
            return unsupported("repeated CREATE TABLE option");
        }
        written.push(named);
    }
    Ok(())
}

/// Whether an option's key is one of the four ways MySQL writes a table's
/// character set.
fn names_a_character_set(key: &Ident) -> bool {
    [
        "CHARSET",
        "DEFAULT CHARSET",
        "CHARACTER SET",
        "DEFAULT CHARACTER SET",
    ]
    .iter()
    .any(|spelling| key.value.eq_ignore_ascii_case(spelling))
}

/// Whether an option's key is one of the two ways MySQL writes a table's
/// collation.
fn names_a_collation(key: &Ident) -> bool {
    ["COLLATE", "DEFAULT COLLATE"]
        .iter()
        .any(|spelling| key.value.eq_ignore_ascii_case(spelling))
}

/// Whether an option's value is one written word, quoted or not.
fn written_word_is(value: &Expr, expected: &str) -> bool {
    match value {
        Expr::Identifier(name) => name.value.eq_ignore_ascii_case(expected),
        Expr::Value(value) => matches!(
            &value.value,
            Value::SingleQuotedString(written) | Value::DoubleQuotedString(written)
                if written.eq_ignore_ascii_case(expected)
        ),
        _ => false,
    }
}

fn reject_table_attributes(table: &CreateTable) -> Result<(), ParseError> {
    if table.or_replace
        || table.external
        || table.dynamic
        || table.global.is_some()
        || table.transient
        || table.volatile
        || table.iceberg
        || table.snapshot
        || !matches!(table.hive_distribution, HiveDistributionStyle::NONE)
        || table.hive_formats.is_some()
        || !matches!(table.table_options, CreateTableOptions::None)
        || table.file_format.is_some()
        || table.location.is_some()
        || table.query.is_some()
        || table.without_rowid
        || table.like.is_some()
        || table.clone.is_some()
        || table.version.is_some()
        || table.comment.is_some()
        || table.on_commit.is_some()
        || table.on_cluster.is_some()
        || table.primary_key.is_some()
        || table.order_by.is_some()
        || table.partition_by.is_some()
        || table.cluster_by.is_some()
        || table.clustered_by.is_some()
        || table.inherits.is_some()
        || table.partition_of.is_some()
        || table.for_values.is_some()
        || table.strict
        || table.copy_grants
        || table.enable_schema_evolution.is_some()
        || table.change_tracking.is_some()
        || table.data_retention_time_in_days.is_some()
        || table.max_data_extension_time_in_days.is_some()
        || table.default_ddl_collation.is_some()
        || table.with_aggregation_policy.is_some()
        || table.with_row_access_policy.is_some()
        || table.with_storage_lifecycle_policy.is_some()
        || table.with_tags.is_some()
        || table.external_volume.is_some()
        || table.base_location.is_some()
        || table.catalog.is_some()
        || table.catalog_sync.is_some()
        || table.storage_serialization_policy.is_some()
        || table.target_lag.is_some()
        || table.warehouse.is_some()
        || table.refresh_mode.is_some()
        || table.initialize.is_some()
        || table.require_user
        || table.diststyle.is_some()
        || table.distkey.is_some()
        || table.sortkey.is_some()
        || table.backup.is_some()
    {
        return unsupported("table attributes");
    }
    Ok(())
}

/// Writes an `ENUM` or a `SET` as the quoted declared type the engine keeps
/// whole.
///
/// A member holding a quote of either kind is refused: the members ride
/// inside a double-quoted SQLite type name and are themselves single-quoted,
/// so either quote would have to be escaped twice over to survive, and
/// refusing is better than a member that reads back as something else. A
/// member holding a comma is refused for a `SET`, whose stored value joins
/// its members with one.
fn render_member_type(
    keyword: &str,
    members: &[sqlparser::ast::EnumMember],
) -> Result<String, ParseError> {
    if members.is_empty() {
        return unsupported("ENUM or SET without members");
    }
    let mut rendered = Vec::with_capacity(members.len());
    for member in members {
        let sqlparser::ast::EnumMember::Name(name) = member else {
            return unsupported("ENUM or SET member with a value");
        };
        if name.contains('\'') || name.contains('"') || name.contains('\\') {
            return unsupported("ENUM or SET member holding a quote");
        }
        if keyword == "SET" && name.contains(',') {
            return unsupported("SET member holding a comma");
        }
        rendered.push(format!("'{name}'"));
    }
    Ok(format!("\"{keyword}({})\"", rendered.join(",")))
}

/// Writes a `SET`, whose members sqlparser gives as plain strings.
fn render_set_type(members: &[String]) -> Result<String, ParseError> {
    let members = members
        .iter()
        .map(|name| sqlparser::ast::EnumMember::Name(name.clone()))
        .collect::<Vec<_>>();
    render_member_type("SET", &members)
}

/// Reads the members out of the declared type an `ENUM` column carries.
///
/// The engine gives the quoted type name back with its quotes, so the shape
/// read here is exactly the shape [`render_enum_type`] wrote.
pub fn enum_members(declared_type: &str) -> Option<Vec<String>> {
    member_type_members(declared_type, "ENUM")
}

/// Reads the members out of the declared type a `SET` column carries.
pub fn set_members(declared_type: &str) -> Option<Vec<String>> {
    member_type_members(declared_type, "SET")
}

fn member_type_members(declared_type: &str, keyword: &str) -> Option<Vec<String>> {
    let inner = declared_type
        .strip_prefix('"')
        .and_then(|rest| rest.strip_suffix('"'))
        .unwrap_or(declared_type);
    let members = inner
        .strip_prefix(&format!("{keyword}("))
        .and_then(|rest| rest.strip_suffix(')'))?;
    members
        .split(',')
        .map(|member| {
            member
                .strip_prefix('\'')
                .and_then(|rest| rest.strip_suffix('\''))
                .map(str::to_owned)
        })
        .collect()
}

/// Reports whether a custom type name is MySQL's `YEAR`.
fn names_the_year_type(name: &sqlparser::ast::ObjectName) -> bool {
    matches!(name.0.as_slice(), [ObjectNamePart::Identifier(ident)]
        if ident.quote_style.is_none() && ident.value.eq_ignore_ascii_case("YEAR"))
}

fn render_column(column: &ColumnDef) -> Result<String, ParseError> {
    let name = render_ident(&column.name);
    let data_type = match &column.data_type {
        // An integer's display width says how wide a client should print the
        // number and nothing about what it may hold. MySQL 8.4 deprecated it
        // and drops it: measured on 8.4.11, `INT(11)`, `TINYINT(4)` and
        // `BIGINT(20) UNSIGNED` all read back without their width, and
        // `INT(3)` still holds every INT. So the width is taken and dropped
        // here too, which is what lets a schema written by a dump or an ORM
        // land at all.
        //
        // `TINYINT(1)` is the one MySQL keeps, because a client reads it as a
        // boolean, and it is exactly what MySQL stores `BOOLEAN` as —
        // measured, both print `tinyint(1)` and both report a length of 1
        // where a plain `TINYINT` reports 4. So the two spellings meet here.
        // `TINYINT(1) UNSIGNED` is not one of them: measured, it reads back as
        // `tinyint unsigned`.
        DataType::TinyInt(Some(1)) => "BOOLEAN".to_owned(),
        DataType::TinyInt(_) => "TINYINT".to_owned(),
        DataType::SmallInt(_) => "SMALLINT".to_owned(),
        DataType::MediumInt(_) => "MEDIUMINT".to_owned(),
        DataType::Int(_) => "INT".to_owned(),
        DataType::Integer(_) => "INTEGER".to_owned(),
        DataType::BigInt(_) => "BIGINT".to_owned(),
        // The engine holds an integer as an i64, and the top value of each of
        // these fits one, so the range can be checked honestly. The declared
        // name is kept whole — the engine takes a multi-word type name — which
        // is what lets SHOW CREATE TABLE and the result metadata read the
        // column back as unsigned. BIGINT UNSIGNED is here on narrower terms:
        // MySQL takes up to 18446744073709551615 and this takes up to
        // i64::MAX, answering 1264 above it.
        DataType::TinyIntUnsigned(_) => "TINYINT UNSIGNED".to_owned(),
        DataType::SmallIntUnsigned(_) => "SMALLINT UNSIGNED".to_owned(),
        DataType::MediumIntUnsigned(_) => "MEDIUMINT UNSIGNED".to_owned(),
        DataType::IntUnsigned(_) => "INT UNSIGNED".to_owned(),
        DataType::IntegerUnsigned(_) => "INTEGER UNSIGNED".to_owned(),
        DataType::BigIntUnsigned(_) => "BIGINT UNSIGNED".to_owned(),
        DataType::TinyText => "TINYTEXT".to_owned(),
        DataType::MediumText => "MEDIUMTEXT".to_owned(),
        DataType::LongText => "LONGTEXT".to_owned(),
        DataType::Text => "TEXT".to_owned(),
        DataType::TinyBlob => "TINYBLOB".to_owned(),
        DataType::MediumBlob => "MEDIUMBLOB".to_owned(),
        DataType::LongBlob => "LONGBLOB".to_owned(),
        DataType::Blob(None) => "BLOB".to_owned(),
        DataType::Varchar(length) => format!("VARCHAR({})", declared_character_length(*length)?),
        DataType::Varbinary(length) => format!("VARBINARY({})", declared_binary_length(*length)?),
        DataType::Char(length) => format!("CHAR({})", declared_character_length(*length)?),
        // MySQL's DOUBLE and the engine's REAL are both IEEE 754 binary64, so
        // the name carries across without changing what a value means. A
        // precision is MySQL's deprecated `DOUBLE(p,s)`, which is refused.
        DataType::Double(sqlparser::ast::ExactNumberInfo::None) => "DOUBLE".to_owned(),
        // MySQL's FLOAT is binary32 and the engine has only binary64, so the
        // value is rounded to binary32 wherever a client can see it.
        DataType::Float(sqlparser::ast::ExactNumberInfo::None) => "FLOAT".to_owned(),
        // The sign is part of the declared name here as it is on an integer,
        // and what it changes is what may be written: measured on MySQL
        // 8.4.11, a negative answers 1264.
        DataType::DoubleUnsigned(sqlparser::ast::ExactNumberInfo::None) => {
            "DOUBLE UNSIGNED".to_owned()
        }
        DataType::FloatUnsigned(sqlparser::ast::ExactNumberInfo::None) => {
            "FLOAT UNSIGNED".to_owned()
        }
        // MySQL stores BOOLEAN and BOOL as TINYINT and reports both as
        // `tinyint(1)`. The name is kept so that the display width survives a
        // round trip; the value is a TINYINT's and is checked as one.
        DataType::Boolean | DataType::Bool => "BOOLEAN".to_owned(),
        // A fractional-second precision is refused: MySQL rounds a fractional
        // value to whole seconds without one, measured, and this stores whole
        // seconds only.
        DataType::Datetime(None) => "DATETIME".to_owned(),
        // A DATE holds the day alone. MySQL normalizes a wide input surface to
        // `YYYY-MM-DD`; this stores that form and only that form, the way it
        // already does for a DATETIME.
        DataType::Date => "DATE".to_owned(),
        // A TIME holds a span of time rather than a moment: measured on MySQL
        // 8.4.11 it runs from `-838:59:59` to `838:59:59`, so it takes more
        // than a day and it takes a sign.
        DataType::Time(None, sqlparser::ast::TimezoneInfo::None) => "TIME".to_owned(),
        // MySQL's ENUM carries its members, and the engine's declared type
        // grammar takes numbers inside its arguments and nothing else — but
        // it does take a **quoted** type name whole, and gives it back
        // unchanged. So the MySQL type is written as one, which keeps the
        // members on the one carrier every other MySQL type already rides:
        // the engine's declared type name. The values are stored as the text
        // they are.
        // MySQL's JSON is a document type: it takes the text the client wrote,
        // parses it, and stores what it parsed, so a column reads back in
        // MySQL's own canonical form rather than as it was written.
        DataType::JSON => "JSON".to_owned(),
        DataType::Enum(members, None) => render_member_type("ENUM", members)?,
        // A SET rides the same carrier an ENUM does, and differs in what it
        // stores: any subset of its members rather than one of them.
        DataType::Set(members) => render_set_type(members)?,
        // sqlparser has no `YEAR` of its own, so it arrives as a custom name.
        // Only that one name is taken here; every other custom name is a type
        // this frontend does not know.
        DataType::Custom(name, arguments) if arguments.is_empty() && names_the_year_type(name) => {
            "YEAR".to_owned()
        }
        // MySQL's TIMESTAMP is a UTC instant rendered in the session zone; this
        // holds the same text a DATETIME holds and converts nothing, so the two
        // differ only for a session that moves its zone.
        DataType::Timestamp(None, sqlparser::ast::TimezoneInfo::None) => "TIMESTAMP".to_owned(),
        DataType::Decimal(info) | DataType::Numeric(info) | DataType::Dec(info) => {
            let (precision, scale) = declared_decimal_size(*info)?;
            format!("DECIMAL({precision},{scale})")
        }
        // The sign has to go in front of the name here, not after it: the
        // engine's declared type takes a word before its arguments and not
        // after them. The MySQL word order is put back where the column is
        // read, so nothing above this sees the inversion.
        DataType::DecimalUnsigned(info) | DataType::DecUnsigned(info) => {
            let (precision, scale) = declared_decimal_size(*info)?;
            format!("UNSIGNED DECIMAL({precision},{scale})")
        }
        _ => return unsupported("column type"),
    };
    reject_duplicate_nullable_column_options(&column.options)?;
    let options = column
        .options
        .iter()
        .map(|option| render_column_option(option, &column.data_type))
        .collect::<Result<Vec<_>, _>>()?
        .into_iter()
        .flatten()
        .collect::<Vec<_>>();
    let mut definition = format!("{name} {data_type}");
    if !options.is_empty() {
        definition.push(' ');
        definition.push_str(&options.join(" "));
    }
    Ok(definition)
}

/// Reads the precision and scale a `DECIMAL` was declared with.
///
/// A bare `DECIMAL` means `DECIMAL(10,0)`, and a lone precision means a scale of
/// zero — measured on MySQL 8.4.11, where a bare one reports a column_length of
/// 11, the same as `DECIMAL(10,0)`. MySQL caps the precision at 65 and the scale
/// at 30 and requires the scale to fit inside the precision.
fn declared_decimal_size(info: ExactNumberInfo) -> Result<(u32, u32), ParseError> {
    let (precision, scale) = match info {
        ExactNumberInfo::None => (10, 0),
        ExactNumberInfo::Precision(precision) => (precision, 0),
        ExactNumberInfo::PrecisionAndScale(precision, scale) => {
            let scale = u64::try_from(scale).map_err(|_| ParseError::Unsupported {
                feature: "DECIMAL scale",
            })?;
            (precision, scale)
        }
    };
    if precision == 0 || precision > MAX_DECIMAL_PRECISION || scale > MAX_DECIMAL_SCALE {
        return unsupported("DECIMAL size");
    }
    if scale > precision {
        return unsupported("DECIMAL scale wider than its precision");
    }
    Ok((precision as u32, scale as u32))
}

/// Reads the precision and scale from a `DECIMAL` already stored as SQLite DDL.
pub fn stored_decimal_size(data_type: &TursoType) -> Result<(u32, u32), ParseError> {
    let Some(TursoTypeSize::TypeSize(precision, scale)) = data_type.size.as_ref() else {
        return unsupported("DECIMAL without a precision and scale");
    };
    let read = |expr: &TursoExpr| -> Result<u64, ParseError> {
        let TursoExpr::Literal(TursoLiteral::Numeric(text)) = expr else {
            return unsupported("DECIMAL size");
        };
        text.parse::<u64>().map_err(|_| ParseError::Unsupported {
            feature: "DECIMAL size",
        })
    };
    declared_decimal_size(ExactNumberInfo::PrecisionAndScale(
        read(precision)?,
        i64::try_from(read(scale)?).map_err(|_| ParseError::Unsupported {
            feature: "DECIMAL scale",
        })?,
    ))
}

/// Reads the character count a `VARCHAR(n)` or `CHAR(n)` was declared with.
///
/// MySQL counts characters here, not bytes: measured on 8.4.11, a
/// `VARCHAR(4)` stores four multi-byte characters. A bare `VARCHAR` has no
/// length, which MySQL rejects, and so does this.
/// MySQL's own limit on a `VARBINARY`, in bytes.
const MAX_VARBINARY_BYTES: u64 = 65_532;

/// Reads the byte count from a declared `VARBINARY`.
///
/// `BINARY(n)` is not here on purpose. Measured on MySQL 8.4.11: it pads a
/// shorter value with NUL bytes to the declared width — `'ab'` in a
/// `BINARY(16)` reads back sixteen bytes long — and the engine has no padding,
/// so taking it would store a different value than MySQL stores.
fn declared_binary_length(length: Option<sqlparser::ast::BinaryLength>) -> Result<u32, ParseError> {
    let Some(sqlparser::ast::BinaryLength::IntegerLength { length }) = length else {
        return unsupported("VARBINARY without a length");
    };
    if length == 0 || length > MAX_VARBINARY_BYTES {
        return unsupported("VARBINARY length");
    }
    u32::try_from(length).map_err(|_| ParseError::Unsupported {
        feature: "VARBINARY length",
    })
}

fn declared_character_length(length: Option<CharacterLength>) -> Result<u32, ParseError> {
    let Some(CharacterLength::IntegerLength { length, unit }) = length else {
        return unsupported("VARCHAR without a length");
    };
    if !matches!(unit, None | Some(CharLengthUnits::Characters)) {
        return unsupported("VARCHAR length unit");
    }
    if length == 0 || length > MAX_VARCHAR_CHARACTERS {
        return unsupported("VARCHAR length");
    }
    u32::try_from(length).map_err(|_| ParseError::Unsupported {
        feature: "VARCHAR length",
    })
}

fn reject_duplicate_nullable_column_options(
    options: &[sqlparser::ast::ColumnOptionDef],
) -> Result<(), ParseError> {
    let mut nullable_options = 0;
    for option in options {
        if matches!(&option.option, ColumnOption::Null | ColumnOption::NotNull) {
            nullable_options += 1;
        }
    }
    if nullable_options > 1 {
        return unsupported("multiple column NULL options");
    }
    Ok(())
}

/// Renders one column attribute, or answers `None` for one that is taken and
/// written nowhere.
/// Whether a `DEFAULT` names the moment the statement runs at.
///
/// MySQL writes it `CURRENT_TIMESTAMP`, with or without its parentheses, and
/// takes `NOW`, `LOCALTIME` and `LOCALTIMESTAMP` as the same thing — measured
/// on 8.4.11, a column written any of them prints back as
/// `DEFAULT CURRENT_TIMESTAMP`. The engine's own `CURRENT_TIMESTAMP` answers
/// the same moment in the same form, this server running in UTC, so it is what
/// the default is written as.
pub(crate) fn names_the_moment_a_statement_runs_at(expr: &Expr) -> bool {
    let Expr::Function(function) = expr else {
        return false;
    };
    let [ObjectNamePart::Identifier(name)] = function.name.0.as_slice() else {
        return false;
    };
    let takes_nothing = match &function.args {
        FunctionArguments::None => true,
        FunctionArguments::List(arguments) => arguments.args.is_empty(),
        FunctionArguments::Subquery(_) => false,
    };
    if !takes_nothing || function.over.is_some() {
        return false;
    }
    ["CURRENT_TIMESTAMP", "NOW", "LOCALTIME", "LOCALTIMESTAMP"]
        .iter()
        .any(|spelling| name.value.eq_ignore_ascii_case(spelling))
}

fn render_column_option(
    option: &sqlparser::ast::ColumnOptionDef,
    data_type: &DataType,
) -> Result<Option<String>, ParseError> {
    let name = render_constraint_name(option.name.as_ref());
    match &option.option {
        ColumnOption::Null if option.name.is_none() => Ok(Some("NULL".to_owned())),
        ColumnOption::NotNull if option.name.is_none() => Ok(Some("NOT NULL".to_owned())),
        ColumnOption::PrimaryKey(_) => unsupported("PRIMARY KEY"),
        ColumnOption::Unique(unique) => {
            reject_unique(unique)?;
            Ok(Some(format!("{name}UNIQUE")))
        }
        ColumnOption::Default(expr) if option.name.is_none() => {
            if names_the_moment_a_statement_runs_at(expr) {
                // MySQL takes this default on a column that holds a moment and
                // on no other — measured on 8.4.11, `DEFAULT CURRENT_TIMESTAMP`
                // on an `INT` answers 1067.
                if !matches!(data_type, DataType::Timestamp(_, _) | DataType::Datetime(_)) {
                    return unsupported(
                        "DEFAULT CURRENT_TIMESTAMP on a column that holds no moment",
                    );
                }
                return Ok(Some("DEFAULT CURRENT_TIMESTAMP".to_owned()));
            }
            Ok(Some(format!("DEFAULT {}", render_default(expr)?)))
        }
        ColumnOption::Check(check) => {
            if check.enforced.is_some() {
                return unsupported("CHECK enforcement attribute");
            }
            Ok(Some(format!(
                "{name}CHECK ({})",
                render_check(&check.expr)?
            )))
        }
        // A dumped schema spells out the charset and collation on every text
        // column. Naming the one this server has describes where it already is,
        // so it is taken; naming another would be a claim about ordering and
        // case that this cannot keep, so it is refused. The engine has no place
        // to keep the words, so they are written nowhere.
        ColumnOption::CharacterSet(name) if option.name.is_none() => {
            if !unqualified_name_is(name, &["utf8mb4"]) {
                return unsupported("column CHARACTER SET");
            }
            Ok(None)
        }
        ColumnOption::Collation(name) if option.name.is_none() => {
            if !unqualified_name_is(name, &["utf8mb4_general_ci", "utf8mb4_0900_ai_ci"]) {
                return unsupported("column COLLATE");
            }
            Ok(None)
        }
        // MySQL parses the inline reference and ignores it. Measured on
        // 8.4.11: a child row naming a parent that does not exist is stored,
        // and `SHOW CREATE TABLE` prints no constraint at all, whatever
        // `ON DELETE` or `ON UPDATE` was written beside it. So the faithful
        // answer is to read it and write nothing — the same answer as for a
        // column charset. The table-level `FOREIGN KEY (a) REFERENCES ...` is
        // a different statement and stays refused: MySQL enforces that one.
        ColumnOption::ForeignKey(_) if option.name.is_none() => Ok(None),
        ColumnOption::ForeignKey(_) => unsupported("column REFERENCES constraint"),
        ColumnOption::Default(_) => unsupported("named DEFAULT constraint"),
        _ => unsupported("column attribute"),
    }
}

/// Answers whether an unqualified name is one of the given words.
fn unqualified_name_is(name: &ObjectName, candidates: &[&str]) -> bool {
    let [ObjectNamePart::Identifier(identifier)] = name.0.as_slice() else {
        return false;
    };
    candidates
        .iter()
        .any(|candidate| identifier.value.eq_ignore_ascii_case(candidate))
}

fn render_table_constraint(constraint: &TableConstraint) -> Result<String, ParseError> {
    match constraint {
        TableConstraint::PrimaryKey(_) => unsupported("PRIMARY KEY"),
        TableConstraint::Unique(unique) => {
            reject_unique(unique)?;
            Ok(format!(
                "{}UNIQUE ({})",
                render_constraint_name(unique.name.as_ref()),
                render_index_columns(&unique.columns)?
            ))
        }
        TableConstraint::Check(check) => {
            if check.enforced.is_some() {
                return unsupported("CHECK enforcement attribute");
            }
            Ok(format!(
                "{}CHECK ({})",
                render_constraint_name(check.name.as_ref()),
                render_check(&check.expr)?
            ))
        }
        TableConstraint::ForeignKey(foreign_key) => {
            // The engine keeps the name a `CONSTRAINT` clause writes, which is
            // what `SHOW CREATE TABLE` prints back and what a later
            // `DROP FOREIGN KEY` names.
            let columns = render_idents(&foreign_key.columns);
            Ok(format!(
                "{}{}",
                render_constraint_name(foreign_key.name.as_ref()),
                render_foreign_key(foreign_key, Some(&columns))?
            ))
        }
        _ => unsupported("table constraint"),
    }
}

fn reject_unique(unique: &sqlparser::ast::UniqueConstraint) -> Result<(), ParseError> {
    if unique.index_name.is_some()
        || !unique.index_type_display.is_none()
        || unique.index_type.is_some()
        || !unique.index_options.is_empty()
        || unique.characteristics.is_some()
        || !matches!(
            unique.nulls_distinct,
            sqlparser::ast::NullsDistinctOption::None
        )
    {
        return unsupported("UNIQUE index attribute");
    }
    Ok(())
}

fn render_foreign_key(
    foreign_key: &sqlparser::ast::ForeignKeyConstraint,
    columns: Option<&str>,
) -> Result<String, ParseError> {
    if foreign_key.index_name.is_some()
        || foreign_key.match_kind.is_some()
        || foreign_key.characteristics.is_some()
    {
        return unsupported("FOREIGN KEY attribute");
    }
    if foreign_key.foreign_table.0.len() != 1 {
        return unsupported("schema-qualified FOREIGN KEY target");
    }
    let name = render_name(&foreign_key.foreign_table)?;
    let referred_columns = if foreign_key.referred_columns.is_empty() {
        String::new()
    } else {
        format!(" ({})", render_idents(&foreign_key.referred_columns))
    };
    let columns = columns.map_or_else(String::new, |columns| format!(" ({columns})"));
    let on_delete = foreign_key
        .on_delete
        .as_ref()
        .map(|action| format!(" ON DELETE {action}"))
        .unwrap_or_default();
    let on_update = foreign_key
        .on_update
        .as_ref()
        .map(|action| format!(" ON UPDATE {action}"))
        .unwrap_or_default();
    Ok(format!(
        "FOREIGN KEY{columns} REFERENCES {name}{referred_columns}{on_delete}{on_update}"
    ))
}

fn render_index_columns(columns: &[IndexColumn]) -> Result<String, ParseError> {
    let mut names = Vec::with_capacity(columns.len());
    for column in columns {
        if column.operator_class.is_some()
            || column.column.options.asc.is_some()
            || column.column.options.nulls_first.is_some()
            || !matches!(&column.column.expr, Expr::Identifier(_))
        {
            return unsupported("indexed column expression or ordering");
        }
        let Expr::Identifier(name) = &column.column.expr else {
            unreachable!("checked indexed column expression")
        };
        names.push(render_ident(name));
    }
    if names.is_empty() {
        return unsupported("empty PRIMARY KEY or UNIQUE column list");
    }
    Ok(names.join(", "))
}

fn render_default(expr: &Expr) -> Result<String, ParseError> {
    match expr {
        Expr::Value(value) => match &value.value {
            Value::Number(value, _) => Ok(value.clone()),
            Value::SingleQuotedString(value) => Ok(format!("'{}'", value.replace('\'', "''"))),
            Value::Boolean(value) => Ok(if *value { "TRUE" } else { "FALSE" }.to_string()),
            Value::Null => Ok("NULL".to_string()),
            _ => unsupported("DEFAULT literal"),
        },
        Expr::UnaryOp { op, expr } => {
            let sign = match op {
                UnaryOperator::Minus => "-",
                UnaryOperator::Plus => "+",
                _ => return unsupported("DEFAULT integer literal"),
            };
            let Expr::Value(value) = expr.as_ref() else {
                return unsupported("DEFAULT integer literal");
            };
            let Value::Number(value, _) = &value.value else {
                return unsupported("DEFAULT integer literal");
            };
            render_signed_integer_default(sign, value)
        }
        _ => unsupported("non-literal DEFAULT expression"),
    }
}

fn render_signed_integer_default(sign: &str, magnitude: &str) -> Result<String, ParseError> {
    let magnitude = magnitude
        .parse::<u64>()
        .map_err(|_| ParseError::Unsupported {
            feature: "DEFAULT integer literal",
        })?;
    let limit = if sign == "-" {
        (i64::MAX as u64) + 1
    } else {
        i64::MAX as u64
    };
    if magnitude > limit {
        return unsupported("DEFAULT integer literal");
    }
    Ok(format!("{sign}{magnitude}"))
}

fn reject_unsupported_mysql_string_escapes(
    sql: &str,
    mode: SessionSqlMode,
) -> Result<(), ParseError> {
    if mode.no_backslash_escapes {
        return Ok(());
    }
    // sqlparser accepts some escapes with semantics that do not match MySQL;
    // reject them before a normalized statement can be persisted.
    let bytes = sql.as_bytes();
    let mut cursor = 0;
    let mut quote = None;
    while cursor < bytes.len() {
        let byte = bytes[cursor];
        if let Some((delimiter, check_escapes)) = quote {
            if check_escapes && byte == b'\\' {
                let Some(escaped) = bytes.get(cursor + 1).copied() else {
                    return unsupported("unsupported MySQL string escape");
                };
                if !matches!(
                    escaped,
                    b'0' | b'\'' | b'"' | b'b' | b'n' | b'r' | b't' | b'Z' | b'\\' | b'%' | b'_'
                ) {
                    return unsupported("unsupported MySQL string escape");
                }
                cursor += 2;
                continue;
            }
            if byte == delimiter {
                if bytes.get(cursor + 1) == Some(&delimiter) {
                    cursor += 2;
                } else {
                    quote = None;
                    cursor += 1;
                }
            } else {
                cursor += 1;
            }
            continue;
        }
        match byte {
            b'\'' => {
                quote = Some((byte, true));
                cursor += 1;
            }
            b'`' => {
                quote = Some((byte, false));
                cursor += 1;
            }
            b'"' => {
                quote = Some((byte, !mode.ansi_quotes));
                cursor += 1;
            }
            b'#' => {
                cursor = bytes[cursor..]
                    .iter()
                    .position(|byte| *byte == b'\n' || *byte == b'\r')
                    .map_or(bytes.len(), |offset| cursor + offset);
            }
            b'-' if bytes.get(cursor + 1) == Some(&b'-') => {
                cursor = bytes[cursor + 2..]
                    .iter()
                    .position(|byte| *byte == b'\n' || *byte == b'\r')
                    .map_or(bytes.len(), |offset| cursor + 2 + offset);
            }
            b'/' if bytes.get(cursor + 1) == Some(&b'*') => {
                let Some(offset) = bytes[cursor + 2..]
                    .windows(2)
                    .position(|window| window == b"*/")
                else {
                    return Ok(());
                };
                cursor += offset + 4;
            }
            _ => cursor += 1,
        }
    }
    Ok(())
}

fn render_check(expr: &Expr) -> Result<String, ParseError> {
    if is_straightforward_check(expr) {
        Ok(expr.to_string())
    } else {
        unsupported("CHECK expression")
    }
}

fn is_straightforward_check(expr: &Expr) -> bool {
    match expr {
        Expr::Identifier(_) => true,
        Expr::CompoundIdentifier(parts) => !parts.is_empty(),
        Expr::Value(value) => matches!(
            value.value,
            Value::Number(_, _) | Value::SingleQuotedString(_) | Value::Boolean(_) | Value::Null
        ),
        Expr::Nested(expr) | Expr::IsNull(expr) | Expr::IsNotNull(expr) => {
            is_straightforward_check(expr)
        }
        Expr::UnaryOp { op, expr } => {
            matches!(op, UnaryOperator::Plus | UnaryOperator::Minus)
                && is_straightforward_check(expr)
        }
        Expr::BinaryOp { left, op, right } => {
            matches!(
                op,
                BinaryOperator::Plus
                    | BinaryOperator::Minus
                    | BinaryOperator::Multiply
                    | BinaryOperator::Modulo
                    | BinaryOperator::Gt
                    | BinaryOperator::Lt
                    | BinaryOperator::GtEq
                    | BinaryOperator::LtEq
                    | BinaryOperator::Eq
                    | BinaryOperator::NotEq
                    | BinaryOperator::And
                    | BinaryOperator::Or
            ) && is_straightforward_check(left)
                && is_straightforward_check(right)
        }
        Expr::Between {
            expr,
            negated,
            low,
            high,
        } => {
            !negated
                && is_straightforward_check(expr)
                && is_straightforward_check(low)
                && is_straightforward_check(high)
        }
        Expr::InList { expr, list, .. } => {
            is_straightforward_check(expr) && list.iter().all(is_straightforward_check)
        }
        _ => false,
    }
}

fn render_name(name: &ObjectName) -> Result<String, ParseError> {
    if !(1..=2).contains(&name.0.len()) {
        return unsupported("object name with more than two parts");
    }
    let mut parts = Vec::with_capacity(name.0.len());
    for part in &name.0 {
        let ObjectNamePart::Identifier(ident) = part else {
            return unsupported("dynamic object name");
        };
        parts.push(render_ident(ident));
    }
    Ok(parts.join("."))
}

fn render_unqualified_name(name: &ObjectName) -> Result<String, ParseError> {
    let [ObjectNamePart::Identifier(ident)] = name.0.as_slice() else {
        return unsupported("schema-qualified rename target");
    };
    Ok(render_ident(ident))
}

fn render_idents(idents: &[Ident]) -> String {
    idents
        .iter()
        .map(render_ident)
        .collect::<Vec<_>>()
        .join(", ")
}

fn render_constraint_name(name: Option<&Ident>) -> String {
    name.map(|name| format!("CONSTRAINT {} ", render_ident(name)))
        .unwrap_or_default()
}

pub(crate) fn render_ident_str(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

fn render_ident(ident: &Ident) -> String {
    render_ident_str(&ident.value)
}

fn unsupported<T>(feature: &'static str) -> Result<T, ParseError> {
    Err(ParseError::Unsupported { feature })
}

#[cfg(test)]
mod tests;
