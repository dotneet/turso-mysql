//! Conservative MySQL parsing for the SQLite-compatible path.

mod account_admin;
mod admin_command;
mod alter_table_indexes;
mod analyze_table;
mod catalog_qualifiers;
mod check_constraints;
mod checked_primary_key;
mod column_statistics;
mod create_table_as_select;
mod create_table_like;
mod current_database;
mod database_options;
mod date_format;
mod dollar_quote;
mod drop_table;
mod drop_view;
mod dump_ddl;
mod flush_tables;
mod from_dual;
mod information_schema;
mod insert_select;
mod json_value;
mod like_pattern;
mod lock_tables;
mod moment_difference;
mod mysql_ddl;
mod named_tables;
mod network_address;
mod nothing_to_run;
mod number_format;
mod prisma_catalog;
mod raw_bytes_in_words;
mod replace_view;
mod safe_updates;
mod select_projection_origins;
mod serial_type;
mod session_queries;
mod session_settings;
mod shift_moment;
mod show_character_sets;
mod show_engines;
mod show_full_tables;
mod show_server_activity;
mod show_stored_programs;
mod show_table_status;
mod show_triggers;
mod statement_reads;
mod statement_writes;
mod static_select_metadata;
mod str_to_date;
mod table_collation;
mod temporal_value;
mod translate;
mod trigger_definition;
mod truncate_table;
mod unknown_columns;
mod update_qualifiers;
mod updated_table_alias;
mod values_call;
mod view_definition;
mod written_bytes;
mod written_literals;
mod written_number;
mod written_value;

use admin_command::{
    admin_command_ends, consume_admin_database_name, consume_admin_qualified_table_name,
    consume_admin_table_name, consume_admin_u64, consume_admin_word, savepoint_command,
    skip_admin_comments, tokenize_admin_command, transaction_token_kind, AdminToken,
    TransactionTokenKind,
};
use alter_table_indexes::{
    checked_index_operation, drop_key_spelled_as_drop_index, is_index_operation,
};
use information_schema::{
    contains_information_schema_object, contains_information_schema_tables,
    reject_information_schema_columns_query_tokens, reject_information_schema_query_tokens,
    reject_information_schema_schemata_query_tokens, tokenize_information_schema_query,
    validate_information_schema_columns_query, validate_information_schema_schemata_query,
    validate_information_schema_tables_query,
};
pub use information_schema::{
    is_connector_j_information_schema_collation_query, is_connector_j_reserved_keywords_query,
    parse_connector_j_foreign_keys, parse_optional_connector_j_information_schema_query,
    parse_optional_connector_j_schemata_listing_query,
    parse_optional_flyway_schema_emptiness_query,
    parse_optional_gorm_information_schema_prepared_query,
    parse_optional_laravel_information_schema_query, parse_optional_performance_schema_read,
    parse_optional_sqlx_database_exists_query, ConnectorJForeignKey,
    ConnectorJInformationSchemaQuery, ConnectorJSchemataListingQuery, ConnectorJTables,
    FlywaySchemaEmptinessQuery, GormInformationSchemaPreparedQuery, LaravelInformationSchemaQuery,
    LaravelSchema, PerformanceSchemaRead, SqlxDatabaseExistsQuery,
};
use mysql_ddl::render_mysql_column;
pub use prisma_catalog::{
    parse_optional_prisma_information_schema_query, PrismaInformationSchemaQuery,
};
pub use raw_bytes_in_words::raw_bytes_in_words_as_hexadecimal;
use static_select_metadata::classify_static_select_expr;
use translate::{
    columns_given_their_default, delete_source_table, direct_signed_integer,
    is_clock_reading_value, names_the_columns_default, render_simple_view_query,
    select_static_result_metadata, translate_delete, translate_insert, translate_select_query,
    translate_update, RenderedSelect, SelectRenderContext,
};
pub use written_literals::{
    literals_written_into_columns, ColumnLiteral, WrittenColumn, WrittenLiterals,
};

pub use account_admin::{
    parse_optional_account_admin_command, AccountAdminPassword, MySqlAccountAdminCommand,
};
pub use admin_command::{parse_admin_command, parse_optional_admin_command};
pub use alter_table_indexes::renamed_tables;
pub use alter_table_indexes::{
    parse_optional_alter_table_indexes, MySqlAlterTableIndexOperation, MySqlAlterTableIndexes,
};
pub use analyze_table::{
    parse_analyze_table, parse_check_table, parse_optional_analyze_table,
    parse_optional_check_table, MySqlAnalyzeTableCommand, MySqlCheckTableCommand,
};
pub use catalog_qualifiers::leave_out_the_catalog_table_in_its_columns;
pub use check_constraints::{
    check_constraints_of, refuse_checks_numbered_out_of_order, table_a_check_is_dropped_from,
    table_with_a_check_changed, MySqlCheckChange, MySqlCheckConstraint,
};
pub use checked_primary_key::{
    parse_checked_primary_key_create_table, CheckedPrimaryKeyCreateTable,
    CheckedPrimaryKeyIntegerType,
};
pub use column_statistics::{parse_optional_histogram_query, MySqlHistogramQuery};
pub use create_table_as_select::{
    parse_optional_create_table_as_select, MySqlCreateTableAsSelect,
    MySqlCreateTableAsSelectColumn, MySqlCreateTableAsSelectSource,
};
pub use create_table_like::{parse_optional_create_table_like, MySqlCreateTableLike};
pub use current_database::{leave_out_the_current_database, write_the_current_database_in};
pub use date_format::{
    days_from_the_year_zero, format_moment, format_width, week_number, year_and_week,
};
pub use dollar_quote::opens_a_dollar_quote;
pub use drop_table::{parse_optional_drop_table, MySqlDropTableCommand};
pub use drop_view::{parse_optional_drop_view, MySqlDropViewCommand};
pub use dump_ddl::{
    parse_optional_alter_table_keys, parse_optional_mysqldump_ddl,
    parse_optional_mysqldump_drop_view, MySqlDumpDdl,
};
pub use flush_tables::{parse_flush_tables, parse_optional_flush_tables, MySqlFlushTablesCommand};
pub use from_dual::{inserts_written_values_only, leave_out_from_dual};
pub use insert_select::{
    direct_insert_select_projection, filtered_insert_select_projection, insert_select_source_sql,
    parse_optional_insert_select, parse_optional_insert_select_without_columns,
    parse_optional_insert_set_as_values, parse_optional_insert_values_without_columns,
    MySqlDirectInsertSelectProjection, MySqlInsertSelect, MySqlInsertSelectWithoutColumns,
    MySqlInsertValuesWithoutColumns,
};
pub use json_value::{
    is_a_json_path_this_reads, json_as_double, json_as_whole_number, json_compare_integer,
    json_compare_string, json_contains, json_equals, json_equals_integer, json_extract, json_keys,
    json_length, json_merge_patch, json_merge_preserve, json_overlaps, json_quote, json_search,
    json_type, json_unquote, normalize_json, JsonError, JsonNumberReading,
};
pub use like_pattern::MySqlLikePattern;
pub use lock_tables::{parse_optional_lock_tables, MySqlLockTablesCommand};
pub use moment_difference::{days_between, units_between};
pub use mysql_ddl::{
    render_counted_create_table_mysql_with_mode, render_create_index_mysql,
    render_create_index_mysql_with_mode, render_create_table_mysql,
    render_create_table_mysql_with_mode, render_create_view_mysql,
    render_create_view_mysql_with_mode, render_show_create_view_mysql,
    render_view_definition_mysql, stored_character_length, stored_temporal_precision,
};
pub use named_tables::{tables_named_by, NamedTable};
pub use network_address::{inet_aton, inet_ntoa, is_ipv4};
pub use nothing_to_run::{nothing_to_run, NothingToRun};
pub use number_format::{format_number, format_written_decimal, truncate_number};
pub use replace_view::{parse_optional_view_replacement, MySqlViewReplacement};
pub use safe_updates::{read_safe_update, ComparedValues, SafeUpdateConjunct, SafeUpdateReading};
pub use select_projection_origins::{
    compound_drops_repeated_rows, select_projection_origins, MySqlSelectProjectionOrigin,
};
pub use serial_type::write_serial_out;
pub use session_queries::{
    parse_optional_named_lock_query, parse_optional_select_database,
    parse_optional_system_variable_query, parse_optional_user_variable_query, MySqlNamedLockCall,
    MySqlNamedLockFunction, MySqlNamedLockQuery, MySqlSelectDatabaseQuery, MySqlSessionCall,
    MySqlSystemVariableQuery, MySqlSystemVariableRead, MySqlUserVariableQuery,
    MySqlUserVariableRead,
};
pub use session_settings::{
    parse_optional_session_settings, parse_optional_session_settings_with_parameters,
    MySqlSessionSetting, MySqlUserVariableAssignment, MySqlUserVariableValue,
    SessionSettingsWithParameters, SqlModeValue,
};
pub use shift_moment::shifted_moment;
pub use show_character_sets::{
    parse_optional_show_character_sets, MySqlShowCharacterSetsCommand, MySqlShowColumnTest,
    MySqlShowListingFilter, MySqlShowValueTest,
};
pub use show_engines::{parse_optional_show_engines, parse_show_engines, MySqlShowEnginesCommand};
pub use show_full_tables::{
    parse_optional_show_full_tables, parse_show_full_tables, MySqlShowFullTablesCommand,
};
pub use show_server_activity::{
    parse_optional_show_processlist, parse_optional_show_status,
    parse_optional_status_counter_read, MySqlShowStatusCommand, MySqlStatusCounterRead,
};
pub use show_stored_programs::{
    parse_optional_show_stored_programs, MySqlShowStoredProgramsCommand, MySqlStoredProgramKind,
};
pub use show_table_status::{
    parse_optional_show_table_status, parse_show_table_status, MySqlShowTableStatusCommand,
};
pub use show_triggers::{
    parse_optional_show_create_trigger, parse_optional_show_triggers,
    MySqlShowCreateTriggerCommand, MySqlShowTriggersCommand,
};
pub use statement_reads::{bytes_read, keep_reads, BytesRead, KeptReads};
pub use statement_writes::{what_a_statement_writes, StatementWrites};
pub use static_select_metadata::{
    ArithmeticOperand, ArithmeticOperator, ArithmeticShape, Branch, ColumnAggregateKind,
    ScalarFunction, StaticIntegerSign, StaticSelectMetadata, StaticSelectProjectionMetadata,
};
pub use str_to_date::{format_reads, read_by_format, FormatShape};
pub use table_collation::{
    alter_table_with_its_collation_on_each_text_column, character_set_of_collation,
    create_table_with_its_collation_on_each_text_column, create_table_with_the_database_collation,
    table_collation_of, table_comment_change, table_counter_change, table_engine_restated,
    table_options_of, widest_character_of_collation, MySqlTableCollation, MySqlTableOptions,
};
pub use temporal_value::{
    normalize_date, normalize_datetime, normalize_datetime_with_precision, normalize_time,
    normalize_time_with_precision, normalize_year, seconds_in_the_time, time_of_seconds,
    year_from_number,
};
pub use translate::{
    MySqlCatalogTable, MySqlColumnsTheKeysDecide, MySqlDerivedColumns, MySqlJoinedDerivedColumn,
    MySqlJoinedTable, MySqlNamedColumn, MySqlSelectSource,
};
pub use trigger_definition::{
    mysql_create_trigger_ddl, trigger_body_readings, trigger_written_as_mysql_keeps_it,
    written_trigger, MySqlTriggerBody, MySqlTriggerEvent, MySqlTriggerTiming, MySqlTriggerValue,
    MySqlTriggerWrite, MySqlTriggerWriteKind, MySqlWrittenTrigger,
};
pub use truncate_table::{parse_optional_truncate_table, MySqlTruncateTableCommand};
pub use unknown_columns::{unknown_column_named_by, UnknownColumn};
pub use update_qualifiers::leave_out_the_table_in_an_update;
pub use values_call::leave_out_the_table_in_values_calls;
pub use view_definition::{
    created_view_name, mysql_create_view_ddl, render_show_create_written_view_mysql,
    select_reads_rows_as_they_come, translated_view_is_kept_as_mysql_prints_it,
    translated_view_select, view_written_as_mysql_prints_it, written_view_columns,
    written_view_definition, MySqlViewColumnReading, MySqlViewSource, MySqlWrittenView,
};
pub use written_bytes::{
    base64_length, crc32, first_byte, first_character_code, quoted_for_sql, to_base64,
};
pub use written_number::{read_written_number, WrittenNumber};
pub use written_value::WrittenValue;

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
        AlterColumnOperation, AlterTable, AlterTableOperation, BinaryOperator, CharLengthUnits,
        CharacterLength, ColumnDef, ColumnOption, ColumnOptionDef, CommentDef, CreateIndex,
        CreateTable, CreateTableOptions, CreateTrigger, CreateView, DataType, Delete,
        ExactNumberInfo, Expr, FromTable, FunctionArguments, HiveDistributionStyle, Ident,
        IndexColumn, Insert, NullsDistinctOption, ObjectName, ObjectNamePart, PrimaryKeyConstraint,
        RenameTableNameKind, SelectFlavor, SelectItem, SetExpr, SqlOption, Statement,
        TableConstraint, TableFactor, TableObject, TriggerEvent as SqlTriggerEvent, TriggerObject,
        TriggerObjectKind, TriggerPeriod, UnaryOperator, Update, Value,
    },
    dialect::{Dialect, MySqlDialect},
    keywords::Keyword,
    parser::{Parser, ParserError},
    tokenizer::{Token, TokenWithSpan, Whitespace},
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

    /// MySQL writes `GROUP BY a WITH ROLLUP`. It is read here, and taken or
    /// refused where the statement is rendered.
    fn supports_group_by_with_modifier(&self) -> bool {
        true
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

    /// MySQL's `->` and `->>` read one path out of the column on their left and
    /// bind to it before anything else does: measured on 8.4.11,
    /// `doc->>'$.lang' = 'en'` compares what was read. sqlparser gives them
    /// PostgreSQL's precedence, below `=`, which reads that as
    /// `doc ->> ('$.lang' = 'en')`.
    fn get_next_precedence(&self, parser: &Parser) -> Option<Result<u8, ParserError>> {
        matches!(
            parser.peek_token_ref().token,
            Token::Arrow | Token::LongArrow
        )
        .then(|| Ok(self.prec_value(sqlparser::dialect::Precedence::DoubleColon)))
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
    /// The type the stored DDL writes that column with.
    ///
    /// Signed counted columns use a rowid alias, so this preserves their
    /// declared type for schema output and table rebuilds.
    pub allocator_column_written_type: &'static str,
    /// The number the table's `AUTO_INCREMENT=<n>` option names, which is the
    /// one the first row takes. `None` where the table named none, or named 0
    /// or 1, which is where the counter starts anyway.
    pub starts_the_counter_at: Option<u64>,
    /// Canonical MySQL DDL, including the checked `AUTO_INCREMENT` declaration.
    pub normalized_mysql_ddl: String,
    /// SQLite-compatible table definition. BIGINT UNSIGNED uses a separate
    /// primary key because a rowid alias cannot hold its upper range.
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
    source_values: Vec<Vec<AutoIncrementSourceValue>>,
    mixed_default_columns: Vec<usize>,
    /// For each row, the columns of `mixed_default_columns` it gives
    /// `DEFAULT`.
    defaults_in_each_row: Vec<Vec<usize>>,
    ignored_null_columns: Vec<usize>,
    /// Whether several rows meet a conflict clause, `IGNORE` or `ON DUPLICATE
    /// KEY UPDATE`, each row answering it for itself.
    rowwise_conflicts: bool,
    /// Whether the statement is a plain `INSERT` of several rows giving some
    /// column `DEFAULT` in some rows only.
    rows_differ_in_their_defaults: bool,
    /// Whether the statement carries `ON DUPLICATE KEY UPDATE`, rather than
    /// `IGNORE` or nothing.
    upserts: bool,
    upsert_columns: Vec<String>,
    /// The columns the upsert clause reads off the row it was offered.
    offered_columns: Vec<String>,
    reads_the_clock: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AutoIncrementSourceValue {
    Written(CheckedInsertValue),
    Parameter(usize),
}

/// What a 0 written into a counted column means.
///
/// Measured on MySQL 8.4.11: it asks for the next number, as NULL does, unless
/// the session's `sql_mode` names `NO_AUTO_VALUE_ON_ZERO`, when it is stored
/// as 0 — and a second one is a duplicate key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WrittenZero {
    AsksForTheNextNumber,
    Stored,
}

/// What one VALUES row asks the allocator to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AutoIncrementRowValue {
    Generated,
    Explicit(i128),
    Parameter(usize),
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

    pub fn rowwise_conflicts(&self) -> bool {
        self.rowwise_conflicts
    }

    /// Whether the rows are written one at a time, each by a statement of its
    /// own: for a conflict clause each row answers, or for the defaults each
    /// row asks for in a column other than the counted one, whose `DEFAULT`
    /// the counter answers in one statement.
    pub fn written_row_by_row(&self, counted_column: &str) -> bool {
        self.rowwise_conflicts || self.rows_differ_beyond(counted_column)
    }

    fn rows_differ_beyond(&self, counted_column: &str) -> bool {
        self.rows_differ_in_their_defaults
            && self.mixed_default_columns.iter().any(|at| {
                !self.columns[*at]
                    .as_str()
                    .eq_ignore_ascii_case(counted_column)
            })
    }

    /// Whether the statement carries `ON DUPLICATE KEY UPDATE`.
    pub fn upserts(&self) -> bool {
        self.upserts
    }

    /// Whether a row writes a reading of the clock, such as `NOW()`.
    pub fn reads_the_clock(&self) -> bool {
        self.reads_the_clock
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
        self.bind_allocator_table_with(table, WrittenZero::AsksForTheNextNumber)
    }

    /// Binds the allocator column, reading a written 0 the way the session's
    /// `sql_mode` says to.
    pub fn bind_allocator_table_with(
        self,
        table: &CheckedAutoIncrementCreateTable,
        written_zero: WrittenZero,
    ) -> Result<BoundAutoIncrementInsert, ParseError> {
        if !self
            .table_name
            .as_str()
            .eq_ignore_ascii_case(&table.table_name)
        {
            return unsupported("AUTO_INCREMENT INSERT table does not match its definition");
        }
        let allocator_column = TursoName::exact(table.allocator_column_name.clone());
        if self
            .upsert_columns
            .iter()
            .any(|column| column.eq_ignore_ascii_case(allocator_column.as_str()))
        {
            return unsupported("ON DUPLICATE KEY UPDATE changes the AUTO_INCREMENT column");
        }
        // Measured on MySQL 8.4.11, the offered row carries the number the
        // row took — `VALUES(id)` beside a `DEFAULT` id reads the number the
        // colliding row spent — and the number the engine's offered row
        // carries here has not been held to that one.
        if self
            .offered_columns
            .iter()
            .any(|column| column.eq_ignore_ascii_case(allocator_column.as_str()))
        {
            return unsupported(
                "ON DUPLICATE KEY UPDATE reads the AUTO_INCREMENT column off the offered row",
            );
        }
        let named_at = self.columns.iter().position(|column| {
            column
                .as_str()
                .eq_ignore_ascii_case(allocator_column.as_str())
        });
        // Each row of an upsert is written by a statement of its own, which
        // leaves out the columns that row gives `DEFAULT` — what MySQL writes
        // for them, measured on 8.4.11, both in the row and in the row the
        // clause is offered.
        let each_row_alone = (self.rowwise_conflicts && self.upserts)
            || self.rows_differ_beyond(allocator_column.as_str());
        if self
            .mixed_default_columns
            .iter()
            .any(|column| Some(*column) != named_at && !each_row_alone)
            || self
                .ignored_null_columns
                .iter()
                .any(|column| Some(*column) != named_at)
        {
            return unsupported("INSERT DEFAULT in some rows only");
        }
        let row_values = match named_at {
            None => vec![AutoIncrementRowValue::Generated; self.row_count.get()],
            Some(at) => self
                .source_values
                .iter()
                .map(|row| match row[at] {
                    AutoIncrementSourceValue::Written(
                        CheckedInsertValue::Default | CheckedInsertValue::Null,
                    ) => Ok(AutoIncrementRowValue::Generated),
                    AutoIncrementSourceValue::Written(CheckedInsertValue::SignedInteger(0))
                        if written_zero == WrittenZero::AsksForTheNextNumber =>
                    {
                        Ok(AutoIncrementRowValue::Generated)
                    }
                    AutoIncrementSourceValue::Written(CheckedInsertValue::SignedInteger(id)) => {
                        Ok(AutoIncrementRowValue::Explicit(id as i128))
                    }
                    AutoIncrementSourceValue::Written(CheckedInsertValue::UnsignedInteger(id)) => {
                        Ok(AutoIncrementRowValue::Explicit(id as i128))
                    }
                    AutoIncrementSourceValue::Parameter(ordinal) => {
                        Ok(AutoIncrementRowValue::Parameter(ordinal))
                    }
                    AutoIncrementSourceValue::Written(CheckedInsertValue::Other) => {
                        unsupported("AUTO_INCREMENT column requires an integer, NULL, or DEFAULT")
                    }
                })
                .collect::<Result<Vec<_>, _>>()?,
        };
        if self.rowwise_conflicts
            && !self.upserts
            && row_values
                .iter()
                .any(|value| *value != AutoIncrementRowValue::Generated)
        {
            return unsupported("multirow IGNORE with explicit AUTO_INCREMENT IDs");
        }
        let insert = match named_at {
            None => self.with_a_row_to_be_numbered()?,
            Some(at)
                if row_values
                    .iter()
                    .all(|value| *value == AutoIncrementRowValue::Generated) =>
            {
                self.asking_the_counter_for_every_row(at)?
            }
            Some(_) => self,
        };
        Ok(BoundAutoIncrementInsert {
            insert,
            allocator_column,
            allocator_at: named_at.filter(|_| {
                row_values
                    .iter()
                    .any(|value| *value != AutoIncrementRowValue::Generated)
            }),
            row_values,
            unsigned_bigint: table.allocator_column_type == MySqlIntegerType::BigIntUnsigned,
        })
    }

    /// The same `INSERT` with a row for the number to go into, where the
    /// statement wrote the row of defaults and has none.
    ///
    /// `INSERT INTO t () VALUES ()` and a statement whose every column is
    /// given `DEFAULT` both render as the engine's `DEFAULT VALUES`, which
    /// writes one row and offers nowhere to put a value. An empty row is put
    /// there instead, and the number goes into it the way it goes into every
    /// other statement's — measured on MySQL 8.4.11, both forms write one row
    /// taking the next number, every other column taking its own default.
    fn with_a_row_to_be_numbered(mut self) -> Result<Self, ParseError> {
        let Stmt::Insert { body, .. } = &mut self.sqlite_statement else {
            return Err(ParseError::TursoParser(
                "checked AUTO_INCREMENT INSERT did not produce an INSERT AST".to_string(),
            ));
        };
        if !matches!(body, turso_parser::ast::InsertBody::DefaultValues) {
            return Ok(self);
        }
        *body = turso_parser::ast::InsertBody::Select(
            turso_parser::ast::Select {
                with: None,
                body: turso_parser::ast::SelectBody {
                    select: turso_parser::ast::OneSelect::Values(vec![Vec::new()]),
                    compounds: Vec::new(),
                },
                order_by: Vec::new(),
                limit: None,
            },
            None,
        );
        Ok(self)
    }

    /// The same `INSERT` with the counted column taken out, where every row
    /// wrote it a value that asks the counter for the next number.
    ///
    /// Measured on MySQL 8.4.11: a written NULL and a written 0 each ask for
    /// the next number, exactly as leaving the column out does — `VALUES
    /// (NULL, 1)` into an empty table writes 1, and `VALUES (0, 2)` after it
    /// writes 2. Taking the column out is how the reserved path already
    /// answers a `DEFAULT` there, so the three spellings meet here.
    ///
    /// A row writing its own number beside one asking for the next is refused.
    /// Measured, `VALUES (NULL, 6), (50, 7), (NULL, 8)` writes 6, 50 and 51 —
    /// the counter moves past each written number as the rows go by, which one
    /// range reserved before the statement runs cannot do.
    fn asking_the_counter_for_every_row(mut self, at: usize) -> Result<Self, ParseError> {
        let Stmt::Insert { columns, body, .. } = &mut self.sqlite_statement else {
            return Err(ParseError::TursoParser(
                "checked AUTO_INCREMENT INSERT did not produce an INSERT AST".to_string(),
            ));
        };
        let turso_parser::ast::InsertBody::Select(select, _) = body else {
            return Err(ParseError::TursoParser(
                "checked AUTO_INCREMENT INSERT did not produce a VALUES body".to_string(),
            ));
        };
        let turso_parser::ast::OneSelect::Values(rows) = &mut select.body.select else {
            return Err(ParseError::TursoParser(
                "checked AUTO_INCREMENT INSERT did not produce VALUES rows".to_string(),
            ));
        };
        if columns.len() != self.columns.len() || at >= columns.len() {
            return Err(ParseError::TursoParser(
                "checked AUTO_INCREMENT INSERT changed shape before its column was taken out"
                    .to_string(),
            ));
        }
        for row in rows.iter() {
            if row.len() != columns.len() || !asks_the_counter_for_a_number(&row[at]) {
                return unsupported(
                    "INSERT names the AUTO_INCREMENT column a value that does not ask the counter for the next number",
                );
            }
        }
        for row in rows.iter_mut() {
            row.remove(at);
        }
        columns.remove(at);
        self.columns.remove(at);
        if self.columns.is_empty() {
            return unsupported("INSERT without an explicit column list");
        }
        Ok(self)
    }
}

/// Whether one written value asks the counter for the next number rather than
/// naming one of its own.
fn asks_the_counter_for_a_number(value: &TursoExpr) -> bool {
    match value {
        TursoExpr::Literal(TursoLiteral::Null) => true,
        TursoExpr::Literal(TursoLiteral::Numeric(digits)) => digits.parse::<i64>() == Ok(0),
        _ => false,
    }
}

/// A checked INSERT whose allocator column has been verified to be omitted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BoundAutoIncrementInsert {
    insert: CheckedAutoIncrementInsert,
    allocator_column: TursoName,
    allocator_at: Option<usize>,
    row_values: Vec<AutoIncrementRowValue>,
    unsigned_bigint: bool,
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

    pub fn row_values(&self) -> &[AutoIncrementRowValue] {
        &self.row_values
    }

    /// Whether the rows are written one at a time; see
    /// [`CheckedAutoIncrementInsert::written_row_by_row`].
    pub fn rowwise_conflicts(&self) -> bool {
        self.insert
            .written_row_by_row(self.allocator_column.as_str())
    }

    /// Whether a row writes a reading of the clock, such as `NOW()`.
    pub fn reads_the_clock(&self) -> bool {
        self.insert.reads_the_clock
    }

    pub fn inject_one_row(&self, row_number: usize, id: u64) -> Result<Stmt, ParseError> {
        self.one_row(row_number, Some(id))
    }

    /// One row of the statement written alone, with `id` in its counted
    /// column, or with the id it names itself where `id` is `None`, and
    /// without the columns it gives `DEFAULT`.
    pub fn one_row(&self, row_number: usize, id: Option<u64>) -> Result<Stmt, ParseError> {
        if row_number >= self.row_count().get() {
            return unsupported("AUTO_INCREMENT rowwise INSERT shape changed");
        }
        if id.is_none() && self.allocator_at.is_none() {
            return unsupported("AUTO_INCREMENT rowwise INSERT row names no id of its own");
        }
        let mut statement = statement_with_only_its_row(&self.insert.sqlite_statement, row_number)?;
        let Stmt::Insert { columns, body, .. } = &mut statement else {
            return unsupported("AUTO_INCREMENT INSERT AST changed");
        };
        let turso_parser::ast::InsertBody::Select(select, _) = body else {
            return unsupported("AUTO_INCREMENT INSERT VALUES body changed");
        };
        let turso_parser::ast::OneSelect::Values(rows) = &mut select.body.select else {
            return unsupported("AUTO_INCREMENT INSERT VALUES rows changed");
        };
        let defaults = self
            .insert
            .defaults_in_each_row
            .get(row_number)
            .map(Vec::as_slice)
            .unwrap_or_default();
        for &at in defaults.iter().rev() {
            if Some(at) == self.allocator_at {
                continue;
            }
            if at >= columns.len() || at >= rows[0].len() {
                return unsupported("AUTO_INCREMENT rowwise INSERT default column is missing");
            }
            columns.remove(at);
            rows[0].remove(at);
        }
        let allocator_at = self
            .allocator_at
            .map(|at| at - defaults.iter().filter(|dropped| **dropped < at).count());
        match (allocator_at, id) {
            (Some(at), Some(id)) => rows[0][at] = Box::new(self.id_literal(id)),
            (Some(_), None) => {}
            (None, Some(id)) => {
                columns.insert(0, self.allocator_column.clone());
                rows[0].insert(0, Box::new(self.id_literal(id)));
            }
            (None, None) => unreachable!("a row naming no id of its own was refused above"),
        }
        Ok(statement)
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
        if self.allocator_at.is_some() {
            return unsupported("AUTO_INCREMENT INSERT needs row-wise ID injection");
        }
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
        if last_id > i64::MAX as u64 && !self.unsigned_bigint {
            return unsupported("AUTO_INCREMENT range outside what the engine holds");
        }

        let mut statement = self.insert.sqlite_statement.clone();
        let Stmt::Insert { columns, body, .. } = &mut statement else {
            return Err(ParseError::TursoParser(
                "checked AUTO_INCREMENT INSERT did not produce an INSERT AST".to_string(),
            ));
        };
        let turso_parser::ast::InsertBody::Select(select, _) = body else {
            return Err(ParseError::TursoParser(
                "checked AUTO_INCREMENT INSERT did not produce a VALUES body".to_string(),
            ));
        };
        if !select.order_by.is_empty()
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
            row.insert(0, Box::new(self.id_literal(id)));
        }
        Ok(statement)
    }

    /// Replaces static generated values while keeping bound `?` positions.
    /// The caller replaces generated ID parameters in the bound value array.
    pub fn inject_row_ids(&self, ids: &[Option<u64>]) -> Result<Stmt, ParseError> {
        if ids.len() != self.row_count().get() {
            return unsupported("AUTO_INCREMENT row ID count changed");
        }
        let Some(at) = self.allocator_at else {
            let Some(first) = ids.first().copied().flatten() else {
                return unsupported("AUTO_INCREMENT row ID is missing");
            };
            if ids
                .iter()
                .enumerate()
                .any(|(offset, id)| *id != first.checked_add(offset as u64))
            {
                return unsupported("AUTO_INCREMENT IDs are not contiguous");
            }
            return self.inject_reserved_range(first);
        };
        let mut statement = self.insert.sqlite_statement.clone();
        let Stmt::Insert { body, .. } = &mut statement else {
            return unsupported("AUTO_INCREMENT INSERT AST changed");
        };
        let turso_parser::ast::InsertBody::Select(select, _) = body else {
            return unsupported("AUTO_INCREMENT INSERT VALUES body changed");
        };
        let turso_parser::ast::OneSelect::Values(rows) = &mut select.body.select else {
            return unsupported("AUTO_INCREMENT INSERT VALUES rows changed");
        };
        for (row, (source, id)) in rows.iter_mut().zip(self.row_values.iter().zip(ids)) {
            match (source, id) {
                (AutoIncrementRowValue::Generated, Some(id)) => {
                    row[at] = Box::new(self.id_literal(*id));
                }
                (AutoIncrementRowValue::Explicit(id), _) if *id > i64::MAX as i128 => {
                    if !self.unsigned_bigint {
                        return unsupported("AUTO_INCREMENT explicit ID is outside signed range");
                    }
                    row[at] = Box::new(self.id_literal(*id as u64));
                }
                _ => {}
            }
        }
        Ok(statement)
    }

    fn id_literal(&self, id: u64) -> TursoExpr {
        let literal = if self.unsigned_bigint && id > i64::MAX as u64 {
            TursoLiteral::String(format!("'{id}'"))
        } else {
            TursoLiteral::Numeric(id.to_string())
        };
        TursoExpr::Literal(literal)
    }
}

/// A copy of a checked `INSERT ... VALUES` holding only one of its rows.
///
/// A counted `INSERT` whose rows name their own ids is written a row at a
/// time, so copying every row for each one made the statement's cost grow
/// with the square of its rows: a dump's 5,000-row `INSERT` took 30 s.
fn statement_with_only_its_row(statement: &Stmt, row_number: usize) -> Result<Stmt, ParseError> {
    let Stmt::Insert {
        with,
        or_conflict,
        tbl_name,
        columns,
        body,
        returning,
    } = statement
    else {
        return unsupported("AUTO_INCREMENT INSERT AST changed");
    };
    let turso_parser::ast::InsertBody::Select(select, upsert) = body else {
        return unsupported("AUTO_INCREMENT INSERT VALUES body changed");
    };
    let turso_parser::ast::OneSelect::Values(rows) = &select.body.select else {
        return unsupported("AUTO_INCREMENT INSERT VALUES rows changed");
    };
    let row = rows
        .get(row_number)
        .cloned()
        .ok_or(ParseError::Unsupported {
            feature: "AUTO_INCREMENT rowwise INSERT row is missing",
        })?;
    Ok(Stmt::Insert {
        with: with.clone(),
        or_conflict: *or_conflict,
        tbl_name: tbl_name.clone(),
        columns: columns.clone(),
        body: turso_parser::ast::InsertBody::Select(
            turso_parser::ast::Select {
                with: select.with.clone(),
                body: turso_parser::ast::SelectBody {
                    select: turso_parser::ast::OneSelect::Values(vec![row]),
                    compounds: select.body.compounds.clone(),
                },
                order_by: select.order_by.clone(),
                limit: select.limit.clone(),
            },
            upsert.clone(),
        ),
        returning: returning.clone(),
    })
}

/// SQLite SQL produced from one checked MySQL `SELECT` statement.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TranslatedSelect {
    pub sqlite_sql: String,
    /// Whether a `GROUP_CONCAT` is rendered, whose cut this rendering warns
    /// about rather than fails on.
    concatenates_groups: bool,
    /// Whether `SQL_CALC_FOUND_ROWS` asked for the rows the statement answers
    /// without its `LIMIT`.
    calculates_found_rows: bool,
    columns_the_keys_decide: Option<MySqlColumnsTheKeysDecide>,
    bare_names_in_result_subqueries: Vec<(MySqlTableName, String)>,
    collation_sensitive_call_columns: Vec<String>,
    collation_sensitive_joined_columns: Vec<(String, String)>,
    json_reading_columns: Vec<String>,
    reads_table: bool,
    orders_a_bare_column: bool,
    checks_type_sensitive_expression: bool,
    renders_a_condition_without_column_types: bool,
    orders_wildcard_ordinal: bool,
    compares_a_placeholder: bool,
    counts_distinct_column: bool,
    tests_a_bare_column: bool,
    compares_a_written_day: bool,
    compares_a_written_number: bool,
    compares_a_large_decimal_integer: bool,
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
    /// One string of bytes written out — `X'..'`, `0x..`, `b'..'` or after
    /// `_binary` — which meets a column of bytes alone.
    Bytes,
    /// One call answering the moment the statement runs, which needs no
    /// argument and answers a value in the form the column it meets holds.
    Now(CheckedComparisonNow),
    /// A SQL NULL literal, which retains ordinary SQL three-valued logic.
    Null,
    /// One binary-protocol parameter at the zero-based statement ordinal.
    Placeholder { ordinal: usize },
    /// Another column. Only the frontend knows both columns' types, and it
    /// says which pairs MySQL and the engine compare alike.
    Column {
        qualifier: Option<String>,
        name: String,
    },
    /// Another call, answering this. The two calls meet when they answer the
    /// same kind.
    Call(CheckedComparisonAnswer),
    /// Not a value the column is compared with: the column is read by an
    /// expression on one side of the comparison, which only some kinds of
    /// column may be read through.
    Operand(CheckedComparisonOperand),
}

/// How a column is read on one side of a comparison.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckedComparisonOperand {
    /// By `+`, `-` or `*` — `WHERE age + 1 > 10`. The column has to be a
    /// signed whole number no wider than an `INT`, so the answer cannot
    /// leave a `BIGINT`: MySQL raises 1690 there where the engine goes on in
    /// floating point, and an unsigned column's difference below zero is 1690
    /// too.
    Arithmetic,
    /// By `COALESCE(col, value)` or `IFNULL(col, value)`. The written value
    /// is held to the column as a comparison of its own would be.
    Fallback,
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
        // `UTC_DATE`, `UTC_TIMESTAMP` and `UTC_TIME` read the clock in UTC,
        // and `SYSDATE` reads it as it runs rather than as the statement
        // began. Measured on MySQL 8.4.11, each answers the shape its local
        // or statement-start relative answers, and this server's own clock
        // reads UTC once for each call.
        if named(&["CURDATE", "CURRENT_DATE", "UTC_DATE"]) {
            return Some(Self::Day);
        }
        if named(&[
            "NOW",
            "CURRENT_TIMESTAMP",
            "UTC_TIMESTAMP",
            "SYSDATE",
            "LOCALTIME",
            "LOCALTIMESTAMP",
        ]) {
            return Some(Self::Moment);
        }
        named(&["CURTIME", "CURRENT_TIME", "UTC_TIME"]).then_some(Self::TimeOfDay)
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
    /// Text read out of a `JSON` column, which MySQL compares under
    /// `utf8mb4_bin` against a word and as a double against a number. A bound
    /// value is compared by what it binds as, which only holds while the
    /// statement has never bound a number there — see the frontend.
    JsonText,
    /// The count `JSON_LENGTH` answers, compared with a number. A bound value
    /// has to bind as a whole number.
    JsonCount,
    /// A document `JSON_CONTAINS` looks for. A bound value has to bind as
    /// text, which the dialect then reads as a document.
    JsonDocument,
    /// The path a JSON reading binds — `JSON_EXTRACT(doc, ?)` — which has to
    /// bind as a path this reads the way MySQL does, or as NULL.
    JsonPath,
    /// The pattern a `LIKE` over text a JSON reading unquoted binds, which
    /// has to bind as a word or as NULL.
    JsonPattern,
    /// A JSON value read out of a column compared with a bound value, which
    /// MySQL compares as a JSON string when a word binds and as a JSON number
    /// when a whole number does — only while the statement has never bound a
    /// number there, as for [`Self::JsonText`].
    JsonValue,
    /// The rows a `COUNT` in a `HAVING` counted. MySQL reads a value bound
    /// against it as a whole number — measured, a bound `'abc'` warns
    /// "Truncated incorrect INTEGER value" — so a bound word has to name one.
    RowCount,
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
    /// The table the outer column is named through, which has to be the one
    /// table the statement reads.
    qualifier: Option<String>,
    column_name: String,
    inner_table: String,
    inner_column_name: String,
    /// The columns the subquery's `WHERE` fixes to one value each, for a
    /// subquery answering a plain column rather than an aggregate. MySQL
    /// answers 1242 when such a subquery finds more than one row, where the
    /// engine takes the first, so the frontend holds these to cover a key of
    /// the table. Empty for a subquery that needs no such proof.
    fixed_columns: Vec<String>,
}

impl CheckedSubqueryComparison {
    /// Returns the table the outer column is named through, if it is.
    pub fn qualifier(&self) -> Option<&str> {
        self.qualifier.as_deref()
    }

    /// Returns the outer column tested for membership.
    pub fn column_name(&self) -> &str {
        &self.column_name
    }

    /// Returns the columns the subquery's `WHERE` fixes to one value each,
    /// which have to cover a key of its table.
    pub fn fixed_columns(&self) -> &[String] {
        &self.fixed_columns
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
    /// The tables of the subquery this comparison was written inside, when it
    /// was written inside one: one, or those it joins. An unqualified name
    /// looks there before it looks at the statement's own table, which is how
    /// MySQL reads one.
    inner_sources: Vec<String>,
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
    pub(crate) fn name_the_inner_sources(&mut self, references: &[&str]) {
        if self.inner_sources.is_empty() {
            self.inner_sources = references
                .iter()
                .map(|reference| (*reference).to_owned())
                .collect();
        }
    }

    /// Returns the subquery this comparison was written inside, if any.
    ///
    /// Measured on MySQL 8.4.11: an unqualified name inside a subquery is the
    /// subquery's column when it has one, and the outer statement's when it
    /// does not — `EXISTS (SELECT 1 FROM b WHERE name = 'one')` reads `a.name`
    /// where `b` carries no `name`.
    pub fn inner_source(&self) -> Option<&str> {
        self.inner_sources.first().map(String::as_str)
    }

    /// Returns every table of the subquery this comparison was written
    /// inside: the one it reads, or each it joins. A name one of them has is
    /// that table's column — measured on MySQL 8.4.11, Laravel's `whereHas`
    /// through a pivot, `EXISTS (SELECT * FROM tags INNER JOIN post_tag ON
    /// ... WHERE name = ?)`, reads `tags.name`.
    pub fn inner_sources(&self) -> &[String] {
        &self.inner_sources
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
    collation_sensitive_call_columns: Vec<String>,
    json_reading_columns: Vec<String>,
    /// Whether a comparison names a column against a word naming a number,
    /// which is read as that number only once the column is known to hold
    /// numbers.
    compares_a_written_number: bool,
    /// Which parameters stand where an `INSERT ... SELECT`'s `SELECT` writes a
    /// row count.
    row_count_parameters: Vec<usize>,
    /// Whether the `SELECT` an `INSERT ... SELECT` copies from was rendered
    /// by the frontend knowing its columns' types, which a statement read
    /// again without a connection cannot do.
    copies_a_select_rendered_knowing_its_types: bool,
    bound_arithmetic_operands: Vec<BoundArithmeticOperand>,
    /// The table the statement writes and the columns it writes a
    /// `CAST(... AS JSON)` into, which the frontend holds to being `JSON`
    /// columns.
    json_cast_columns: Option<(String, Vec<String>)>,
    /// Whether an `UPDATE` reads a column through `COALESCE(col, n)`, which is
    /// held to the column's kind only on a reading that knows it.
    falls_back_in_a_set: bool,
}

/// A `?` an `UPDATE` adds to, takes from or multiplies a value by —
/// GORM's `gorm.Expr("balance - ?", 10)` — and the column the answer is
/// written into. MySQL reads the bound value by that column's type, so the
/// frontend holds what binds there to it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BoundArithmeticOperand {
    pub(crate) ordinal: usize,
    pub(crate) written_column: String,
}

impl BoundArithmeticOperand {
    /// The zero-based place of the `?` among the statement's parameters.
    pub const fn ordinal(&self) -> usize {
        self.ordinal
    }

    /// The column the arithmetic's answer is written into.
    pub fn written_column(&self) -> &str {
        &self.written_column
    }
}

impl TranslatedDml {
    /// Returns each `?` the statement's `SET` does arithmetic with.
    pub fn bound_arithmetic_operands(&self) -> &[BoundArithmeticOperand] {
        &self.bound_arithmetic_operands
    }

    /// Reports whether an `UPDATE` reads a column through `COALESCE(col, n)`,
    /// which a reading of the statement has to know the table's column kinds
    /// to take.
    pub fn falls_back_in_a_set(&self) -> bool {
        self.falls_back_in_a_set
    }

    /// Returns the table the statement writes and the columns it writes a
    /// `CAST(... AS JSON)` into, when it writes one.
    pub fn json_cast_columns(&self) -> Option<(&str, &[String])> {
        self.json_cast_columns
            .as_ref()
            .map(|(table, columns)| (table.as_str(), columns.as_slice()))
    }

    /// Reports whether the `SELECT` this `INSERT ... SELECT` copies from was
    /// rendered knowing its columns' types.
    pub fn copies_a_select_rendered_knowing_its_types(&self) -> bool {
        self.copies_a_select_rendered_knowing_its_types
    }

    /// Returns which parameters stand where the statement writes a row count.
    pub fn row_count_parameters(&self) -> &[usize] {
        &self.row_count_parameters
    }

    /// Reports whether a comparison names a column against a word naming a
    /// number, which a second reading knowing the columns' types renders.
    pub fn compares_a_written_number(&self) -> bool {
        self.compares_a_written_number
    }

    /// Returns the columns read by a call the statement compares under
    /// `utf8mb4_0900_ai_ci`'s weights.
    pub fn collation_sensitive_call_columns(&self) -> &[String] {
        &self.collation_sensitive_call_columns
    }

    /// Returns the columns a JSON reading in the `WHERE` reads, each of which
    /// has to be a `JSON` column of the table the statement writes.
    pub fn json_reading_columns(&self) -> &[String] {
        &self.json_reading_columns
    }

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
/// `BIGINT UNSIGNED` reaches the full u64 range. The i128 return value keeps
/// both signed and unsigned limits in one representation.
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
    /// Returns the inclusive bounds used by strict assignment checks.
    pub const fn bounds(self) -> (i128, i128) {
        match self {
            Self::TinyInt => (-128, 127),
            Self::SmallInt => (-32_768, 32_767),
            Self::MediumInt => (-8_388_608, 8_388_607),
            Self::Int => (-2_147_483_648, 2_147_483_647),
            Self::BigInt => (i64::MIN as i128, i64::MAX as i128),
            // Measured on MySQL 8.4.11: these are the top values each unsigned
            // type accepts, and one past any of them answers 1264. The first
            // four fit an i64; BIGINT UNSIGNED needs the full u64 range.
            Self::TinyIntUnsigned => (0, 255),
            Self::SmallIntUnsigned => (0, 65_535),
            Self::MediumIntUnsigned => (0, 16_777_215),
            Self::IntUnsigned => (0, 4_294_967_295),
            Self::BigIntUnsigned => (0, u64::MAX as i128),
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

/// How a column of bytes — a `BLOB` of any size, a `VARBINARY` or a
/// `BINARY` — holds them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ByteStringColumn {
    /// Any number of bytes up to a most: a `VARBINARY` its declared count, a
    /// `BLOB` its type's — 255 for a `TINYBLOB`, 65535 for a `BLOB`,
    /// 16777215 for a `MEDIUMBLOB` and 4294967295 for a `LONGBLOB`.
    Varying { most_bytes: u64 },
    /// Exactly its declared count of bytes, which a `BINARY` holds whatever
    /// it is given: measured on 8.4.11, a shorter value is filled out with
    /// zero bytes.
    Padded { width: u32 },
}

impl ByteStringColumn {
    /// Reads how a column declared with this type holds its bytes, or
    /// nothing when it holds characters or numbers.
    pub fn of_declared_type(data_type: &DataType) -> Option<Self> {
        let most_bytes = match data_type {
            DataType::TinyBlob => 255,
            DataType::Blob(None) => 65_535,
            DataType::MediumBlob => 16_777_215,
            DataType::LongBlob => 4_294_967_295,
            DataType::Varbinary(length) => u64::from(declared_binary_length(*length).ok()?),
            DataType::Binary(width) => {
                return Some(Self::Padded {
                    width: declared_padded_width(*width).ok()?,
                })
            }
            _ => return None,
        };
        Some(Self::Varying { most_bytes })
    }
}

/// Answers whether the engine type name a column is stored under names a
/// column of bytes rather than of characters.
pub fn holds_bytes(engine_type_name: &str) -> bool {
    [
        "TINYBLOB",
        "BLOB",
        "MEDIUMBLOB",
        "LONGBLOB",
        "VARBINARY",
        "BINARY",
    ]
    .iter()
    .any(|name| engine_type_name.eq_ignore_ascii_case(name))
}

/// Private MySQL numeric metadata rebuilt from durable normalized table DDL.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MySqlNumericSpec {
    columns: Vec<Option<MySqlIntegerType>>,
    character_lengths: Vec<Option<u32>>,
    fixed_widths: Vec<bool>,
    byte_strings: Vec<Option<ByteStringColumn>>,
    words: Vec<bool>,
    datetimes: Vec<bool>,
    timestamps: Vec<bool>,
    dates: Vec<bool>,
    times: Vec<bool>,
    temporal_precisions: Vec<Option<u8>>,
    years: Vec<bool>,
    bits: Vec<bool>,
    enums: Vec<Option<Vec<String>>>,
    sets: Vec<Option<Vec<String>>>,
    jsons: Vec<bool>,
    floats: Vec<bool>,
    unsigned_reals: Vec<bool>,
    three_byte_texts: Vec<bool>,
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

    /// Reports whether a stored column holds words: a `VARCHAR`, a `CHAR` or
    /// a `TEXT` of any size.
    pub fn holds_words(&self, index: usize) -> bool {
        self.words.get(index).copied().unwrap_or(false)
    }

    /// Returns how a column holding bytes rather than characters holds them.
    pub fn byte_string(&self, index: usize) -> Option<ByteStringColumn> {
        self.byte_strings.get(index).copied().flatten()
    }

    /// Reports whether a stored column position holds a `DATETIME`.
    pub fn is_datetime(&self, index: usize) -> bool {
        self.datetimes.get(index).copied().unwrap_or(false)
    }

    /// Reports whether a stored column position holds a `TIMESTAMP`.
    pub fn is_timestamp(&self, index: usize) -> bool {
        self.timestamps.get(index).copied().unwrap_or(false)
    }

    /// Reports whether a stored column position holds a `DATE`.
    pub fn is_date(&self, index: usize) -> bool {
        self.dates.get(index).copied().unwrap_or(false)
    }

    /// Reports whether a stored column position holds a `TIME`.
    pub fn is_time(&self, index: usize) -> bool {
        self.times.get(index).copied().unwrap_or(false)
    }

    /// Returns the fractional-second precision declared for a temporal column.
    pub fn temporal_precision(&self, index: usize) -> Option<u8> {
        self.temporal_precisions.get(index).copied().flatten()
    }

    /// Reports whether a stored column position holds a `YEAR`.
    pub fn is_year(&self, index: usize) -> bool {
        self.years.get(index).copied().unwrap_or(false)
    }

    /// Reports whether a stored column position holds a `BIT(1)`.
    pub fn is_bit(&self, index: usize) -> bool {
        self.bits.get(index).copied().unwrap_or(false)
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

    /// Reports whether a stored column holds MySQL's binary32 `FLOAT`.
    pub fn is_float(&self, index: usize) -> bool {
        self.floats.get(index).copied().unwrap_or(false)
    }

    /// Reports whether a stored column position holds an unsigned `DOUBLE` or
    /// `FLOAT`, which takes no negative value.
    pub fn is_unsigned_real(&self, index: usize) -> bool {
        self.unsigned_reals.get(index).copied().unwrap_or(false)
    }

    /// Reports whether a stored column holds words in `utf8mb3`, which has no
    /// character past the Basic Multilingual Plane.
    pub fn holds_three_byte_characters(&self, index: usize) -> bool {
        self.three_byte_texts.get(index).copied().unwrap_or(false)
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
            && !self.timestamps.iter().any(|is_timestamp| *is_timestamp)
            && !self.dates.iter().any(|is_date| *is_date)
            && !self.times.iter().any(|is_time| *is_time)
            && !self.years.iter().any(|is_year| *is_year)
            && !self.enums.iter().any(Option::is_some)
            && !self.sets.iter().any(Option::is_some)
            && !self.jsons.iter().any(|is_json| *is_json)
            && !self.floats.iter().any(|is_float| *is_float)
            && !self.unsigned_reals.iter().any(|is_unsigned| *is_unsigned)
            && !self
                .three_byte_texts
                .iter()
                .any(|is_three_byte| *is_three_byte)
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

    pub fn collation_sensitive_call_columns(&self) -> &[String] {
        &self.collation_sensitive_call_columns
    }

    /// Returns the columns a JSON reading in a condition reads, each of which
    /// has to be a `JSON` column of the one table the statement reads.
    pub fn json_reading_columns(&self) -> &[String] {
        &self.json_reading_columns
    }

    pub fn checks_type_sensitive_expression(&self) -> bool {
        self.checks_type_sensitive_expression
    }

    /// Reports whether a `CASE`, `IF`, `IFNULL` or `COALESCE` naming a column
    /// was written without knowing what kinds of values its columns hold,
    /// which decides how it has to be written.
    pub fn renders_a_condition_without_column_types(&self) -> bool {
        self.renders_a_condition_without_column_types
    }

    /// Reports whether a comparison names a column against a word naming a
    /// number, which a second reading knowing which columns hold numbers
    /// renders as that number.
    pub fn compares_a_written_number(&self) -> bool {
        self.compares_a_written_number
    }

    /// Reports whether this statement's rendering depends on a column's type.
    ///
    /// An `ORDER BY` over a bare column and a comparison against a `?` are the
    /// two places it does, and both want the same answer: whether the column is
    /// text, and so wants MySQL's collation.
    pub fn needs_column_types(&self) -> bool {
        self.orders_a_bare_column
            || self.checks_type_sensitive_expression
            || self.compares_a_placeholder
            || self.counts_distinct_column
            || self.tests_a_bare_column
            || self.compares_a_written_day
            || self.compares_a_written_number
            || self.compares_a_large_decimal_integer
            || self.orders_wildcard_ordinal
            || self.checked_comparisons.iter().any(|comparison| {
                matches!(
                    comparison.operator(),
                    CheckedSelectComparisonOperator::In | CheckedSelectComparisonOperator::NotIn
                )
            })
            || self.static_answers().any(|answer| {
                matches!(answer, StaticSelectMetadata::ScalarCall { columns, .. } if !columns.is_empty())
            })
            || self.static_answers().any(|answer| {
                matches!(
                    answer,
                    StaticSelectMetadata::ColumnAggregate {
                        kind: ColumnAggregateKind::Sum | ColumnAggregateKind::Avg,
                        ..
                    } | StaticSelectMetadata::ScalarCall {
                        function: ScalarFunction::CutsDigits
                            | ScalarFunction::KeepsNumericShape
                            | ScalarFunction::Truncates
                            | ScalarFunction::RoundsToPlaces { .. }
                            | ScalarFunction::RoundsToWhole
                            | ScalarFunction::Negates
                            | ScalarFunction::DividesWhole
                            | ScalarFunction::NegatesTruth
                            | ScalarFunction::TestsTruth
                            | ScalarFunction::Modulo
                            | ScalarFunction::Widest
                            | ScalarFunction::Hexadecimal
                            | ScalarFunction::GroupsDigits
                            | ScalarFunction::NullsOnMatch
                            | ScalarFunction::WritesInAnotherRadix
                            | ScalarFunction::FindsThePlace
                            | ScalarFunction::ReadsThePlace,
                        ..
                    } | StaticSelectMetadata::Arithmetic(_)
                        | StaticSelectMetadata::RoundedAggregate { .. }
                )
            })
    }

    /// Returns each projection's answer whose shape this statement fixes,
    /// whatever table it is read out of.
    fn static_answers(&self) -> impl Iterator<Item = &StaticSelectMetadata> {
        self.static_result_metadata
            .iter()
            .filter_map(|projection| match projection {
                StaticSelectProjectionMetadata::Literal(metadata) => Some(metadata.answer()),
                _ => None,
            })
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

    /// Reports whether `SQL_CALC_FOUND_ROWS` asked for the rows the statement
    /// answers without its `LIMIT`, which running it notes on the connection.
    pub const fn calculates_found_rows(&self) -> bool {
        self.calculates_found_rows
    }

    /// Returns the columns a grouped statement projects beside its keys,
    /// which it may name only when the keys decide them.
    pub const fn columns_the_keys_decide(&self) -> Option<&MySqlColumnsTheKeysDecide> {
        self.columns_the_keys_decide.as_ref()
    }

    /// Returns each column named with its table whose words a `GROUP_CONCAT`
    /// orders, as the name the table goes by and the column's name, which
    /// has to be under the collation that order follows.
    pub fn collation_sensitive_joined_columns(&self) -> &[(String, String)] {
        &self.collation_sensitive_joined_columns
    }

    /// Returns each name a subquery standing as a result column reads without
    /// a table, with the table the subquery reads, which the name has to be a
    /// column of.
    pub fn bare_names_in_result_subqueries(&self) -> &[(MySqlTableName, String)] {
        &self.bare_names_in_result_subqueries
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
    ExpectedOneStatement {
        actual: usize,
    },
    ExpectedAdminCommand,
    ExpectedAccountAdminCommand,
    ExpectedTransactionCommand,
    TrailingAdminCommandTokens,
    TrailingAccountAdminCommandTokens,
    InvalidAccountUsername {
        reason: &'static str,
    },
    UnsupportedAccountHost,
    InvalidDatabaseName {
        reason: &'static str,
    },
    InvalidTableName {
        reason: &'static str,
    },
    InvalidSavepointName {
        reason: &'static str,
    },
    ExpectedCreateTable,
    ExpectedCreateIndex,
    ExpectedCreateView,
    ExpectedCreateTrigger,
    ExpectedAlterTable,
    ExpectedSelect,
    ExpectedDml,
    JsonLiteralDefault,
    JsonIndex,
    /// A collation MySQL does not have, which MySQL answers with 1273.
    UnknownCollation,
    /// A character set MySQL does not have, which MySQL answers with 1115.
    UnknownCharacterSet,
    /// A collation beside another character set than its own, which MySQL
    /// answers with 1253.
    CollationOfAnotherCharacterSet,
    /// Two different character sets named for one thing, which MySQL answers
    /// with 1302.
    ConflictingCharacterSets,
    Unsupported {
        feature: &'static str,
    },
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
            Self::ExpectedAccountAdminCommand => {
                f.write_str("expected CREATE USER, GRANT SELECT, or REVOKE SELECT")
            }
            Self::ExpectedTransactionCommand => {
                f.write_str("expected BEGIN, START TRANSACTION, COMMIT, or ROLLBACK")
            }
            Self::TrailingAdminCommandTokens => {
                f.write_str("unexpected token after database-management command")
            }
            Self::TrailingAccountAdminCommandTokens => {
                f.write_str("unexpected token after account-management command")
            }
            Self::InvalidAccountUsername { reason } => {
                write!(f, "invalid MySQL account username: {reason}")
            }
            Self::UnsupportedAccountHost => f.write_str("only @'%' account host is supported"),
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
            Self::JsonLiteralDefault => f.write_str("JSON column cannot have a literal default"),
            Self::JsonIndex => f.write_str("JSON column cannot be indexed directly"),
            Self::UnknownCollation => f.write_str("unknown collation"),
            Self::UnknownCharacterSet => f.write_str("unknown character set"),
            Self::CollationOfAnotherCharacterSet => {
                f.write_str("collation is not valid for the character set")
            }
            Self::ConflictingCharacterSets => f.write_str("conflicting character sets"),
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
    /// Create one logical database, or with `IF NOT EXISTS` leave one that is
    /// already there as it stands.
    CreateDatabase {
        name: MySqlDatabaseName,
        only_if_missing: bool,
        /// What every table made in it takes when it names no collation.
        collation: MySqlTableCollation,
    },
    /// Change the collation a database gives the tables made in it from now
    /// on; `None` names the selected database.
    AlterDatabase {
        name: Option<MySqlDatabaseName>,
        /// `None` when the statement names neither a character set nor a
        /// collation, which leaves the database as it is.
        collation: Option<MySqlTableCollation>,
    },
    /// Drop one logical database.
    DropDatabase {
        name: MySqlDatabaseName,
        /// `IF EXISTS`: a database that is not there is a note, not an error.
        only_if_present: bool,
    },
    /// Print the `CREATE DATABASE` that makes a database as it is now.
    ShowCreateDatabase {
        name: MySqlDatabaseName,
        /// The name as the statement wrote it, which MySQL prints.
        written_name: String,
        only_if_missing: bool,
    },
    /// Select one logical database for the current session.
    Use { name: MySqlDatabaseName },
    /// List the logical databases visible to this session.
    ListDatabases,
    /// List the visible databases whose names a pattern matches.
    ///
    /// Measured on MySQL 8.4.11 with `lower_case_table_names=1`: the pattern
    /// matches a name by its case, as `SHOW TABLES LIKE` does and unlike every
    /// other `SHOW ... LIKE` — `LIKE 'TURSO%'` finds no `turso_oracle`.
    ListDatabasesLike { pattern: MySqlLikePattern },
}

impl MySqlAdminCommand {
    /// Returns the command's logical database name, when it has one.
    pub fn name(&self) -> Option<&MySqlDatabaseName> {
        match self {
            Self::CreateDatabase { name, .. }
            | Self::DropDatabase { name, .. }
            | Self::ShowCreateDatabase { name, .. }
            | Self::Use { name } => Some(name),
            Self::AlterDatabase { name, .. } => name.as_ref(),
            Self::ListDatabases | Self::ListDatabasesLike { .. } => None,
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
/// MySQL's table has twenty-one and these are the ones whose values this
/// server holds, or knows it does not keep. A query naming any other column is
/// refused rather than answered with a value that would be made up.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MySqlInformationSchemaTablesColumn {
    TableSchema,
    TableName,
    TableType,
    Engine,
    /// How many bytes the rows take, which InnoDB keeps and this does not, so
    /// it is NULL as `SHOW TABLE STATUS` answers it.
    DataLength,
    /// How many bytes the indexes take, NULL for the same reason.
    IndexLength,
    TableCollation,
    TableComment,
}

impl MySqlInformationSchemaTablesColumn {
    /// Reads one column by the name a query wrote, whatever its case.
    fn named(name: &str) -> Option<Self> {
        Some(match () {
            () if name.eq_ignore_ascii_case("TABLE_SCHEMA") => Self::TableSchema,
            () if name.eq_ignore_ascii_case("TABLE_NAME") => Self::TableName,
            () if name.eq_ignore_ascii_case("TABLE_TYPE") => Self::TableType,
            () if name.eq_ignore_ascii_case("ENGINE") => Self::Engine,
            () if name.eq_ignore_ascii_case("DATA_LENGTH") => Self::DataLength,
            () if name.eq_ignore_ascii_case("INDEX_LENGTH") => Self::IndexLength,
            () if name.eq_ignore_ascii_case("TABLE_COLLATION") => Self::TableCollation,
            () if name.eq_ignore_ascii_case("TABLE_COMMENT") => Self::TableComment,
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
/// MySQL's table has twenty-two and these are the ones whose values this
/// server holds, in the order MySQL declares them in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MySqlInformationSchemaColumnsColumn {
    ColumnName,
    OrdinalPosition,
    ColumnDefault,
    IsNullable,
    DataType,
    ColumnType,
    ColumnKey,
    Extra,
    ColumnComment,
    CharacterMaximumLength,
    NumericPrecision,
    NumericScale,
    CollationName,
}

impl MySqlInformationSchemaColumnsColumn {
    /// Reads one column by the name a query wrote, whatever its case.
    fn named(name: &str) -> Option<Self> {
        Some(match () {
            () if name.eq_ignore_ascii_case("COLUMN_NAME") => Self::ColumnName,
            () if name.eq_ignore_ascii_case("ORDINAL_POSITION") => Self::OrdinalPosition,
            () if name.eq_ignore_ascii_case("COLUMN_DEFAULT") => Self::ColumnDefault,
            () if name.eq_ignore_ascii_case("IS_NULLABLE") => Self::IsNullable,
            () if name.eq_ignore_ascii_case("DATA_TYPE") => Self::DataType,
            () if name.eq_ignore_ascii_case("COLUMN_TYPE") => Self::ColumnType,
            () if name.eq_ignore_ascii_case("COLUMN_KEY") => Self::ColumnKey,
            () if name.eq_ignore_ascii_case("EXTRA") => Self::Extra,
            () if name.eq_ignore_ascii_case("COLUMN_COMMENT") => Self::ColumnComment,
            () if name.eq_ignore_ascii_case("CHARACTER_MAXIMUM_LENGTH") => {
                Self::CharacterMaximumLength
            }
            () if name.eq_ignore_ascii_case("NUMERIC_PRECISION") => Self::NumericPrecision,
            () if name.eq_ignore_ascii_case("NUMERIC_SCALE") => Self::NumericScale,
            () if name.eq_ignore_ascii_case("COLLATION_NAME") => Self::CollationName,
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

/// The scope a variable read names.
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
    /// `SHOW [GLOBAL | SESSION] VARIABLES` with no pattern: every variable.
    pub fn every(scope: MySqlVariableScope) -> Self {
        Self {
            scope,
            pattern: None,
        }
    }

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
    reject_json_defaults_and_keys(&table)?;
    let mut remaining = table.clone();
    let keyed_column = unique_written_on_the_primary_key(&mut remaining)?;
    if keyed_column.is_none()
        && !table.constraints.iter().any(|constraint| {
            matches!(
                constraint,
                TableConstraint::Index(_)
                    | TableConstraint::Unique(_)
                    | TableConstraint::ForeignKey(_)
            )
        })
    {
        return Ok(None);
    }
    let [ObjectNamePart::Identifier(table_ident)] = table.name.0.as_slice() else {
        return unsupported("schema-qualified CREATE TABLE name");
    };
    let table_name =
        MySqlTableName::parse(&table_ident.value).map_err(|_| ParseError::Unsupported {
            feature: "CREATE TABLE name",
        })?;
    // The column's own `UNIQUE` comes before every key the table writes after
    // its columns, so it takes its name first.
    let mut indexes = Vec::new();
    if let Some(column) = keyed_column {
        indexes.push(MySqlInlineIndex {
            name: column.clone(),
            columns: vec![column],
            unique: true,
        });
    }
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
            Some(index_name) => {
                MySqlTableName::parse(&index_name.value).map_err(|_| ParseError::Unsupported {
                    feature: "inline KEY name",
                })?;
                index_name.value.clone()
            }
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

/// Takes a `UNIQUE` off the column the primary key is over, and answers that
/// column.
///
/// Measured on MySQL 8.4.11: `name VARCHAR(255) NOT NULL UNIQUE, PRIMARY KEY
/// (name)` — the table Sequelize keeps its migrations in — and `id SERIAL
/// PRIMARY KEY` each make two indexes, `PRIMARY` and a unique one named after
/// the column. The engine would read the two as one key, so the `UNIQUE` is
/// made an index of its own. A key over several columns is not the column's
/// key, and a column's `UNIQUE` there stays where it is.
fn unique_written_on_the_primary_key(
    table: &mut CreateTable,
) -> Result<Option<String>, ParseError> {
    let key_clauses = table
        .constraints
        .iter()
        .filter_map(|constraint| match constraint {
            TableConstraint::PrimaryKey(key) => Some(key),
            _ => None,
        })
        .collect::<Vec<_>>();
    let keyed_by_clause = match key_clauses.as_slice() {
        [key] => match key.columns.as_slice() {
            [column] => match &column.column.expr {
                Expr::Identifier(name) => Some(name.value.clone()),
                _ => None,
            },
            _ => None,
        },
        _ => None,
    };
    let Some(column) = table.columns.iter_mut().find(|column| {
        let keyed_inline = column
            .options
            .iter()
            .any(|option| matches!(option.option, ColumnOption::PrimaryKey(_)));
        keyed_inline
            || keyed_by_clause
                .as_ref()
                .is_some_and(|name| name.eq_ignore_ascii_case(&column.name.value))
    }) else {
        return Ok(None);
    };
    let mut unique_options = 0;
    for option in &column.options {
        if let ColumnOption::Unique(unique) = &option.option {
            if option.name.is_some() || unique.name.is_some() {
                return unsupported("named UNIQUE on the PRIMARY KEY column");
            }
            reject_unique(unique)?;
            unique_options += 1;
        }
    }
    match unique_options {
        0 => Ok(None),
        1 => {
            column
                .options
                .retain(|option| !matches!(option.option, ColumnOption::Unique(_)));
            Ok(Some(column.name.value.clone()))
        }
        _ => unsupported("UNIQUE written twice on one column"),
    }
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
///
/// MySQL's own synonyms are taken: `FIELDS` for `COLUMNS`, which is what a
/// schema reader written against MySQL often sends, and `IN` for `FROM`.
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
    // `FIELDS` is MySQL's own synonym for `COLUMNS`, and `IN` for `FROM`. A
    // schema reader written against MySQL reaches for either, and `SHOW INDEX`
    // already takes all of its own spellings here.
    if !["COLUMNS", "FIELDS"]
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
    let (mut database, table) = consume_admin_qualified_table_name(&tokens, &mut cursor)?;
    if consume_admin_word(&tokens, &mut cursor, "FROM")
        || consume_admin_word(&tokens, &mut cursor, "IN")
    {
        if database.is_some() {
            return unsupported("two SHOW COLUMNS database qualifiers");
        }
        database = Some(consume_admin_database_name(&tokens, &mut cursor)?);
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
    let (mut database, table) = consume_admin_qualified_table_name(&tokens, &mut cursor)?;
    // Measured on MySQL 8.4.11: Connector/J's `SHOW KEYS FROM `posts` FROM
    // `db`` and Sequelize's `SHOW INDEX FROM users FROM sequelize` answer what
    // `db.posts` does, and the database written after the table stands in
    // place of one written before it — `SHOW INDEX FROM mysql.a1 FROM probe`
    // lists `probe.a1`.
    if ["FROM", "IN"]
        .iter()
        .any(|word| consume_admin_word(&tokens, &mut cursor, word))
    {
        database = Some(consume_admin_database_name(&tokens, &mut cursor)?);
    }
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

/// Parses `SHOW CREATE VIEW [db.]name`, which names its view the way
/// `SHOW CREATE TABLE` names its table.
pub fn parse_optional_show_create_view(
    sql: &str,
    mode: SessionSqlMode,
) -> Result<Option<MySqlShowCreateTableCommand>, ParseError> {
    let tokens = tokenize_admin_command(sql, mode)?;
    let mut cursor = skip_admin_comments(&tokens, 0);
    if !consume_admin_word(&tokens, &mut cursor, "SHOW")
        || !consume_admin_word(&tokens, &mut cursor, "CREATE")
        || !consume_admin_word(&tokens, &mut cursor, "VIEW")
    {
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
    /// `START TRANSACTION READ WRITE`, which a session made read-only can
    /// still write in: measured on MySQL 8.4.11, an `INSERT` inside one is
    /// taken after `SET SESSION TRANSACTION READ ONLY`.
    BeginReadWrite,
    /// `START TRANSACTION WITH CONSISTENT SNAPSHOT`, which takes the read
    /// snapshot at the statement rather than at the first read.
    BeginWithConsistentSnapshot,
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
    let tokens = statement_reads::tokens(&dialect, sql)
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
    // Measured on MySQL 8.4.11: `SESSION` and `LOCAL`, written as a word or
    // as `@@SESSION.` and `@@LOCAL.`, and `@@autocommit` with no scope all
    // set the session's autocommit.
    let assignment = match tokens {
        [set, name, equals, value]
            if is_unquoted_word(set, "SET")
                && (is_unquoted_word(name, "AUTOCOMMIT")
                    || is_unquoted_word(name, "@@AUTOCOMMIT"))
                && matches!(equals, Token::Eq) =>
        {
            value
        }
        [set, session, name, equals, value]
            if is_unquoted_word(set, "SET")
                && (is_unquoted_word(session, "SESSION") || is_unquoted_word(session, "LOCAL"))
                && is_unquoted_word(name, "AUTOCOMMIT")
                && matches!(equals, Token::Eq) =>
        {
            value
        }
        [set, session, Token::Period, name, equals, value]
            if is_unquoted_word(set, "SET")
                && (is_unquoted_word(session, "@@SESSION")
                    || is_unquoted_word(session, "@@LOCAL"))
                && is_unquoted_word(name, "AUTOCOMMIT")
                && matches!(equals, Token::Eq) =>
        {
            value
        }
        _ => return unsupported("SET autocommit syntax"),
    };
    // Measured on MySQL 8.4.11: `ON`, `OFF`, `TRUE`, `FALSE` and the quoted
    // `'ON'` and `'OFF'` are taken beside 0 and 1; 2 answers 1231.
    let enabled = match assignment {
        Token::Number(value, false) if value == "0" => false,
        Token::Number(value, false) if value == "1" => true,
        Token::SingleQuotedString(value) if value.eq_ignore_ascii_case("ON") => true,
        Token::SingleQuotedString(value) if value.eq_ignore_ascii_case("OFF") => false,
        value if is_unquoted_word(value, "ON") || is_unquoted_word(value, "TRUE") => true,
        value if is_unquoted_word(value, "OFF") || is_unquoted_word(value, "FALSE") => false,
        _ => return unsupported("SET autocommit value; expected 0, 1, ON or OFF"),
    };
    Ok(Some(MySqlAutocommitSetting { enabled }))
}

/// Reports whether a statement carries no parameter and answers no rows —
/// a schema change, a session setting or a lock — judged by the word it begins
/// with.
///
/// A client that prepares every statement, as Laravel does, prepares these
/// too, and one the checked prepared path does not take can still be run the
/// way the text path runs it: there is nothing to bind and no column to
/// describe before it runs. A write is left out: the checked prepared path
/// takes every write the text path does, so one it refuses is refused as it is
/// prepared.
pub fn answers_no_rows_and_binds_nothing(sql: &str, mode: SessionSqlMode) -> bool {
    let Ok(tokens) = statement_reads::tokens(&SessionMySqlDialect::new(mode), sql) else {
        return false;
    };
    if tokens
        .iter()
        .any(|token| matches!(token, Token::Placeholder(_)))
    {
        return false;
    }
    let Some(first) = tokens
        .iter()
        .find(|token| !matches!(token, Token::Whitespace(_)))
    else {
        return false;
    };
    [
        "ALTER", "CREATE", "DROP", "RENAME", "TRUNCATE", "SET", "LOCK", "UNLOCK", "FLUSH",
    ]
    .iter()
    .any(|keyword| is_unquoted_word(first, keyword))
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
    if token_kind == TransactionTokenKind::ConsistentSnapshot {
        return Ok(Some(MySqlTransactionCommand::BeginWithConsistentSnapshot));
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
            // `READ ONLY` is a promise MySQL keeps with 1792, and this keeps it
            // too rather than accepting the words and ignoring them.
            match modes.as_slice() {
                [] => MySqlTransactionCommand::Begin,
                [sqlparser::ast::TransactionMode::AccessMode(
                    sqlparser::ast::TransactionAccessMode::ReadWrite,
                )] => MySqlTransactionCommand::BeginReadWrite,
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

/// The table one `CREATE TABLE` writes, and whether it said to write it only
/// where it is not there.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MySqlCreatedTable {
    table: MySqlTableName,
    only_if_missing: bool,
    temporary: bool,
}

impl MySqlCreatedTable {
    /// Returns the table the statement writes.
    pub const fn table(&self) -> &MySqlTableName {
        &self.table
    }

    /// Whether the statement said `IF NOT EXISTS`.
    pub const fn only_if_missing(&self) -> bool {
        self.only_if_missing
    }

    /// Whether the statement said `TEMPORARY`.
    ///
    /// MySQL lets a temporary table stand beside a permanent one of the same
    /// name and shadow it, so a name already taken says nothing about one.
    pub const fn temporary(&self) -> bool {
        self.temporary
    }
}

/// The table a `CREATE TABLE` names, or nothing where the statement is not one.
///
/// MySQL answers a name that is already there before it looks at anything else
/// — measured on 8.4.11, error 1050 without `IF NOT EXISTS` and note 1050 with
/// it, both leaving the table exactly as it stands whatever the rest of the
/// statement says. So the caller looks the name up before running anything.
pub fn parse_optional_created_table(
    sql: &str,
    mode: SessionSqlMode,
) -> Result<Option<MySqlCreatedTable>, ParseError> {
    let Ok(Statement::CreateTable(table)) = parse_one_statement(sql, mode) else {
        return Ok(None);
    };
    let [ObjectNamePart::Identifier(name)] = table.name.0.as_slice() else {
        return unsupported("schema-qualified CREATE TABLE name");
    };
    let name = MySqlTableName::parse(&name.value).map_err(|_| ParseError::Unsupported {
        feature: "CREATE TABLE name",
    })?;
    Ok(Some(MySqlCreatedTable {
        table: name,
        only_if_missing: table.if_not_exists,
        temporary: table.temporary,
    }))
}

/// The words a stored `CREATE TABLE` carries for a column an `UPDATE`
/// rewrites, exactly as this renders them.
pub const ON_UPDATE_MOMENT: &str = " ON UPDATE CURRENT_TIMESTAMP";

/// The columns of one `CREATE TABLE` that an `UPDATE` rewrites to the moment
/// it runs at, in the order the statement declared them.
///
/// The engine has no such attribute, so the words live only in the stored
/// MySQL DDL and the caller has to read them out of it before handing the rest
/// to the engine's own parser.
pub fn columns_rewritten_on_update(
    sql: &str,
    mode: SessionSqlMode,
) -> Result<Vec<String>, ParseError> {
    let Ok(Statement::CreateTable(table)) = parse_one_statement(sql, mode) else {
        return Ok(Vec::new());
    };
    Ok(table
        .columns
        .iter()
        .filter(|column| column_is_rewritten_on_update(column))
        .map(|column| column.name.value.clone())
        .collect())
}

/// The words a stored `CREATE TABLE` begins a column's comment with, exactly
/// as this renders them.
pub const COLUMN_COMMENT_WORDS: &str = " COMMENT '";

/// The comment each column of one `CREATE TABLE` carries, by column name, for
/// the columns that carry one.
///
/// The engine has no attribute for a comment, so the words live only in the
/// stored MySQL DDL and the caller has to read them out of it before handing
/// the rest to the engine's own parser.
pub fn column_comments(
    sql: &str,
    mode: SessionSqlMode,
) -> Result<Vec<(String, String)>, ParseError> {
    let Ok(Statement::CreateTable(table)) = parse_one_statement(sql, mode) else {
        return Ok(Vec::new());
    };
    Ok(table
        .columns
        .iter()
        .filter_map(|column| {
            let text = column_comment(column)?;
            (!text.is_empty()).then(|| (column.name.value.clone(), text.to_owned()))
        })
        .collect())
}

/// What an `ALTER TABLE` that names a place for a column means.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MySqlColumnPlacement {
    /// The table has to be written again with the column standing there.
    TableWrittenAgain(MySqlTableRewrite),
    /// The place asked for is the one the column takes anyway, so the
    /// statement means what it means with the words left off, which is what
    /// this carries.
    AlreadyAtTheEnd(String),
    /// The statement names a column the table has not got, which MySQL
    /// answers 1054 for — measured, `AFTER missing` says
    /// `Unknown column 'missing'`.
    NoSuchColumn(String),
    /// A `CHANGE` renames a column onto a name the table already carries,
    /// which MySQL answers 1060 for — measured, `Duplicate column name 'n'`.
    DuplicateColumn(String),
}

/// The table one `ALTER TABLE` makes of another, and the columns whose values
/// move across to it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MySqlTableRewrite {
    /// The `CREATE TABLE` the table becomes, under the name it already has.
    pub create_sql: String,
    /// Each column of the new table that takes its values from the old one,
    /// as the name it has now and the name it had. The two differ only where a
    /// `CHANGE` renamed the column. A column the statement adds is not among
    /// them: it takes its own default, which is what MySQL gives it in every
    /// row already there.
    pub carried_columns: Vec<(String, String)>,
}

/// The unqualified table one `ALTER TABLE` names, where it names one.
pub fn alter_table_target(sql: &str, mode: SessionSqlMode) -> Option<String> {
    let Ok(Statement::AlterTable(alter)) = parse_one_statement(sql, mode) else {
        return None;
    };
    match alter.name.0.as_slice() {
        [ObjectNamePart::Identifier(name)] => Some(name.value.clone()),
        _ => None,
    }
}

/// Reads an `ALTER TABLE ... ADD COLUMN ... FIRST` or `... AFTER x` against the
/// table it changes, and answers the table it becomes.
///
/// The engine puts a new column last and MySQL puts it where the statement
/// says, and a table's column order is something a client sees — in
/// `SELECT *`, in `SHOW CREATE TABLE`, and in an `INSERT` that names no
/// columns. So the table is written again with the column in that place.
///
/// Answers `None` where the statement names no position, or names the last
/// column, which is where the column lands anyway.
pub fn table_with_a_column_placed(
    stored_ddl: &str,
    alter_sql: &str,
    mode: SessionSqlMode,
) -> Result<Option<MySqlColumnPlacement>, ParseError> {
    let Ok(Statement::AlterTable(alter)) = parse_one_statement(alter_sql, mode) else {
        return Ok(None);
    };
    if alter.operations.len() > 1 {
        return table_with_columns_added_in_place(stored_ddl, &alter.operations, mode);
    }
    let [operation] = alter.operations.as_slice() else {
        return Ok(None);
    };
    // What the statement does to the column, and where it asks for it. A
    // `MODIFY` and a `CHANGE` restate the column whole as well as moving it,
    // and a `CHANGE` renames it, so each says which column the values come
    // from as well as which column they land in.
    let (column_def, position, moved) = match operation {
        AlterTableOperation::AddColumn {
            column_def,
            column_position: Some(position),
            if_not_exists: false,
            ..
        } => (column_def.clone(), position, None),
        AlterTableOperation::ModifyColumn {
            col_name,
            data_type,
            options,
            column_position: Some(position),
        } => (
            restated_column(col_name, data_type, options),
            position,
            Some(col_name.value.clone()),
        ),
        AlterTableOperation::ChangeColumn {
            old_name,
            new_name,
            data_type,
            options,
            column_position: Some(position),
        } => (
            restated_column(new_name, data_type, options),
            position,
            Some(old_name.value.clone()),
        ),
        _ => return Ok(None),
    };
    let Ok(Statement::CreateTable(stored)) = parse_one_statement(stored_ddl, mode) else {
        return Err(ParseError::ExpectedCreateTable);
    };
    let mut table = stored;
    // The column being moved leaves the list before the place is counted,
    // which is how MySQL counts one: measured, `MODIFY n ... AFTER s` over
    // (id, n, s) leaves (id, s, n).
    let mut carried_columns = table
        .columns
        .iter()
        .map(|column| (column.name.value.clone(), column.name.value.clone()))
        .collect::<Vec<_>>();
    if let Some(moved) = &moved {
        let Some(at) = table
            .columns
            .iter()
            .position(|column| column.name.value.eq_ignore_ascii_case(moved))
        else {
            return Ok(Some(MySqlColumnPlacement::NoSuchColumn(moved.clone())));
        };
        if column_has_auto_increment(&table.columns[at]) {
            return unsupported("moving the column a table counts on");
        }
        if names_the_key(&table, moved) {
            return unsupported("moving the column a table's key is over");
        }
        if !column_def.name.value.eq_ignore_ascii_case(moved)
            && table.columns.iter().any(|column| {
                column
                    .name
                    .value
                    .eq_ignore_ascii_case(&column_def.name.value)
            })
        {
            return Ok(Some(MySqlColumnPlacement::DuplicateColumn(
                column_def.name.value,
            )));
        }
        table.columns.remove(at);
        let mut renamed = carried_columns.remove(at);
        column_def.name.value.clone_into(&mut renamed.0);
        carried_columns.push(renamed);
    }
    let at = match position {
        sqlparser::ast::MySQLColumnPosition::First => 0,
        sqlparser::ast::MySQLColumnPosition::After(named) => {
            let Some(at) = table
                .columns
                .iter()
                .position(|column| column.name.value.eq_ignore_ascii_case(&named.value))
            else {
                return Ok(Some(MySqlColumnPlacement::NoSuchColumn(
                    named.value.clone(),
                )));
            };
            // The last column's place is the one a column being added would
            // take anyway. A column being moved there is another matter: the
            // engine leaves it where it stands.
            if moved.is_none() && at + 1 == table.columns.len() {
                return Ok(Some(MySqlColumnPlacement::AlreadyAtTheEnd(
                    alter_without_its_column_position(alter_sql, mode)?,
                )));
            }
            at + 1
        }
    };
    table.columns.insert(at, column_def);
    Ok(Some(MySqlColumnPlacement::TableWrittenAgain(
        MySqlTableRewrite {
            create_sql: render_table_written_again(&table, mode)?,
            carried_columns,
        },
    )))
}

/// Reads an `ALTER TABLE` adding several columns, one or more of them with a
/// place — Laravel writes each `->after()` of one migration this way — and
/// answers the table it becomes.
///
/// Measured on MySQL 8.4.11: the clauses go in the order written, each placed
/// in the table the ones before it left, so `ADD a AFTER x, ADD b AFTER a`
/// leaves `x, a, b`, `ADD h AFTER y, ADD i AFTER y` leaves `y, i, h`, and a
/// clause naming no place puts its column last at its turn. `AFTER` a column
/// only a later clause adds is 1054.
///
/// Answers `None` for a statement doing anything else, or naming no place.
fn table_with_columns_added_in_place(
    stored_ddl: &str,
    operations: &[AlterTableOperation],
    mode: SessionSqlMode,
) -> Result<Option<MySqlColumnPlacement>, ParseError> {
    let additions = operations
        .iter()
        .map(|operation| match operation {
            AlterTableOperation::AddColumn {
                column_def,
                column_position,
                if_not_exists: false,
                ..
            } => Some((column_def, column_position.as_ref())),
            _ => None,
        })
        .collect::<Option<Vec<_>>>();
    let Some(additions) = additions else {
        return Ok(None);
    };
    if additions.iter().all(|(_, position)| position.is_none()) {
        return Ok(None);
    }
    let Ok(Statement::CreateTable(stored)) = parse_one_statement(stored_ddl, mode) else {
        return Err(ParseError::ExpectedCreateTable);
    };
    let mut table = stored;
    let carried_columns = table
        .columns
        .iter()
        .map(|column| (column.name.value.clone(), column.name.value.clone()))
        .collect::<Vec<_>>();
    for (column_def, position) in additions {
        let named = |name: &str| {
            table
                .columns
                .iter()
                .position(|column| column.name.value.eq_ignore_ascii_case(name))
        };
        if named(&column_def.name.value).is_some() {
            return Ok(Some(MySqlColumnPlacement::DuplicateColumn(
                column_def.name.value.clone(),
            )));
        }
        let at = match position {
            None => table.columns.len(),
            Some(sqlparser::ast::MySQLColumnPosition::First) => 0,
            Some(sqlparser::ast::MySQLColumnPosition::After(after)) => {
                let Some(at) = named(&after.value) else {
                    return Ok(Some(MySqlColumnPlacement::NoSuchColumn(
                        after.value.clone(),
                    )));
                };
                at + 1
            }
        };
        table.columns.insert(at, column_def.clone());
    }
    Ok(Some(MySqlColumnPlacement::TableWrittenAgain(
        MySqlTableRewrite {
            create_sql: render_table_written_again(&table, mode)?,
            carried_columns,
        },
    )))
}

/// The `CREATE TABLE` a stored table is written again as, once a statement
/// has changed it: a table that counts its own ids keeps the column it counts
/// on counting.
pub(crate) fn render_table_written_again(
    table: &CreateTable,
    mode: SessionSqlMode,
) -> Result<String, ParseError> {
    match table.columns.iter().position(column_has_auto_increment) {
        Some(ordinal) => render_auto_increment_mysql_ddl(table, ordinal, mode),
        None => checked_primary_key::render_mysql_create_table(table, mode),
    }
}

/// What an `ALTER TABLE ... ALTER COLUMN c SET DEFAULT` or `DROP DEFAULT`
/// means for the table it changes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MySqlColumnDefaultChange {
    /// The same change written as a `MODIFY COLUMN` restating each column
    /// whole with its new default, which the `ALTER TABLE` path already runs.
    Restated(String),
    /// The statement names a column the table has not got, which MySQL
    /// answers 1054 for — measured, `Unknown column 'nope' in 's1'`.
    NoSuchColumn(String),
}

/// Reads an `ALTER TABLE` whose every operation sets or drops a column's
/// default — Rails' `change_column_default` — against the table it changes,
/// and answers it as a `MODIFY COLUMN` of each column with its new default.
///
/// Measured on MySQL 8.4.11: `ALTER COLUMN a SET DEFAULT 5` and the shorter
/// `ALTER a SET DEFAULT 5` leave the column as it was but for the default,
/// which prints the way the same default written in a `CREATE TABLE` prints
/// and is checked the same way — a word naming no number on an `INT` is 1067,
/// a default on a `TEXT` is 1101 — and `DROP DEFAULT` on a `NOT NULL` column
/// leaves it printing `` `b` int NOT NULL ``.
///
/// Refused, where MySQL takes it: `DROP DEFAULT` on a column that may hold
/// NULL, which MySQL then prints with no `DEFAULT` at all — `` `a` int, `` —
/// a shape this has no way to keep, a nullable column with no default of its
/// own printing `DEFAULT NULL` here.
///
/// Answers `None` for any other statement, including one mixing these with
/// other operations, which is left to be refused where it is read.
pub fn alter_column_default_restated(
    stored_ddl: &str,
    alter_sql: &str,
    mode: SessionSqlMode,
) -> Result<Option<MySqlColumnDefaultChange>, ParseError> {
    let Ok(Statement::AlterTable(alter)) = parse_one_statement(alter_sql, mode) else {
        return Ok(None);
    };
    let changes =
        alter
            .operations
            .iter()
            .map(|operation| match operation {
                AlterTableOperation::AlterColumn {
                    column_name,
                    op:
                        op @ (AlterColumnOperation::SetDefault { .. }
                        | AlterColumnOperation::DropDefault),
                } => Some((column_name, op)),
                _ => None,
            })
            .collect::<Option<Vec<_>>>();
    let Some(changes) = changes.filter(|changes| !changes.is_empty()) else {
        return Ok(None);
    };
    let Ok(Statement::CreateTable(stored)) = parse_one_statement(stored_ddl, mode) else {
        return Err(ParseError::ExpectedCreateTable);
    };
    let mut restated = Vec::with_capacity(changes.len());
    for (column_name, op) in changes {
        let Some(column) = stored
            .columns
            .iter()
            .find(|column| column.name.value.eq_ignore_ascii_case(&column_name.value))
        else {
            return Ok(Some(MySqlColumnDefaultChange::NoSuchColumn(
                column_name.value.clone(),
            )));
        };
        let mut column = column.clone();
        column
            .options
            .retain(|option| !matches!(option.option, ColumnOption::Default(_)));
        match op {
            // Measured on MySQL 8.4.11: 1101 for a default on a `TEXT`, which
            // a `CREATE TABLE` here still takes.
            AlterColumnOperation::SetDefault { .. }
                if matches!(
                    column.data_type,
                    DataType::TinyText
                        | DataType::Text
                        | DataType::MediumText
                        | DataType::LongText
                        | DataType::TinyBlob
                        | DataType::Blob(_)
                        | DataType::MediumBlob
                        | DataType::LongBlob
                        | DataType::JSON
                ) =>
            {
                return unsupported("a default on a TEXT, BLOB or JSON column");
            }
            AlterColumnOperation::SetDefault { value } => column.options.push(ColumnOptionDef {
                name: None,
                option: ColumnOption::Default(value.clone()),
            }),
            AlterColumnOperation::DropDefault => {
                if !column
                    .options
                    .iter()
                    .any(|option| matches!(option.option, ColumnOption::NotNull))
                {
                    return unsupported("DROP DEFAULT on a column that may hold NULL");
                }
            }
            _ => unreachable!("only default changes are collected"),
        }
        restated.push(format!("MODIFY COLUMN {column}"));
    }
    Ok(Some(MySqlColumnDefaultChange::Restated(format!(
        "ALTER TABLE {} {}",
        render_mysql_object_name(&alter.name)?,
        restated.join(", ")
    ))))
}

/// Whether one column is the one the table's primary key is over.
fn names_the_key(table: &CreateTable, column: &str) -> bool {
    let inline = table.columns.iter().any(|declared| {
        declared.name.value.eq_ignore_ascii_case(column)
            && declared
                .options
                .iter()
                .any(|option| matches!(option.option, ColumnOption::PrimaryKey(_)))
    });
    let clause = table.constraints.iter().any(|constraint| {
        let TableConstraint::PrimaryKey(key) = constraint else {
            return false;
        };
        key.columns.iter().any(|named| {
            matches!(&named.column.expr, Expr::Identifier(name)
                if name.value.eq_ignore_ascii_case(column))
        })
    });
    inline || clause
}

/// The same `ALTER TABLE` with the words naming a place left off.
///
/// The words stand at the end of the statement, so the text is cut there. It
/// is cut rather than the statement written out again because a written value
/// — a column's own default — must keep the spelling it arrived with.
fn alter_without_its_column_position(
    sql: &str,
    mode: SessionSqlMode,
) -> Result<String, ParseError> {
    let dialect = SessionMySqlDialect::new(mode);
    let tokens = statement_reads::tokens_with_location(&dialect, sql)
        .map_err(|error| ParseError::Sqlparser(error.to_string()))?;
    let words = tokens
        .iter()
        .filter(|token| !matches!(token.token, Token::Whitespace(_) | Token::SemiColon))
        .collect::<Vec<_>>();
    let names_a_place = |at: usize| {
        matches!(&words[at].token, Token::Word(word)
            if word.quote_style.is_none()
                && (word.value.eq_ignore_ascii_case("FIRST")
                    || word.value.eq_ignore_ascii_case("AFTER")))
    };
    let at = match words.len() {
        0 | 1 => return unsupported("ALTER TABLE column position"),
        length if names_a_place(length - 1) => length - 1,
        length if length >= 2 && names_a_place(length - 2) => length - 2,
        _ => return unsupported("ALTER TABLE column position"),
    };
    let Some(offset) = byte_offset_of_location(sql, words[at].span.start) else {
        return unsupported("ALTER TABLE column position");
    };
    Ok(sql[..offset].trim_end().to_owned())
}

/// Leaves the `_utf8mb4` introducer off every word written with one.
///
/// Measured on MySQL 8.4.11: `email = _utf8mb4'ANN@X.COM'` finds the row
/// holding `ann@x.com`, as the word without it does — an introducer names the
/// character set the word is written in, this server speaks that one alone,
/// and the word keeps the same collation and the same readiness to take the
/// column's. Any other introducer stays, and is refused where the statement
/// is read.
fn without_utf8mb4_introducers(
    sql: &str,
    mode: SessionSqlMode,
) -> Result<std::borrow::Cow<'_, str>, ParseError> {
    if !sql.to_ascii_lowercase().contains("_utf8mb4") {
        return Ok(std::borrow::Cow::Borrowed(sql));
    }
    let tokens = statement_reads::tokens_with_location(&SessionMySqlDialect::new(mode), sql)
        .map_err(|error| ParseError::Sqlparser(error.to_string()))?;
    let mut cuts = Vec::new();
    for (at, token) in tokens.iter().enumerate() {
        let Token::Word(word) = &token.token else {
            continue;
        };
        if word.quote_style.is_some() || !word.value.eq_ignore_ascii_case("_utf8mb4") {
            continue;
        }
        let Some(introduced) = tokens[at + 1..]
            .iter()
            .find(|token| !matches!(token.token, Token::Whitespace(_)))
        else {
            continue;
        };
        if !matches!(
            introduced.token,
            Token::SingleQuotedString(_) | Token::DoubleQuotedString(_)
        ) {
            continue;
        }
        let (Some(start), Some(end)) = (
            byte_offset_of_location(sql, token.span.start),
            byte_offset_of_location(sql, introduced.span.start),
        ) else {
            return unsupported("SELECT character set introducer");
        };
        cuts.push(start..end);
    }
    if cuts.is_empty() {
        return Ok(std::borrow::Cow::Borrowed(sql));
    }
    let mut kept = String::with_capacity(sql.len());
    let mut from = 0;
    for cut in cuts {
        kept.push_str(&sql[from..cut.start]);
        from = cut.end;
    }
    kept.push_str(&sql[from..]);
    Ok(std::borrow::Cow::Owned(kept))
}

/// The byte offset one line-and-column location stands at.
fn byte_offset_of_location(sql: &str, location: sqlparser::tokenizer::Location) -> Option<usize> {
    if location.line == 0 || location.column == 0 {
        return None;
    }
    let (mut line, mut column) = (1, 1);
    for (offset, character) in sql.char_indices() {
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
    (line == location.line && column == location.column).then_some(sql.len())
}

/// Parses the deliberately narrow MySQL `AUTO_INCREMENT` table shape.
///
/// This is separate from [`parse_create_table`] while the frontend has no
/// allocator-backed execution path. It accepts exactly one signed integer
/// `AUTO_INCREMENT` column whose primary key makes it non-null.
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
    let tokens = statement_reads::tokens(&dialect, sql)
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
    // `AUTO_INCREMENT=<n>` after the columns says where the counter starts and
    // is not the column's own marker, so the `=` after it tells the two apart.
    let positions = tokens
        .iter()
        .enumerate()
        .filter_map(|(index, token)| {
            (is_unquoted_word(token, "AUTO_INCREMENT")
                && !matches!(tokens.get(index + 1), Some(Token::Eq)))
            .then_some(index)
        })
        .collect::<Vec<_>>();
    let [position] = positions.as_slice() else {
        return unsupported("exactly one AUTO_INCREMENT token");
    };
    let position = *position;
    if position < 2 || position + 1 >= tokens.len() {
        return unsupported("AUTO_INCREMENT token order");
    }
    // The column may declare the key itself, or the table may write it as a
    // clause after the columns — the spelling MySQL prints and every dumped
    // schema carries. Where it is written as a clause the words are moved onto
    // the column before the checks below run, so what follows the marker here
    // is the end of the column instead.
    let ends_the_definition = |index: usize| {
        matches!(tokens.get(index), Some(Token::Comma | Token::RParen))
            || tokens
                .get(index)
                .is_some_and(|token| is_unquoted_word(token, "COMMENT"))
    };
    // `NOT NULL` and `PRIMARY KEY` may follow the marker in either order —
    // Django writes `AUTO_INCREMENT NOT NULL PRIMARY KEY` — and which of them
    // the column carries, and how often, is checked on the parsed column.
    let mut after = position + 1;
    loop {
        let pair = |first: &str, second: &str| {
            after + 1 < tokens.len()
                && is_unquoted_word(tokens[after], first)
                && is_unquoted_word(tokens[after + 1], second)
        };
        if pair("PRIMARY", "KEY") || pair("NOT", "NULL") {
            after += 2;
        } else {
            break;
        }
    }
    if !ends_the_definition(after) {
        return unsupported("AUTO_INCREMENT token order; expected PRIMARY KEY");
    }
    Ok(())
}

/// Whether an `INSERT` offers the row of defaults — `INSERT INTO t () VALUES
/// ()`, which names no columns and writes no values.
fn writes_the_row_of_defaults(insert: &sqlparser::ast::Insert) -> bool {
    let Some(source) = insert.source.as_deref() else {
        return false;
    };
    let sqlparser::ast::SetExpr::Values(values) = source.body.as_ref() else {
        return false;
    };
    matches!(values.rows.as_slice(), [row] if row.is_empty())
}

fn is_unquoted_word(token: &Token, expected: &str) -> bool {
    matches!(
        token,
        Token::Word(word)
            if word.quote_style.is_none() && word.value.eq_ignore_ascii_case(expected)
    )
}

#[cfg(test)]
thread_local! {
    /// How many times this thread read a statement in [`parse_select`] rather
    /// than answering from the last read.
    pub(crate) static SELECT_READS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// Parses exactly one MySQL `SELECT` statement and translates the supported
/// semantics-preserving subset to SQLite SQL.
///
/// The server reads each statement more than once on its way to running it —
/// once to authorize it and again to prepare it — so the last statement this
/// thread read is kept with its answer, which follows from the text and the
/// mode alone.
pub fn parse_select(sql: &str, mode: SessionSqlMode) -> Result<TranslatedSelect, ParseError> {
    type LastRead = Option<(String, SessionSqlMode, Result<TranslatedSelect, ParseError>)>;
    thread_local! {
        static LAST_READ: std::cell::RefCell<LastRead> = const { std::cell::RefCell::new(None) };
    }
    if let Some(answer) = LAST_READ.with(|last| {
        last.borrow()
            .as_ref()
            .filter(|(read, read_mode, _)| read == sql && *read_mode == mode)
            .map(|(_, _, answer)| answer.clone())
    }) {
        return answer;
    }
    #[cfg(test)]
    SELECT_READS.with(|reads| reads.set(reads.get() + 1));
    let answer = parse_select_with_column_types(sql, mode, &[], &[], &[]);
    LAST_READ.with(|last| *last.borrow_mut() = Some((sql.to_owned(), mode, answer.clone())));
    answer
}

/// Parses a checked `SELECT`, told which columns hold text or a moment, what
/// columns the table has, and which columns declare `ENUM` or `SET` members.
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
    set_columns: &[(String, Vec<String>)],
    moment_columns: &[String],
) -> Result<TranslatedSelect, ParseError> {
    parse_select_knowing_decimal_columns(
        sql,
        mode,
        text_columns,
        table_columns,
        member_columns,
        set_columns,
        moment_columns,
        &[],
    )
}

#[allow(clippy::too_many_arguments)]
pub fn parse_select_knowing_decimal_columns(
    sql: &str,
    mode: SessionSqlMode,
    text_columns: &[String],
    table_columns: &[String],
    member_columns: &[(String, Vec<String>)],
    set_columns: &[(String, Vec<String>)],
    moment_columns: &[String],
    decimal_columns: &[(String, u32)],
) -> Result<TranslatedSelect, ParseError> {
    parse_select_knowing_numeric_columns(
        sql,
        mode,
        text_columns,
        table_columns,
        member_columns,
        set_columns,
        moment_columns,
        decimal_columns,
        &[],
        &[],
    )
}

#[allow(clippy::too_many_arguments)]
pub fn parse_select_knowing_numeric_columns(
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
) -> Result<TranslatedSelect, ParseError> {
    parse_select_knowing_json_columns(
        sql,
        mode,
        text_columns,
        table_columns,
        member_columns,
        set_columns,
        moment_columns,
        decimal_columns,
        integer_columns,
        real_columns,
        &[],
    )
}

#[allow(clippy::too_many_arguments)]
pub fn parse_select_knowing_json_columns(
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
) -> Result<TranslatedSelect, ParseError> {
    parse_select_inner(
        sql,
        mode,
        text_columns,
        table_columns,
        member_columns,
        set_columns,
        moment_columns,
        decimal_columns,
        integer_columns,
        real_columns,
        json_columns,
        false,
    )
}

/// Parses a checked `SELECT` over several tables, told the kinds of every
/// table's columns by name: which hold words, which whole numbers, and which
/// a `DECIMAL` or a `BIGINT UNSIGNED` keeps in a stored form of its own.
///
/// The frontend passes these only when each name is of one kind in every
/// table, so a column named with its table is read knowing its kind.
pub fn parse_select_knowing_the_kinds_of_joined_columns(
    sql: &str,
    mode: SessionSqlMode,
    text_columns: &[String],
    integer_columns: &[String],
    exact_columns: &[(String, u32)],
) -> Result<TranslatedSelect, ParseError> {
    parse_select_inner(
        sql,
        mode,
        text_columns,
        &[],
        &[],
        &[],
        &[],
        exact_columns,
        integer_columns,
        &[],
        &[],
        true,
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
    parse_select_inner(
        sql,
        mode,
        text_columns,
        table_columns,
        member_columns,
        &[],
        &[],
        &[],
        &[],
        &[],
        &[],
        false,
    )
}

#[allow(clippy::too_many_arguments)]
fn parse_select_inner(
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
    knows_the_kinds_of_joined_columns: bool,
) -> Result<TranslatedSelect, ParseError> {
    let sql = &*without_utf8mb4_introducers(sql, mode)?;
    let statement = parse_one_statement(sql, mode)?;
    let Statement::Query(mut query) = statement else {
        return Err(ParseError::ExpectedSelect);
    };
    translate::name_the_columns_grouped_by_place(&mut query)?;
    translate::leave_the_one_table_out(&mut query);
    let tokens = statement_reads::tokens(&SessionMySqlDialect::new(mode), sql)
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
        collation_sensitive_call_columns,
        collation_sensitive_joined_columns,
        json_reading_columns,
        source_table,
        source_tables,
        checked_comparisons,
        locks_rows,
        row_count_parameters,
        parameter_count,
        orders_a_bare_column,
        checks_type_sensitive_expression,
        renders_a_condition_without_column_types,
        orders_wildcard_ordinal,
        compares_a_placeholder,
        counts_distinct_column,
        tests_a_bare_column,
        compares_a_written_day,
        compares_a_written_number,
        compares_a_large_decimal_integer,
        checked_subquery_comparisons,
        concatenates_groups,
        calculates_found_rows,
        columns_the_keys_decide,
        bare_names_in_result_subqueries,
    } = translate_select_query(
        &query,
        sql,
        mode,
        text_columns,
        table_columns,
        member_columns,
        set_columns,
        moment_columns,
        decimal_columns,
        integer_columns,
        real_columns,
        json_columns,
        false,
        knows_the_kinds_of_joined_columns,
    )?;
    Ok(TranslatedSelect {
        collation_sensitive_call_columns,
        collation_sensitive_joined_columns,
        json_reading_columns,
        reads_table: !source_tables.is_empty(),
        orders_a_bare_column,
        checks_type_sensitive_expression,
        renders_a_condition_without_column_types,
        orders_wildcard_ordinal,
        compares_a_placeholder,
        counts_distinct_column,
        tests_a_bare_column,
        compares_a_written_day,
        compares_a_written_number,
        compares_a_large_decimal_integer,
        checked_subquery_comparisons,
        sqlite_sql,
        source_table,
        source_tables,
        static_result_metadata,
        checked_comparisons,
        locks_rows,
        row_count_parameters,
        parameter_count,
        concatenates_groups,
        calculates_found_rows,
        columns_the_keys_decide,
        bare_names_in_result_subqueries,
    })
}

/// Parses one checked MySQL `SELECT` into Turso's SQLite AST.
pub fn parse_select_ast(sql: &str, mode: SessionSqlMode) -> Result<Stmt, ParseError> {
    let translated = parse_select(sql, mode)?;
    translated.parse_ast()
}

/// Parses exactly one MySQL `INSERT`, `UPDATE`, or `DELETE` statement in the checked DML subset.
pub fn parse_dml(sql: &str, mode: SessionSqlMode) -> Result<TranslatedDml, ParseError> {
    parse_dml_rewriting_on_update(sql, mode, &[])
}

/// Parses one checked DML statement, told which of the table's columns an
/// `UPDATE` rewrites to the moment it runs at.
///
/// The engine has no such attribute, so what it means is written into the
/// statement here, and only the frontend can say which columns carry it.
pub fn parse_dml_rewriting_on_update(
    sql: &str,
    mode: SessionSqlMode,
    rewritten_on_update: &[(String, u8)],
) -> Result<TranslatedDml, ParseError> {
    parse_dml_knowing_decimal_columns(sql, mode, rewritten_on_update, &[])
}

pub fn parse_dml_knowing_decimal_columns(
    sql: &str,
    mode: SessionSqlMode,
    rewritten_on_update: &[(String, u8)],
    decimal_columns: &[(String, u32)],
) -> Result<TranslatedDml, ParseError> {
    translate_dml(
        sql,
        mode,
        SelectRenderContext::new(sql, mode, &[], &[], &[], &[], &[], rewritten_on_update)
            .knowing_decimal_columns(decimal_columns),
        decimal_columns,
    )
}

/// Parses one checked DML statement knowing its table's columns: which an
/// `UPDATE` rewrites to the moment it runs at, which hold a `DECIMAL` or a
/// whole number, and which hold words — a `?` compared with one of those is
/// compared under the column's collation, and binds a word.
pub fn parse_dml_knowing_column_types(
    sql: &str,
    mode: SessionSqlMode,
    rewritten_on_update: &[(String, u8)],
    decimal_columns: &[(String, u32)],
    integer_columns: &[String],
    text_columns: &[String],
) -> Result<TranslatedDml, ParseError> {
    translate_dml(
        sql,
        mode,
        SelectRenderContext::new(
            sql,
            mode,
            text_columns,
            &[],
            &[],
            &[],
            &[],
            rewritten_on_update,
        )
        .knowing_decimal_columns(decimal_columns)
        .knowing_the_integer_columns(integer_columns),
        decimal_columns,
    )
}

fn translate_dml(
    sql: &str,
    mode: SessionSqlMode,
    mut render_context: SelectRenderContext<'_>,
    decimal_columns: &[(String, u32)],
) -> Result<TranslatedDml, ParseError> {
    let unaliased = updated_table_alias::without_the_updated_tables_alias(sql, mode)?;
    let sql = unaliased.as_deref().unwrap_or(sql);
    let statement = parse_one_statement(sql, mode)?;
    let read_tables;
    let mut inherited_comparisons = Vec::new();
    let mut row_count_parameters = Vec::new();
    let mut json_cast_columns = None;
    let (sqlite_sql, checked_update, source_table) = match statement {
        Statement::Insert(insert) => {
            let rendered = translate_insert(&insert, sql, mode, decimal_columns, None)?;
            read_tables = rendered.read_tables;
            inherited_comparisons = rendered.checked_comparisons;
            row_count_parameters = rendered.row_count_parameters;
            if !rendered.json_cast_columns.is_empty() {
                let TableObject::TableName(table) = &insert.table else {
                    return unsupported("INSERT table source");
                };
                json_cast_columns = Some((
                    insert_name(table)?.as_str().to_owned(),
                    rendered.json_cast_columns,
                ));
            }
            // An INSERT ... SELECT compares against the table the SELECT reads,
            // not the one it writes, so that is the table the comparisons are
            // checked against.
            (rendered.sqlite_sql, None, rendered.compared_table)
        }
        Statement::Update(update) => {
            let (rendered, tables, checked) = translate_update(&update, &mut render_context)?;
            read_tables = tables;
            let table = checked.table_name().to_owned();
            if !render_context.json_cast_columns.is_empty() {
                json_cast_columns = Some((
                    table.clone(),
                    std::mem::take(&mut render_context.json_cast_columns),
                ));
            }
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
        collation_sensitive_call_columns: render_context.collation_sensitive_call_columns,
        json_reading_columns: render_context.json_reading_columns,
        compares_a_written_number: render_context.compares_a_written_number,
        row_count_parameters,
        copies_a_select_rendered_knowing_its_types: false,
        bound_arithmetic_operands: render_context.bound_arithmetic_operands,
        json_cast_columns,
        falls_back_in_a_set: render_context.falls_back_in_a_set,
    })
}

/// Why an `INSERT ... SELECT` is refused when its `SELECT` has to know its
/// columns' types to be rendered — an `ORDER BY` over a bare column, or a
/// comparison against a `?`. The frontend renders such a `SELECT` itself and
/// hands it to [`parse_insert_select_knowing_its_select`].
pub const INSERT_SELECT_NEEDING_COLUMN_TYPES: &str = "INSERT SELECT needing column types";

/// Parses an `INSERT INTO t (a, b) <SELECT>` whose `SELECT` the frontend has
/// rendered knowing its columns' types, from the text
/// [`insert_select_source_sql`] answered for the same statement.
pub fn parse_insert_select_knowing_its_select(
    sql: &str,
    mode: SessionSqlMode,
    select: &TranslatedSelect,
) -> Result<TranslatedDml, ParseError> {
    let Statement::Insert(insert) = parse_one_statement(sql, mode)? else {
        return Err(ParseError::ExpectedDml);
    };
    if insert
        .source
        .as_deref()
        .is_none_or(|source| matches!(source.body.as_ref(), SetExpr::Values(_)))
    {
        return unsupported("INSERT without a SELECT");
    }
    let rendered = translate_insert(&insert, sql, mode, &[], Some(select))?;
    Ok(TranslatedDml {
        sqlite_sql: rendered.sqlite_sql,
        checked_update: None,
        checked_comparisons: rendered.checked_comparisons,
        source_table: rendered.compared_table,
        read_tables: rendered.read_tables,
        checked_subquery_comparisons: Vec::new(),
        ordered_columns: Vec::new(),
        collation_sensitive_call_columns: select.collation_sensitive_call_columns.clone(),
        json_reading_columns: select.json_reading_columns.clone(),
        compares_a_written_number: false,
        row_count_parameters: rendered.row_count_parameters,
        copies_a_select_rendered_knowing_its_types: true,
        bound_arithmetic_operands: Vec::new(),
        json_cast_columns: None,
        falls_back_in_a_set: false,
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
/// literals or the readings of the clock an ordinary `INSERT` writes. The
/// allocator column is checked separately by
/// [`CheckedAutoIncrementInsert::bind_allocator_table`] because only the
/// frontend has the durable table definition.
pub fn parse_auto_increment_insert(
    sql: &str,
    mode: SessionSqlMode,
) -> Result<CheckedAutoIncrementInsert, ParseError> {
    statement_reads::counted_insert(sql, mode, statement_reads::CountedValues::Written, || {
        parse_checked_auto_increment_insert(sql, mode, is_written_insert_value)
    })
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
    statement_reads::counted_insert(sql, mode, statement_reads::CountedValues::Bound, || {
        parse_checked_auto_increment_insert(sql, mode, is_prepared_insert_value)
    })
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
    let table_name = insert_name(table)?;
    if insert.columns.is_empty() && !writes_the_row_of_defaults(insert) {
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
        if row.len() != columns.len() {
            return unsupported("INSERT VALUES column count");
        }
        if row.is_empty() && !columns.is_empty() {
            return unsupported("INSERT VALUES column count");
        }
    }
    let rowwise_conflicts = values.rows.len() > 1 && (insert.on.is_some() || insert.ignore);
    // A column written to itself — GORM's `ON DUPLICATE KEY UPDATE id = id`,
    // how it spells doing nothing on a collision — is left as it stood, so it
    // is not a column the clause changes.
    let upsert_columns = match &insert.on {
        Some(sqlparser::ast::OnInsert::DuplicateKeyUpdate(assignments)) => assignments
            .iter()
            .filter_map(|assignment| match &assignment.target {
                sqlparser::ast::AssignmentTarget::ColumnName(name) => insert_name(name)
                    .ok()
                    .map(|name| name.as_str().to_owned())
                    .filter(|column| {
                        !writes_the_column_to_itself(&assignment.value, column, &table_name)
                    }),
                _ => None,
            })
            .collect(),
        _ => Vec::new(),
    };
    let offered_columns = translate::offered_row_reads(insert)
        .into_iter()
        .map(|read| read.column)
        .collect();
    let mut normalized_insert = insert.clone();
    let mut mixed_default_columns = Vec::new();
    let mut ignored_null_columns = Vec::new();
    for (at, column) in columns.iter().enumerate() {
        let has_default = values
            .rows
            .iter()
            .any(|row| names_the_columns_default(&row[at], column.as_str()));
        let has_other = values
            .rows
            .iter()
            .any(|row| !names_the_columns_default(&row[at], column.as_str()));
        if has_default && has_other {
            mixed_default_columns.push(at);
        }
        if insert.ignore
            && values.rows.iter().any(
                |row| matches!(&row[at], Expr::Value(value) if matches!(value.value, Value::Null)),
            )
        {
            ignored_null_columns.push(at);
        }
    }
    if let Some(source) = normalized_insert.source.as_mut() {
        if let sqlparser::ast::SetExpr::Values(values) = source.body.as_mut() {
            for row in &mut values.rows {
                for at in &mixed_default_columns {
                    if names_the_columns_default(&row[*at], columns[*at].as_str()) {
                        row[*at] = Expr::Value(sqlparser::ast::Value::Null.into());
                    }
                }
                for at in &ignored_null_columns {
                    if matches!(&row[*at], Expr::Value(value) if matches!(value.value, Value::Null))
                    {
                        row[*at] = Expr::Value(Value::Number("0".to_string(), false).into());
                    }
                }
            }
        }
    }
    // A plain `INSERT` of several rows giving a column `DEFAULT` in some rows
    // only is written a row at a time too, each row leaving out the columns
    // it gives `DEFAULT` — measured on MySQL 8.4.11, Drizzle's `values
    // (default, 1, 'Hello', 'First post', ...), (default, 1, 'Draft', 'Not
    // yet', default, 0)` takes the column's default in the rows that ask for
    // it, the ids counting on from one statement's first, and a row that
    // fails leaves none of the others written.
    let rows_differ_in_their_defaults = values.rows.len() > 1
        && !mixed_default_columns.is_empty()
        && insert.on.is_none()
        && !insert.ignore;
    let normalized_values = match normalized_insert
        .source
        .as_deref()
        .map(|source| source.body.as_ref())
    {
        Some(sqlparser::ast::SetExpr::Values(values)) => values,
        _ => return unsupported("INSERT source"),
    };
    // A column given `DEFAULT` in every row is left out of the rendered
    // statement, so the allocator sees the list the engine will run.
    let names = columns.iter().map(TursoName::as_str).collect::<Vec<_>>();
    let defaulted = columns_given_their_default(&names, normalized_values)?;
    let mut reads_the_clock = false;
    for row in &normalized_values.rows {
        for (_, value) in row.iter().enumerate().filter(|(at, _)| !defaulted[*at]) {
            if is_clock_reading_value(value) {
                reads_the_clock = true;
            } else if !accepts_value(value) {
                return unsupported("INSERT VALUES expression");
            }
        }
    }
    let mut ordinal = 0;
    let source_values = values
        .rows
        .iter()
        .map(|row| {
            row.iter()
                .enumerate()
                .filter_map(|(at, value)| {
                    // A `?` a `CAST(? AS JSON)` binds is written into its column as
                    // the document it reads as, NULL where it binds NULL.
                    let parameter = translate::is_a_bare_placeholder(value)
                        || translate::json_cast_operand(value)
                            .is_some_and(translate::is_a_bare_placeholder);
                    let source = if parameter {
                        let at_parameter = ordinal;
                        ordinal += 1;
                        AutoIncrementSourceValue::Parameter(at_parameter)
                    } else {
                        AutoIncrementSourceValue::Written(written_insert_value(value, names[at]))
                    };
                    (!defaulted[at]).then_some(source)
                })
                .collect::<Vec<_>>()
        })
        .collect::<Vec<_>>();
    let still_written = |at: usize| at - defaulted[..at].iter().filter(|removed| **removed).count();
    let defaults_in_each_row = values
        .rows
        .iter()
        .map(|row| {
            mixed_default_columns
                .iter()
                .filter(|at| names_the_columns_default(&row[**at], columns[**at].as_str()))
                .map(|at| still_written(*at))
                .collect()
        })
        .collect();
    let mixed_default_columns = mixed_default_columns
        .into_iter()
        .map(still_written)
        .collect();
    let ignored_null_columns = ignored_null_columns
        .into_iter()
        .filter_map(|at| {
            (!defaulted[at])
                .then_some(at - defaulted[..at].iter().filter(|removed| **removed).count())
        })
        .collect();
    let columns = columns
        .into_iter()
        .enumerate()
        .filter(|(at, _)| !defaulted[*at])
        .map(|(_, column)| column)
        .collect::<Vec<_>>();
    // Every column defaulted renders as `DEFAULT VALUES`, which has no row of
    // its own. The allocator writes its number into one made at bind time,
    // where the column it owns is known — measured on 8.4.11, `INSERT INTO t
    // () VALUES ()`, `VALUES (DEFAULT, DEFAULT, DEFAULT)` and `(n) VALUES
    // (DEFAULT)` all write one row taking the next number, every other column
    // taking its own default.
    if columns.is_empty() && values.rows.len() != 1 {
        return unsupported("INSERT of several rows of defaults");
    }

    // Reuse the existing checked SQL normalizer only after the stricter shape
    // checks above. The executable path exposes the typed AST, not this SQL.
    let normalized = translate_insert(&normalized_insert, sql, mode, &[], None)?;
    let sqlite_statement = parse_normalized_dml(&normalized.sqlite_sql)?;
    let row_count = NonZeroUsize::new(values.rows.len()).ok_or(ParseError::Unsupported {
        feature: "INSERT without VALUES rows",
    })?;
    Ok(CheckedAutoIncrementInsert {
        table_name,
        columns,
        row_count,
        sqlite_statement,
        source_values,
        mixed_default_columns,
        defaults_in_each_row,
        ignored_null_columns,
        rowwise_conflicts,
        rows_differ_in_their_defaults,
        upserts: insert.on.is_some(),
        upsert_columns,
        offered_columns,
        reads_the_clock,
    })
}

/// Whether an upsert assignment writes `column` its own value, naming it bare
/// or qualified by the table written.
fn writes_the_column_to_itself(value: &Expr, column: &str, table: &TursoName) -> bool {
    match value {
        Expr::Identifier(ident) => ident.value.eq_ignore_ascii_case(column),
        Expr::CompoundIdentifier(parts) => {
            matches!(parts.as_slice(), [qualifier, ident]
                if qualifier.value.eq_ignore_ascii_case(table.as_str())
                    && ident.value.eq_ignore_ascii_case(column))
        }
        _ => false,
    }
}

/// What one row of an `INSERT` writes into a column.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckedInsertValue {
    /// A written whole number.
    SignedInteger(i64),
    /// A nonnegative whole number above the engine's signed width.
    UnsignedInteger(u64),
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
        .or_else(|| match value {
            Expr::Value(literal) => match &literal.value {
                Value::Number(number, false) => number
                    .parse::<u64>()
                    .ok()
                    .map(CheckedInsertValue::UnsignedInteger),
                _ => None,
            },
            _ => None,
        })
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

/// What the frontend holds one `INSERT ... ON DUPLICATE KEY UPDATE` to before
/// it runs, or `None` for any other statement.
pub fn parse_optional_upsert(
    sql: &str,
    mode: SessionSqlMode,
) -> Result<Option<CheckedUpsert>, ParseError> {
    match parse_one_statement(sql, mode)? {
        Statement::Insert(insert) => translate::checked_upsert(&insert),
        _ => Ok(None),
    }
}

/// One `INSERT ... ON DUPLICATE KEY UPDATE`: the table it writes, the columns
/// its clause assigns, and the columns the clause compares between the row
/// already there and the row offered, with `<=>`.
///
/// The engine compares the value offered as it was written where MySQL first
/// puts it into the column's type, so the frontend holds each comparison to a
/// column whose type and offered values make those two the same.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckedUpsert {
    pub table: String,
    pub assigned: Vec<String>,
    pub comparisons: Vec<OfferedRowComparison>,
    /// Whether some column is given `DEFAULT` in some rows and a value in
    /// others, which only a counted table writes, a row at a time.
    pub defaults_in_some_rows: bool,
}

/// One column an upsert compares between the two rows, and the values each
/// row of the statement offers for it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OfferedRowComparison {
    pub column: String,
    pub offered: Vec<OfferedValue>,
}

/// What a row of an `INSERT` offers for a column, as far as comparing it with
/// the column's stored value goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OfferedValue {
    Null,
    /// A word in quotes.
    Word,
    /// A written whole number, `TRUE` or `FALSE`.
    WholeNumber,
    /// A written number with a point.
    NumberWithAPoint,
    /// Anything else: a bound value, a call, the column's default.
    Other,
}

/// How many times one statement calls `VALUES(col)` in an `ON DUPLICATE KEY
/// UPDATE`, or 0 for a statement that is no such `INSERT`.
///
/// MySQL 8.0.20 deprecated the call in favour of a name on the offered row,
/// and measured on 8.4.11 it raises warning 1287 once for each call written,
/// however many rows the statement offers — when the statement is prepared,
/// and not again when it is executed.
pub fn count_offered_row_calls(sql: &str, mode: SessionSqlMode) -> usize {
    match parse_one_statement(sql, mode) {
        Ok(Statement::Insert(insert)) => translate::offered_row_calls(&insert),
        _ => 0,
    }
}

fn validate_auto_increment_insert_token_shape(
    sql: &str,
    mode: SessionSqlMode,
) -> Result<(), ParseError> {
    let dialect = SessionMySqlDialect::without_executable_comments(mode);
    let tokens = statement_reads::tokens(&dialect, sql)
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

fn is_written_insert_value(expr: &Expr) -> bool {
    is_direct_insert_literal(expr)
        || translate::json_cast_operand(expr).is_some_and(is_direct_insert_literal)
}

fn is_direct_insert_literal(expr: &Expr) -> bool {
    if let Some(bytes) = written_value::written_byte_string(expr) {
        return bytes.is_some();
    }
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
        || translate::is_a_bare_placeholder(expr)
        || translate::json_cast_operand(expr).is_some_and(|operand| {
            is_direct_insert_literal(operand) || translate::is_a_bare_placeholder(operand)
        })
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
        byte_strings: table
            .columns
            .iter()
            .map(|column| ByteStringColumn::of_declared_type(&column.data_type))
            .collect(),
        words: table
            .columns
            .iter()
            .map(|column| {
                matches!(
                    column.data_type,
                    DataType::Varchar(_)
                        | DataType::Char(_)
                        | DataType::Text
                        | DataType::TinyText
                        | DataType::MediumText
                        | DataType::LongText
                )
            })
            .collect(),
        datetimes: table
            .columns
            .iter()
            .map(|column| matches!(column.data_type, DataType::Datetime(_)))
            .collect(),
        timestamps: table
            .columns
            .iter()
            .map(|column| {
                matches!(
                    column.data_type,
                    DataType::Timestamp(_, sqlparser::ast::TimezoneInfo::None)
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
                    DataType::Time(_, sqlparser::ast::TimezoneInfo::None)
                )
            })
            .collect(),
        temporal_precisions: table
            .columns
            .iter()
            .map(|column| match column.data_type {
                DataType::Datetime(precision)
                | DataType::Timestamp(precision, sqlparser::ast::TimezoneInfo::None)
                | DataType::Time(precision, sqlparser::ast::TimezoneInfo::None) => {
                    precision.map_or(Some(0), |value| u8::try_from(value).ok())
                }
                _ => None,
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
        bits: table
            .columns
            .iter()
            .map(|column| matches!(column.data_type, DataType::Bit(_)))
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
        floats: table
            .columns
            .iter()
            .map(|column| {
                matches!(
                    column.data_type,
                    DataType::Float(sqlparser::ast::ExactNumberInfo::None)
                        | DataType::FloatUnsigned(sqlparser::ast::ExactNumberInfo::None)
                        | DataType::Float4
                )
            })
            .collect(),
        unsigned_reals: table
            .columns
            .iter()
            .map(|column| {
                matches!(
                    column.data_type,
                    DataType::DoubleUnsigned(sqlparser::ast::ExactNumberInfo::None)
                        | DataType::DoublePrecisionUnsigned
                        | DataType::RealUnsigned
                        | DataType::FloatUnsigned(sqlparser::ast::ExactNumberInfo::None)
                        | DataType::DecimalUnsigned(_)
                        | DataType::DecUnsigned(_)
                )
            })
            .collect(),
        three_byte_texts: {
            let table_holds_three_byte_characters = check_table_options(&table.table_options)
                .is_ok_and(|options| options.collation.character_set() == "utf8mb3");
            table
                .columns
                .iter()
                .map(|column| {
                    holds_three_byte_characters(column, table_holds_three_byte_characters)
                })
                .collect()
        },
    })
}

/// Whether a column holds words in `utf8mb3`: it names that character set or
/// one of its collations, or names neither and its table is `utf8mb3`.
fn holds_three_byte_characters(column: &ColumnDef, table_holds_them: bool) -> bool {
    if !a_column_of_words(&column.data_type) {
        return false;
    }
    let named = column
        .options
        .iter()
        .find_map(|option| match &option.option {
            ColumnOption::Collation(name) => Some(unqualified_name_is(
                name,
                &["utf8mb3_unicode_ci", "utf8_unicode_ci"],
            )),
            _ => None,
        })
        .or_else(|| {
            column
                .options
                .iter()
                .find_map(|option| match &option.option {
                    ColumnOption::CharacterSet(name) => {
                        Some(unqualified_name_is(name, &["utf8mb3", "utf8"]))
                    }
                    _ => None,
                })
        });
    named.unwrap_or(table_holds_them)
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
    let normalized = drop_key_spelled_as_drop_index(sql);
    let statement = parse_one_statement(normalized.as_deref().unwrap_or(sql), mode)?;
    let Statement::AlterTable(alter) = statement else {
        return Err(ParseError::ExpectedAlterTable);
    };
    if alter.if_exists
        || alter.only
        || alter.location.is_some()
        || alter.on_cluster.is_some()
        || alter.table_type.is_some()
        || alter.operations.is_empty()
    {
        return unsupported("ALTER TABLE option");
    }
    if alter.operations.len() > 1
        && alter.operations[..alter.operations.len() - 1]
            .iter()
            .any(|operation| matches!(operation, AlterTableOperation::RenameTable { .. }))
    {
        return unsupported("ALTER TABLE mixing RENAME TABLE with other operations");
    }
    if alter.operations.iter().any(is_index_operation)
        && alter
            .operations
            .iter()
            .any(|operation| matches!(operation, AlterTableOperation::RenameTable { .. }))
    {
        return unsupported("ALTER TABLE mixing RENAME TABLE with index operations");
    }
    let table_name = render_mysql_object_name(&alter.name)?;
    alter
        .operations
        .iter()
        .map(|operation| {
            if is_index_operation(operation) {
                checked_index_operation(operation)?;
                Ok(format!("ALTER TABLE {table_name} {operation}"))
            } else if let Some((unplaced, position)) = column_added_in_place(operation) {
                a_place_no_other_clause_moves(position, &alter.operations)?;
                translate_alter_table_operation(&table_name, &unplaced)?;
                let place = match position {
                    sqlparser::ast::MySQLColumnPosition::First => "FIRST".to_owned(),
                    sqlparser::ast::MySQLColumnPosition::After(after) => {
                        format!("AFTER {}", render_mysql_sqlparser_ident(after))
                    }
                };
                Ok(format!(
                    "{} {place}",
                    render_mysql_alter_table_operation(&table_name, &unplaced, mode)?
                ))
            } else {
                translate_alter_table_operation(&table_name, operation)?;
                render_mysql_alter_table_operation(&table_name, operation, mode)
            }
        })
        .collect()
}

/// An `ADD COLUMN` naming a place — sqlx's `ADD COLUMN slug ... AFTER title`
/// beside a `RENAME COLUMN` and a `MODIFY` — as the column it adds and the
/// place, which its own statement puts back once the column is rendered.
fn column_added_in_place(
    operation: &AlterTableOperation,
) -> Option<(AlterTableOperation, &sqlparser::ast::MySQLColumnPosition)> {
    let AlterTableOperation::AddColumn {
        column_keyword,
        if_not_exists: false,
        column_def,
        column_position: Some(position),
    } = operation
    else {
        return None;
    };
    Some((
        AlterTableOperation::AddColumn {
            column_keyword: *column_keyword,
            if_not_exists: false,
            column_def: column_def.clone(),
            column_position: None,
        },
        position,
    ))
}

/// Refuses a place naming a column another clause of the statement renames
/// or drops.
///
/// Each clause runs in turn here, each against the table the ones before it
/// left. MySQL renames and drops first and reads every place against what
/// is left: measured on 8.4.11, `ADD a AFTER b, RENAME COLUMN b TO bb` is
/// 1054 there, and so is `ADD z AFTER c, DROP COLUMN c`, where running the
/// clauses in turn would take both. A place naming a column no clause moves
/// comes out the same either way.
fn a_place_no_other_clause_moves(
    position: &sqlparser::ast::MySQLColumnPosition,
    operations: &[AlterTableOperation],
) -> Result<(), ParseError> {
    let sqlparser::ast::MySQLColumnPosition::After(after) = position else {
        return Ok(());
    };
    let names_it = |name: &Ident| name.value.eq_ignore_ascii_case(&after.value);
    let moved = operations.iter().any(|operation| match operation {
        AlterTableOperation::RenameColumn {
            old_column_name,
            new_column_name,
        } => names_it(old_column_name) || names_it(new_column_name),
        AlterTableOperation::ChangeColumn {
            old_name, new_name, ..
        } => names_it(old_name) || names_it(new_name),
        AlterTableOperation::DropColumn { column_names, .. } => column_names.iter().any(names_it),
        _ => false,
    });
    if moved {
        return unsupported("a column placed after one the same ALTER TABLE renames or drops");
    }
    Ok(())
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
    let dump_ddl = parse_optional_mysqldump_ddl(sql)?;
    let sql = dump_ddl.as_ref().map_or(sql, MySqlDumpDdl::normalized_sql);
    let statement = parse_one_statement(sql, mode)?;
    let Statement::CreateView(view) = statement else {
        return Err(ParseError::ExpectedCreateView);
    };
    let normalized = translate_create_view(&view, mode)?;
    parse_normalized_create_view(&normalized)
}

/// Parses exactly one checked MySQL `CREATE TRIGGER` statement into Turso's SQLite AST.
pub fn parse_create_trigger_ast(sql: &str, mode: SessionSqlMode) -> Result<Stmt, ParseError> {
    let dump_ddl = parse_optional_mysqldump_ddl(sql)?;
    let sql = dump_ddl.as_ref().map_or(sql, MySqlDumpDdl::normalized_sql);
    trigger_definition::engine_trigger(sql, mode)
}

/// Parses exactly one supported MySQL schema DDL statement into Turso's SQLite AST.
pub fn parse_schema_ddl_ast(sql: &str, mode: SessionSqlMode) -> Result<Stmt, ParseError> {
    let dump_ddl = parse_optional_mysqldump_ddl(sql)?;
    let sql = dump_ddl.as_ref().map_or(sql, MySqlDumpDdl::normalized_sql);
    reject_unsupported_mysql_string_escapes(sql, mode)?;
    let statement = match parse_one_statement(sql, mode) {
        Ok(statement) => statement,
        Err(error) => {
            return match trigger_definition::parse_trigger(sql, mode) {
                Ok(Some(_)) => trigger_definition::engine_trigger(sql, mode),
                _ => Err(error),
            }
        }
    };
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
            let normalized = translate_create_view(&view, mode)?;
            parse_normalized_create_view(&normalized)
        }
        Statement::CreateTrigger(_) => trigger_definition::engine_trigger(sql, mode),
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
    statement_reads::statement(sql, mode, || {
        let dialect = SessionMySqlDialect::new(mode);
        let tokens = statement_reads::tokens_with_location(&dialect, sql)
            .map_err(|error| ParseError::Sqlparser(ParserError::from(error).to_string()))?;
        let tokens = count_a_column_in_parentheses_as_the_column(
            spell_lock_in_share_mode_as_for_share(tokens),
        );
        let mut statements = Parser::new(&dialect)
            .with_tokens_with_locations(tokens)
            .parse_statements()
            .map_err(|error| ParseError::Sqlparser(error.to_string()))?;
        let actual = statements.len();
        match statements.pop() {
            Some(statement) if actual == 1 => Ok(statement),
            _ => Err(ParseError::ExpectedOneStatement { actual }),
        }
    })
}

/// `LOCK IN SHARE MODE` is MySQL's older spelling of `FOR SHARE`, and
/// sqlparser reads only the newer one.
///
/// Measured on MySQL 8.4.11, the two are read alike wherever a locking clause
/// may stand — after `LIMIT`, in a subquery, after a `UNION` — except that the
/// older spelling takes none of `NOWAIT`, `SKIP LOCKED` or `OF`. Those are
/// left spelled as written, so they stay the syntax error MySQL answers. Each
/// replacement keeps the place it was written at, so what is read out of the
/// statement's text by place still reads the same text.
fn spell_lock_in_share_mode_as_for_share(mut tokens: Vec<TokenWithSpan>) -> Vec<TokenWithSpan> {
    let words = tokens
        .iter()
        .enumerate()
        .filter(|(_, token)| !matches!(token.token, Token::Whitespace(_)))
        .map(|(at, _)| at)
        .collect::<Vec<_>>();
    let mut older_spellings = Vec::new();
    for (position, window) in words.windows(4).enumerate() {
        let [lock, in_, share, mode] = [window[0], window[1], window[2], window[3]];
        let spelled_the_older_way = is_unquoted_word(&tokens[lock].token, "LOCK")
            && is_unquoted_word(&tokens[in_].token, "IN")
            && is_unquoted_word(&tokens[share].token, "SHARE")
            && is_unquoted_word(&tokens[mode].token, "MODE");
        let followed_by_an_option = words.get(position + 4).is_some_and(|&next| {
            ["NOWAIT", "SKIP", "OF"]
                .iter()
                .any(|option| is_unquoted_word(&tokens[next].token, option))
        });
        if spelled_the_older_way && !followed_by_an_option {
            older_spellings.push((lock, in_, mode));
        }
    }
    for (lock, in_, mode) in older_spellings {
        tokens[lock].token = Token::make_keyword("FOR");
        tokens[in_].token = Token::Whitespace(Whitespace::Space);
        tokens[mode].token = Token::Whitespace(Whitespace::Space);
    }
    tokens
}

/// `COUNT(DISTINCT(col))` is `COUNT(DISTINCT col)`: GORM writes a distinct
/// count with the column in parentheses, and the parentheses around a column
/// change nothing it names.
///
/// Measured on MySQL 8.4.11, `COUNT(DISTINCT(user_id))` counts what
/// `COUNT(DISTINCT user_id)` counts and answers the same `LONGLONG` of 21,
/// named after the call as written — which is read out of the statement's
/// text by place, so the parentheses are taken out without moving anything.
/// `COUNT((user_id))` is taken the same way.
fn count_a_column_in_parentheses_as_the_column(
    mut tokens: Vec<TokenWithSpan>,
) -> Vec<TokenWithSpan> {
    let words = tokens
        .iter()
        .enumerate()
        .filter(|(_, token)| !matches!(token.token, Token::Whitespace(_)))
        .map(|(at, _)| at)
        .collect::<Vec<_>>();
    let is_name = |at: usize| matches!(tokens[at].token, Token::Word(_));
    let is = |at: usize, expected: &Token| tokens[at].token == *expected;
    let mut parentheses = Vec::new();
    for (position, &count) in words.iter().enumerate() {
        if !is_unquoted_word(&tokens[count].token, "COUNT") {
            continue;
        }
        let rest = match &words[position + 1..] {
            [open, distinct, rest @ ..]
                if is(*open, &Token::LParen)
                    && is_unquoted_word(&tokens[*distinct].token, "DISTINCT") =>
            {
                rest
            }
            [open, rest @ ..] if is(*open, &Token::LParen) => rest,
            _ => continue,
        };
        let column_length = match rest {
            [_, table, period, name, ..]
                if is_name(*table) && is(*period, &Token::Period) && is_name(*name) =>
            {
                3
            }
            [_, name, ..] if is_name(*name) => 1,
            _ => continue,
        };
        let (Some(&inner_open), Some(&inner_close), Some(&close)) = (
            rest.first(),
            rest.get(column_length + 1),
            rest.get(column_length + 2),
        ) else {
            continue;
        };
        if is(inner_open, &Token::LParen)
            && is(inner_close, &Token::RParen)
            && is(close, &Token::RParen)
        {
            parentheses.push((inner_open, inner_close));
        }
    }
    for (open, close) in parentheses {
        tokens[open].token = Token::Whitespace(Whitespace::Space);
        tokens[close].token = Token::Whitespace(Whitespace::Space);
    }
    tokens
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
    let reading = statement_reads::engine_reading(sql);
    let command = reading.first.map_err(ParseError::TursoParser)?;
    let Some(TursoCmd::Stmt(statement @ Stmt::Select(_))) = command else {
        return Err(ParseError::TursoParser(
            "normalized SELECT did not produce a SELECT AST".to_string(),
        ));
    };
    if reading.another_follows.map_err(ParseError::TursoParser)? {
        return Err(ParseError::ExpectedOneStatement { actual: 2 });
    }
    Ok(statement)
}

/// Reads one statement written the way the engine's own parser reads one.
///
/// This is for a statement the frontend writes itself — moving its own rows
/// from one table to another — rather than one a client wrote, which goes
/// through the MySQL reader.
pub fn parse_engine_statement(sql: &str) -> Result<Stmt, ParseError> {
    parse_normalized_dml(sql)
}

fn parse_normalized_dml(sql: &str) -> Result<Stmt, ParseError> {
    let reading = statement_reads::engine_reading(sql);
    let command = reading.first.map_err(ParseError::TursoParser)?;
    let Some(TursoCmd::Stmt(
        statement @ (Stmt::Insert { .. } | Stmt::Update(_) | Stmt::Delete { .. }),
    )) = command
    else {
        return Err(ParseError::TursoParser(
            "normalized DML did not produce an INSERT, UPDATE, or DELETE AST".to_string(),
        ));
    };
    if reading.another_follows.map_err(ParseError::TursoParser)? {
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
    // Measured on MySQL 8.4.11: a table with no counted column takes
    // `AUTO_INCREMENT=<n>` and prints nothing back for it, so there is nothing
    // here to keep.
    reject_attributes_and_check_options(table)?;
    reject_json_defaults_and_keys(table)?;
    let mut table = table.clone();
    normalize_primary_key_columns(&mut table)?;
    for column in &table.columns {
        reject_attributes_this_rendering_would_lose(column)?;
    }
    reject_a_key_over_a_column_that_may_be_null(&table)?;
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

pub(crate) fn reject_json_defaults_and_keys(table: &CreateTable) -> Result<(), ParseError> {
    let json_columns = table
        .columns
        .iter()
        .filter(|column| matches!(column.data_type, DataType::JSON))
        .collect::<Vec<_>>();
    if json_columns.is_empty() {
        return Ok(());
    }
    for column in &json_columns {
        for option in &column.options {
            match &option.option {
                ColumnOption::Default(expr) => reject_json_default(expr)?,
                ColumnOption::PrimaryKey(_) | ColumnOption::Unique(_) => {
                    return Err(ParseError::JsonIndex);
                }
                _ => {}
            }
        }
    }
    for constraint in &table.constraints {
        let columns = match constraint {
            TableConstraint::PrimaryKey(key) => &key.columns,
            TableConstraint::Unique(key) => &key.columns,
            TableConstraint::Index(key) => &key.columns,
            _ => continue,
        };
        if columns.iter().any(|indexed| {
            let Expr::Identifier(name) = &indexed.column.expr else {
                return false;
            };
            json_columns
                .iter()
                .any(|column| column.name.value.eq_ignore_ascii_case(&name.value))
        }) {
            return Err(ParseError::JsonIndex);
        }
    }
    Ok(())
}

fn reject_json_default(expr: &Expr) -> Result<(), ParseError> {
    match expr {
        Expr::Value(value) if matches!(value.value, Value::Null) => Ok(()),
        Expr::Value(_) => Err(ParseError::JsonLiteralDefault),
        _ => unsupported("JSON expression DEFAULT"),
    }
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
    reject_json_defaults_and_keys(table)?;
    let starts_the_counter_at = reject_attributes_and_check_options(table)?.starts_the_counter_at;
    let table = &table_with_its_key_written_inline(table.clone());
    // A foreign key and a `CHECK` are the table-level constraints a counted
    // table takes, the same as an ordinary one: both renderings below write
    // whatever constraints the table carries, and a table with a counted id is
    // exactly the table a child row points at.
    if table.constraints.iter().any(|constraint| {
        !matches!(
            constraint,
            TableConstraint::ForeignKey(_) | TableConstraint::Check(_)
        )
    }) {
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
    // Measured on MySQL 8.4.11: a `CHECK` naming the counted column is 3818,
    // the number being given only once the check has run.
    let counted_name = &table.columns[allocator_column_ordinal].name.value;
    if table.constraints.iter().any(|constraint| {
        matches!(constraint, TableConstraint::Check(check)
            if check_may_name_the_column(&check.expr, counted_name))
    }) {
        return unsupported("CHECK naming the AUTO_INCREMENT column");
    }
    let allocator_column_type = match table.columns[allocator_column_ordinal].data_type {
        DataType::IntUnsigned(_) | DataType::IntegerUnsigned(_) => MySqlIntegerType::IntUnsigned,
        DataType::BigInt(_) => MySqlIntegerType::BigInt,
        DataType::BigIntUnsigned(_) => MySqlIntegerType::BigIntUnsigned,
        _ => MySqlIntegerType::Int,
    };
    // MySQL takes a start past what the column can hold and answers 1467 for
    // the first row instead — measured, `AUTO_INCREMENT=99999999999` on an
    // `INT` creates the table and the `INSERT` fails. A start no row could ever
    // be given is refused here rather than kept until then.
    let (_, ceiling) = allocator_column_type.bounds();
    if starts_the_counter_at.is_some_and(|start| start > ceiling as u64) {
        return unsupported("AUTO_INCREMENT start past what the column holds");
    }

    let sqlite_columns = table
        .columns
        .iter()
        .enumerate()
        .map(|(ordinal, column)| {
            if ordinal == allocator_column_ordinal {
                let key_type = if allocator_column_type == MySqlIntegerType::BigIntUnsigned {
                    "mysql_uint64 NOT NULL PRIMARY KEY"
                } else {
                    "INTEGER PRIMARY KEY"
                };
                Ok(format!("{} {key_type}", render_ident(&column.name)))
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
        allocator_column_type,
        allocator_column_written_type: written_auto_increment_type(
            &table.columns[allocator_column_ordinal],
        )?,
        starts_the_counter_at,
        normalized_mysql_ddl: render_auto_increment_mysql_ddl(
            table,
            allocator_column_ordinal,
            mode,
        )?,
        sqlite_statement,
    })
}

/// Reports whether a `CHECK` expression may read one column: an expression
/// of any form but the plain ones a `CHECK` here is written with is taken to.
fn check_may_name_the_column(expr: &Expr, column: &str) -> bool {
    match expr {
        Expr::Identifier(name) => name.value.eq_ignore_ascii_case(column),
        Expr::CompoundIdentifier(parts) => parts
            .last()
            .is_none_or(|name| name.value.eq_ignore_ascii_case(column)),
        Expr::Value(_) => false,
        Expr::Nested(inner) | Expr::IsNull(inner) | Expr::IsNotNull(inner) => {
            check_may_name_the_column(inner, column)
        }
        Expr::UnaryOp { expr, .. } => check_may_name_the_column(expr, column),
        Expr::BinaryOp { left, right, .. } => {
            check_may_name_the_column(left, column) || check_may_name_the_column(right, column)
        }
        _ => true,
    }
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
    // BIGINT UNSIGNED needs its own stored key: a rowid alias ends at i64::MAX.
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
            | DataType::BigIntUnsigned(_)
    ) {
        return unsupported("AUTO_INCREMENT column type");
    }
    // A comment is the one attribute that may stand beside the three, and a
    // dumped schema puts one on this very column. Where it stands among them
    // depends on whether the key was written inline or as a clause, so it is
    // taken out and the rest checked as before.
    let rest = column
        .options
        .iter()
        .filter(|option| !matches!(option.option, ColumnOption::Comment(_)))
        .collect::<Vec<_>>();
    if column
        .options
        .iter()
        .filter(|option| matches!(option.option, ColumnOption::Comment(_)))
        .any(|option| option.name.is_some())
    {
        return unsupported("named column COMMENT");
    }
    // MySQL takes the three in any order — Django writes `bigint
    // AUTO_INCREMENT NOT NULL PRIMARY KEY` — so each is counted rather than
    // read at a place.
    let (mut not_null, mut auto_increment, mut primary_key) = (0, 0, 0);
    for option in rest {
        if option.name.is_some() {
            return unsupported("AUTO_INCREMENT column attributes");
        }
        match &option.option {
            ColumnOption::NotNull => not_null += 1,
            ColumnOption::DialectSpecific(tokens) if is_auto_increment_tokens(tokens) => {
                auto_increment += 1;
            }
            primary if is_plain_inline_primary_key(primary) => primary_key += 1,
            _ => return unsupported("AUTO_INCREMENT column attributes"),
        }
    }
    if not_null > 1 || auto_increment != 1 || primary_key != 1 {
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
    let options = check_table_options(&table.table_options)?.kept().written();
    // `IF NOT EXISTS` says what to do about a table that is already there, not
    // what the table is, and MySQL never prints it back, so it is left out of
    // the stored DDL.
    Ok(format!(
        "CREATE {temporary}TABLE {} ({}){options}",
        render_mysql_object_name(&table.name)?,
        definitions.join(", ")
    ))
}

fn render_auto_increment_mysql_column(column: &ColumnDef) -> Result<String, ParseError> {
    let data_type = written_auto_increment_type(column)?;
    let name = render_mysql_sqlparser_ident(&column.name);
    let comment = written_comment(column);
    Ok(format!(
        "{name} {data_type} NOT NULL AUTO_INCREMENT PRIMARY KEY{comment}"
    ))
}

/// The type the stored DDL writes a counted column with.
///
/// Signed counted columns use a rowid alias; the unsigned BIGINT keeps its
/// declared type on a separately stored primary key.
fn written_auto_increment_type(column: &ColumnDef) -> Result<&'static str, ParseError> {
    match column.data_type {
        DataType::Int(_) => Ok("INT"),
        DataType::Integer(_) => Ok("INTEGER"),
        DataType::IntUnsigned(_) => Ok("INT UNSIGNED"),
        DataType::IntegerUnsigned(_) => Ok("INTEGER UNSIGNED"),
        DataType::BigInt(_) => Ok("BIGINT"),
        DataType::BigIntUnsigned(_) => Ok("BIGINT UNSIGNED"),
        _ => unsupported("AUTO_INCREMENT column type"),
    }
}

/// The ` COMMENT '<text>'` a column carries, ready to append, or nothing where
/// it carries none.
///
/// Measured on MySQL 8.4.11: an empty comment is dropped entirely, and the
/// words are printed last, after `AUTO_INCREMENT` and after
/// `ON UPDATE CURRENT_TIMESTAMP`.
pub(crate) fn written_comment(column: &ColumnDef) -> String {
    match column_comment(column) {
        Some(text) if !text.is_empty() => format!(" COMMENT {}", quoted_mysql_text(text)),
        _ => String::new(),
    }
}

/// The text a column's `COMMENT` holds, as the statement wrote it.
pub(crate) fn column_comment(column: &ColumnDef) -> Option<&str> {
    column
        .options
        .iter()
        .find_map(|option| match &option.option {
            ColumnOption::Comment(text) if option.name.is_none() => Some(text.as_str()),
            _ => None,
        })
}

/// One piece of text quoted the way MySQL's own `SHOW CREATE TABLE` writes it.
///
/// Measured on 8.4.11 by reading the printed bytes: a quote is doubled, a
/// backslash is written twice, a newline becomes `\n`, a carriage return `\r`
/// and a zero byte `\0`. Everything else is printed as it stands — a tab, a
/// double quote, a `%` and a `_` each come back raw, and 0x1A does too, which
/// is why `\Z` is not written here.
pub fn quoted_mysql_text(text: &str) -> String {
    let mut quoted = String::with_capacity(text.len() + 2);
    quoted.push('\'');
    for character in text.chars() {
        match character {
            '\'' => quoted.push_str("''"),
            '\\' => quoted.push_str("\\\\"),
            '\n' => quoted.push_str("\\n"),
            '\r' => quoted.push_str("\\r"),
            '\0' => quoted.push_str("\\0"),
            _ => quoted.push(character),
        }
    }
    quoted.push('\'');
    quoted
}

/// Whether a column says an `UPDATE` that changes its row rewrites it to the
/// moment the statement runs at.
pub(crate) fn column_is_rewritten_on_update(column: &ColumnDef) -> bool {
    column.options.iter().any(|option| {
        option.name.is_none()
            && matches!(&option.option, ColumnOption::OnUpdate(expr)
                if names_the_moment_a_statement_runs_at(expr))
    })
}

fn render_mysql_checked_column(
    column: &ColumnDef,
    mode: SessionSqlMode,
) -> Result<String, ParseError> {
    // The engine's definition carries neither `ON UPDATE` nor a comment, so
    // the round trip below would lose both. They are put back here, where the
    // source column can still be seen, in the order MySQL prints them.
    let rest = render_mysql_checked_column_without_its_own_words(column, mode)?;
    let on_update = if column_is_rewritten_on_update(column) {
        match declared_fraction_digits(&column.data_type) {
            0 => ON_UPDATE_MOMENT.to_owned(),
            digits => format!("{ON_UPDATE_MOMENT}({digits})"),
        }
    } else {
        String::new()
    };
    Ok(format!("{rest}{on_update}{}", written_comment(column)))
}

fn render_mysql_checked_column_without_its_own_words(
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

fn translate_create_view(view: &CreateView, mode: SessionSqlMode) -> Result<String, ParseError> {
    refuse_view_options(view)?;
    let view_name = render_unqualified_name(&view.name)?;
    // A `SELECT` with a condition is translated the way a `SELECT` written on
    // its own is, which is what holds each comparison to MySQL's rules; the
    // frontend holds its values to the columns' types before the view is made.
    let query = if view_definition::kept_as_mysql_prints_it(&view.query) {
        let body = view_definition::view_body(
            &view.query,
            mode,
            None,
            view_definition::Written::ForTheTranslator,
        )?;
        parse_select(&body, mode)?.as_sql().to_owned()
    } else {
        render_simple_view_query(&view.query)?
    };
    Ok(format!("CREATE VIEW {view_name} AS {query}"))
}

pub(crate) fn refuse_view_options(view: &CreateView) -> Result<(), ParseError> {
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
    Ok(())
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
            reject_attributes_this_rendering_would_lose(column_def)?;
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
            let restated = restated_column(col_name, data_type, options);
            reject_attributes_this_rendering_would_lose(&restated)?;
            Ok(format!(
                "ALTER TABLE {table_name} ALTER COLUMN {} TO {}",
                render_ident(col_name),
                render_column(&restated)?
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
            let restated = restated_column(new_name, data_type, options);
            reject_attributes_this_rendering_would_lose(&restated)?;
            Ok(format!(
                "ALTER TABLE {table_name} ALTER COLUMN {} TO {}",
                render_ident(old_name),
                render_column(&restated)?
            ))
        }
        _ => unsupported("ALTER TABLE operation"),
    }
}

/// Refuses a column attribute the engine's own definition cannot carry, on
/// the paths that read the stored MySQL DDL back out of that definition.
///
/// `ON UPDATE CURRENT_TIMESTAMP` lives in the stored MySQL DDL alone. The
/// keyed `CREATE TABLE` paths render it from the statement as written and keep
/// it; a table with no key of its own, and every `ALTER TABLE`, rebuild the
/// stored definition from the engine's, where the words are not. Dropping them
/// would print a table the statement did not ask for, so the statement is
/// refused instead.
fn reject_attributes_this_rendering_would_lose(column: &ColumnDef) -> Result<(), ParseError> {
    if column_is_rewritten_on_update(column) {
        return unsupported(
            "ON UPDATE CURRENT_TIMESTAMP on a table rendered from the engine's own definition",
        );
    }
    if column_comment(column).is_some() {
        return unsupported("column COMMENT on a table rendered from the engine's own definition");
    }
    Ok(())
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

/// MySQL makes every column of a compound primary key non-null even when the
/// declaration omits that option. The engine needs it written on each column
/// to enforce the same rule and print the same table later.
fn normalize_primary_key_columns(table: &mut CreateTable) -> Result<(), ParseError> {
    let keys = table
        .constraints
        .iter()
        .filter_map(|constraint| match constraint {
            TableConstraint::PrimaryKey(key) if key.columns.len() > 1 => Some(key),
            _ => None,
        })
        .collect::<Vec<_>>();
    for key in keys {
        for part in &key.columns {
            let Expr::Identifier(named) = &part.column.expr else {
                return unsupported("PRIMARY KEY column expression");
            };
            let Some(column) = table
                .columns
                .iter_mut()
                .find(|column| column.name.value.eq_ignore_ascii_case(&named.value))
            else {
                return unsupported("PRIMARY KEY naming a column the table does not have");
            };
            if column.options.iter().any(|option| {
                matches!(option.option, ColumnOption::Null)
                    || matches!(&option.option, ColumnOption::Default(Expr::Value(value))
                        if matches!(value.value, Value::Null))
            }) {
                return unsupported("NULL PRIMARY KEY");
            }
            if !column
                .options
                .iter()
                .any(|option| matches!(option.option, ColumnOption::NotNull))
            {
                column.options.insert(
                    0,
                    ColumnOptionDef {
                        name: None,
                        option: ColumnOption::NotNull,
                    },
                );
            }
        }
    }
    Ok(())
}

/// Refuses a single table-level `PRIMARY KEY (a)` naming a column the
/// statement did not declare `NOT NULL`.
///
/// MySQL makes every column of a key `NOT NULL` whether the statement said so
/// or not — measured on 8.4.11, a nullable column named by one prints back as
/// `NOT NULL` — and the engine leaves it as it was declared. Every schema a
/// migration tool writes declares them, so the shape that would differ is
/// refused rather than answered differently.
fn reject_a_key_over_a_column_that_may_be_null(table: &CreateTable) -> Result<(), ParseError> {
    for constraint in &table.constraints {
        let TableConstraint::PrimaryKey(key) = constraint else {
            continue;
        };
        for column in &key.columns {
            let Expr::Identifier(named) = &column.column.expr else {
                return unsupported("PRIMARY KEY column expression");
            };
            let Some(declared) = table
                .columns
                .iter()
                .find(|column| column.name.value.eq_ignore_ascii_case(&named.value))
            else {
                return unsupported("PRIMARY KEY naming a column the table does not have");
            };
            if !declared
                .options
                .iter()
                .any(|option| matches!(option.option, ColumnOption::NotNull))
            {
                return unsupported("PRIMARY KEY over a column that may be null");
            }
        }
    }
    Ok(())
}

/// Refuses every table attribute this does not answer, and every table option
/// but the ones naming what a table is written back as anyway.
pub(crate) fn reject_attributes_and_check_options(
    table: &CreateTable,
) -> Result<CheckedTableOptions, ParseError> {
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
/// `COLLATE=utf8mb4_unicode_ci` is taken as well, and is what the table's text
/// columns are compared under unless they name their own.
///
/// Anything else is a claim about storage, ordering or case this cannot keep:
/// measured, `COLLATE=utf8mb4_bin`, `DEFAULT CHARSET=latin1` and
/// `ROW_FORMAT=DYNAMIC` are each printed back, so each is refused rather than
/// quietly dropped.
pub(crate) fn check_table_options(
    options: &CreateTableOptions,
) -> Result<CheckedTableOptions, ParseError> {
    let options = match options {
        CreateTableOptions::None => return Ok(CheckedTableOptions::default()),
        CreateTableOptions::Plain(options) => options,
        CreateTableOptions::With(_)
        | CreateTableOptions::Options(_)
        | CreateTableOptions::TableProperties(_) => return unsupported("CREATE TABLE option"),
    };
    let mut written = Vec::with_capacity(options.len());
    let mut checked = CheckedTableOptions::default();
    let mut character_set = None;
    for option in options {
        let named = match option {
            SqlOption::KeyValue { key, value }
                if key.value.eq_ignore_ascii_case("AUTO_INCREMENT") =>
            {
                let start = written_counter_start(value)?;
                checked.starts_the_counter_at = (start > 1).then_some(start);
                "AUTO_INCREMENT"
            }
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
                // Measured on MySQL 8.4.11: `utf8` is read as `utf8mb3`.
                character_set = Some(if written_word_is(value, "utf8mb4") {
                    "utf8mb4"
                } else if written_word_is(value, "utf8mb3") || written_word_is(value, "utf8") {
                    "utf8mb3"
                } else {
                    return unsupported("CREATE TABLE character set");
                });
                "CHARACTER SET"
            }
            SqlOption::KeyValue { key, value } if names_a_collation(key) => {
                let Some(collation) = written_word(value)
                    .as_deref()
                    .and_then(MySqlTableCollation::from_name)
                else {
                    return unsupported("CREATE TABLE collation");
                };
                checked.collation = collation;
                "COLLATE"
            }
            // Measured on MySQL 8.4.11: the comment is printed last, after
            // the collation, and an empty one is not printed at all.
            SqlOption::Comment(CommentDef::WithEq(comment) | CommentDef::WithoutEq(comment)) => {
                checked.comment = checked_table_comment(comment)?;
                "COMMENT"
            }
            _ => return unsupported("CREATE TABLE option"),
        };
        if written.contains(&named) {
            return unsupported("repeated CREATE TABLE option");
        }
        written.push(named);
    }
    // A collation names its character set, so naming it alone is enough. A
    // character set named alone takes its own default collation, which for
    // `utf8mb3` is `utf8mb3_general_ci`, a comparison this server does not
    // have; and a collation of another character set is 1253 in MySQL.
    if let Some(character_set) = character_set {
        let collation_named = written.contains(&"COLLATE");
        if character_set != checked.collation.character_set()
            || (character_set == "utf8mb3" && !collation_named)
        {
            return unsupported("CREATE TABLE character set beside another collation");
        }
    }
    Ok(checked)
}

/// What a table's options say, past the ones that say nothing.
#[derive(Debug, Default)]
pub(crate) struct CheckedTableOptions {
    pub(crate) starts_the_counter_at: Option<u64>,
    pub(crate) collation: MySqlTableCollation,
    pub(crate) comment: Option<String>,
}

impl CheckedTableOptions {
    /// The options the stored `CREATE TABLE` of this table ends with.
    pub(crate) fn kept(&self) -> MySqlTableOptions {
        MySqlTableOptions {
            collation: self.collation,
            comment: self.comment.clone(),
        }
    }
}

/// A table comment as it is kept, `None` for an empty one.
///
/// Measured on MySQL 8.4.11: an empty comment is not printed and reads back
/// as the empty string, the same as a table given none. MySQL answers 1628 for
/// a comment longer than 2048 characters, which is refused here.
pub(crate) fn checked_table_comment(comment: &str) -> Result<Option<String>, ParseError> {
    if comment.chars().count() > 2048 {
        return unsupported("table COMMENT longer than 2048 characters");
    }
    Ok((!comment.is_empty()).then(|| comment.to_owned()))
}

/// The number a `AUTO_INCREMENT=<n>` table option names.
///
/// Measured on MySQL 8.4.11: the value is a plain whole number and nothing
/// else — `AUTO_INCREMENT=-5` and `AUTO_INCREMENT='7'` are both 1064 — and 0
/// and 1 both leave the counter where it starts, which is why they read as no
/// start at all. A fraction is taken and rounded down, which is refused here
/// rather than rounded differently.
fn written_counter_start(value: &Expr) -> Result<u64, ParseError> {
    let Expr::Value(written) = value else {
        return unsupported("CREATE TABLE AUTO_INCREMENT value");
    };
    let Value::Number(digits, false) = &written.value else {
        return unsupported("CREATE TABLE AUTO_INCREMENT value");
    };
    let Ok(start) = digits.parse::<u64>() else {
        return unsupported("CREATE TABLE AUTO_INCREMENT value");
    };
    Ok(start)
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
    written_word(value).is_some_and(|written| written.eq_ignore_ascii_case(expected))
}

/// The one word an option's value is written as, quoted or not.
fn written_word(value: &Expr) -> Option<String> {
    match value {
        Expr::Identifier(name) => Some(name.value.clone()),
        Expr::Value(value) => match &value.value {
            Value::SingleQuotedString(written) | Value::DoubleQuotedString(written) => {
                Some(written.clone())
            }
            _ => None,
        },
        _ => None,
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
        // Narrow unsigned integers fit the engine's i64 storage. BIGINT
        // UNSIGNED uses a MySQL-only encoded type to retain all 64 bits.
        DataType::TinyIntUnsigned(_) => "TINYINT UNSIGNED".to_owned(),
        DataType::SmallIntUnsigned(_) => "SMALLINT UNSIGNED".to_owned(),
        DataType::MediumIntUnsigned(_) => "MEDIUMINT UNSIGNED".to_owned(),
        DataType::IntUnsigned(_) => "INT UNSIGNED".to_owned(),
        DataType::IntegerUnsigned(_) => "INTEGER UNSIGNED".to_owned(),
        DataType::BigIntUnsigned(_) => "mysql_uint64".to_owned(),
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
        DataType::Binary(width) => format!("BINARY({})", declared_padded_width(*width)?),
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
        // MySQL's other spellings of the same two types, which a column keeps
        // nothing of: measured on 8.4.11, `DOUBLE PRECISION`, `REAL` and
        // `FLOAT8` print `double`, `DOUBLE PRECISION UNSIGNED` and `REAL
        // UNSIGNED` print `double unsigned`, and `FLOAT4` prints `float`.
        // `REAL` is a `DOUBLE` because the `REAL_AS_FLOAT` mode is off, which
        // it is in MySQL's default and in the only mode this server runs in.
        // Django writes `double precision` for every `FloatField`.
        DataType::DoublePrecision | DataType::Real | DataType::Float8 => "DOUBLE".to_owned(),
        DataType::DoublePrecisionUnsigned | DataType::RealUnsigned => "DOUBLE UNSIGNED".to_owned(),
        DataType::Float4 => "FLOAT".to_owned(),
        // Hibernate maps a Java `Boolean` to `bit`, which MySQL reads as
        // `bit(1)`: measured on 8.4.11, both print `bit(1)`. The engine holds
        // the one bit as the integer 0 or 1, and it crosses the wire as the
        // byte MySQL sends for it. A wider one holds a number of its own
        // width, which is not taken.
        DataType::Bit(None | Some(1)) => "BIT".to_owned(),
        DataType::Bit(_) => return unsupported("BIT wider than one bit"),
        // MySQL stores BOOLEAN and BOOL as TINYINT and reports both as
        // `tinyint(1)`. The name is kept so that the display width survives a
        // round trip; the value is a TINYINT's and is checked as one.
        DataType::Boolean | DataType::Bool => "BOOLEAN".to_owned(),
        DataType::Datetime(precision) => render_temporal_type("DATETIME", *precision)?,
        // A DATE holds the day alone. MySQL normalizes a wide input surface to
        // `YYYY-MM-DD`; this stores that form and only that form, the way it
        // already does for a DATETIME.
        DataType::Date => "DATE".to_owned(),
        // A TIME holds a span of time rather than a moment: measured on MySQL
        // 8.4.11 it runs from `-838:59:59` to `838:59:59`, so it takes more
        // than a day and it takes a sign.
        DataType::Time(precision, sqlparser::ast::TimezoneInfo::None) => {
            render_temporal_type("TIME", *precision)?
        }
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
        DataType::Timestamp(precision, sqlparser::ast::TimezoneInfo::None) => {
            render_temporal_type("TIMESTAMP", *precision)?
        }
        DataType::Decimal(info) | DataType::Numeric(info) | DataType::Dec(info) => {
            let (precision, scale) = declared_decimal_size(*info)?;
            format!("mysql_decimal({precision},{scale})")
        }
        // The sign has to go in front of the name here, not after it: the
        // engine's declared type takes a word before its arguments and not
        // after them. The MySQL word order is put back where the column is
        // read, so nothing above this sees the inversion.
        DataType::DecimalUnsigned(info) | DataType::DecUnsigned(info) => {
            let (precision, scale) = declared_decimal_size(*info)?;
            format!("mysql_decimal_unsigned({precision},{scale})")
        }
        _ => return unsupported("column type"),
    };
    reject_duplicate_nullable_column_options(&column.options)?;
    // Measured on MySQL 8.4.11: `NOT NULL DEFAULT NULL`, in either order, is
    // 1067, and so is an `ALTER COLUMN ... SET DEFAULT NULL` on such a column.
    if column
        .options
        .iter()
        .any(|option| matches!(option.option, ColumnOption::NotNull))
        && column.options.iter().any(|option| {
            matches!(&option.option, ColumnOption::Default(Expr::Value(value))
                if matches!(value.value, Value::Null))
        })
    {
        return unsupported("DEFAULT NULL on a NOT NULL column");
    }
    check_column_character_set(column)?;
    let collation = engine_collation_of(column);
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
    definition.push_str(collation);
    Ok(definition)
}

fn render_temporal_type(name: &str, precision: Option<u64>) -> Result<String, ParseError> {
    match precision {
        None | Some(0) => Ok(name.to_owned()),
        Some(precision @ 1..=6) => Ok(format!("{name}({precision})")),
        _ => unsupported("temporal precision"),
    }
}

/// The collation this server declares a column of words with.
///
/// A column, its keys, and queries against it use one frozen UCA 9 order.
pub(crate) const WORDS_COLLATION: &str = " COLLATE MYSQL_UCA9_AI_CI";

/// The engine collation a column is declared with: the one it names, or
/// MySQL's default for a column of words, or none for any other column.
///
/// A key uses the column's collation, so keys and queries meet under the same
/// weights.
pub(crate) fn engine_collation_of(column: &ColumnDef) -> &'static str {
    if !a_column_of_words(&column.data_type) {
        return "";
    }
    let named = column
        .options
        .iter()
        .find_map(|option| match &option.option {
            ColumnOption::Collation(name) => Some(name),
            _ => None,
        });
    match named {
        Some(name) if unqualified_name_is(name, &["utf8mb4_bin"]) => " COLLATE MYSQL_UTF8MB4_BIN",
        Some(name) if unqualified_name_is(name, &["utf8mb4_unicode_ci"]) => {
            " COLLATE MYSQL_UCA400_CI"
        }
        Some(name) if unqualified_name_is(name, &["utf8mb3_unicode_ci", "utf8_unicode_ci"]) => {
            " COLLATE MYSQL_UTF8MB3_UCA400_CI"
        }
        _ => WORDS_COLLATION,
    }
}

/// Refuses a column whose character set and collation do not go together, or
/// that names a character set whose own default collation this server does
/// not have.
///
/// Measured on MySQL 8.4.11: a collation names its character set, so
/// `COLLATE utf8mb3_unicode_ci` alone makes a `utf8mb3` column; `utf8` is read
/// as `utf8mb3`; a collation of another character set than the one named is
/// 1253; and `CHARACTER SET utf8mb3` alone takes `utf8mb3_general_ci`, whose
/// comparison this server does not have. An `ENUM` or a `SET` in `utf8mb3`
/// is refused as not measured.
pub(crate) fn check_column_character_set(column: &ColumnDef) -> Result<(), ParseError> {
    let mut character_set = None;
    let mut collation = None;
    for option in &column.options {
        match &option.option {
            ColumnOption::CharacterSet(name) => {
                character_set = Some(if unqualified_name_is(name, &["utf8mb4"]) {
                    "utf8mb4"
                } else {
                    "utf8mb3"
                });
            }
            ColumnOption::Collation(name) => {
                collation = Some(
                    if unqualified_name_is(name, &["utf8mb3_unicode_ci", "utf8_unicode_ci"]) {
                        "utf8mb3"
                    } else {
                        "utf8mb4"
                    },
                );
            }
            _ => {}
        }
    }
    let character_set = match (character_set, collation) {
        (Some(named), Some(collated)) if named != collated => {
            return unsupported("column COLLATE of another CHARACTER SET")
        }
        (Some("utf8mb3"), None) => return unsupported("column CHARACTER SET utf8mb3 alone"),
        (named, collated) => named.or(collated),
    };
    if character_set == Some("utf8mb3")
        && matches!(column.data_type, DataType::Enum(..) | DataType::Set(_))
    {
        return unsupported("ENUM or SET in utf8mb3");
    }
    Ok(())
}

/// Reports whether a column holds words rather than bytes or numbers.
///
/// MySQL gives a character column the table's collation and a binary one none,
/// so `payload = 'ABC'` finds no row holding `abc` where `name = 'ABC'` finds
/// the row holding `abc`. These are the types that take the collation.
pub(crate) fn a_column_of_words(data_type: &DataType) -> bool {
    matches!(
        data_type,
        DataType::Char(_)
            | DataType::Varchar(_)
            | DataType::TinyText
            | DataType::Text
            | DataType::MediumText
            | DataType::LongText
            | DataType::Enum(_, _)
            | DataType::Set(_)
    )
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

/// MySQL's own limit on a `BINARY`, in bytes; wider is 1074.
const MOST_PADDED_BYTES: u64 = 255;

/// Reads the byte count a `BINARY` holds. Measured on MySQL 8.4.11, a bare
/// `BINARY` is `binary(1)`. `BINARY(0)` holds only the empty value, and is
/// refused.
fn declared_padded_width(width: Option<u64>) -> Result<u32, ParseError> {
    match width.unwrap_or(1) {
        0 => unsupported("BINARY(0)"),
        width if width > MOST_PADDED_BYTES => unsupported("BINARY wider than 255 bytes"),
        width => Ok(u32::try_from(width).expect("a BINARY width fits")),
    }
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
    moment_precision(expr).is_some()
}

/// The fractional-second digits a call naming the moment asks for, or `None`
/// for anything that is not such a call. `CURRENT_TIMESTAMP` and
/// `CURRENT_TIMESTAMP()` ask for none; `CURRENT_TIMESTAMP(3)` for three.
pub(crate) fn moment_precision(expr: &Expr) -> Option<u64> {
    let Expr::Function(function) = expr else {
        return None;
    };
    let [ObjectNamePart::Identifier(name)] = function.name.0.as_slice() else {
        return None;
    };
    if function.over.is_some()
        || !["CURRENT_TIMESTAMP", "NOW", "LOCALTIME", "LOCALTIMESTAMP"]
            .iter()
            .any(|spelling| name.value.eq_ignore_ascii_case(spelling))
    {
        return None;
    }
    match &function.args {
        FunctionArguments::None => Some(0),
        FunctionArguments::List(arguments) => match arguments.args.as_slice() {
            [] => Some(0),
            [sqlparser::ast::FunctionArg::Unnamed(sqlparser::ast::FunctionArgExpr::Expr(
                Expr::Value(value),
            ))] => match &value.value {
                Value::Number(digits, false) => digits.parse().ok().filter(|digits| *digits <= 6),
                _ => None,
            },
            _ => None,
        },
        FunctionArguments::Subquery(_) => None,
    }
}

/// The fractional-second digits a `DATETIME` or `TIMESTAMP` declares.
fn declared_fraction_digits(data_type: &DataType) -> u64 {
    match data_type {
        DataType::Timestamp(Some(digits), _) | DataType::Datetime(Some(digits)) => *digits,
        _ => 0,
    }
}

/// The engine's reading of the moment with `digits` fractional-second digits,
/// the default a `DATETIME(3) DEFAULT CURRENT_TIMESTAMP(3)` stores.
///
/// The engine's clock reads to the millisecond, so a fourth digit and beyond
/// are zeros; fewer digits are cut off rather than rounded, since rounding a
/// clock reading up names a moment that has not come yet. The length the text
/// is cut at is `20 + digits`, which is how the digits are read back out of a
/// stored definition.
pub fn moment_with_fraction_sql(digits: u8) -> String {
    format!(
        "substr(strftime('%Y-%m-%d %H:%M:%f', 'now') || '000', 1, {})",
        20 + u32::from(digits)
    )
}

/// The same upsert with its update half also writing the moment into each of
/// `stamped`, a column with the places of a second it keeps, the way an
/// `UPDATE` rewrites an `ON UPDATE CURRENT_TIMESTAMP` column.
pub fn stamping_the_moments(
    statement: &Stmt,
    stamped: &[(String, u8)],
) -> Result<Stmt, ParseError> {
    let mut statement = statement.clone();
    let Stmt::Insert {
        body: turso_parser::ast::InsertBody::Select(_, Some(upsert)),
        ..
    } = &mut statement
    else {
        return unsupported("stamping the moment into a statement that is no upsert");
    };
    let turso_parser::ast::UpsertDo::Set { sets, .. } = &mut upsert.do_clause else {
        return unsupported("stamping the moment into an upsert that changes nothing");
    };
    let assignments = stamped
        .iter()
        .map(|(column, places)| {
            let moment = match places {
                0 => "CURRENT_TIMESTAMP".to_owned(),
                places => moment_with_fraction_sql(*places),
            };
            format!("{} = {moment}", render_ident_str(column))
        })
        .collect::<Vec<_>>()
        .join(", ");
    let Stmt::Update(update) = parse_normalized_dml(&format!("UPDATE t SET {assignments}"))? else {
        return Err(ParseError::TursoParser(
            "the moments stamped did not read as an UPDATE".to_string(),
        ));
    };
    sets.extend(update.sets);
    Ok(statement)
}

/// Whether an engine default is the reading of the clock written in
/// parentheses, which is how a MySQL `DEFAULT (now())` is kept.
pub fn reads_the_clock_as_an_expression(expr: &TursoExpr) -> bool {
    matches!(expr, TursoExpr::Parenthesized(inner)
        if matches!(inner.as_slice(), [inner]
            if matches!(inner.as_ref(), TursoExpr::Literal(TursoLiteral::CurrentTimestamp))))
}

/// Reads back the digits of a reading [`moment_with_fraction_sql`] wrote, or
/// `None` for any other expression.
pub fn moment_with_fraction_digits(expr: &TursoExpr) -> Option<u8> {
    let expr = match expr {
        TursoExpr::Parenthesized(inner) => match inner.as_slice() {
            [inner] => inner.as_ref(),
            _ => return None,
        },
        expr => expr,
    };
    let TursoExpr::FunctionCall { name, args, .. } = expr else {
        return None;
    };
    let [text, start, length] = args.as_slice() else {
        return None;
    };
    if !name.as_str().eq_ignore_ascii_case("substr")
        || !matches!(start.as_ref(), TursoExpr::Literal(TursoLiteral::Numeric(one)) if one == "1")
    {
        return None;
    }
    let TursoExpr::Literal(TursoLiteral::Numeric(length)) = length.as_ref() else {
        return None;
    };
    let TursoExpr::Binary(clock, turso_parser::ast::Operator::Concat, zeros) = text.as_ref() else {
        return None;
    };
    if !matches!(zeros.as_ref(), TursoExpr::Literal(TursoLiteral::String(zeros)) if zeros == "'000'")
    {
        return None;
    }
    let TursoExpr::FunctionCall { name, args, .. } = clock.as_ref() else {
        return None;
    };
    let reads_the_clock = name.as_str().eq_ignore_ascii_case("strftime")
        && matches!(args.as_slice(), [format, now]
            if matches!(format.as_ref(), TursoExpr::Literal(TursoLiteral::String(format)) if format == "'%Y-%m-%d %H:%M:%f'")
                && matches!(now.as_ref(), TursoExpr::Literal(TursoLiteral::String(now)) if now == "'now'"));
    if !reads_the_clock {
        return None;
    }
    length
        .parse::<u8>()
        .ok()
        .and_then(|length| length.checked_sub(20))
        .filter(|digits| (1..=6).contains(digits))
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
            if matches!(data_type, DataType::JSON) {
                reject_json_default(expr)?;
            }
            if let Some(digits) = moment_precision(expr) {
                // MySQL takes this default on a column that holds a moment and
                // on no other — measured on 8.4.11, `DEFAULT CURRENT_TIMESTAMP`
                // on an `INT` answers 1067.
                if !matches!(data_type, DataType::Timestamp(_, _) | DataType::Datetime(_)) {
                    return unsupported(
                        "DEFAULT CURRENT_TIMESTAMP on a column that holds no moment",
                    );
                }
                // Measured on 8.4.11: the default reads the moment to exactly
                // the digits the column holds, and any other count is 1067.
                if digits != declared_fraction_digits(data_type) {
                    return unsupported("CURRENT_TIMESTAMP default at another precision");
                }
                if digits == 0 {
                    return Ok(Some("DEFAULT CURRENT_TIMESTAMP".to_owned()));
                }
                return Ok(Some(format!(
                    "DEFAULT ({})",
                    moment_with_fraction_sql(digits as u8)
                )));
            }
            if let Expr::Nested(inner) = expr {
                return reading_of_the_clock_default(inner, data_type).map(Some);
            }
            if matches!(data_type, DataType::Bit(_)) {
                return Ok(Some(format!("DEFAULT {}", bit_default(expr)?)));
            }
            // Measured on MySQL 8.4.11: a `BINARY(3)` written `DEFAULT 'y'`
            // prints `DEFAULT 'y\0\0'`, filled out to its width, which a
            // default kept as it was written would not print.
            if let DataType::Binary(width) = data_type {
                let fills_the_width = match expr {
                    Expr::Value(value) => match &value.value {
                        Value::SingleQuotedString(word) => {
                            u64::try_from(word.len()).ok() == Some(width.unwrap_or(1))
                        }
                        Value::Null => true,
                        _ => false,
                    },
                    _ => false,
                };
                if !fills_the_width {
                    return unsupported("a BINARY default narrower than its column");
                }
            }
            if let Some(range) = whole_number_range_of(data_type) {
                let integer = whole_number_default(expr, range)?;
                // The engine holds a BIGINT UNSIGNED in a type of its own that
                // reads its default as a word.
                if matches!(data_type, DataType::BigIntUnsigned(_)) {
                    return match integer {
                        Some(integer) => Ok(Some(format!("DEFAULT '{integer}'"))),
                        None if matches!(expr, Expr::Value(value) if matches!(&value.value, Value::Null)) => {
                            Ok(Some("DEFAULT NULL".to_owned()))
                        }
                        None => unsupported("BIGINT UNSIGNED DEFAULT literal"),
                    };
                }
                if let Some(integer) = integer {
                    return Ok(Some(format!("DEFAULT {integer}")));
                }
            }
            if floating_point_type(data_type) {
                reject_a_floating_default_mysql_would_print_otherwise(expr, data_type)?;
            }
            if let Some((precision, scale)) = decimal_size_of(data_type)? {
                if matches!(expr, Expr::Value(value) if matches!(&value.value, Value::Null)) {
                    return Ok(Some("DEFAULT NULL".to_owned()));
                }
                let written = decimal_default_text(expr)?;
                let rounded = round_decimal_to_scale(&written, precision, scale)?;
                return Ok(Some(format!("DEFAULT '{rounded}'")));
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
        // column. The collations this server has are taken, and the column is
        // declared with the matching engine collation where it is rendered;
        // naming another would be a claim about ordering and case that this
        // cannot keep, so it is refused.
        // Which collation goes with which character set is checked over the
        // whole column, in `check_column_character_set`.
        ColumnOption::CharacterSet(name) if option.name.is_none() => {
            if !unqualified_name_is(name, &["utf8mb4", "utf8mb3", "utf8"]) {
                return unsupported("column CHARACTER SET");
            }
            Ok(None)
        }
        ColumnOption::Collation(name) if option.name.is_none() => {
            if !a_column_of_words(data_type)
                || !unqualified_name_is(
                    name,
                    &[
                        "utf8mb4_0900_ai_ci",
                        "utf8mb4_bin",
                        "utf8mb4_unicode_ci",
                        "utf8mb3_unicode_ci",
                        "utf8_unicode_ci",
                    ],
                )
            {
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
        // The engine has no attribute for this, so it is written nowhere in
        // the SQLite definition and put back into the stored MySQL DDL by
        // `render_mysql_checked_column`. What it means — the column is
        // rewritten by an `UPDATE` that changes the row and does not name it —
        // is done by the statement renderer, which is the only place that can
        // see both the table and the assignment list.
        ColumnOption::OnUpdate(expr) if option.name.is_none() => {
            let Some(digits) = moment_precision(expr) else {
                return unsupported("ON UPDATE expression");
            };
            if !matches!(data_type, DataType::Timestamp(_, _) | DataType::Datetime(_)) {
                return unsupported("ON UPDATE CURRENT_TIMESTAMP on a column that holds no moment");
            }
            // Measured on 8.4.11: another count of digits than the column
            // holds is 1294.
            if digits != declared_fraction_digits(data_type) {
                return unsupported("ON UPDATE CURRENT_TIMESTAMP at another precision");
            }
            Ok(None)
        }
        // The engine has no attribute for this either, so it is written
        // nowhere in the SQLite definition and put back into the stored MySQL
        // DDL by `render_mysql_checked_column`.
        ColumnOption::Comment(_) if option.name.is_none() => Ok(None),
        ColumnOption::Default(_) => unsupported("named DEFAULT constraint"),
        _ => unsupported("column attribute"),
    }
}

/// The engine default of a column declared with an expression default that
/// reads the clock — SQLAlchemy writes `DEFAULT (now())` for
/// `server_default=func.now()`.
///
/// Measured on MySQL 8.4.11: `(now())`, `(NOW())`, `(current_timestamp)` and
/// `(current_timestamp())` over a `DATETIME` or a `TIMESTAMP` each store the
/// moment the row is written, whole seconds, and print back as `DEFAULT
/// (now())`, where `DEFAULT now()` without its parentheses prints as
/// `DEFAULT CURRENT_TIMESTAMP`. The engine's own reading of the moment is
/// written in parentheses, which is what says it was an expression. Every
/// other expression is refused, and so is a column holding fractional
/// seconds or a day, whose readings were not measured here.
fn reading_of_the_clock_default(inner: &Expr, data_type: &DataType) -> Result<String, ParseError> {
    let Expr::Function(function) = inner else {
        return unsupported("DEFAULT expression");
    };
    let [ObjectNamePart::Identifier(name)] = function.name.0.as_slice() else {
        return unsupported("DEFAULT expression");
    };
    let reads_the_clock = ["NOW", "CURRENT_TIMESTAMP"]
        .iter()
        .any(|spelling| name.value.eq_ignore_ascii_case(spelling));
    if !reads_the_clock || moment_precision(inner) != Some(0) {
        return unsupported("DEFAULT expression");
    }
    if !matches!(data_type, DataType::Timestamp(_, _) | DataType::Datetime(_))
        || declared_fraction_digits(data_type) != 0
    {
        return unsupported("DEFAULT (now()) on a column that holds no whole-second moment");
    }
    Ok("DEFAULT (CURRENT_TIMESTAMP)".to_owned())
}

/// The engine default of a `BIT(1)` column, the integer its bit is.
///
/// Measured on MySQL 8.4.11: `DEFAULT 0`, `1`, `FALSE`, `TRUE`, `b'0'`,
/// `b'1'` and `x'01'` are taken and print as `b'0'` or `b'1'`, and `DEFAULT 2`,
/// `b'10'` and `'1'` — a word, whose byte is wider than the one bit — are
/// 1067. A word is refused here, `''` included, which MySQL takes as `b'0'`,
/// and so is a hexadecimal one.
fn bit_default(expr: &Expr) -> Result<&'static str, ParseError> {
    let Expr::Value(value) = expr else {
        return unsupported("BIT DEFAULT expression");
    };
    match &value.value {
        Value::Null => Ok("NULL"),
        Value::Number(digits, false) | Value::SingleQuotedByteStringLiteral(digits)
            if digits == "0" =>
        {
            Ok("0")
        }
        Value::Number(digits, false) | Value::SingleQuotedByteStringLiteral(digits)
            if digits == "1" =>
        {
            Ok("1")
        }
        Value::Boolean(false) => Ok("0"),
        Value::Boolean(true) => Ok("1"),
        _ => unsupported("BIT DEFAULT other than 0 or 1"),
    }
}

/// The smallest and largest value a column of whole numbers holds, or `None`
/// for a column of any other kind.
fn whole_number_range_of(data_type: &DataType) -> Option<(i128, i128)> {
    let (low, high) = match data_type {
        DataType::TinyInt(_) | DataType::Bool | DataType::Boolean => {
            (i8::MIN.into(), i8::MAX.into())
        }
        DataType::SmallInt(_) => (i16::MIN.into(), i16::MAX.into()),
        DataType::MediumInt(_) => (-(1 << 23), (1 << 23) - 1),
        DataType::Int(_) | DataType::Integer(_) => (i32::MIN.into(), i32::MAX.into()),
        DataType::BigInt(_) => (i64::MIN.into(), i64::MAX.into()),
        DataType::TinyIntUnsigned(_) => (0, u8::MAX.into()),
        DataType::SmallIntUnsigned(_) => (0, u16::MAX.into()),
        DataType::MediumIntUnsigned(_) => (0, (1 << 24) - 1),
        DataType::IntUnsigned(_) | DataType::IntegerUnsigned(_) => (0, u32::MAX.into()),
        DataType::BigIntUnsigned(_) => (0, u64::MAX.into()),
        _ => return None,
    };
    Some((low, high))
}

/// The whole number a written default stores in a column of whole numbers, or
/// `None` for a default that is not a written number or a word at all — `NULL`
/// and `TRUE` among them.
///
/// Measured on MySQL 8.4.11: a word that reads as a number is taken and
/// rounded half away from zero, and so is a written number with a point —
/// `INT DEFAULT '4.5'` and `DEFAULT 4.5` both print `DEFAULT '5'`, `'-4.5'`
/// prints `'-5'`, `' 7'` and `'7 '` print `'7'`, `'007'` prints `'7'`, `'.5'`
/// prints `'1'`, `'5.'` prints `'5'`, and `'1e2'` and `'25e-1'` print `'100'`
/// and `'3'`. What lands outside the column's range after rounding is 1067 —
/// `TINYINT DEFAULT '300'`, `'127.5'`, `TINYINT UNSIGNED DEFAULT '-1'` — while
/// `INT UNSIGNED DEFAULT '-0.4'` rounds to 0 and is taken. A word that is not a
/// number is 1067 as well: `''`, `'abc'`, `'5a'`, `'0x10'`, `'1,5'`, `'1 000'`.
///
/// Refused as well, though MySQL takes them: a written number with an
/// exponent, which MySQL reads as a binary64 and rounds half to even — `2.5e0`
/// prints `'2'` where `'2.5e0'` prints `'3'` — a word ending in a bare `e`,
/// which MySQL reads as though the `e` were not there, and a word with a tab
/// or another space than the plain one around it.
fn whole_number_default(
    expr: &Expr,
    (low, high): (i128, i128),
) -> Result<Option<i128>, ParseError> {
    let (sign, value) = match expr {
        Expr::Value(value) => ("", &value.value),
        Expr::UnaryOp {
            op: UnaryOperator::Minus,
            expr,
        } => match expr.as_ref() {
            Expr::Value(value) => ("-", &value.value),
            _ => return unsupported("DEFAULT integer literal"),
        },
        Expr::UnaryOp {
            op: UnaryOperator::Plus,
            expr,
        } => match expr.as_ref() {
            Expr::Value(value) => ("", &value.value),
            _ => return unsupported("DEFAULT integer literal"),
        },
        _ => return Ok(None),
    };
    let written = match value {
        Value::Number(digits, false) if !digits.contains(['e', 'E']) => {
            format!("{sign}{digits}")
        }
        Value::Number(_, _) => return unsupported("DEFAULT number written with an exponent"),
        Value::SingleQuotedString(word) if sign.is_empty() => {
            number_in_a_word(word).ok_or(ParseError::Unsupported {
                feature: "a word that is not a number as the default of a column of numbers",
            })?
        }
        _ if sign.is_empty() => return Ok(None),
        _ => return unsupported("DEFAULT integer literal"),
    };
    let rounded = round_half_away_from_zero(&written)?;
    if !(low..=high).contains(&rounded) {
        return unsupported("DEFAULT outside the range of its column");
    }
    Ok(Some(rounded))
}

/// The number a word names, spelled the way `BigDecimal` reads one, when the
/// whole word is one: plain spaces around it, a sign, digits with a point
/// anywhere among them, and an exponent of at most three digits.
fn number_in_a_word(word: &str) -> Option<String> {
    let word = word.trim_matches(' ');
    let (sign, unsigned) = match word.as_bytes().first()? {
        b'-' => ("-", &word[1..]),
        b'+' => ("", &word[1..]),
        _ => ("", word),
    };
    let (mantissa, exponent) = match unsigned.split_once(['e', 'E']) {
        Some((mantissa, exponent)) => (mantissa, Some(exponent)),
        None => (unsigned, None),
    };
    let (whole, fraction) = mantissa.split_once('.').unwrap_or((mantissa, ""));
    let all_digits = |text: &str| text.bytes().all(|byte| byte.is_ascii_digit());
    if (whole.is_empty() && fraction.is_empty()) || !all_digits(whole) || !all_digits(fraction) {
        return None;
    }
    let exponent = match exponent {
        None => String::new(),
        Some(exponent) => {
            let digits = exponent.strip_prefix(['-', '+']).unwrap_or(exponent);
            if !(1..=3).contains(&digits.len()) || !all_digits(digits) {
                return None;
            }
            format!("e{exponent}")
        }
    };
    let whole = if whole.is_empty() { "0" } else { whole };
    let fraction = if fraction.is_empty() { "0" } else { fraction };
    Some(format!("{sign}{whole}.{fraction}{exponent}"))
}

fn round_half_away_from_zero(written: &str) -> Result<i128, ParseError> {
    use bigdecimal::{BigDecimal, RoundingMode, ToPrimitive};
    use std::str::FromStr;

    let parsed = BigDecimal::from_str(written).map_err(|_| ParseError::Unsupported {
        feature: "DEFAULT number",
    })?;
    parsed
        .with_scale_round(0, RoundingMode::HalfUp)
        .to_i128()
        .ok_or(ParseError::Unsupported {
            feature: "DEFAULT outside the range of its column",
        })
}

fn floating_point_type(data_type: &DataType) -> bool {
    matches!(
        data_type,
        DataType::Double(_)
            | DataType::DoubleUnsigned(_)
            | DataType::DoublePrecision
            | DataType::DoublePrecisionUnsigned
            | DataType::Real
            | DataType::RealUnsigned
            | DataType::Float8
            | DataType::Float(_)
            | DataType::FloatUnsigned(_)
            | DataType::Float4
    )
}

/// Refuses a word as the default of a `DOUBLE` or a `FLOAT` unless MySQL
/// prints it back exactly as it was written.
///
/// Measured on MySQL 8.4.11: a word that is not a number is 1067 — `''`, `'x'`
/// — and one that is prints the number the column holds, not the word:
/// `DOUBLE DEFAULT ' 1'` prints `'1'`, `'1.50'` prints `'1.5'`, `'1e2'` prints
/// `'100'`, `'1234567890123456'` prints `'1.234567890123456e15'` and `FLOAT
/// DEFAULT '1234567'` prints `'1234570'`. The engine keeps the word as it was
/// written, so the words taken are the ones MySQL prints unchanged — up to
/// fifteen digits on a `DOUBLE` and six on a `FLOAT`, measured up to
/// `'123456789012345'`, `'0.0000001'` and `FLOAT DEFAULT '0.000001'` — and a
/// negative one on an unsigned column, 1067 in MySQL, is refused too.
fn reject_a_floating_default_mysql_would_print_otherwise(
    expr: &Expr,
    data_type: &DataType,
) -> Result<(), ParseError> {
    let word = match expr {
        Expr::Value(value) => match &value.value {
            Value::SingleQuotedString(word) | Value::Number(word, false) => word.clone(),
            _ => return Ok(()),
        },
        Expr::UnaryOp {
            op: UnaryOperator::Minus,
            expr,
        } => match expr.as_ref() {
            Expr::Value(value) => match &value.value {
                Value::Number(digits, false) => format!("-{digits}"),
                _ => return unsupported("DEFAULT literal"),
            },
            _ => return unsupported("DEFAULT literal"),
        },
        Expr::UnaryOp { .. } => return unsupported("DEFAULT literal"),
        _ => return Ok(()),
    };
    let (single, most_places) = match data_type {
        DataType::Float(_) | DataType::FloatUnsigned(_) | DataType::Float4 => (true, 6),
        _ => (false, 7),
    };
    let unsigned = matches!(
        data_type,
        DataType::DoubleUnsigned(_)
            | DataType::DoublePrecisionUnsigned
            | DataType::RealUnsigned
            | DataType::FloatUnsigned(_)
    );
    let magnitude = match word.strip_prefix('-') {
        Some(_) if unsigned => {
            return unsupported("a negative default on an unsigned column");
        }
        Some(magnitude) => magnitude,
        None => &word,
    };
    let (whole, fraction) = magnitude.split_once('.').unwrap_or((magnitude, ""));
    let printed_as_written = !whole.is_empty()
        && whole.bytes().all(|byte| byte.is_ascii_digit())
        && (whole == "0" || !whole.starts_with('0'))
        && fraction.bytes().all(|byte| byte.is_ascii_digit())
        && (magnitude.len() == whole.len() || fraction.ends_with(|last: char| last != '0'))
        && fraction.len() <= most_places;
    let significant_digits = format!("{whole}{fraction}").trim_start_matches('0').len();
    let most_digits = if single { 6 } else { 15 };
    if !printed_as_written || significant_digits > most_digits {
        return unsupported("a word as the default of a floating-point column");
    }
    Ok(())
}

fn decimal_size_of(data_type: &DataType) -> Result<Option<(u32, u32)>, ParseError> {
    match data_type {
        DataType::Decimal(info)
        | DataType::Numeric(info)
        | DataType::Dec(info)
        | DataType::DecimalUnsigned(info)
        | DataType::DecUnsigned(info) => declared_decimal_size(*info).map(Some),
        _ => Ok(None),
    }
}

fn decimal_default_text(expr: &Expr) -> Result<String, ParseError> {
    match expr {
        Expr::Value(value) => match &value.value {
            Value::Number(written, false) | Value::SingleQuotedString(written) => {
                Ok(written.clone())
            }
            _ => unsupported("DECIMAL DEFAULT literal"),
        },
        Expr::UnaryOp { op, expr } => {
            let sign = match op {
                UnaryOperator::Minus => "-",
                UnaryOperator::Plus => "+",
                _ => return unsupported("DECIMAL DEFAULT literal"),
            };
            let Expr::Value(value) = expr.as_ref() else {
                return unsupported("DECIMAL DEFAULT literal");
            };
            let Value::Number(written, false) = &value.value else {
                return unsupported("DECIMAL DEFAULT literal");
            };
            Ok(format!("{sign}{written}"))
        }
        _ => unsupported("DECIMAL DEFAULT literal"),
    }
}

pub fn round_decimal_to_scale(
    written: &str,
    precision: u32,
    scale: u32,
) -> Result<String, ParseError> {
    use bigdecimal::{BigDecimal, RoundingMode};
    use std::str::FromStr;

    if !(1..=65).contains(&precision) || scale > 30 || scale > precision {
        return unsupported("DECIMAL DEFAULT precision or scale");
    }
    let parsed = BigDecimal::from_str(written).map_err(|_| ParseError::Unsupported {
        feature: "DECIMAL literal",
    })?;
    let (coefficient, input_scale) = parsed.as_bigint_and_exponent();
    let digits = coefficient.to_string().trim_start_matches('-').len() as i128;
    if parsed != 0 && digits - i128::from(input_scale) > i128::from(precision - scale) {
        return unsupported("DECIMAL DEFAULT out of range");
    }
    let rounded = if parsed == 0 || i128::from(input_scale) - i128::from(scale) > digits {
        BigDecimal::from(0).with_scale_round(i64::from(scale), RoundingMode::HalfUp)
    } else {
        parsed.with_scale_round(i64::from(scale), RoundingMode::HalfUp)
    };
    let (coefficient, _) = rounded.as_bigint_and_exponent();
    if coefficient.to_string().trim_start_matches('-').len() > precision as usize {
        return unsupported("DECIMAL DEFAULT out of range");
    }
    Ok(rounded.to_plain_string())
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
        // A key over several columns is the join table every schema has, and
        // the engine spells it the same way. A key over one column is left
        // refused here so it keeps reaching the checked path that gives it a
        // marker of its own; that path moves the words onto the column.
        TableConstraint::PrimaryKey(key) if key.columns.len() > 1 => {
            // Measured on MySQL 8.4.11: a `CONSTRAINT` name on the key is
            // dropped, the key always being named PRIMARY — Drizzle names
            // every composite key it writes.
            if key.index_name.is_some()
                || key.index_type.is_some()
                || !key.index_options.is_empty()
                || key.characteristics.is_some()
            {
                return unsupported("PRIMARY KEY attribute");
            }
            Ok(format!(
                "PRIMARY KEY ({})",
                render_index_columns(&key.columns)?
            ))
        }
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

/// Whether `word` is written anywhere in `text`, in any letter case.
///
/// A recognizer asks this of every statement before reading one, and a
/// statement may run to a MiB. Comparing a whole window at every place cost
/// 10 ms a call over 0.6 MB in a debug build; comparing the rest only where
/// the first letter matches costs under 2 ms.
pub fn mentions_ignoring_case(text: &str, word: &str) -> bool {
    let (text, word) = (text.as_bytes(), word.as_bytes());
    let Some((first, rest)) = word.split_first() else {
        return true;
    };
    let mut at = 0;
    while at + word.len() <= text.len() {
        if text[at].eq_ignore_ascii_case(first)
            && text[at + 1..at + word.len()].eq_ignore_ascii_case(rest)
        {
            return true;
        }
        at += 1;
    }
    false
}

#[cfg(test)]
mod tests;
