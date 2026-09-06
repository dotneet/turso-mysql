mod catalog;

use std::{
    collections::{HashMap, HashSet},
    error::Error,
    fmt,
    sync::{Arc, Mutex},
    time::Duration,
};

use turso_core::{
    AssignmentOperation, AssignmentValidator, Connection, DatabaseFileOwner, IO, IOExt as _,
    LimboError, Numeric, PrepareOptions, ReprepareContext, ReprepareParser, Result,
    SchemaSqlFormatter, SchemaSqlKind, Statement, StatementStatusCounter, Value,
    storage::auto_increment::{AutoIncrementKey, DurableRangeAllocator},
};
use turso_mysql_parser::{
    CheckedAutoIncrementCreateTable, CheckedAutoIncrementInsert, CheckedPrimaryKeyCreateTable,
    CheckedSelectComparison, CheckedSelectComparisonRhs, CheckedSubqueryComparison,
    CheckedUpdateAssignmentValue, MySqlCreateTableWithKeys, MySqlDropTableCommand, MySqlTableName,
    MySqlTransactionCommand,
    ParseError as MySqlParseError, SessionSqlMode,
    parse_auto_increment_create_table, parse_auto_increment_insert,
    parse_auto_increment_insert_target, parse_autocommit_setting,
    parse_checked_primary_key_create_table, parse_create_table_ast, parse_create_view_ast,
    parse_dml, parse_optional_autocommit_setting,
    parse_prepared_auto_increment_insert, parse_schema_ddl_ast, parse_select,
    parse_transaction_command, render_create_index_mysql_with_mode,
    render_create_table_mysql_with_mode, render_create_trigger_mysql_with_mode,
    render_create_view_mysql_with_mode, StaticSelectMetadata, StaticSelectProjectionMetadata,
};
use turso_parser::ast::{
    AlterTableBody, Cmd, ColumnConstraint, CreateTableBody, Expr, InsertBody, Literal, OneSelect,
    ResultColumn, SelectTable, Stmt, UnaryOperator,
};

use crate::schema_sql::{
    SchemaSqlSessionContext, SchemaSqlV2Metadata, decode_schema_sql, decode_schema_sql_any,
    encode_schema_sql_v2,
};
use crate::drop_table::{MySqlDropTableError, MySqlDropTableResult};

/// MySQL statement entry for one connection and immutable schema parsing context.
#[derive(Clone)]
pub struct MySqlConnection {
    inner: Arc<Connection>,
    schema_context: SchemaSqlSessionContext,
    auto_increment: Option<AutoIncrementExecutionCapability>,
    session_autocommit: Arc<Mutex<bool>>,
    /// Set while a `START TRANSACTION READ ONLY` is open. MySQL answers 1792 to
    /// a write inside one, so this frontend has to know it is in one to answer
    /// the same rather than accept a transaction whose promise it does not keep.
    read_only_transaction: Arc<Mutex<bool>>,
    prepared_statements: Arc<Mutex<PreparedStatementRegistry>>,
    prepared_statement_authority: MySqlPreparedStatementAuthority,
}

#[derive(Clone)]
pub(crate) struct AutoIncrementExecutionCapability {
    allocator: DurableRangeAllocator,
    io: Arc<dyn IO>,
}

/// How many times a catalog read retries an allocator another statement
/// is using. MySQL never fails SHOW CREATE TABLE for a concurrent INSERT.
const ALLOCATOR_PEEK_ATTEMPTS: usize = 8;

/// Failure stage for a checked MySQL query prepare.
///
/// Keeping parser rejection separate from a core prepare failure lets protocol
/// adapters return a syntax error only when the MySQL parser actually rejected
/// the statement. Core currently uses `LimboError::ParseError` for some schema
/// lookup failures too, so flattening both stages would mislabel missing objects
/// as malformed SQL.
#[derive(Debug)]
pub enum MySqlQueryError {
    /// A write was attempted inside a `START TRANSACTION READ ONLY`.
    ReadOnlyTransaction,
    /// An omitted required column has no default in a checked empty INSERT.
    MissingRequiredDefault(String),
    /// The MySQL parser or checked translator rejected the query text.
    Syntax(String),
    /// Valid MySQL syntax lies outside the implemented compatibility surface.
    Unsupported(String),
    /// The checked Turso AST reached core, which then failed to prepare it.
    Engine(LimboError),
}

/// One row of `SHOW INDEX`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MySqlIndexEntry {
    key_name: String,
    column_name: String,
    sequence_in_index: u32,
    unique: bool,
    nullable: bool,
}

impl MySqlIndexEntry {
    /// Returns the index name, which is `PRIMARY` for the primary key.
    pub fn key_name(&self) -> &str {
        &self.key_name
    }

    /// Returns the indexed column.
    pub fn column_name(&self) -> &str {
        &self.column_name
    }

    /// Returns this column's one-based position within the index.
    pub const fn sequence_in_index(&self) -> u32 {
        self.sequence_in_index
    }

    /// Returns whether the index rejects duplicates.
    pub const fn unique(&self) -> bool {
        self.unique
    }

    /// Returns whether the indexed column accepts NULL.
    pub const fn nullable(&self) -> bool {
        self.nullable
    }
}

/// Failure while rendering one checked MySQL `SHOW CREATE TABLE`.
#[derive(Debug)]
pub enum MySqlShowCreateTableError {
    /// No object of that name exists in the selected database.
    MissingTable,
    /// The name belongs to a view, which MySQL answers with a different result
    /// shape than a base table.
    NotTable,
    /// The stored definition is outside the DDL this frontend can print back.
    Unsupported,
    Engine(LimboError),
}

/// One rendered `SHOW CREATE TABLE` row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MySqlShowCreateTableResult {
    table: String,
    create_statement: String,
}

impl MySqlShowCreateTableResult {
    /// Returns the table name, as the `Table` column reports it.
    pub fn table(&self) -> &str {
        &self.table
    }

    /// Returns the DDL text, as the `Create Table` column reports it.
    pub fn create_statement(&self) -> &str {
        &self.create_statement
    }
}

/// Failure while dropping one checked MySQL view.
#[derive(Debug)]
pub enum MySqlDropViewError {
    MissingView,
    NotView,
    Engine(LimboError),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MySqlWriteResult {
    /// Rows selected by this successful statement's affected-row mode.
    pub affected_rows: u64,
    /// First generated ID for this statement, or zero when none was generated.
    pub last_insert_id: u64,
}

/// The kind of schema object returned by [`MySqlConnection::list_tables`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MySqlTableKind {
    /// A stored base table.
    BaseTable,
    /// A stored view, which MySQL also returns from `SHOW TABLES`.
    View,
}

/// One user-visible table or view from the selected MySQL database.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MySqlTable {
    name: String,
    kind: MySqlTableKind,
}

/// The key classification available in the initial MySQL column metadata slice.
///
/// The order is MySQL's precedence: a stronger key overrides a weaker one on
/// the same column, so `PRI` outranks `UNI` and `UNI` outranks `MUL`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum MySqlColumnKey {
    /// The column has no supported key declaration.
    None,
    /// The column leads an index that does not make it unique on its own.
    Multiple,
    /// The column has an inline UNIQUE declaration.
    Unique,
    /// The column has an inline PRIMARY KEY declaration.
    Primary,
}

/// The supported typed values of a persisted MySQL column DEFAULT clause.
///
/// `None` on [`MySqlColumnMetadata::default_value`] means that the column has
/// no DEFAULT clause. An explicit `NULL` is represented by [`Self::Null`].
/// Integer text is canonical signed decimal text, while `value` is its checked
/// signed 64-bit value. Text values have their SQL quotes and doubled quotes
/// decoded; backslash handling was already applied using the SQL mode stored
/// in the schema envelope.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MySqlColumnDefault {
    /// An explicit `DEFAULT NULL` clause.
    Null,
    /// A signed integer DEFAULT literal.
    Integer { text: String, value: i64 },
    /// A decoded single-quoted string DEFAULT literal.
    Text(String),
    /// A `TRUE` or `FALSE` DEFAULT literal.
    Boolean(bool),
}

/// One column reconstructed from its persisted normalized MySQL DDL.
///
/// `type_name`, `default_sql`, and `extra` are not inferred from Core's
/// SQLite-compatible table definition. The first slice accepts only durable
/// DDL shapes for which every field can be reconstructed exactly.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MySqlColumnMetadata {
    name: String,
    type_name: String,
    character_length: Option<u32>,
    decimal_size: Option<(u32, u32)>,
    nullable: bool,
    key: MySqlColumnKey,
    default_sql: Option<String>,
    default_value: Option<MySqlColumnDefault>,
    extra: String,
}

impl MySqlColumnMetadata {
    /// Returns the stored column name.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Returns the normalized MySQL type name from the stored DDL.
    pub fn type_name(&self) -> &str {
        &self.type_name
    }

    /// Returns the declared character count, for the types that carry one.
    pub const fn character_length(&self) -> Option<u32> {
        self.character_length
    }

    /// Returns the declared precision and scale of a `DECIMAL`.
    pub const fn decimal_size(&self) -> Option<(u32, u32)> {
        self.decimal_size
    }

    /// Returns whether the stored declaration permits NULL values.
    pub const fn nullable(&self) -> bool {
        self.nullable
    }

    /// Returns the supported key classification.
    pub const fn key(&self) -> MySqlColumnKey {
        self.key
    }

    /// Returns the normalized SQL literal DEFAULT expression, when present.
    ///
    /// String values retain their quotes, and `NULL` is returned as the text
    /// `NULL`; callers must not treat this as a decoded wire value.
    pub fn default_sql(&self) -> Option<&str> {
        self.default_sql.as_deref()
    }

    /// Returns the typed DEFAULT value, when a DEFAULT clause is present.
    ///
    /// This is suitable for protocol conversion after the caller handles the
    /// distinction between an omitted DEFAULT and an explicit `NULL`. It is
    /// not itself a MySQL wire-protocol value.
    pub fn default_value(&self) -> Option<&MySqlColumnDefault> {
        self.default_value.as_ref()
    }

    /// Returns the exact supported Extra value.
    ///
    /// It is `AUTO_INCREMENT` only when a canonical durable v2 definition
    /// proves the allocator-owned column; otherwise it is empty.
    pub fn extra(&self) -> &str {
        &self.extra
    }
}

/// Failure while recovering MySQL column metadata from persistent schema SQL.
#[derive(Debug)]
pub enum MySqlColumnMetadataError {
    /// The selected database has no user table with this name.
    TableNotFound,
    /// The persisted schema row violates a durable MySQL schema invariant.
    CorruptDefinition,
    /// The persisted DDL is valid but lies outside the initial metadata slice.
    UnsupportedDefinition,
    /// Core could not read the trusted schema catalog.
    Engine(LimboError),
}

impl fmt::Display for MySqlColumnMetadataError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TableNotFound => f.write_str("MySQL table metadata was not found"),
            Self::CorruptDefinition => f.write_str("MySQL table metadata is corrupt"),
            Self::UnsupportedDefinition => {
                f.write_str("MySQL table metadata is not supported by this slice")
            }
            Self::Engine(error) => error.fmt(f),
        }
    }
}

impl Error for MySqlColumnMetadataError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Engine(error) => Some(error),
            Self::TableNotFound | Self::CorruptDefinition | Self::UnsupportedDefinition => None,
        }
    }
}

/// One more than the server protocol row limit, so a full result cannot be
/// mistaken for a truncated catalog listing.
const TABLE_LIST_SCAN_LIMIT: usize = 4097;

/// One more than the largest index set this provider will inspect.
const COLUMN_INDEX_SCAN_LIMIT: usize = 4097;

impl MySqlTable {
    /// Returns the table or view name as it is stored by the MySQL frontend.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Returns whether this entry is a base table or a view.
    pub const fn kind(&self) -> MySqlTableKind {
        self.kind
    }
}

/// Metadata returned after a checked MySQL statement is prepared.
///
/// The frontend keeps the executable statement private to its connection-local
/// registry. Protocol adapters can use this metadata to produce a prepare
/// response before later looking up the statement by ID to bind or execute it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MySqlPreparedStatementMetadata {
    /// Stable, non-zero ID assigned by this connection.
    pub statement_id: u32,
    /// Number of positional parameter slots accepted by the statement.
    pub parameter_count: u16,
    /// Metadata for every result column in source order.
    pub result_columns: Vec<MySqlPreparedResultColumn>,
}

/// Metadata for one prepared-statement result column.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MySqlPreparedResultColumn {
    /// Column name reported by the prepared statement.
    pub name: String,
    /// Core's normalized primitive type, when it can determine one.
    pub type_name: Option<String>,
}

/// Opaque provenance for one prepared result column's type.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MySqlPreparedResultColumnTypeMetadata {
    declared_type_name: Option<String>,
    static_metadata: Option<StaticSelectMetadata>,
    source_reference: Option<(String, usize)>,
    parameter_marker: Option<ParameterMarker>,
}

/// A result column that is nothing but a `?`, and the type its executions have
/// settled on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ParameterMarker {
    ordinal: usize,
    kind: MySqlMarkerType,
}

impl ParameterMarker {
    /// Returns the type this column reports.
    pub const fn kind(&self) -> MySqlMarkerType {
        self.kind
    }

    /// Applies one execution's bound value.
    ///
    /// MySQL keeps the type the first non-NULL value established, so a NULL
    /// after an integer still reports the integer type.
    fn observe(&mut self, value: Option<&MySqlPreparedValue>) {
        self.kind = match (self.kind, value) {
            (kind, None | Some(MySqlPreparedValue::Null)) => kind,
            (
                MySqlMarkerType::Untyped | MySqlMarkerType::Integer,
                Some(MySqlPreparedValue::Integer(_)),
            ) => MySqlMarkerType::Integer,
            (
                MySqlMarkerType::Untyped | MySqlMarkerType::Real,
                Some(MySqlPreparedValue::Real(_)),
            ) => MySqlMarkerType::Real,
            // MySQL converts the value and raises its own warning across the
            // other transitions, which this frontend does not do yet. Reporting
            // the converted type without converting the value would send a row
            // that does not match its own metadata, so the row decides the
            // type, as it did before markers were tracked.
            _ => MySqlMarkerType::RowDecides,
        };
    }
}

/// The types a `?` result column reports.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MySqlMarkerType {
    /// Nothing but NULLs so far. MySQL reports its generic string type.
    Untyped,
    Integer,
    Real,
    /// A transition this frontend cannot report without also converting the
    /// value. The row decides the type.
    RowDecides,
}

impl MySqlPreparedResultColumnTypeMetadata {
    /// Returns the exact declared type for a direct table column, if present.
    pub fn declared_type_name(&self) -> Option<&str> {
        self.declared_type_name.as_deref()
    }

    /// Returns whether this result column came from a direct table column.
    pub const fn is_declared(&self) -> bool {
        self.declared_type_name.is_some()
    }

    /// Returns source metadata for a static checked-SELECT expression, if any.
    pub fn static_metadata(&self) -> Option<&StaticSelectMetadata> {
        self.static_metadata.as_ref()
    }

    /// Returns the query-visible source table reference and column ordinal.
    ///
    /// Aliases are preserved, while literals and other expressions return
    /// `None`. This provenance is refreshed when a prepared statement is
    /// reprepared after a schema change.
    pub fn source_reference(&self) -> Option<(&str, usize)> {
        self.source_reference
            .as_ref()
            .map(|(table, ordinal)| (table.as_str(), *ordinal))
    }

    /// Returns the marker state when this column is nothing but a `?`.
    pub const fn parameter_marker(&self) -> Option<ParameterMarker> {
        self.parameter_marker
    }
}

/// An owned value accepted by a checked MySQL prepared `SELECT`.
#[derive(Debug, Clone, PartialEq)]
pub enum MySqlPreparedValue {
    /// SQL NULL.
    Null,
    /// A signed integer value.
    Integer(i64),
    /// A floating-point value.
    Real(f64),
    /// UTF-8 text.
    Text(String),
    /// Binary bytes.
    Blob(Vec<u8>),
}

/// One owned result row returned by a prepared `SELECT`.
pub type MySqlPreparedResultRow = Vec<MySqlPreparedValue>;

/// Owned rows returned by a prepared `SELECT`.
pub type MySqlPreparedResultRows = Vec<MySqlPreparedResultRow>;

/// Successful result from a checked MySQL prepared statement.
#[derive(Debug, Clone, PartialEq)]
pub enum MySqlPreparedExecutionResult {
    /// A `SELECT` statement returned rows.
    Rows(MySqlPreparedResultRows),
    /// An `INSERT`, `UPDATE`, or `DELETE` statement completed.
    Write(MySqlWriteResult),
}

/// Failure while managing one connection-local prepared statement.
#[derive(Debug)]
pub enum MySqlPreparedStatementError {
    /// An omitted required column has no default at execution time.
    MissingRequiredDefault(String),
    /// The checked prepare rejected the supplied SQL.
    Prepare(MySqlQueryError),
    /// The configured number of prepared statements is already active.
    PreparedStatementLimitReached { maximum: usize },
    /// Every non-zero MySQL statement ID has already been assigned.
    StatementIdExhausted,
    /// The client referenced no statement stored on this connection.
    UnknownStatement { statement_id: u32 },
    /// The supplied values did not match the statement's parameter count.
    ParameterCountMismatch { expected: usize, actual: usize },
    /// Core could not reset the stored statement.
    Engine(LimboError),
}

impl fmt::Display for MySqlPreparedStatementError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingRequiredDefault(column) => {
                write!(f, "field '{column}' doesn't have a default value")
            }
            Self::Prepare(error) => error.fmt(f),
            Self::PreparedStatementLimitReached { maximum } => write!(
                f,
                "maximum MySQL prepared statement count reached: {maximum}"
            ),
            Self::StatementIdExhausted => {
                f.write_str("MySQL prepared statement ID space is exhausted")
            }
            Self::UnknownStatement { statement_id } => {
                write!(f, "unknown MySQL prepared statement ID {statement_id}")
            }
            Self::ParameterCountMismatch { expected, actual } => write!(
                f,
                "MySQL prepared statement expects {expected} parameters, received {actual}"
            ),
            Self::Engine(error) => error.fmt(f),
        }
    }
}

impl Error for MySqlPreparedStatementError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Prepare(error) => Some(error),
            Self::Engine(error) => Some(error),
            Self::PreparedStatementLimitReached { .. }
            | Self::StatementIdExhausted
            | Self::UnknownStatement { .. }
            | Self::MissingRequiredDefault(_)
            | Self::ParameterCountMismatch { .. } => None,
        }
    }
}

/// The MySQL 8.4 default for `max_prepared_stmt_count`.
pub const DEFAULT_MAX_PREPARED_STMT_COUNT: usize = 16_382;

/// The largest accepted value for `max_prepared_stmt_count`.
pub const MAX_PREPARED_STMT_COUNT: usize = 4_194_304;

/// A rejected prepared-statement quota configuration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MySqlPreparedStatementAuthorityError {
    /// The requested maximum is outside MySQL's supported range.
    MaximumOutOfRange { maximum: usize },
}

impl fmt::Display for MySqlPreparedStatementAuthorityError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MaximumOutOfRange { maximum } => write!(
                f,
                "MySQL prepared statement maximum {maximum} is outside 0..={MAX_PREPARED_STMT_COUNT}"
            ),
        }
    }
}

impl Error for MySqlPreparedStatementAuthorityError {}

/// A cloneable prepared-statement quota shared by explicitly connected sessions.
///
/// The authority counts retained prepared statements rather than prepare
/// attempts. A permit is held by each retained statement and returns the slot
/// when that statement is removed or dropped.
#[derive(Clone)]
pub struct MySqlPreparedStatementAuthority {
    inner: Arc<Mutex<PreparedStatementAuthorityState>>,
}

struct PreparedStatementAuthorityState {
    maximum: usize,
    active: usize,
}

impl Default for MySqlPreparedStatementAuthority {
    fn default() -> Self {
        Self::new(DEFAULT_MAX_PREPARED_STMT_COUNT)
            .expect("the MySQL prepared statement default must be valid")
    }
}

impl MySqlPreparedStatementAuthority {
    /// Creates an authority with a MySQL-compatible maximum.
    pub fn new(maximum: usize) -> std::result::Result<Self, MySqlPreparedStatementAuthorityError> {
        validate_prepared_statement_maximum(maximum)?;
        Ok(Self {
            inner: Arc::new(Mutex::new(PreparedStatementAuthorityState {
                maximum,
                active: 0,
            })),
        })
    }

    /// Returns the current maximum number of retained prepared statements.
    pub fn maximum(&self) -> usize {
        self.inner
            .lock()
            .expect("MySQL prepared statement authority mutex poisoned")
            .maximum
    }

    /// Returns the number of retained prepared statements currently counted.
    pub fn active_count(&self) -> usize {
        self.inner
            .lock()
            .expect("MySQL prepared statement authority mutex poisoned")
            .active
    }

    /// Changes the maximum without invalidating already retained statements.
    ///
    /// Lowering below the current active count blocks new prepares until enough
    /// statements are removed, matching MySQL's dynamic variable semantics.
    pub fn set_maximum(
        &self,
        maximum: usize,
    ) -> std::result::Result<(), MySqlPreparedStatementAuthorityError> {
        validate_prepared_statement_maximum(maximum)?;
        self.inner
            .lock()
            .expect("MySQL prepared statement authority mutex poisoned")
            .maximum = maximum;
        Ok(())
    }

    fn reserve(
        &self,
    ) -> std::result::Result<MySqlPreparedStatementPermit, MySqlPreparedStatementError> {
        let mut authority = self
            .inner
            .lock()
            .expect("MySQL prepared statement authority mutex poisoned");
        if authority.active >= authority.maximum {
            return Err(MySqlPreparedStatementError::PreparedStatementLimitReached {
                maximum: authority.maximum,
            });
        }
        authority.active += 1;
        drop(authority);
        Ok(MySqlPreparedStatementPermit {
            authority: Arc::clone(&self.inner),
        })
    }
}

fn validate_prepared_statement_maximum(
    maximum: usize,
) -> std::result::Result<(), MySqlPreparedStatementAuthorityError> {
    if maximum > MAX_PREPARED_STMT_COUNT {
        return Err(MySqlPreparedStatementAuthorityError::MaximumOutOfRange { maximum });
    }
    Ok(())
}

struct MySqlPreparedStatementPermit {
    authority: Arc<Mutex<PreparedStatementAuthorityState>>,
}

impl Drop for MySqlPreparedStatementPermit {
    fn drop(&mut self) {
        let mut authority = self
            .authority
            .lock()
            .expect("MySQL prepared statement authority mutex poisoned");
        assert!(
            authority.active > 0,
            "MySQL prepared statement authority permit underflow"
        );
        authority.active -= 1;
    }
}

struct PreparedStatementRegistry {
    next_id: Option<u32>,
    generation: u64,
    reserved_ids: HashSet<u32>,
    statements: HashMap<u32, PreparedStatement>,
}

struct PreparedStatement {
    _permit: MySqlPreparedStatementPermit,
    statement: Option<Statement>,
    metadata: MySqlPreparedStatementMetadata,
    result_column_type_metadata: Vec<MySqlPreparedResultColumnTypeMetadata>,
    static_result_projections: Vec<StaticSelectProjectionMetadata>,
    execution_plan: PreparedExecutionPlan,
    /// How many times core had reprepared this statement when its metadata was
    /// last rebuilt. Core only ever adds to this, so a change means a schema
    /// reprepare happened, which is where MySQL returns a `?` column to its
    /// generic type.
    reprepares_at_last_refresh: u64,
}

enum PreparedExecutionPlan {
    Select {
        reads_table: bool,
        source_table: Option<MySqlTableName>,
        checked_comparisons: Vec<CheckedSelectComparison>,
    },
    OrdinaryWrite {
        is_update: bool,
        insert_target: Option<CheckedInsertTarget>,
    },
    AutoIncrementInsert(Box<PreparedAutoIncrementInsert>),
}

/// Which columns a checked INSERT fills in, so the caller can report the NOT
/// NULL error MySQL would report.
enum CheckedInsertTarget {
    /// `INSERT INTO t DEFAULT VALUES` names no columns at all.
    DefaultValues(MySqlTableName),
    /// `INSERT INTO t (c1, ..., cn) VALUES (...)`.
    Listed(ListedInsert),
}

struct ListedInsert {
    table: MySqlTableName,
    /// Column names in the order the statement lists them.
    columns: Vec<String>,
    /// One entry per VALUES row, holding what each listed column receives.
    rows: Vec<Vec<InsertedValue>>,
}

/// What one listed column receives, as far as it is known before execution.
enum InsertedValue {
    /// A literal `NULL`.
    Null,
    /// A `?` marker, with the index its value is bound at.
    Marker(usize),
    /// Anything else the checked INSERT grammar allows, none of which the
    /// frontend can tell is a NULL before the statement runs.
    Value,
}

#[derive(Default)]
struct InsertColumnRules {
    /// Every NOT NULL column the statement can hand a value to. A NULL in one
    /// of these raises 1048, which suppresses the 1364 check.
    not_null: Vec<String>,
    /// The NOT NULL columns that also have no default, which the statement has
    /// to list itself.
    required: Vec<String>,
}

impl ListedInsert {
    /// Whether the first row puts a NULL in a NOT NULL column. MySQL stores a
    /// row's values before checking that the row filled every required column,
    /// so such a row reports 1048 and never reaches the 1364 check. A default
    /// does not exempt a column here: 1048 fires on any NOT NULL column handed
    /// a NULL.
    ///
    /// Only the first row matters. MySQL stops at the first row that fails, and
    /// every row shares the statement's column list, so a required column the
    /// statement never lists already fails on row one:
    /// `VALUES (1, 1), (2, NULL)` with a third required column reports 1364,
    /// not the 1048 of the row it never reaches.
    fn first_row_hands_null_to_a_not_null_column(
        &self,
        not_null: &[String],
        bound: &[MySqlPreparedValue],
    ) -> bool {
        let Some(row) = self.rows.first() else {
            return false;
        };
        debug_assert_eq!(row.len(), self.columns.len());
        row.iter().zip(&self.columns).any(|(value, column)| {
            let is_null = match value {
                InsertedValue::Null => true,
                InsertedValue::Marker(index) => {
                    matches!(bound.get(*index), Some(MySqlPreparedValue::Null))
                }
                InsertedValue::Value => false,
            };
            is_null && not_null.iter().any(|name| name.eq_ignore_ascii_case(column))
        })
    }

    fn lists(&self, column: &str) -> bool {
        self.columns
            .iter()
            .any(|name| name.eq_ignore_ascii_case(column))
    }
}

struct PreparedAutoIncrementInsert {
    sql: String,
    insert: CheckedAutoIncrementInsert,
    table: AutoIncrementTable,
    parameter_count: usize,
}

struct PreparedStatementReservation {
    statement_id: u32,
    generation: u64,
    registry: Arc<Mutex<PreparedStatementRegistry>>,
    permit: Option<MySqlPreparedStatementPermit>,
}

impl Drop for PreparedStatementReservation {
    fn drop(&mut self) {
        let mut registry = self
            .registry
            .lock()
            .expect("MySQL prepared statement registry mutex poisoned");
        if registry.generation == self.generation {
            registry.reserved_ids.remove(&self.statement_id);
        }
    }
}

impl Default for PreparedStatementRegistry {
    fn default() -> Self {
        Self {
            next_id: Some(1),
            generation: 0,
            reserved_ids: HashSet::new(),
            statements: HashMap::new(),
        }
    }
}

/// Selects which successful UPDATE rows the MySQL protocol reports.
///
/// MySQL normally reports rows whose stored value changed. Clients that
/// negotiate `CLIENT_FOUND_ROWS` instead receive every row matched by the
/// UPDATE predicate.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum MySqlAffectedRowsMode {
    /// Report only rows whose stored value changed.
    #[default]
    Changed,
    /// Report every row matched by the UPDATE predicate.
    Matched,
}

impl fmt::Display for MySqlQueryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingRequiredDefault(column) => {
                write!(f, "field '{column}' doesn't have a default value")
            }
            Self::ReadOnlyTransaction => {
                f.write_str("cannot execute statement in a READ ONLY transaction")
            }
            Self::Syntax(error) => f.write_str(error),
            Self::Unsupported(error) => f.write_str(error),
            Self::Engine(error) => error.fmt(f),
        }
    }
}

impl Error for MySqlQueryError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::MissingRequiredDefault(_) | Self::ReadOnlyTransaction => None,
            Self::Syntax(_) => None,
            Self::Unsupported(_) => None,
            Self::Engine(error) => Some(error),
        }
    }
}

impl From<MySqlQueryError> for LimboError {
    fn from(error: MySqlQueryError) -> Self {
        match error {
            MySqlQueryError::MissingRequiredDefault(_) => Self::NullValue,
            MySqlQueryError::ReadOnlyTransaction => Self::ReadOnly,
            MySqlQueryError::Syntax(error) => Self::ParseError(error),
            MySqlQueryError::Unsupported(error) => Self::ParseError(error),
            MySqlQueryError::Engine(error) => error,
        }
    }
}

impl MySqlConnection {
    pub fn new(inner: Arc<Connection>, schema_context: SchemaSqlSessionContext) -> Result<Self> {
        Self::new_with_prepared_statement_authority(
            inner,
            schema_context,
            MySqlPreparedStatementAuthority::default(),
        )
    }

    /// Creates a connection using an explicitly shared prepared-statement quota.
    pub fn new_with_prepared_statement_authority(
        inner: Arc<Connection>,
        schema_context: SchemaSqlSessionContext,
        prepared_statement_authority: MySqlPreparedStatementAuthority,
    ) -> Result<Self> {
        if inner.dialect().database_file_owner() != DatabaseFileOwner::MySql
            || inner.dialect().name() != "mysql"
        {
            return Err(LimboError::InvalidArgument(
                "MySqlConnection requires a MySQL-owned database".to_string(),
            ));
        }
        if !schema_context.supports_current_table_loader() {
            return Err(LimboError::ParseError(
                "the current MySQL table slice supports only binary character contexts".to_string(),
            ));
        }
        // MySQL has no SQLite DQS misfeature. Left on, an identifier that does
        // not resolve becomes a string literal, so `SELECT id, nosuchcolumn
        // FROM t` answers with a fabricated `nosuchcolumn` beside a real value
        // instead of MySQL's 1054. Measured on MySQL 8.4.11: `SELECT $`, which
        // is what a real client's `select $$` probe reduces to, is 1054, and
        // `select $$` itself is 1064.
        inner.set_dqs_dml(false);
        Ok(Self {
            inner,
            schema_context,
            auto_increment: None,
            session_autocommit: Arc::new(Mutex::new(true)),
            read_only_transaction: Arc::new(Mutex::new(false)),
            prepared_statements: Arc::new(Mutex::new(PreparedStatementRegistry::default())),
            prepared_statement_authority,
        })
    }

    pub(crate) fn new_with_auto_increment_and_prepared_statement_authority(
        inner: Arc<Connection>,
        schema_context: SchemaSqlSessionContext,
        allocator: DurableRangeAllocator,
        io: Arc<dyn IO>,
        prepared_statement_authority: MySqlPreparedStatementAuthority,
    ) -> Result<Self> {
        let mut connection = Self::new_with_prepared_statement_authority(
            inner,
            schema_context,
            prepared_statement_authority,
        )?;
        connection.auto_increment = Some(AutoIncrementExecutionCapability { allocator, io });
        Ok(connection)
    }

    #[cfg(test)]
    fn inner(&self) -> &Arc<Connection> {
        &self.inner
    }

    /// Close the underlying database connection.
    ///
    /// Prepared statements are cleared only after the underlying close
    /// succeeds. A failed close therefore leaves the registry and its quota
    /// permits unchanged.
    pub fn close(&self) -> Result<()> {
        let result = self.inner.close();
        if result.is_ok() {
            // Keep the quota accurate while clones of this session still exist.
            self.clear_prepared_statements();
        }
        result
    }

    pub fn last_insert_id(&self) -> u64 {
        self.inner.mysql_last_insert_id()
    }


    /// Prepares and stores one checked MySQL `SELECT` or DML statement.
    ///
    /// This validates and compiles SQL but does not run it or start a transaction.
    /// AUTO_INCREMENT inserts reserve their range only when they execute.
    pub fn prepare_checked_statement(
        &self,
        sql: &str,
    ) -> std::result::Result<MySqlPreparedStatementMetadata, MySqlPreparedStatementError> {
        let reservation = self.reserve_prepared_statement()?;
        let mut static_result_metadata = Vec::new();
        let (statement, execution_plan) = match self
            .parse_select_knowing_column_types(sql)
            .map_err(|_| MySqlParseError::ExpectedSelect)
        {
            Ok(translated) => {
                Self::reject_internal_catalog_select(&translated)
                    .map_err(MySqlPreparedStatementError::Prepare)?;
                self.validate_select_comparison_columns(
                    translated.source_table(),
                    translated.checked_comparisons(),
                )
                .map_err(|error| {
                    MySqlPreparedStatementError::Prepare(MySqlQueryError::Unsupported(
                        error.to_string(),
                    ))
                })?;
                static_result_metadata = translated.static_result_metadata().to_vec();
                let statement = translated.parse_ast().map_err(|error| {
                    MySqlPreparedStatementError::Prepare(MySqlQueryError::Syntax(error.to_string()))
                })?;
                let reads_table = translated.reads_table();
                let source_table = translated
                    .source_table()
                    .map(MySqlTableName::parse)
                    .transpose()
                    .map_err(|error| {
                        MySqlPreparedStatementError::Prepare(MySqlQueryError::Syntax(
                            error.to_string(),
                        ))
                    })?;
                let checked_comparisons = translated.checked_comparisons().to_vec();
                let mode = self.parser_mode();
                let options =
                    PrepareOptions::default().with_reprepare_parser(Arc::new(FrozenSelectParser {
                        mode,
                        source_table: translated.source_table().map(str::to_owned),
                        checked_comparisons: checked_comparisons.clone(),
                    }));
                let statement = self
                    .inner
                    .prepare_translated_stmt_with_options(statement, sql, &options)
                    .map_err(|error| {
                        MySqlPreparedStatementError::Prepare(MySqlQueryError::Engine(error))
                    })?;
                if statement.parameters_count() != translated.parameter_count() {
                    return Err(MySqlPreparedStatementError::Prepare(
                        MySqlQueryError::Engine(LimboError::InternalError(
                            "checked SELECT parameter count changed during prepare".to_string(),
                        )),
                    ));
                }
                (
                    Some(statement),
                    PreparedExecutionPlan::Select {
                        reads_table,
                        source_table,
                        checked_comparisons,
                    },
                )
            }
            Err(MySqlParseError::ExpectedSelect) => self.prepare_checked_dml_statement(sql)?,
            Err(error) => {
                return Err(MySqlPreparedStatementError::Prepare(
                    mysql_query_parse_error(error),
                ));
            }
        };

        let statement_id = reservation.statement_id;
        let (metadata, result_column_type_metadata) = match &statement {
            Some(statement) => (
                prepared_statement_metadata(statement_id, statement)?,
                prepared_result_column_type_metadata(statement, &static_result_metadata),
            ),
            None => (
                prepared_auto_increment_statement_metadata(statement_id, &execution_plan)?,
                Vec::new(),
            ),
        };
        self.commit_prepared_statement(
            reservation,
            statement,
            metadata.clone(),
            result_column_type_metadata,
            static_result_metadata,
            execution_plan,
        )?;
        Ok(metadata)
    }

    fn commit_prepared_statement(
        &self,
        mut reservation: PreparedStatementReservation,
        statement: Option<Statement>,
        metadata: MySqlPreparedStatementMetadata,
        result_column_type_metadata: Vec<MySqlPreparedResultColumnTypeMetadata>,
        static_result_projections: Vec<StaticSelectProjectionMetadata>,
        execution_plan: PreparedExecutionPlan,
    ) -> std::result::Result<(), MySqlPreparedStatementError> {
        let mut registry = self
            .prepared_statements
            .lock()
            .expect("MySQL prepared statement registry mutex poisoned");
        if registry.generation != reservation.generation {
            return Err(MySqlPreparedStatementError::Prepare(
                MySqlQueryError::Unsupported(
                    "prepared statement was cleared during prepare".to_string(),
                ),
            ));
        }
        if !registry.reserved_ids.remove(&reservation.statement_id) {
            return Err(MySqlPreparedStatementError::Prepare(
                MySqlQueryError::Engine(LimboError::InternalError(
                    "prepared statement reservation was lost".to_string(),
                )),
            ));
        }
        if registry
            .next_id
            .is_some_and(|next_id| reservation.statement_id >= next_id)
        {
            registry.next_id = reservation.statement_id.checked_add(1);
        }
        registry.statements.insert(
            reservation.statement_id,
            PreparedStatement {
                _permit: reservation
                    .permit
                    .take()
                    .expect("prepared statement reservation permit was already consumed"),
                reprepares_at_last_refresh: statement.as_ref().map_or(0, |statement| {
                    statement.stmt_status(StatementStatusCounter::Reprepare)
                }),
                statement,
                metadata,
                result_column_type_metadata,
                static_result_projections,
                execution_plan,
            },
        );
        Ok(())
    }

    fn reserve_prepared_statement(
        &self,
    ) -> std::result::Result<PreparedStatementReservation, MySqlPreparedStatementError> {
        let permit = self.prepared_statement_authority.reserve()?;
        let mut registry = self
            .prepared_statements
            .lock()
            .expect("MySQL prepared statement registry mutex poisoned");
        let mut statement_id = match registry.next_id {
            Some(statement_id) => statement_id,
            None => return Err(MySqlPreparedStatementError::StatementIdExhausted),
        };
        while registry.reserved_ids.contains(&statement_id) {
            statement_id = statement_id
                .checked_add(1)
                .ok_or(MySqlPreparedStatementError::StatementIdExhausted)?;
        }
        registry.reserved_ids.insert(statement_id);
        Ok(PreparedStatementReservation {
            statement_id,
            generation: registry.generation,
            registry: Arc::clone(&self.prepared_statements),
            permit: Some(permit),
        })
    }

    fn prepare_checked_dml_statement(
        &self,
        sql: &str,
    ) -> std::result::Result<(Option<Statement>, PreparedExecutionPlan), MySqlPreparedStatementError>
    {
        let mode = self.parser_mode();
        let translated = match parse_dml(sql, mode) {
            Ok(translated) => translated,
            Err(MySqlParseError::ExpectedDml) => {
                return Err(MySqlPreparedStatementError::Prepare(
                    MySqlQueryError::Unsupported(
                        "prepared statements support only SELECT, INSERT, UPDATE, and DELETE"
                            .to_string(),
                    ),
                ));
            }
            Err(error) => {
                return Err(MySqlPreparedStatementError::Prepare(
                    mysql_query_parse_error(error),
                ));
            }
        };
        self.validate_select_comparison_columns(
            translated.source_table(),
            translated.checked_comparisons(),
        )
        .map_err(|error| {
            MySqlPreparedStatementError::Prepare(MySqlQueryError::Unsupported(error.to_string()))
        })?;
        let statement = translated.parse_ast().map_err(|error| {
            MySqlPreparedStatementError::Prepare(MySqlQueryError::Syntax(error.to_string()))
        })?;
        let is_update = matches!(statement, Stmt::Update(_));
        let insert_target =
            checked_insert_target(&statement).map_err(MySqlPreparedStatementError::Engine)?;
        if matches!(statement, Stmt::Insert { .. }) {
            if let Some(table) = self.prepared_auto_increment_insert_table(sql, mode)? {
                return self.prepare_checked_auto_increment_insert(sql, mode, table);
            }
        }
        if is_update {
            self.reject_prepared_auto_increment_update(translated.checked_update())?;
        }
        let options =
            PrepareOptions::default().with_reprepare_parser(Arc::new(FrozenDmlParser { mode }));
        let statement = self
            .inner
            .prepare_translated_stmt_with_options(statement, sql, &options)
            .map_err(|error| {
                MySqlPreparedStatementError::Prepare(MySqlQueryError::Engine(error))
            })?;
        Ok((
            Some(statement),
            PreparedExecutionPlan::OrdinaryWrite {
                is_update,
                insert_target,
            },
        ))
    }

    fn prepared_auto_increment_insert_table(
        &self,
        sql: &str,
        mode: SessionSqlMode,
    ) -> std::result::Result<Option<AutoIncrementTable>, MySqlPreparedStatementError> {
        let target = parse_auto_increment_insert_target(sql, mode).map_err(|error| {
            MySqlPreparedStatementError::Prepare(mysql_query_parse_error(error))
        })?;
        let Some(target) = target else {
            return Ok(None);
        };
        self.load_auto_increment_table(&target)
            .map_err(|error| MySqlPreparedStatementError::Prepare(MySqlQueryError::Engine(error)))?
            .map_or(Ok(None), |table| Ok(Some(table)))
    }

    fn prepare_checked_auto_increment_insert(
        &self,
        sql: &str,
        mode: SessionSqlMode,
        table: AutoIncrementTable,
    ) -> std::result::Result<(Option<Statement>, PreparedExecutionPlan), MySqlPreparedStatementError>
    {
        let insert = parse_prepared_auto_increment_insert(sql, mode).map_err(|error| {
            MySqlPreparedStatementError::Prepare(mysql_query_parse_error(error))
        })?;
        let bound = insert
            .clone()
            .bind_allocator_table(&table.definition)
            .map_err(|error| {
                MySqlPreparedStatementError::Prepare(MySqlQueryError::Unsupported(
                    error.to_string(),
                ))
            })?;
        let prototype = bound.inject_reserved_range(1).map_err(|error| {
            MySqlPreparedStatementError::Prepare(MySqlQueryError::Unsupported(error.to_string()))
        })?;
        let options = injected_auto_increment_prepare_options(&table, prototype.clone());
        let prototype = self
            .inner
            .prepare_translated_stmt_with_options(prototype, sql, &options)
            .map_err(|error| {
                MySqlPreparedStatementError::Prepare(MySqlQueryError::Engine(error))
            })?;
        let parameter_count = prototype.parameters_count();
        Ok((
            None,
            PreparedExecutionPlan::AutoIncrementInsert(Box::new(PreparedAutoIncrementInsert {
                sql: sql.to_string(),
                insert,
                table,
                parameter_count,
            })),
        ))
    }

    fn reject_prepared_auto_increment_update(
        &self,
        update: Option<&turso_mysql_parser::CheckedUpdate>,
    ) -> std::result::Result<(), MySqlPreparedStatementError> {
        let Some(update) = update else {
            return Ok(());
        };
        let Some(table) = self
            .load_auto_increment_table(update.table_name())
            .map_err(|error| {
                MySqlPreparedStatementError::Prepare(MySqlQueryError::Engine(error))
            })?
        else {
            return Ok(());
        };
        if update.assignments().iter().any(|assignment| {
            assignment
                .column_name()
                .eq_ignore_ascii_case(&table.definition.allocator_column_name)
        }) {
            return Err(MySqlPreparedStatementError::Prepare(
                MySqlQueryError::Unsupported(
                    "prepared AUTO_INCREMENT column updates are not supported".to_string(),
                ),
            ));
        }
        Ok(())
    }

    /// Returns copied metadata for one statement stored on this connection.
    pub fn prepared_statement_metadata(
        &self,
        statement_id: u32,
    ) -> Option<MySqlPreparedStatementMetadata> {
        self.prepared_statements
            .lock()
            .expect("MySQL prepared statement registry mutex poisoned")
            .statements
            .get(&statement_id)
            .map(|statement| statement.metadata.clone())
    }

    /// Returns opaque declared-type metadata parallel to the result columns.
    pub fn prepared_statement_result_column_type_metadata(
        &self,
        statement_id: u32,
    ) -> Option<Vec<MySqlPreparedResultColumnTypeMetadata>> {
        self.prepared_statements
            .lock()
            .expect("MySQL prepared statement registry mutex poisoned")
            .statements
            .get(&statement_id)
            .map(|statement| statement.result_column_type_metadata.clone())
    }

    /// Binds and executes one checked prepared `SELECT`.
    ///
    /// The statement is reset before binding and after execution so its
    /// compiled program can be reused while the final parameter bindings stay
    /// available to the caller. Table reads start an implicit transaction only
    /// when this method is called, not when the statement is prepared.
    pub fn execute_prepared_select(
        &self,
        statement_id: u32,
        values: &[MySqlPreparedValue],
        timeout: Option<Duration>,
    ) -> std::result::Result<MySqlPreparedResultRows, MySqlPreparedStatementError> {
        self.require_prepared_select(statement_id)?;
        match self.execute_prepared_statement_with_row_callback(
            statement_id,
            values,
            timeout,
            MySqlAffectedRowsMode::Changed,
            |_| Ok(()),
        )? {
            MySqlPreparedExecutionResult::Rows(rows) => Ok(rows),
            MySqlPreparedExecutionResult::Write(_) => Err(MySqlPreparedStatementError::Prepare(
                MySqlQueryError::Unsupported("prepared statement is not a SELECT".to_string()),
            )),
        }
    }

    /// Binds and executes one checked prepared `SELECT`, validating each row
    /// before retaining it in the returned result.
    pub fn execute_prepared_select_with_row_callback(
        &self,
        statement_id: u32,
        values: &[MySqlPreparedValue],
        timeout: Option<Duration>,
        callback: impl FnMut(&[MySqlPreparedValue]) -> Result<()>,
    ) -> std::result::Result<MySqlPreparedResultRows, MySqlPreparedStatementError> {
        self.require_prepared_select(statement_id)?;
        match self.execute_prepared_statement_with_row_callback(
            statement_id,
            values,
            timeout,
            MySqlAffectedRowsMode::Changed,
            callback,
        )? {
            MySqlPreparedExecutionResult::Rows(rows) => Ok(rows),
            MySqlPreparedExecutionResult::Write(_) => Err(MySqlPreparedStatementError::Prepare(
                MySqlQueryError::Unsupported("prepared statement is not a SELECT".to_string()),
            )),
        }
    }

    fn require_prepared_select(
        &self,
        statement_id: u32,
    ) -> std::result::Result<(), MySqlPreparedStatementError> {
        let registry = self
            .prepared_statements
            .lock()
            .expect("MySQL prepared statement registry mutex poisoned");
        let prepared = registry
            .statements
            .get(&statement_id)
            .ok_or(MySqlPreparedStatementError::UnknownStatement { statement_id })?;
        if !matches!(
            prepared.execution_plan,
            PreparedExecutionPlan::Select { .. }
        ) {
            return Err(MySqlPreparedStatementError::Prepare(
                MySqlQueryError::Unsupported("prepared statement is not a SELECT".to_string()),
            ));
        }
        Ok(())
    }

    /// Binds and executes one checked prepared statement.
    ///
    /// SELECT statements return rows. Ordinary DML returns MySQL affected rows
    /// and a zero last-insert ID. The stored statement is reset after every
    /// execution attempt, including a bind, timeout, or callback failure.
    pub fn execute_prepared_statement(
        &self,
        statement_id: u32,
        values: &[MySqlPreparedValue],
        timeout: Option<Duration>,
        affected_rows_mode: MySqlAffectedRowsMode,
    ) -> std::result::Result<MySqlPreparedExecutionResult, MySqlPreparedStatementError> {
        self.execute_prepared_statement_with_row_callback(
            statement_id,
            values,
            timeout,
            affected_rows_mode,
            |_| Ok(()),
        )
    }

    /// Binds and executes one checked prepared statement, validating SELECT
    /// rows before retaining them in the returned result.
    pub fn execute_prepared_statement_with_row_callback(
        &self,
        statement_id: u32,
        values: &[MySqlPreparedValue],
        timeout: Option<Duration>,
        affected_rows_mode: MySqlAffectedRowsMode,
        mut callback: impl FnMut(&[MySqlPreparedValue]) -> Result<()>,
    ) -> std::result::Result<MySqlPreparedExecutionResult, MySqlPreparedStatementError> {
        let mut registry = self
            .prepared_statements
            .lock()
            .expect("MySQL prepared statement registry mutex poisoned");
        let prepared = registry
            .statements
            .get_mut(&statement_id)
            .ok_or(MySqlPreparedStatementError::UnknownStatement { statement_id })?;
        let expected = usize::from(prepared.metadata.parameter_count);
        if values.len() != expected {
            return Err(MySqlPreparedStatementError::ParameterCountMismatch {
                expected,
                actual: values.len(),
            });
        }
        observe_parameter_markers(&mut prepared.result_column_type_metadata, values);

        if let Some(statement) = prepared.statement.as_mut() {
            statement
                .reset()
                .map_err(MySqlPreparedStatementError::Engine)?;
        }
        let timeout = if let PreparedExecutionPlan::OrdinaryWrite {
            insert_target: Some(target),
            ..
        } = &prepared.execution_plan
        {
            let deadline = self.write_deadline(timeout);
            self.check_write_deadline(deadline)
                .map_err(|error| MySqlPreparedStatementError::Engine(error.into()))?;
            self.begin_implicit_transaction_for_write()
                .map_err(|error| MySqlPreparedStatementError::Engine(error.into()))?;
            let missing = self
                .missing_required_insert_column(target, values)
                .map_err(MySqlPreparedStatementError::Engine)?;
            self.check_write_deadline(deadline)
                .map_err(|error| MySqlPreparedStatementError::Engine(error.into()))?;
            if let Some(column) = missing {
                return Err(MySqlPreparedStatementError::MissingRequiredDefault(column));
            }
            self.remaining_write_timeout(deadline)
                .map_err(|error| MySqlPreparedStatementError::Engine(error.into()))?
        } else {
            timeout
        };
        let result = self.execute_bound_prepared_statement(
            prepared,
            values,
            timeout,
            affected_rows_mode,
            &mut callback,
        );
        let metadata_refresh_result = if result.is_ok()
            && matches!(
                &prepared.execution_plan,
                PreparedExecutionPlan::Select { .. }
            )
        {
            refresh_prepared_statement_entry(statement_id, prepared)
        } else {
            Ok(())
        };
        let reset_result = prepared.statement.as_mut().map_or(Ok(()), Statement::reset);
        match (result, metadata_refresh_result, reset_result) {
            (_, _, Err(error)) => Err(MySqlPreparedStatementError::Engine(error)),
            (_, Err(error), Ok(())) => Err(error),
            (Ok(result), Ok(()), Ok(())) => Ok(result),
            (Err(error), Ok(()), Ok(())) => Err(MySqlPreparedStatementError::Engine(error)),
        }
    }

    fn execute_bound_prepared_statement(
        &self,
        prepared: &mut PreparedStatement,
        values: &[MySqlPreparedValue],
        timeout: Option<Duration>,
        affected_rows_mode: MySqlAffectedRowsMode,
        callback: &mut impl FnMut(&[MySqlPreparedValue]) -> Result<()>,
    ) -> Result<MySqlPreparedExecutionResult> {
        if let PreparedExecutionPlan::Select {
            source_table,
            checked_comparisons,
            ..
        } = &prepared.execution_plan
        {
            self.validate_select_comparison_columns(
                source_table.as_ref().map(MySqlTableName::as_str),
                checked_comparisons,
            )?;
            Self::validate_select_comparison_values(checked_comparisons, values)?;
        }
        let values = values
            .iter()
            .map(mysql_prepared_value_to_core)
            .collect::<Result<Vec<_>>>()?;

        match &prepared.execution_plan {
            PreparedExecutionPlan::Select { reads_table, .. } => {
                if *reads_table {
                    self.begin_implicit_transaction_for_table_read()?;
                }
                let statement = prepared.statement.as_mut().ok_or_else(|| {
                    LimboError::InternalError(
                        "prepared SELECT has no reusable core statement".to_string(),
                    )
                })?;
                bind_prepared_values(statement, &values)?;
                if let Some(timeout) = timeout {
                    statement.set_query_timeout_override(Some(Some(timeout)));
                }
                let mut rows = Vec::new();
                statement.run_with_row_callback(|row| {
                    let row = row
                        .get_values()
                        .map(|value| mysql_prepared_value_from_core(value.clone()))
                        .collect::<Vec<_>>();
                    callback(&row)?;
                    rows.push(row);
                    Ok(())
                })?;
                Ok(MySqlPreparedExecutionResult::Rows(rows))
            }
            PreparedExecutionPlan::OrdinaryWrite { is_update, .. } => {
                let deadline = self.write_deadline(timeout);
                self.check_write_deadline(deadline)?;
                self.begin_implicit_transaction_for_write()?;
                let statement = prepared.statement.as_mut().ok_or_else(|| {
                    LimboError::InternalError(
                        "prepared write has no reusable core statement".to_string(),
                    )
                })?;
                bind_prepared_values(statement, &values)?;
                let timeout = self.remaining_write_timeout(deadline)?;
                run_checked_write_statement(statement, timeout)?;
                Ok(MySqlPreparedExecutionResult::Write(MySqlWriteResult {
                    affected_rows: self.affected_rows(*is_update, affected_rows_mode)?,
                    last_insert_id: 0,
                }))
            }
            PreparedExecutionPlan::AutoIncrementInsert(insert) => self
                .execute_prepared_auto_increment_insert(
                    insert,
                    &values,
                    timeout,
                    affected_rows_mode,
                ),
        }
    }

    fn execute_prepared_auto_increment_insert(
        &self,
        insert: &PreparedAutoIncrementInsert,
        values: &[Value],
        timeout: Option<Duration>,
        affected_rows_mode: MySqlAffectedRowsMode,
    ) -> Result<MySqlPreparedExecutionResult> {
        let deadline = self.write_deadline(timeout);
        self.check_write_deadline(deadline)?;
        self.begin_implicit_transaction_for_write()?;

        let table = self
            .load_auto_increment_table(insert.insert.table_name().as_str())?
            .ok_or(LimboError::SchemaUpdated)?;
        if table.key != insert.table.key
            || table.stored_sql != insert.table.stored_sql
            || !table.name.eq_ignore_ascii_case(&insert.table.name)
        {
            return Err(LimboError::SchemaUpdated);
        }
        self.reject_insert_target_triggers(&table.name)?;
        let bound = insert
            .insert
            .clone()
            .bind_allocator_table(&table.definition)
            .map_err(|error| LimboError::ParseError(error.to_string()))?;
        let capability = self.auto_increment.as_ref().ok_or_else(|| {
            LimboError::ParseError(
                "AUTO_INCREMENT INSERT requires a registry-backed allocator capability".to_string(),
            )
        })?;
        let count = u64::try_from(bound.row_count().get()).map_err(|_| {
            LimboError::InvalidArgument("AUTO_INCREMENT INSERT row count is too large".to_string())
        })?;
        let mut reservation = capability.allocator.reserve(table.key, count)?;
        let range = capability.io.block(|| reservation.step())?;
        self.check_write_deadline(deadline)?;
        let expected_last = range
            .first()
            .checked_add(count - 1)
            .ok_or(LimboError::IntegerOverflow)?;
        if range.first() == 0
            || range.last() != expected_last
            || range.last() > auto_increment_ceiling(&table)
        {
            return Err(LimboError::Corrupt(
                "AUTO_INCREMENT allocator returned a range outside the column's type".to_string(),
            ));
        }

        let statement = bound
            .inject_reserved_range(range.first())
            .map_err(|error| LimboError::ParseError(error.to_string()))?;
        let options = injected_auto_increment_prepare_options(&table, statement.clone());
        let mut statement =
            self.inner
                .prepare_translated_stmt_with_options(statement, &insert.sql, &options)?;
        if statement.parameters_count() != insert.parameter_count {
            return Err(LimboError::InternalError(
                "prepared AUTO_INCREMENT INSERT changed its parameter count".to_string(),
            ));
        }
        bind_prepared_values(&mut statement, values)?;
        let result = (|| -> Result<()> {
            let timeout = self
                .remaining_write_timeout(deadline)
                .map_err(Into::<LimboError>::into)?;
            run_checked_write_statement(&mut statement, timeout)
        })();
        let reset_result = statement.reset();
        match (result, reset_result) {
            (_, Err(error)) => Err(error),
            (Err(error), Ok(())) => Err(error),
            (Ok(()), Ok(())) => {
                self.inner.set_mysql_last_insert_id(range.first());
                Ok(MySqlPreparedExecutionResult::Write(MySqlWriteResult {
                    affected_rows: self.affected_rows(false, affected_rows_mode)?,
                    last_insert_id: range.first(),
                }))
            }
        }
    }

    /// Gives one operation exclusive access to a stored statement.
    ///
    /// The registry holds the statement for the whole operation, preventing a
    /// connection-local prepared statement from being used concurrently.
    pub fn with_prepared_statement<T>(
        &self,
        statement_id: u32,
        operation: impl FnOnce(&mut Statement) -> std::result::Result<T, LimboError>,
    ) -> std::result::Result<T, MySqlPreparedStatementError> {
        let mut registry = self
            .prepared_statements
            .lock()
            .expect("MySQL prepared statement registry mutex poisoned");
        let prepared = registry
            .statements
            .get_mut(&statement_id)
            .ok_or(MySqlPreparedStatementError::UnknownStatement { statement_id })?;
        if matches!(
            &prepared.execution_plan,
            PreparedExecutionPlan::Select {
                checked_comparisons,
                ..
            } if !checked_comparisons.is_empty()
        ) {
            return Err(MySqlPreparedStatementError::Prepare(
                MySqlQueryError::Unsupported(
                    "SELECT comparison statements require the checked prepared-statement API"
                        .to_string(),
                ),
            ));
        }
        let statement = prepared.statement.as_mut().ok_or_else(|| {
            MySqlPreparedStatementError::Prepare(MySqlQueryError::Unsupported(
                "prepared AUTO_INCREMENT INSERT has no reusable core statement".to_string(),
            ))
        })?;
        operation(statement).map_err(MySqlPreparedStatementError::Engine)
    }

    /// Resets one stored statement and clears all bindings.
    pub fn reset_prepared_statement(
        &self,
        statement_id: u32,
    ) -> std::result::Result<(), MySqlPreparedStatementError> {
        let mut registry = self
            .prepared_statements
            .lock()
            .expect("MySQL prepared statement registry mutex poisoned");
        let prepared = registry
            .statements
            .get_mut(&statement_id)
            .ok_or(MySqlPreparedStatementError::UnknownStatement { statement_id })?;
        if let Some(statement) = prepared.statement.as_mut() {
            statement
                .reset()
                .map_err(MySqlPreparedStatementError::Engine)?;
            statement.clear_bindings();
        }
        Ok(())
    }

    /// Removes one statement from this connection's registry.
    ///
    /// Unknown IDs are a no-op because clients may close a statement after a
    /// connection-level cleanup already removed it.
    pub fn remove_prepared_statement(&self, statement_id: u32) -> bool {
        self.prepared_statements
            .lock()
            .expect("MySQL prepared statement registry mutex poisoned")
            .statements
            .remove(&statement_id)
            .is_some()
    }

    /// Removes every stored statement without reusing any issued ID.
    pub fn clear_prepared_statements(&self) {
        let mut registry = self
            .prepared_statements
            .lock()
            .expect("MySQL prepared statement registry mutex poisoned");
        registry.generation = registry
            .generation
            .checked_add(1)
            .expect("MySQL prepared statement registry generation exhausted");
        registry.reserved_ids.clear();
        registry.statements.clear();
    }

    /// Returns whether Core currently has no explicit transaction open.
    pub fn is_auto_commit(&self) -> bool {
        self.inner.get_auto_commit()
    }

    /// Returns the MySQL session's autocommit setting.
    pub fn session_autocommit(&self) -> bool {
        *self
            .session_autocommit
            .lock()
            .expect("MySQL autocommit state mutex poisoned")
    }

    /// Executes one checked explicit transaction-control command.
    pub fn execute_transaction_command(
        &self,
        sql: &str,
    ) -> std::result::Result<(), MySqlQueryError> {
        let command =
            parse_transaction_command(sql, self.parser_mode()).map_err(mysql_query_parse_error)?;
        match command {
            MySqlTransactionCommand::Begin | MySqlTransactionCommand::BeginReadOnly
                if !self.inner.get_auto_commit() =>
            {
                self.inner
                    .prepare("COMMIT")
                    .and_then(|mut statement| statement.run_ignore_rows())
                    .map_err(MySqlQueryError::Engine)?;
            }
            MySqlTransactionCommand::Commit | MySqlTransactionCommand::Rollback
                if self.inner.get_auto_commit() =>
            {
                return Ok(());
            }
            // The chaining forms end a transaction and begin another at once.
            // Measured on MySQL 8.4.11: they leave the session in a transaction
            // even when autocommit is on and there was none to end, so the
            // ending half is skipped rather than the whole statement.
            MySqlTransactionCommand::CommitAndChain
            | MySqlTransactionCommand::RollbackAndChain
                if self.inner.get_auto_commit() =>
            {
                return self.run_transaction_statement(
                    Stmt::Begin {
                        typ: None,
                        name: None,
                    },
                    sql,
                );
            }
            _ => {}
        }
        // Every one of these settles what the next transaction is, so the flag
        // is cleared first and set again only by the READ ONLY form.
        *self.read_only_transaction.lock().unwrap() =
            matches!(command, MySqlTransactionCommand::BeginReadOnly);
        let statement = match command {
            MySqlTransactionCommand::Begin | MySqlTransactionCommand::BeginReadOnly => Stmt::Begin {
                typ: None,
                name: None,
            },
            MySqlTransactionCommand::Commit | MySqlTransactionCommand::CommitAndChain => {
                Stmt::Commit { name: None }
            }
            MySqlTransactionCommand::Rollback | MySqlTransactionCommand::RollbackAndChain => {
                Stmt::Rollback {
                    tx_name: None,
                    savepoint_name: None,
                }
            }
        };
        let chains = matches!(
            command,
            MySqlTransactionCommand::CommitAndChain | MySqlTransactionCommand::RollbackAndChain
        );
        if chains {
            self.run_transaction_statement(statement, sql)?;
            return self.run_transaction_statement(
                Stmt::Begin {
                    typ: None,
                    name: None,
                },
                sql,
            );
        }
        self.run_transaction_statement(statement, sql)
    }

    fn run_transaction_statement(
        &self,
        statement: Stmt,
        sql: &str,
    ) -> std::result::Result<(), MySqlQueryError> {
        self.inner
            .prepare_translated_stmt(statement, sql)
            .and_then(|mut statement| statement.run_ignore_rows())
            .map_err(MySqlQueryError::Engine)
    }

    /// Refuses a write inside a `START TRANSACTION READ ONLY`.
    ///
    /// Measured on MySQL 8.4.11: a write there answers 1792 and the transaction
    /// stays open. A DDL statement is not held to this, because it commits what
    /// came before it and so leaves the read-only transaction before it runs —
    /// measured, `START TRANSACTION READ ONLY; CREATE TABLE u (...)` is taken.
    fn reject_write_in_read_only_transaction(&self) -> std::result::Result<(), MySqlQueryError> {
        if *self.read_only_transaction.lock().unwrap() && !self.inner.get_auto_commit() {
            return Err(MySqlQueryError::ReadOnlyTransaction);
        }
        Ok(())
    }

    /// Returns whether SQL belongs to the checked transaction-control surface.
    pub fn is_transaction_command(&self, sql: &str) -> std::result::Result<bool, MySqlQueryError> {
        turso_mysql_parser::parse_optional_transaction_command(sql, self.parser_mode())
            .map(|command| command.is_some())
            .map_err(mysql_query_parse_error)
    }

    /// Applies one checked MySQL autocommit setting.
    pub fn set_autocommit(&self, enabled: bool) -> std::result::Result<(), MySqlQueryError> {
        let mut setting = self
            .session_autocommit
            .lock()
            .expect("MySQL autocommit state mutex poisoned");
        if enabled && !self.inner.get_auto_commit() {
            self.inner
                .prepare("COMMIT")
                .and_then(|mut statement| statement.run_ignore_rows())
                .map_err(MySqlQueryError::Engine)?;
        }
        *setting = enabled;
        Ok(())
    }

    /// Resets the state that belongs to one authenticated MySQL connection.
    ///
    /// Rollback must happen before autocommit is restored so an active
    /// transaction cannot be committed as part of cleanup. Prepared statements
    /// and the last generated ID are connection-local state and are cleared
    /// after those transaction operations succeed.
    pub fn reset_connection(&self) -> std::result::Result<(), MySqlQueryError> {
        self.execute_transaction_command("ROLLBACK")?;
        self.set_autocommit(true)?;
        self.clear_prepared_statements();
        self.set_last_insert_id(0);
        Ok(())
    }

    /// Executes one checked `SET [SESSION] autocommit = 0|1` statement.
    pub fn execute_autocommit_setting(
        &self,
        sql: &str,
    ) -> std::result::Result<(), MySqlQueryError> {
        let setting =
            parse_autocommit_setting(sql, self.parser_mode()).map_err(mysql_query_parse_error)?;
        self.set_autocommit(setting.enabled)
    }

    /// Returns whether SQL belongs to the checked autocommit-setting surface.
    pub fn is_autocommit_setting(&self, sql: &str) -> std::result::Result<bool, MySqlQueryError> {
        parse_optional_autocommit_setting(sql, self.parser_mode())
            .map(|setting| setting.is_some())
            .map_err(mysql_query_parse_error)
    }

    fn begin_implicit_transaction_for_write(&self) -> std::result::Result<(), MySqlQueryError> {
        if self.session_autocommit() || !self.inner.get_auto_commit() {
            return Ok(());
        }
        self.inner
            .prepare("BEGIN")
            .and_then(|mut statement| statement.run_ignore_rows())
            .map_err(MySqlQueryError::Engine)
    }

    fn begin_implicit_transaction_for_table_read(
        &self,
    ) -> std::result::Result<(), MySqlQueryError> {
        self.begin_implicit_transaction_for_write()
    }

    #[doc(hidden)]
    pub fn is_last_insert_id_result(&self, statement: &Statement, index: usize) -> bool {
        self.inner.dialect().name() == "mysql"
            && statement.result_is_function(index, "last_insert_id", 0)
    }

    pub(crate) fn set_last_insert_id(&self, id: u64) {
        self.inner.set_mysql_last_insert_id(id);
    }

    /// Prepare one statement in the supported MySQL subset.
    pub fn prepare(&self, sql: &str) -> Result<Statement> {
        let mode = self.parser_mode();
        if let Ok(checked) = parse_checked_primary_key_create_table(sql, mode) {
            return self.prepare_checked_primary_key_create_table(checked);
        }
        let stmt = match parse_schema_ddl_ast(sql, mode) {
            Ok(stmt) => stmt,
            Err(MySqlParseError::Unsupported {
                feature: "schema statement",
            }) => return self.prepare_non_schema(sql),
            Err(error) => match parse_auto_increment_create_table(sql, mode) {
                Ok(checked) => return self.prepare_auto_increment_create_table(checked),
                Err(_) => return Err(LimboError::ParseError(error.to_string())),
            },
        };
        if let Stmt::AlterTable(alter) = &stmt {
            self.reject_alter_with_auto_increment_table(&alter.name.name)?;
            self.reject_alter_with_marked_trigger()?;
            self.reject_alter_with_marked_view(&alter.body)?;
        }
        if matches!(stmt, Stmt::CreateTrigger { .. }) {
            self.reject_duplicate_marked_insert_trigger(&stmt)?;
        }
        let input = match &stmt {
            Stmt::CreateTable { .. } => render_create_table_mysql_with_mode(&stmt, mode)
                .map_err(|error| LimboError::ParseError(error.to_string()))?,
            Stmt::CreateIndex { .. } => render_create_index_mysql_with_mode(&stmt, mode)
                .map_err(|error| LimboError::ParseError(error.to_string()))?,
            Stmt::CreateView { .. } => render_create_view_mysql_with_mode(&stmt, mode)
                .map_err(|error| LimboError::ParseError(error.to_string()))?,
            Stmt::CreateTrigger { .. } => render_create_trigger_mysql_with_mode(&stmt, mode)
                .map_err(|error| LimboError::ParseError(error.to_string()))?,
            Stmt::AlterTable(_) => sql.to_string(),
            _ => unreachable!("MySQL schema parser returned an unsupported statement"),
        };
        let options = PrepareOptions::default()
            .with_reprepare_parser(Arc::new(FrozenSchemaDdlParser { mode }))
            .with_schema_sql_formatter(Arc::new(self.schema_context));
        self.inner
            .prepare_translated_stmt_with_options(stmt, &input, &options)
    }

    /// Executes one checked schema statement with MySQL implicit-commit semantics.
    pub fn execute_schema_ddl(&self, sql: &str) -> std::result::Result<(), MySqlQueryError> {
        if let Some(statements) = self.expanded_alter_table(sql)? {
            return self.execute_expanded_alter_table(&statements);
        }

        let mut statement = match self.prepare(sql) {
            Ok(statement) => statement,
            Err(error) => {
                if !self.inner.get_auto_commit() {
                    self.inner
                        .prepare("COMMIT")
                        .and_then(|mut statement| statement.run_ignore_rows())
                        .map_err(MySqlQueryError::Engine)?;
                }
                return Err(MySqlQueryError::Engine(error));
            }
        };
        if !self.inner.get_auto_commit() {
            self.inner
                .prepare("COMMIT")
                .and_then(|mut statement| statement.run_ignore_rows())
                .map_err(MySqlQueryError::Engine)?;
        }
        let result = statement.run_ignore_rows().map_err(MySqlQueryError::Engine);
        drop(statement);
        if !self.inner.get_auto_commit() {
            self.inner
                .prepare("ROLLBACK")
                .and_then(|mut statement| statement.run_ignore_rows())
                .map_err(MySqlQueryError::Engine)?;
        }
        result
    }

    /// Returns the statements a multi-operation `ALTER TABLE` means, if that is
    /// what this is.
    ///
    /// A statement naming one operation is left alone, so the ordinary path
    /// keeps answering it and its errors keep their shape.
    fn expanded_alter_table(
        &self,
        sql: &str,
    ) -> std::result::Result<Option<Vec<String>>, MySqlQueryError> {
        if !sql
            .split_whitespace()
            .next()
            .is_some_and(|word| word.eq_ignore_ascii_case("ALTER"))
        {
            return Ok(None);
        }
        let statements =
            match turso_mysql_parser::split_alter_table_operations(sql, self.parser_mode()) {
                Ok(statements) => statements,
                // Leave the ordinary path to report it, so an ALTER this cannot
                // split fails the way it did before.
                Err(_) => return Ok(None),
            };
        if statements.len() < 2 {
            return Ok(None);
        }
        Ok(Some(statements))
    }

    /// Runs the statements one `ALTER TABLE` split into, all or none.
    ///
    /// Each goes through the ordinary schema path, so it passes the checks an
    /// `ALTER` has to pass and is remembered by the durable DDL of its own
    /// operation rather than of the whole statement. Measured on MySQL 8.4.11:
    /// `ADD COLUMN c, ADD COLUMN a` against a table that already has `a` adds
    /// neither, so a failure part-way leaves the table as it was.
    fn execute_expanded_alter_table(
        &self,
        statements: &[String],
    ) -> std::result::Result<(), MySqlQueryError> {
        // DDL commits what came before it, which is what MySQL does.
        if !self.inner.get_auto_commit() {
            self.run_internal("COMMIT")?;
        }
        self.run_internal("BEGIN")?;
        let applied = statements.iter().try_for_each(|statement| {
            self.prepare(statement)
                .and_then(|mut statement| statement.run_ignore_rows())
                .map(|_| ())
                .map_err(MySqlQueryError::Engine)
        });
        if applied.is_err() {
            // A failed rollback leaves the connection in a state the caller
            // cannot reason about, so it replaces the original error.
            self.run_internal("ROLLBACK")?;
            return applied;
        }
        self.run_internal("COMMIT")?;
        if !self.inner.get_auto_commit() {
            self.run_internal("ROLLBACK")?;
        }
        Ok(())
    }

    /// Runs a `CREATE TABLE` that declares plain indexes inline.
    ///
    /// The engine has no inline non-unique index, so this becomes a
    /// `CREATE TABLE` and one `CREATE INDEX` per key. MySQL applies the whole
    /// statement or none of it, so they run inside one transaction: a key that
    /// names a column the table does not have leaves no table behind.
    pub fn execute_create_table_with_keys(
        &self,
        checked: &MySqlCreateTableWithKeys,
    ) -> std::result::Result<(), MySqlQueryError> {
        // DDL commits what came before it, which is what MySQL does.
        if !self.inner.get_auto_commit() {
            self.run_internal("COMMIT")?;
        }
        self.run_internal("BEGIN")?;
        let applied = self.apply_create_table_with_keys(checked);
        if applied.is_err() {
            // A failed rollback leaves the connection in a state the caller
            // cannot reason about, so it replaces the original error.
            self.run_internal("ROLLBACK")?;
            return applied;
        }
        self.run_internal("COMMIT")?;
        if !self.inner.get_auto_commit() {
            self.run_internal("ROLLBACK")?;
        }
        Ok(())
    }

    fn apply_create_table_with_keys(
        &self,
        checked: &MySqlCreateTableWithKeys,
    ) -> std::result::Result<(), MySqlQueryError> {
        self.prepare(checked.table_sql())
            .and_then(|mut statement| statement.run_ignore_rows())
            .map_err(MySqlQueryError::Engine)?;
        for index in checked.indexes() {
            let columns = index
                .columns()
                .iter()
                .map(|column| format!("`{}`", column.replace('`', "``")))
                .collect::<Vec<_>>()
                .join(", ");
            let sql = format!(
                "CREATE INDEX `{}` ON `{}` ({columns})",
                index.name().replace('`', "``"),
                checked.table().as_str().replace('`', "``")
            );
            self.prepare(&sql)
                .and_then(|mut statement| statement.run_ignore_rows())
                .map_err(MySqlQueryError::Engine)?;
        }
        Ok(())
    }

    fn run_internal(&self, sql: &str) -> std::result::Result<(), MySqlQueryError> {
        self.inner
            .prepare(sql)
            .and_then(|mut statement| statement.run_ignore_rows())
            .map(|_| ())
            .map_err(MySqlQueryError::Engine)
    }

    /// Drops one view, committing preceding work before checking its existence.
    pub fn drop_view(&self, name: &MySqlTableName) -> std::result::Result<(), MySqlDropViewError> {
        if !self.inner.get_auto_commit() {
            self.inner
                .prepare("COMMIT")
                .and_then(|mut statement| statement.run_ignore_rows())
                .map_err(MySqlDropViewError::Engine)?;
        }
        let tables = self.list_tables().map_err(MySqlDropViewError::Engine)?;
        match tables.iter().find(|table| table.name() == name.as_str()) {
            None => return Err(MySqlDropViewError::MissingView),
            Some(table) if table.kind() != MySqlTableKind::View => {
                return Err(MySqlDropViewError::NotView);
            }
            Some(_) => {}
        }
        let stmt = Stmt::DropView {
            if_exists: false,
            view_name: turso_parser::ast::QualifiedName::single(turso_parser::ast::Name::exact(
                name.as_str().to_owned(),
            )),
        };
        let sql = format!("DROP VIEW \"{}\"", name.as_str().replace('"', "\"\""));
        let result = self
            .inner
            .prepare_translated_stmt(stmt, &sql)
            .and_then(|mut statement| statement.run_ignore_rows())
            .map_err(MySqlDropViewError::Engine);
        if !self.inner.get_auto_commit() {
            self.inner
                .prepare("ROLLBACK")
                .and_then(|mut statement| statement.run_ignore_rows())
                .map_err(MySqlDropViewError::Engine)?;
        }
        result
    }

    /// Drops one checked table, committing preceding work before checking its existence.
    pub fn drop_table(
        &self,
        command: &MySqlDropTableCommand,
    ) -> std::result::Result<MySqlDropTableResult, MySqlDropTableError> {
        if !self.inner.get_auto_commit() {
            self.inner
                .prepare("COMMIT")
                .and_then(|mut statement| statement.run_ignore_rows())
                .map_err(MySqlDropTableError::Engine)?;
        }
        let tables = self.list_tables().map_err(MySqlDropTableError::Engine)?;
        match tables.iter().find(|table| table.name() == command.table().as_str()) {
            None if command.if_exists() => {
                return Ok(MySqlDropTableResult { dropped: false });
            }
            None => return Err(MySqlDropTableError::MissingTable),
            Some(table) if table.kind() != MySqlTableKind::BaseTable => {
                if command.if_exists() {
                    return Ok(MySqlDropTableResult { dropped: false });
                }
                return Err(MySqlDropTableError::MissingTable);
            }
            Some(_) => {}
        }
        let stmt = Stmt::DropTable {
            if_exists: false,
            tbl_name: turso_parser::ast::QualifiedName::single(turso_parser::ast::Name::exact(
                command.table().as_str().to_owned(),
            )),
        };
        let sql = format!(
            "DROP TABLE \"{}\"",
            command.table().as_str().replace('"', "\"\"")
        );
        let result = self
            .inner
            .prepare_translated_stmt(stmt, &sql)
            .and_then(|mut statement| statement.run_ignore_rows())
            .map_err(MySqlDropTableError::Engine);
        if !self.inner.get_auto_commit() {
            self.inner
                .prepare("ROLLBACK")
                .and_then(|mut statement| statement.run_ignore_rows())
                .map_err(MySqlDropTableError::Engine)?;
        }
        result.map(|_| MySqlDropTableResult { dropped: true })
    }

    fn prepare_auto_increment_create_table(
        &self,
        checked: CheckedAutoIncrementCreateTable,
    ) -> Result<Statement> {
        let database_identity = self
            .inner
            .schema_catalog_validation_context()
            .ok_or_else(|| {
                LimboError::ParseError(
                    "AUTO_INCREMENT requires a registry-backed durable database identity"
                        .to_string(),
                )
            })?
            .database_identity()
            .to_owned();
        let metadata = SchemaSqlV2Metadata::new(database_identity, new_allocator_identity()?)
            .map_err(|error| LimboError::InternalError(error.to_string()))?;
        let formatter = AutoIncrementSchemaSqlFormatter {
            context: self.schema_context,
            metadata,
            normalized_mysql_ddl: checked.normalized_mysql_ddl.clone(),
            sqlite_statement: checked.sqlite_statement.clone(),
        };
        let options = PrepareOptions::default()
            .with_reprepare_parser(Arc::new(FrozenAutoIncrementDdlParser {
                mode: self.parser_mode(),
            }))
            .with_schema_sql_formatter(Arc::new(formatter));
        self.inner.prepare_translated_stmt_with_options(
            checked.sqlite_statement,
            &checked.normalized_mysql_ddl,
            &options,
        )
    }

    fn prepare_checked_primary_key_create_table(
        &self,
        checked: CheckedPrimaryKeyCreateTable,
    ) -> Result<Statement> {
        let options = PrepareOptions::default()
            .with_reprepare_parser(Arc::new(FrozenSchemaDdlParser {
                mode: self.parser_mode(),
            }))
            .with_schema_sql_formatter(Arc::new(self.schema_context));
        self.inner.prepare_translated_stmt_with_options(
            checked.sqlite_statement,
            &checked.normalized_mysql_ddl,
            &options,
        )
    }

    /// Prepare one statement from the checked MySQL `SELECT` subset.
    ///
    /// The returned error preserves whether failure happened before or after
    /// checked translation. Protocol adapters need this boundary because core
    /// engine errors must not be guessed to be MySQL syntax errors.
    pub fn prepare_select(&self, sql: &str) -> std::result::Result<Statement, MySqlQueryError> {
        self.prepare_select_with_metadata(sql)
            .map(|(statement, _)| statement)
    }

    /// Prepares one checked MySQL `SELECT` and retains static expression metadata.
    pub fn prepare_select_with_metadata(
        &self,
        sql: &str,
    ) -> std::result::Result<
        (Statement, Vec<Option<StaticSelectMetadata>>),
        MySqlQueryError,
    > {
        let translated = self.parse_select_knowing_column_types(sql)?;
        Self::reject_internal_catalog_select(&translated)?;
        Self::reject_raw_select_comparisons(&translated)?;
        self.validate_select_comparison_columns(
            translated.source_table(),
            translated.checked_comparisons(),
        )
        .map_err(|error| MySqlQueryError::Unsupported(error.to_string()))?;
        self.validate_subquery_comparison_columns(
            translated.source_table(),
            translated.checked_subquery_comparisons(),
        )
        .map_err(|error| MySqlQueryError::Unsupported(error.to_string()))?;
        if translated.reads_table() {
            self.begin_implicit_transaction_for_table_read()?;
        }
        let stmt = translated
            .parse_ast()
            .map_err(|error| MySqlQueryError::Syntax(error.to_string()))?;
        let mode = self.parser_mode();
        let options =
            PrepareOptions::default().with_reprepare_parser(Arc::new(FrozenSelectParser {
                mode,
                source_table: translated.source_table().map(str::to_owned),
                checked_comparisons: translated.checked_comparisons().to_vec(),
            }));
        let stmt = self
            .inner
            .prepare_translated_stmt_with_options(stmt, sql, &options)
            .map_err(MySqlQueryError::Engine)?;
        if stmt.parameters_count() != translated.parameter_count() {
            return Err(MySqlQueryError::Engine(LimboError::InternalError(
                "checked SELECT parameter count changed during prepare".to_string(),
            )));
        }
        let static_result_metadata =
            aligned_static_result_metadata(&stmt, translated.static_result_metadata());
        Ok((stmt, static_result_metadata))
    }

    fn prepare_non_schema(&self, sql: &str) -> Result<Statement> {
        let mode = self.parser_mode();
        match parse_dml(sql, mode) {
            Ok(translated) => {
                self.validate_select_comparison_columns(
                    translated.source_table(),
                    translated.checked_comparisons(),
                )?;
                let stmt = translated
                    .parse_ast()
                    .map_err(|error| LimboError::ParseError(error.to_string()))?;
                let options = PrepareOptions::default()
                    .with_reprepare_parser(Arc::new(FrozenDmlParser { mode }));
                self.inner
                    .prepare_translated_stmt_with_options(stmt, sql, &options)
            }
            Err(MySqlParseError::ExpectedDml) => self.prepare_select(sql).map_err(Into::into),
            Err(error) => Err(LimboError::ParseError(error.to_string())),
        }
    }

    fn reject_internal_catalog_select(
        translated: &turso_mysql_parser::TranslatedSelect,
    ) -> std::result::Result<(), MySqlQueryError> {
        if translated
            .source_tables()
            .iter()
            .any(|source| turso_core::schema::is_system_table(source.table().as_str()))
        {
            return Err(MySqlQueryError::Unsupported(
                "SELECT from an internal catalog is unsupported".to_string(),
            ));
        }
        Ok(())
    }

    fn reject_raw_select_comparisons(
        translated: &turso_mysql_parser::TranslatedSelect,
    ) -> std::result::Result<(), MySqlQueryError> {
        if translated.checked_comparisons().iter().any(|comparison| {
            matches!(
                comparison.rhs(),
                CheckedSelectComparisonRhs::Placeholder { .. }
            )
        }) {
            return Err(MySqlQueryError::Unsupported(
                "SELECT comparison parameters require the checked prepared-statement API"
                    .to_string(),
            ));
        }
        Ok(())
    }

    /// Holds an `IN (SELECT ...)` to the rule a literal comparison obeys.
    ///
    /// MySQL compares the two columns by coercing one to the other's type and
    /// the engine compares them by affinity, so the two can name different
    /// rows. Requiring both to be the same kind is what removes the question.
    fn validate_subquery_comparison_columns(
        &self,
        source_table: Option<&str>,
        comparisons: &[CheckedSubqueryComparison],
    ) -> Result<()> {
        for comparison in comparisons {
            let source_table = source_table.ok_or_else(|| {
                LimboError::InvalidArgument(
                    "SELECT IN requires a table column on its left".to_string(),
                )
            })?;
            let outer = self.column_kind(source_table, comparison.column_name())?;
            let inner =
                self.column_kind(comparison.inner_table(), comparison.inner_column_name())?;
            if outer != inner {
                return Err(LimboError::InvalidArgument(format!(
                    "SELECT IN compares {} with {}, whose types are not the same kind",
                    comparison.column_name(),
                    comparison.inner_column_name()
                )));
            }
        }
        Ok(())
    }

    /// Returns whether one column holds signed integers or text, refusing the
    /// types this has no comparison rule for.
    fn column_kind(&self, table: &str, column_name: &str) -> Result<ColumnKind> {
        let table = MySqlTableName::parse(table)
            .map_err(|error| LimboError::ParseError(error.to_string()))?;
        let columns = self.list_columns(&table).map_err(|error| match error {
            MySqlColumnMetadataError::Engine(error) => error,
            MySqlColumnMetadataError::TableNotFound => LimboError::SchemaUpdated,
            MySqlColumnMetadataError::CorruptDefinition => {
                LimboError::Corrupt("invalid SELECT table metadata".to_string())
            }
            MySqlColumnMetadataError::UnsupportedDefinition => {
                LimboError::ParseError("unsupported SELECT table metadata".to_string())
            }
        })?;
        let column = columns
            .iter()
            .find(|column| column.name().eq_ignore_ascii_case(column_name))
            .ok_or(LimboError::SchemaUpdated)?;
        if is_integer_type(column.type_name()) {
            return Ok(ColumnKind::Integer);
        }
        if is_text_type(column.type_name()) {
            return Ok(ColumnKind::Text);
        }
        Err(LimboError::InvalidArgument(format!(
            "SELECT IN requires a signed integer or text column, found {}",
            column.type_name()
        )))
    }

    /// Parses a checked `SELECT`, telling the parser which columns are text
    /// when that changes how the statement renders.
    ///
    /// An `ORDER BY` over a bare column and a comparison against a `?` are the
    /// two places it depends on it, and only those are parsed a second time —
    /// MySQL compares and orders text without regard to case, and the engine
    /// will not unless it is asked to.
    fn parse_select_knowing_column_types(
        &self,
        sql: &str,
    ) -> std::result::Result<turso_mysql_parser::TranslatedSelect, MySqlQueryError> {
        let mode = self.parser_mode();
        let translated =
            parse_select(sql, mode).map_err(|error| MySqlQueryError::Syntax(error.to_string()))?;
        if !translated.needs_column_types() {
            return Ok(translated);
        }
        let Some(source_table) = translated.source_table() else {
            return Ok(translated);
        };
        let Ok(table) = MySqlTableName::parse(source_table) else {
            return Ok(translated);
        };
        let Ok(columns) = self.list_columns(&table) else {
            return Ok(translated);
        };
        let text_columns = columns
            .iter()
            .filter(|column| is_text_type(column.type_name()))
            .map(|column| column.name().to_owned())
            .collect::<Vec<_>>();
        if text_columns.is_empty() {
            return Ok(translated);
        }
        turso_mysql_parser::parse_select_with_text_columns(sql, mode, &text_columns)
            .map_err(|error| MySqlQueryError::Syntax(error.to_string()))
    }

    fn validate_select_comparison_columns(
        &self,
        source_table: Option<&str>,
        comparisons: &[CheckedSelectComparison],
    ) -> Result<()> {
        if comparisons.is_empty() {
            return Ok(());
        }
        let source_table = source_table.ok_or_else(|| {
            LimboError::InvalidArgument(
                "SELECT comparison requires a table column as its left operand".to_string(),
            )
        })?;
        let table = MySqlTableName::parse(source_table)
            .map_err(|error| LimboError::ParseError(error.to_string()))?;
        let columns = self.list_columns(&table).map_err(|error| match error {
            MySqlColumnMetadataError::Engine(error) => error,
            MySqlColumnMetadataError::TableNotFound => LimboError::SchemaUpdated,
            MySqlColumnMetadataError::CorruptDefinition => {
                LimboError::Corrupt("invalid SELECT table metadata".to_string())
            }
            MySqlColumnMetadataError::UnsupportedDefinition => {
                LimboError::ParseError("unsupported SELECT table metadata".to_string())
            }
        })?;
        for comparison in comparisons {
            let mut matching = columns
                .iter()
                .filter(|column| column.name().eq_ignore_ascii_case(comparison.column_name()));
            let Some(column) = matching.next() else {
                return Err(LimboError::SchemaUpdated);
            };
            if matching.next().is_some() {
                return Err(LimboError::Corrupt(
                    "duplicate SELECT comparison column metadata".to_string(),
                ));
            }
            if !checked_comparison_fits_column(
                comparison.rhs(),
                column.type_name(),
                comparison.collated(),
            ) {
                return Err(checked_comparison_column_refusal(
                    comparison.rhs(),
                    comparison.column_name(),
                    column.type_name(),
                ));
            }
        }
        Ok(())
    }

    fn validate_select_comparison_values(
        comparisons: &[CheckedSelectComparison],
        values: &[MySqlPreparedValue],
    ) -> Result<()> {
        for comparison in comparisons {
            let CheckedSelectComparisonRhs::Placeholder { ordinal } = comparison.rhs() else {
                continue;
            };
            let value = values.get(*ordinal).ok_or_else(|| {
                LimboError::InternalError(
                    "SELECT comparison placeholder is outside prepared parameters".to_string(),
                )
            })?;
            // A collated comparison names a text column, which is the one
            // that takes a string.
            let fits = match value {
                MySqlPreparedValue::Integer(_) | MySqlPreparedValue::Null => true,
                MySqlPreparedValue::Text(_) => comparison.collated(),
                _ => false,
            };
            if !fits {
                return Err(LimboError::InvalidArgument(format!(
                    "SELECT comparison parameter for {} does not fit the column's type",
                    comparison.column_name()
                )));
            }
        }
        Ok(())
    }

    pub fn execute(&self, sql: &str) -> Result<()> {
        match parse_auto_increment_insert(sql, self.parser_mode()) {
            Ok(insert) => match self.load_auto_increment_table(insert.table_name().as_str())? {
                Some(table) => self.execute_auto_increment_insert(sql, insert, table),
                None => self.prepare(sql)?.run_ignore_rows(),
            },
            Err(_) => {
                if let Some(target) = parse_auto_increment_insert_target(sql, self.parser_mode())
                    .map_err(|error| LimboError::ParseError(error.to_string()))?
                {
                    if self.load_auto_increment_table(&target)?.is_some() {
                        return Err(LimboError::ParseError(
                            "AUTO_INCREMENT INSERT supports only an explicit column list and direct literal VALUES rows".to_string(),
                        ));
                    }
                }
                self.prepare(sql)?.run_ignore_rows()
            }
        }
    }

    pub fn execute_checked_write(
        &self,
        sql: &str,
        timeout: Option<Duration>,
    ) -> std::result::Result<MySqlWriteResult, MySqlQueryError> {
        self.execute_checked_write_with_affected_rows_mode(
            sql,
            timeout,
            MySqlAffectedRowsMode::Changed,
        )
    }

    /// Executes one checked DML statement and returns the selected MySQL
    /// affected-row count.
    pub fn execute_checked_write_with_affected_rows_mode(
        &self,
        sql: &str,
        timeout: Option<Duration>,
        affected_rows_mode: MySqlAffectedRowsMode,
    ) -> std::result::Result<MySqlWriteResult, MySqlQueryError> {
        self.reject_write_in_read_only_transaction()?;
        let deadline = self.write_deadline(timeout);
        self.check_write_deadline(deadline)?;
        self.begin_implicit_transaction_for_write()?;
        match parse_auto_increment_insert(sql, self.parser_mode()) {
            Ok(insert) => match self
                .load_auto_increment_table(insert.table_name().as_str())
                .map_err(MySqlQueryError::Engine)?
            {
                Some(table) => {
                    self.check_write_deadline(deadline)?;
                    let id = self
                        .execute_auto_increment_insert_with_deadline(sql, insert, table, deadline)
                        .map_err(MySqlQueryError::Engine)?;
                    Ok(MySqlWriteResult {
                        affected_rows: self.affected_rows(false, affected_rows_mode)?,
                        last_insert_id: id,
                    })
                }
                None => self.execute_ordinary_checked_write(sql, deadline, affected_rows_mode),
            },
            Err(_) => {
                if let Some(target) = parse_auto_increment_insert_target(sql, self.parser_mode())
                    .map_err(mysql_query_parse_error)?
                {
                    if self
                        .load_auto_increment_table(&target)
                        .map_err(MySqlQueryError::Engine)?
                        .is_some()
                    {
                        self.check_write_deadline(deadline)?;
                        return Err(MySqlQueryError::Unsupported(
                            "AUTO_INCREMENT INSERT supports only an explicit column list and direct literal VALUES rows".to_string(),
                        ));
                    }
                }
                self.execute_ordinary_checked_write(sql, deadline, affected_rows_mode)
            }
        }
    }

    fn execute_ordinary_checked_write(
        &self,
        sql: &str,
        deadline: Option<turso_core::MonotonicInstant>,
        affected_rows_mode: MySqlAffectedRowsMode,
    ) -> std::result::Result<MySqlWriteResult, MySqlQueryError> {
        let mode = self.parser_mode();
        let translated = parse_dml(sql, mode).map_err(mysql_query_parse_error)?;
        // A DML `WHERE` is held to the rule a `SELECT` `WHERE` obeys, so the
        // rows a comparison names cannot depend on the statement asking.
        self.validate_select_comparison_columns(
            translated.source_table(),
            translated.checked_comparisons(),
        )
        .map_err(|error| MySqlQueryError::Unsupported(error.to_string()))?;
        if let Some(update) = translated.checked_update() {
            if let Some(table) = self
                .load_auto_increment_table(update.table_name())
                .map_err(MySqlQueryError::Engine)?
            {
                let allocator_column = &table.definition.allocator_column_name;
                for assignment in update.assignments().iter().filter(|assignment| {
                    assignment
                        .column_name()
                        .eq_ignore_ascii_case(allocator_column)
                }) {
                    match assignment.value() {
                        CheckedUpdateAssignmentValue::SelfAssignment => {}
                        CheckedUpdateAssignmentValue::SignedInteger(value) if value > 0 => {
                            self.advance_auto_increment_past(&table, value as u64, deadline)?;
                        }
                        CheckedUpdateAssignmentValue::SignedInteger(_) => {}
                        CheckedUpdateAssignmentValue::Other => {
                            return Err(MySqlQueryError::Unsupported(
                                "AUTO_INCREMENT column updates require a direct integer literal"
                                    .to_string(),
                            ));
                        }
                    }
                }
            }
        }
        let statement = translated
            .parse_ast()
            .map_err(|error| MySqlQueryError::Syntax(error.to_string()))?;
        let is_update = matches!(statement, Stmt::Update(_));
        let insert_target = checked_insert_target(&statement).map_err(MySqlQueryError::Engine)?;
        let options =
            PrepareOptions::default().with_reprepare_parser(Arc::new(FrozenDmlParser { mode }));
        let mut statement = self
            .inner
            .prepare_translated_stmt_with_options(statement, sql, &options)
            .map_err(MySqlQueryError::Engine)?;
        if let Some(target) = insert_target {
            self.check_write_deadline(deadline)?;
            let missing = self
                .missing_required_insert_column(&target, &[])
                .map_err(MySqlQueryError::Engine)?;
            self.check_write_deadline(deadline)?;
            if let Some(column) = missing {
                return Err(MySqlQueryError::MissingRequiredDefault(column));
            }
        }
        let timeout = self.remaining_write_timeout(deadline)?;
        run_checked_write_statement(&mut statement, timeout).map_err(MySqlQueryError::Engine)?;
        Ok(MySqlWriteResult {
            affected_rows: self.affected_rows(is_update, affected_rows_mode)?,
            last_insert_id: 0,
        })
    }

    /// The `DEFAULT VALUES` form keeps going through `list_columns`, so it still
    /// refuses tables whose metadata this frontend cannot describe. An ordinary
    /// INSERT into such a table works; an empty-row one does not.
    fn missing_insert_default(&self, table: &MySqlTableName) -> Result<Option<String>> {
        let columns = self.list_columns(table).map_err(|error| match error {
            MySqlColumnMetadataError::Engine(error) => error,
            MySqlColumnMetadataError::TableNotFound => LimboError::SchemaUpdated,
            MySqlColumnMetadataError::CorruptDefinition => {
                LimboError::Corrupt("invalid INSERT table metadata".into())
            }
            MySqlColumnMetadataError::UnsupportedDefinition => {
                LimboError::ParseError("unsupported INSERT table metadata".into())
            }
        })?;
        Ok(columns
            .into_iter()
            .find(|column| !column.nullable && column.default_value.is_none())
            .map(|column| column.name))
    }

    /// Returns the column MySQL would name in error 1364, if the INSERT leaves
    /// a required column without a value.
    ///
    /// MySQL stores the values it was given first, so an explicit NULL in a NOT
    /// NULL column raises 1048 and suppresses the 1364 check entirely. Only
    /// when nothing hands a NULL to a required column does it report the first
    /// required column the statement never lists, in table definition order.
    fn missing_required_insert_column(
        &self,
        target: &CheckedInsertTarget,
        bound: &[MySqlPreparedValue],
    ) -> Result<Option<String>> {
        match target {
            CheckedInsertTarget::DefaultValues(table) => self.missing_insert_default(table),
            CheckedInsertTarget::Listed(insert) => {
                let rules = self.insert_column_rules(&insert.table)?;
                if insert.first_row_hands_null_to_a_not_null_column(&rules.not_null, bound) {
                    return Ok(None);
                }
                Ok(rules.required.into_iter().find(|name| !insert.lists(name)))
            }
        }
    }

    /// The two column lists the NOT NULL rules need, both in table definition
    /// order.
    ///
    /// This reads the core schema instead of going through `list_columns`,
    /// whose stricter metadata shape rejects tables an ordinary INSERT may
    /// legitimately use, such as one carrying an index.
    fn insert_column_rules(&self, table: &MySqlTableName) -> Result<InsertColumnRules> {
        let schema = self.inner.current_schema();
        let core_table = schema
            .get_table(table.as_str())
            .ok_or(LimboError::SchemaUpdated)?;
        let mut rules = InsertColumnRules::default();
        for column in core_table.columns() {
            // A rowid alias and a generated column are filled in by the engine,
            // so the statement never has to name either one.
            if !column.notnull() || column.is_rowid_alias() || column.is_generated() {
                continue;
            }
            let Some(name) = column.name.clone() else {
                continue;
            };
            if column.default.is_none() {
                rules.required.push(name.clone());
            }
            rules.not_null.push(name);
        }
        Ok(rules)
    }

    fn affected_rows(
        &self,
        is_update: bool,
        affected_rows_mode: MySqlAffectedRowsMode,
    ) -> std::result::Result<u64, MySqlQueryError> {
        let rows = match (is_update, affected_rows_mode) {
            (true, MySqlAffectedRowsMode::Changed) => self.inner.mysql_changed_rows(),
            _ => self.inner.changes(),
        };
        u64::try_from(rows).map_err(|_| {
            MySqlQueryError::Engine(LimboError::InternalError(
                "successful MySQL write produced a negative affected-row count".to_string(),
            ))
        })
    }

    fn advance_auto_increment_past(
        &self,
        table: &AutoIncrementTable,
        high_water: u64,
        deadline: Option<turso_core::MonotonicInstant>,
    ) -> std::result::Result<(), MySqlQueryError> {
        if high_water > auto_increment_ceiling(table) {
            return Err(MySqlQueryError::Engine(LimboError::Constraint(
                "AUTO_INCREMENT value is outside the column's type".to_string(),
            )));
        }
        let capability = self.auto_increment.as_ref().ok_or_else(|| {
            MySqlQueryError::Unsupported(
                "AUTO_INCREMENT update requires a registry-backed allocator capability".to_string(),
            )
        })?;
        self.check_write_deadline(deadline)?;
        let mut operation = capability
            .allocator
            .advance_past(table.key, high_water)
            .map_err(MySqlQueryError::Engine)?;
        capability
            .io
            .block(|| operation.step())
            .map_err(MySqlQueryError::Engine)?;
        self.check_write_deadline(deadline)
    }

    fn write_deadline(&self, timeout: Option<Duration>) -> Option<turso_core::MonotonicInstant> {
        timeout.map(|duration| {
            self.auto_increment
                .as_ref()
                .map(|capability| capability.io.current_time_monotonic())
                .unwrap_or_else(turso_core::MonotonicInstant::now)
                + duration
        })
    }

    fn check_write_deadline(
        &self,
        deadline: Option<turso_core::MonotonicInstant>,
    ) -> std::result::Result<(), MySqlQueryError> {
        if let Some(deadline) = deadline {
            let now = self
                .auto_increment
                .as_ref()
                .map(|capability| capability.io.current_time_monotonic())
                .unwrap_or_else(turso_core::MonotonicInstant::now);
            if now >= deadline {
                return Err(MySqlQueryError::Engine(LimboError::Interrupt));
            }
        }
        Ok(())
    }

    fn remaining_write_timeout(
        &self,
        deadline: Option<turso_core::MonotonicInstant>,
    ) -> std::result::Result<Option<Duration>, MySqlQueryError> {
        let Some(deadline) = deadline else {
            return Ok(None);
        };
        let now = self
            .auto_increment
            .as_ref()
            .map(|capability| capability.io.current_time_monotonic())
            .unwrap_or_else(turso_core::MonotonicInstant::now);
        if now >= deadline {
            return Err(MySqlQueryError::Engine(LimboError::Interrupt));
        }
        Ok(Some(deadline.duration_since(now)))
    }

    fn execute_auto_increment_insert(
        &self,
        sql: &str,
        insert: turso_mysql_parser::CheckedAutoIncrementInsert,
        table: AutoIncrementTable,
    ) -> Result<()> {
        self.execute_auto_increment_insert_with_deadline(sql, insert, table, None)?;
        Ok(())
    }

    fn execute_auto_increment_insert_with_deadline(
        &self,
        sql: &str,
        insert: turso_mysql_parser::CheckedAutoIncrementInsert,
        table: AutoIncrementTable,
        deadline: Option<turso_core::MonotonicInstant>,
    ) -> Result<u64> {
        self.reject_insert_target_triggers(&table.name)?;
        self.check_write_deadline(deadline)
            .map_err(Into::<LimboError>::into)?;
        let bound = insert
            .bind_allocator_table(&table.definition)
            .map_err(|error| LimboError::ParseError(error.to_string()))?;
        let capability = self.auto_increment.as_ref().ok_or_else(|| {
            LimboError::ParseError(
                "AUTO_INCREMENT INSERT requires a registry-backed allocator capability".to_string(),
            )
        })?;
        let count = u64::try_from(bound.row_count().get()).map_err(|_| {
            LimboError::InvalidArgument("AUTO_INCREMENT INSERT row count is too large".to_string())
        })?;
        let mut reservation = capability.allocator.reserve(table.key, count)?;
        let range = capability.io.block(|| reservation.step())?;
        self.check_write_deadline(deadline)
            .map_err(Into::<LimboError>::into)?;
        let expected_last = range
            .first()
            .checked_add(count - 1)
            .ok_or(LimboError::IntegerOverflow)?;
        if range.first() == 0
            || range.last() != expected_last
            || range.last() > auto_increment_ceiling(&table)
        {
            return Err(LimboError::Corrupt(
                "AUTO_INCREMENT allocator returned a range outside the column's type".to_string(),
            ));
        }
        let statement = bound
            .inject_reserved_range(range.first())
            .map_err(|error| LimboError::ParseError(error.to_string()))?;
        let options = PrepareOptions::default()
            .with_reprepare_parser(Arc::new(FrozenInjectedAutoIncrementInsertParser {
                statement: statement.clone(),
            }))
            .with_assignment_validator(Arc::new(InjectedAutoIncrementAssignmentValidator {
                table_name: table.name,
                table_sql: table.stored_sql,
                allocator_column_ordinal: table.definition.allocator_column_ordinal,
            }));
        let mut statement = self
            .inner
            .prepare_translated_stmt_with_options(statement, sql, &options)?;
        let timeout = self
            .remaining_write_timeout(deadline)
            .map_err(Into::<LimboError>::into)?;
        run_checked_write_statement(&mut statement, timeout)?;
        self.inner.set_mysql_last_insert_id(range.first());
        Ok(range.first())
    }

    fn load_auto_increment_table(&self, target: &str) -> Result<Option<AutoIncrementTable>> {
        let rows = self
            .inner
            .prepare("SELECT name, sql FROM sqlite_schema WHERE type = 'table'")?
            .run_collect_rows()?;
        for row in rows {
            let [name, sql] = row.as_slice() else {
                return Err(LimboError::InternalError(
                    "sqlite_schema table row has an invalid shape".to_string(),
                ));
            };
            let name = name.to_string().trim_matches('\'').to_owned();
            if !name.eq_ignore_ascii_case(target) {
                continue;
            }
            let sql = sql.to_string();
            let Some(decoded) = decode_schema_sql(SchemaSqlKind::Table, sql.trim_matches('\''))
                .map_err(|error| LimboError::Corrupt(error.to_string()))?
            else {
                return Ok(None);
            };
            let Some(metadata) = decoded.v2_metadata() else {
                return Ok(None);
            };
            let expected_database_identity = self
                .inner
                .schema_catalog_validation_context()
                .ok_or_else(|| {
                    LimboError::Corrupt(
                        "AUTO_INCREMENT table has no durable database identity".to_string(),
                    )
                })?
                .database_identity();
            if metadata.database_id.into_bytes() != *expected_database_identity {
                return Err(LimboError::Corrupt(
                    "AUTO_INCREMENT table belongs to a different durable database".to_string(),
                ));
            }
            let definition = parse_auto_increment_create_table(
                decoded.normalized_ddl,
                SessionSqlMode {
                    ansi_quotes: decoded.context.sql_mode.ansi_quotes,
                    no_backslash_escapes: decoded.context.sql_mode.no_backslash_escapes,
                },
            )
            .map_err(|_| {
                LimboError::Corrupt(
                    "AUTO_INCREMENT table has an invalid durable definition".to_string(),
                )
            })?;
            if !definition.table_name.eq_ignore_ascii_case(&name)
                || !definition.table_name.eq_ignore_ascii_case(target)
            {
                return Err(LimboError::Corrupt(
                    "AUTO_INCREMENT table definition does not match its catalog name".to_string(),
                ));
            }
            let key = AutoIncrementKey::new(metadata.allocator_id.into_bytes()).map_err(|_| {
                LimboError::Corrupt(
                    "AUTO_INCREMENT table has an invalid allocator identity".to_string(),
                )
            })?;
            return Ok(Some(AutoIncrementTable {
                name,
                definition,
                key,
                stored_sql: sql.trim_matches('\'').to_owned(),
            }));
        }
        Ok(None)
    }

    fn reject_insert_target_triggers(&self, target: &str) -> Result<()> {
        let rows = self
            .inner
            .prepare("SELECT tbl_name FROM sqlite_schema WHERE type = 'trigger'")?
            .run_collect_rows()?;
        for row in rows {
            let Some(table_name) = row.first() else {
                return Err(LimboError::InternalError(
                    "sqlite_schema trigger row is missing its table name".to_string(),
                ));
            };
            if table_name
                .to_string()
                .trim_matches('\'')
                .eq_ignore_ascii_case(target)
            {
                return Err(LimboError::ParseError(
                    "AUTO_INCREMENT INSERT is not supported for a table with triggers".to_string(),
                ));
            }
        }
        Ok(())
    }

    pub fn parser_mode(&self) -> SessionSqlMode {
        SessionSqlMode {
            ansi_quotes: self.schema_context.sql_mode.ansi_quotes,
            no_backslash_escapes: self.schema_context.sql_mode.no_backslash_escapes,
        }
    }

    fn reject_alter_with_marked_view(&self, body: &AlterTableBody) -> Result<()> {
        let operation = match body {
            AlterTableBody::AddColumn(_) => return Ok(()),
            AlterTableBody::DropColumn(_) => "ALTER TABLE DROP COLUMN",
            AlterTableBody::RenameTo(_) => "ALTER TABLE RENAME TO",
            AlterTableBody::RenameColumn { .. } => "ALTER TABLE RENAME COLUMN",
            AlterTableBody::AlterColumn { .. } => "ALTER TABLE ALTER COLUMN",
        };
        let rows = self
            .inner
            .prepare("SELECT sql FROM sqlite_schema WHERE type = 'view'")?
            .run_collect_rows()?;
        for row in rows {
            let Some(sql) = row.first() else {
                return Err(LimboError::InternalError(
                    "sqlite_schema view row is missing SQL".to_string(),
                ));
            };
            let sql = sql.to_string();
            if decode_schema_sql_any(sql.trim_matches('\''))
                .map_err(|error| LimboError::Corrupt(error.to_string()))?
                .is_some_and(|decoded| decoded.context.kind == turso_core::SchemaSqlKind::View)
            {
                return Err(LimboError::ParseError(format!(
                    "{operation} is not supported while a MySQL-marked view exists"
                )));
            }
        }
        Ok(())
    }

    fn reject_alter_with_auto_increment_table(
        &self,
        target: &turso_parser::ast::Name,
    ) -> Result<()> {
        let rows = self
            .inner
            .prepare("SELECT name, sql FROM sqlite_schema WHERE type = 'table'")?
            .run_collect_rows()?;
        for row in rows {
            let [name, sql] = row.as_slice() else {
                return Err(LimboError::InternalError(
                    "sqlite_schema table row has an invalid shape".to_string(),
                ));
            };
            if !name
                .to_string()
                .trim_matches('\'')
                .eq_ignore_ascii_case(target.as_str())
            {
                continue;
            }
            let sql = sql.to_string();
            if decode_schema_sql_any(sql.trim_matches('\''))
                .map_err(|error| LimboError::Corrupt(error.to_string()))?
                .is_some_and(|decoded| decoded.v2_metadata().is_some())
            {
                return Err(LimboError::ParseError(
                    "ALTER TABLE is not supported for an AUTO_INCREMENT table".to_string(),
                ));
            }
        }
        Ok(())
    }

    fn reject_alter_with_marked_trigger(&self) -> Result<()> {
        let rows = self
            .inner
            .prepare("SELECT sql FROM sqlite_schema WHERE type = 'trigger'")?
            .run_collect_rows()?;
        for row in rows {
            let Some(sql) = row.first() else {
                return Err(LimboError::InternalError(
                    "sqlite_schema trigger row is missing SQL".to_string(),
                ));
            };
            let sql = sql.to_string();
            if decode_schema_sql_any(sql.trim_matches('\''))
                .map_err(|error| LimboError::Corrupt(error.to_string()))?
                .is_some_and(|decoded| decoded.context.kind == turso_core::SchemaSqlKind::Trigger)
            {
                return Err(LimboError::ParseError(
                    "ALTER TABLE is not supported while a MySQL-marked trigger exists".to_string(),
                ));
            }
        }
        Ok(())
    }

    fn reject_duplicate_marked_insert_trigger(&self, stmt: &Stmt) -> Result<()> {
        let Stmt::CreateTrigger { tbl_name, .. } = stmt else {
            unreachable!("checked CREATE TRIGGER statement");
        };
        let rows = self
            .inner
            .prepare("SELECT tbl_name, sql FROM sqlite_schema WHERE type = 'trigger'")?
            .run_collect_rows()?;
        for row in rows {
            let [table_name, _sql] = row.as_slice() else {
                return Err(LimboError::InternalError(
                    "sqlite_schema trigger row has an invalid shape".to_string(),
                ));
            };
            if table_name
                .to_string()
                .eq_ignore_ascii_case(tbl_name.name.as_str())
            {
                return Err(LimboError::ParseError(
                    "a trigger already exists for this table; MySQL trigger ordering is not supported"
                        .to_string(),
                ));
            }
        }
        Ok(())
    }
}

fn find_unquoted_sql_fragment(
    sql: &str,
    fragment: &str,
    no_backslash_escapes: bool,
) -> Option<usize> {
    let bytes = sql.as_bytes();
    let fragment = fragment.as_bytes();
    let mut quote = None;
    let mut index = 0;
    while index < bytes.len() {
        if let Some(delimiter) = quote {
            if delimiter == b'\'' && bytes[index] == b'\\' && !no_backslash_escapes {
                index = (index + 2).min(bytes.len());
            } else if bytes[index] == delimiter {
                if bytes.get(index + 1) == Some(&delimiter) {
                    index += 2;
                } else {
                    quote = None;
                    index += 1;
                }
            } else {
                index += 1;
            }
        } else if bytes[index] == b'\'' || bytes[index] == b'`' {
            quote = Some(bytes[index]);
            index += 1;
        } else if bytes[index..].starts_with(fragment) {
            return Some(index);
        } else {
            index += 1;
        }
    }
    None
}

fn prepared_statement_metadata(
    statement_id: u32,
    statement: &Statement,
) -> std::result::Result<MySqlPreparedStatementMetadata, MySqlPreparedStatementError> {
    let parameter_count = u16::try_from(statement.parameters_count()).map_err(|_| {
        MySqlPreparedStatementError::Prepare(MySqlQueryError::Unsupported(
            "prepared statement has more parameters than MySQL can represent".to_string(),
        ))
    })?;
    let result_column_count = u16::try_from(statement.num_columns()).map_err(|_| {
        MySqlPreparedStatementError::Prepare(MySqlQueryError::Unsupported(
            "prepared statement has more result columns than MySQL can represent".to_string(),
        ))
    })?;
    let result_columns = (0..usize::from(result_column_count))
        .map(|index| MySqlPreparedResultColumn {
            name: statement.get_column_name(index).into_owned(),
            type_name: statement
                .get_column_type_name(index)
                .or_else(|| statement.get_column_inferred_type(index)),
        })
        .collect();
    Ok(MySqlPreparedStatementMetadata {
        statement_id,
        parameter_count,
        result_columns,
    })
}

/// Keeps what the `?` columns settled on across a metadata rebuild.
///
/// The rebuild runs after every successful SELECT, not only after a schema
/// reprepare, and MySQL keeps a marker's inferred type across ordinary
/// executions and COM_STMT_RESET. The caller decides whether a reprepare
/// happened; this only copies the state over.
fn carry_parameter_markers(
    previous: &[MySqlPreparedResultColumnTypeMetadata],
    rebuilt: &mut [MySqlPreparedResultColumnTypeMetadata],
) {
    if previous.len() != rebuilt.len() {
        return;
    }

    for (old, new) in previous.iter().zip(rebuilt) {
        if let (Some(old_marker), Some(new_marker)) =
            (old.parameter_marker, new.parameter_marker.as_mut())
        {
            if old_marker.ordinal == new_marker.ordinal {
                new_marker.kind = old_marker.kind;
            }
        }
    }
}

/// Applies one execution's bound values to the `?` result columns.
///
/// A statement reset keeps this, matching COM_STMT_RESET, which MySQL leaves
/// the inferred type alone across.
fn observe_parameter_markers(
    result_column_type_metadata: &mut [MySqlPreparedResultColumnTypeMetadata],
    values: &[MySqlPreparedValue],
) {
    for column in result_column_type_metadata {
        if let Some(marker) = column.parameter_marker.as_mut() {
            marker.observe(values.get(marker.ordinal));
        }
    }
}

fn prepared_result_column_type_metadata(
    statement: &Statement,
    static_result_metadata: &[StaticSelectProjectionMetadata],
) -> Vec<MySqlPreparedResultColumnTypeMetadata> {
    let static_result_metadata = aligned_static_result_metadata(statement, static_result_metadata);
    (0..statement.num_columns())
        .map(|index| MySqlPreparedResultColumnTypeMetadata {
            declared_type_name: statement.get_column_decltype(index),
            static_metadata: static_result_metadata[index].clone(),
            source_reference: statement
                .get_column_source_reference(index)
                .map(|(table, ordinal)| (table.into_owned(), ordinal)),
            // A reprepare rebuilds this, which is how MySQL returns a marker to
            // generic after an automatic schema reprepare.
            parameter_marker: statement
                .get_column_parameter_ordinal(index)
                .map(|ordinal| ParameterMarker {
                    ordinal,
                    kind: MySqlMarkerType::Untyped,
                }),
        })
        .collect()
}

fn refresh_prepared_statement_entry(
    statement_id: u32,
    prepared: &mut PreparedStatement,
) -> std::result::Result<(), MySqlPreparedStatementError> {
    let Some(statement) = prepared.statement.as_ref() else {
        return Ok(());
    };
    let metadata = prepared_statement_metadata(statement_id, statement)?;
    let reprepares = statement.stmt_status(StatementStatusCounter::Reprepare);
    let mut result_column_type_metadata =
        prepared_result_column_type_metadata(statement, &prepared.static_result_projections);
    // A reprepare returns a `?` column to its generic type; an ordinary
    // execution leaves it alone.
    if reprepares == prepared.reprepares_at_last_refresh {
        carry_parameter_markers(
            &prepared.result_column_type_metadata,
            &mut result_column_type_metadata,
        );
    }
    prepared.reprepares_at_last_refresh = reprepares;
    prepared.metadata = metadata;
    prepared
        .result_column_type_metadata
        .clone_from(&result_column_type_metadata);
    Ok(())
}

fn aligned_static_result_metadata(
    statement: &Statement,
    projections: &[StaticSelectProjectionMetadata],
) -> Vec<Option<StaticSelectMetadata>> {
    let result_column_count = statement.num_columns();
    let no_static_metadata = || vec![None; result_column_count];
    let wildcard_count = projections
        .iter()
        .filter(|projection| matches!(projection, StaticSelectProjectionMetadata::Wildcard))
        .count();
    if wildcard_count == 0 {
        if projections.len() != result_column_count {
            return no_static_metadata();
        }
        return projections
            .iter()
            .map(|projection| match projection {
                StaticSelectProjectionMetadata::Literal(metadata) => Some(metadata.clone()),
                StaticSelectProjectionMetadata::Wildcard
                | StaticSelectProjectionMetadata::Other => None,
            })
            .collect();
    }
    if wildcard_count != 1 {
        return no_static_metadata();
    }
    let fixed_projection_count = projections.len() - 1;
    let wildcard_column_count = result_column_count.checked_sub(fixed_projection_count);
    let Some(wildcard_column_count) = wildcard_column_count else {
        return no_static_metadata();
    };
    let mut metadata = Vec::with_capacity(result_column_count);
    for projection in projections {
        match projection {
            StaticSelectProjectionMetadata::Literal(value) => metadata.push(Some(value.clone())),
            StaticSelectProjectionMetadata::Other => metadata.push(None),
            StaticSelectProjectionMetadata::Wildcard => {
                metadata.extend(std::iter::repeat_n(None, wildcard_column_count));
            }
        }
    }
    if metadata.len() == result_column_count {
        metadata
    } else {
        no_static_metadata()
    }
}

fn prepared_auto_increment_statement_metadata(
    statement_id: u32,
    execution_plan: &PreparedExecutionPlan,
) -> std::result::Result<MySqlPreparedStatementMetadata, MySqlPreparedStatementError> {
    let PreparedExecutionPlan::AutoIncrementInsert(insert) = execution_plan else {
        return Err(MySqlPreparedStatementError::Prepare(
            MySqlQueryError::Engine(LimboError::InternalError(
                "prepared statement metadata source is missing a core statement".to_string(),
            )),
        ));
    };
    let parameter_count = u16::try_from(insert.parameter_count).map_err(|_| {
        MySqlPreparedStatementError::Prepare(MySqlQueryError::Unsupported(
            "prepared statement has more parameters than MySQL can represent".to_string(),
        ))
    })?;
    Ok(MySqlPreparedStatementMetadata {
        statement_id,
        parameter_count,
        result_columns: Vec::new(),
    })
}

fn bind_prepared_values(statement: &mut Statement, values: &[Value]) -> Result<()> {
    for (index, value) in values.iter().enumerate() {
        let index =
            std::num::NonZero::new(index + 1).expect("prepared parameter index starts at one");
        statement.bind_at(index, value.clone())?;
    }
    Ok(())
}

fn mysql_prepared_value_to_core(value: &MySqlPreparedValue) -> Result<Value> {
    match value {
        MySqlPreparedValue::Null => Ok(Value::Null),
        MySqlPreparedValue::Integer(value) => Ok(Value::from_i64(*value)),
        MySqlPreparedValue::Real(value) => Ok(Value::from_f64(*value)),
        MySqlPreparedValue::Text(value) => Ok(Value::from_text(value.clone())),
        MySqlPreparedValue::Blob(value) => Value::from_slice(value).map_err(Into::into),
    }
}

fn mysql_prepared_value_from_core(value: Value) -> MySqlPreparedValue {
    match value {
        Value::Null => MySqlPreparedValue::Null,
        Value::Numeric(Numeric::Integer(value)) => MySqlPreparedValue::Integer(value),
        Value::Numeric(Numeric::Float(value)) => MySqlPreparedValue::Real(value.into()),
        Value::Text(value) => MySqlPreparedValue::Text(value.as_str().to_owned()),
        Value::Blob(value) => MySqlPreparedValue::Blob(value.to_vec()),
    }
}

fn mysql_metadata_parse_error(error: MySqlParseError) -> MySqlColumnMetadataError {
    if matches!(error, MySqlParseError::Unsupported { .. }) {
        MySqlColumnMetadataError::UnsupportedDefinition
    } else {
        MySqlColumnMetadataError::CorruptDefinition
    }
}

fn mysql_column_metadata(
    column: &turso_parser::ast::ColumnDefinition,
) -> std::result::Result<MySqlColumnMetadata, MySqlColumnMetadataError> {
    let data_type = column
        .col_type
        .as_ref()
        .ok_or(MySqlColumnMetadataError::UnsupportedDefinition)?;
    if data_type.array_dimensions != 0 {
        return Err(MySqlColumnMetadataError::UnsupportedDefinition);
    }
    let mut character_length = None;
    let mut decimal_size = None;
    // A VARBINARY carries a declared size the same way, and the same reader
    // recovers it; what differs is that the count is bytes rather than
    // characters, which is decided where the length is used rather than here.
    let sized_text = ["VARCHAR", "CHAR", "VARBINARY"]
        .into_iter()
        .find(|name| data_type.name.eq_ignore_ascii_case(name));
    let type_name = if let Some(sized_text) = sized_text {
        character_length = Some(
            turso_mysql_parser::stored_character_length(data_type)
                .map_err(|_| MySqlColumnMetadataError::UnsupportedDefinition)?,
        );
        sized_text
    } else if data_type.name.eq_ignore_ascii_case("DECIMAL") {
        decimal_size = Some(
            turso_mysql_parser::stored_decimal_size(data_type)
                .map_err(|_| MySqlColumnMetadataError::UnsupportedDefinition)?,
        );
        "DECIMAL"
    } else {
        if data_type.size.is_some() {
            return Err(MySqlColumnMetadataError::UnsupportedDefinition);
        }
        match data_type.name.as_str() {
            "TINYINT" => "TINYINT",
            "SMALLINT" => "SMALLINT",
            "MEDIUMINT" => "MEDIUMINT",
            "INT" => "INT",
            "INTEGER" => "INTEGER",
            "BIGINT" => "BIGINT",
            // The sign is part of the declared name rather than a flag beside
            // it, so it travels with the type through the stored DDL.
            "TINYINT UNSIGNED" => "TINYINT UNSIGNED",
            "SMALLINT UNSIGNED" => "SMALLINT UNSIGNED",
            "MEDIUMINT UNSIGNED" => "MEDIUMINT UNSIGNED",
            "INT UNSIGNED" => "INT UNSIGNED",
            "INTEGER UNSIGNED" => "INTEGER UNSIGNED",
            "TEXT" => "TEXT",
            "BLOB" => "BLOB",
            "DOUBLE" => "DOUBLE",
            "FLOAT" => "FLOAT",
            "BOOLEAN" => "BOOLEAN",
            "DATETIME" => "DATETIME",
            "TIMESTAMP" => "TIMESTAMP",
            _ => return Err(MySqlColumnMetadataError::UnsupportedDefinition),
        }
    };

    let mut nullable = true;
    let mut key = MySqlColumnKey::None;
    let mut default_sql = None;
    let mut default_value = None;
    for constraint in &column.constraints {
        match &constraint.constraint {
            ColumnConstraint::NotNull {
                nullable: true,
                conflict_clause: None,
            } => {}
            ColumnConstraint::NotNull {
                nullable: false,
                conflict_clause: None,
            } => nullable = false,
            ColumnConstraint::Unique(None) => {
                if key != MySqlColumnKey::None {
                    return Err(MySqlColumnMetadataError::UnsupportedDefinition);
                }
                key = MySqlColumnKey::Unique;
            }
            ColumnConstraint::PrimaryKey {
                order: None,
                conflict_clause: None,
                auto_increment: false,
            } => {
                if key != MySqlColumnKey::None {
                    return Err(MySqlColumnMetadataError::UnsupportedDefinition);
                }
                key = MySqlColumnKey::Primary;
                nullable = false;
            }
            ColumnConstraint::Default(expr) if constraint.name.is_none() => {
                if default_sql.is_some() {
                    return Err(MySqlColumnMetadataError::UnsupportedDefinition);
                }
                let (sql, value) = mysql_column_default(expr)?;
                default_sql = Some(sql);
                default_value = Some(value);
            }
            ColumnConstraint::Check { .. } => {}
            _ => return Err(MySqlColumnMetadataError::UnsupportedDefinition),
        }
    }

    Ok(MySqlColumnMetadata {
        character_length,
        decimal_size,
        name: column.col_name.as_str().to_owned(),
        type_name: type_name.to_owned(),
        nullable,
        key,
        default_sql,
        default_value,
        extra: String::new(),
    })
}

fn is_integer_type(type_name: &str) -> bool {
    matches!(
        type_name,
        // A BOOLEAN is stored and ranged as a TINYINT.
        "TINYINT"
            | "SMALLINT"
            | "MEDIUMINT"
            | "INT"
            | "INTEGER"
            | "BIGINT"
            | "BOOLEAN"
            // An unsigned column compares against an integer literal the same
            // way a signed one does; only its stored range is different, and
            // the range is checked where a value is written, not compared.
            | "TINYINT UNSIGNED"
            | "SMALLINT UNSIGNED"
            | "MEDIUMINT UNSIGNED"
            | "INT UNSIGNED"
            | "INTEGER UNSIGNED"
    )
}

/// The column kinds a comparison can be reasoned about.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ColumnKind {
    Integer,
    Text,
}

fn is_text_type(type_name: &str) -> bool {
    matches!(type_name, "VARCHAR" | "CHAR" | "TEXT")
}

/// Answers whether a comparison's right side can meet this column at all.
///
/// A checked comparison names one column and one literal form, and the two have
/// to agree: MySQL compares a string to an integer column by coercing the
/// string, and the engine would compare them as text, so the pair is refused
/// rather than answered differently.
fn checked_comparison_fits_column(
    rhs: &CheckedSelectComparisonRhs,
    type_name: &str,
    collated: bool,
) -> bool {
    match rhs {
        CheckedSelectComparisonRhs::SignedInteger(_) => is_integer_type(type_name),
        CheckedSelectComparisonRhs::Text(_) => is_text_type(type_name),
        CheckedSelectComparisonRhs::Null => {
            is_integer_type(type_name) || is_text_type(type_name)
        }
        // A parameter carries no type until it is bound, so a text column is
        // only safe when the rendered SQL already asked for the collation.
        CheckedSelectComparisonRhs::Placeholder { .. } => {
            is_integer_type(type_name) || (collated && is_text_type(type_name))
        }
    }
}

fn checked_comparison_column_refusal(
    rhs: &CheckedSelectComparisonRhs,
    column_name: &str,
    type_name: &str,
) -> LimboError {
    let wanted = match rhs {
        CheckedSelectComparisonRhs::SignedInteger(_) => "a signed integer column",
        CheckedSelectComparisonRhs::Text(_) => "a text column",
        CheckedSelectComparisonRhs::Null => "a signed integer or text column",
        CheckedSelectComparisonRhs::Placeholder { .. } => {
            "a signed integer column, because a parameter carries no type"
        }
    };
    LimboError::InvalidArgument(format!(
        "SELECT comparison on {column_name} requires {wanted}, found {type_name}"
    ))
}

fn validate_frozen_select_comparison_columns(
    schema: &turso_core::schema::Schema,
    source_table: Option<&str>,
    comparisons: &[CheckedSelectComparison],
) -> Result<()> {
    if comparisons.is_empty() {
        return Ok(());
    }
    let source_table = source_table.ok_or(LimboError::SchemaUpdated)?;
    if let Some(table) = schema.get_table(source_table) {
        let stored_sql = schema
            .table_sql(source_table)
            .ok_or(LimboError::SchemaUpdated)?;
        decode_schema_sql(SchemaSqlKind::Table, stored_sql)
            .map_err(|_| LimboError::Corrupt("invalid SELECT schema provenance".to_string()))?
            .ok_or(LimboError::SchemaUpdated)?;
        for comparison in comparisons {
            let Some((_, column)) = table.get_column_by_name(comparison.column_name()) else {
                return Err(LimboError::SchemaUpdated);
            };
            if !checked_comparison_fits_column(
                comparison.rhs(),
                &column.ty_str,
                comparison.collated(),
            ) {
                return Err(checked_comparison_column_refusal(
                    comparison.rhs(),
                    comparison.column_name(),
                    &column.ty_str,
                ));
            }
        }
        return Ok(());
    }
    let Some(view) = schema.get_view(source_table) else {
        return Err(LimboError::SchemaUpdated);
    };
    decode_schema_sql(SchemaSqlKind::View, &view.sql)
        .map_err(|_| LimboError::Corrupt("invalid SELECT schema provenance".to_string()))?
        .ok_or(LimboError::SchemaUpdated)?;
    for comparison in comparisons {
        let Some(column) = view.columns.iter().find(|column| {
            column
                .name
                .as_deref()
                .is_some_and(|name| name.eq_ignore_ascii_case(comparison.column_name()))
        }) else {
            return Err(LimboError::SchemaUpdated);
        };
        if !checked_comparison_fits_column(comparison.rhs(), &column.ty_str, comparison.collated())
        {
            return Err(checked_comparison_column_refusal(
                comparison.rhs(),
                comparison.column_name(),
                &column.ty_str,
            ));
        }
    }
    Ok(())
}

fn mysql_column_default(
    expression: &Expr,
) -> std::result::Result<(String, MySqlColumnDefault), MySqlColumnMetadataError> {
    match expression {
        Expr::Literal(Literal::Numeric(value)) => {
            let typed = mysql_integer_default(value)?;
            Ok((value.clone(), typed))
        }
        Expr::Literal(Literal::String(value)) => {
            let decoded = mysql_text_default(value)?;
            Ok((value.clone(), MySqlColumnDefault::Text(decoded)))
        }
        Expr::Literal(Literal::Null) => Ok(("NULL".to_string(), MySqlColumnDefault::Null)),
        Expr::Literal(Literal::True) => Ok(("TRUE".to_string(), MySqlColumnDefault::Boolean(true))),
        Expr::Literal(Literal::False) => {
            Ok(("FALSE".to_string(), MySqlColumnDefault::Boolean(false)))
        }
        Expr::Unary(operator, expression) => {
            let Expr::Literal(Literal::Numeric(value)) = expression.as_ref() else {
                return Err(MySqlColumnMetadataError::UnsupportedDefinition);
            };
            let sign = match operator {
                UnaryOperator::Negative => "-",
                UnaryOperator::Positive => "+",
                _ => return Err(MySqlColumnMetadataError::UnsupportedDefinition),
            };
            let text = format!("{sign}{value}");
            let typed = mysql_integer_default(&text)?;
            Ok((text, typed))
        }
        _ => Err(MySqlColumnMetadataError::UnsupportedDefinition),
    }
}

fn mysql_integer_default(
    text: &str,
) -> std::result::Result<MySqlColumnDefault, MySqlColumnMetadataError> {
    let value = text
        .parse::<i64>()
        .map_err(|_| MySqlColumnMetadataError::UnsupportedDefinition)?;
    Ok(MySqlColumnDefault::Integer {
        text: value.to_string(),
        value,
    })
}

fn mysql_text_default(value: &str) -> std::result::Result<String, MySqlColumnMetadataError> {
    let Some(content) = value
        .strip_prefix('\'')
        .and_then(|value| value.strip_suffix('\''))
    else {
        return Err(MySqlColumnMetadataError::UnsupportedDefinition);
    };
    let mut decoded = String::with_capacity(content.len());
    let mut chars = content.chars();
    while let Some(character) = chars.next() {
        if character == '\'' {
            if chars.next() != Some('\'') {
                return Err(MySqlColumnMetadataError::UnsupportedDefinition);
            }
            decoded.push('\'');
        } else {
            decoded.push(character);
        }
    }
    Ok(decoded)
}

struct FrozenSchemaDdlParser {
    mode: SessionSqlMode,
}

struct FrozenAutoIncrementDdlParser {
    mode: SessionSqlMode,
}

struct FrozenDmlParser {
    mode: SessionSqlMode,
}

/// Holds an `UPDATE` or `DELETE` `WHERE` to the same rule a `SELECT` `WHERE`
/// obeys, so the two engines cannot disagree about which rows it names.
fn validate_dml_comparison_columns(
    schema: &turso_core::schema::Schema,
    translated: &turso_mysql_parser::TranslatedDml,
) -> Result<()> {
    validate_frozen_select_comparison_columns(
        schema,
        translated.source_table(),
        translated.checked_comparisons(),
    )
}

struct FrozenSelectParser {
    mode: SessionSqlMode,
    source_table: Option<String>,
    checked_comparisons: Vec<CheckedSelectComparison>,
}

struct AutoIncrementTable {
    name: String,
    definition: CheckedAutoIncrementCreateTable,
    key: AutoIncrementKey,
    stored_sql: String,
}

/// Returns the highest key the allocator may hand out for one table.
///
/// The column's own type decides it, not the widest integer the engine can
/// hold: an `INT` stops at 2147483647 and an `INT UNSIGNED` at 4294967295, and
/// MySQL answers 1467 once the numbering reaches either.
fn auto_increment_ceiling(table: &AutoIncrementTable) -> u64 {
    let (_, max) = table.definition.allocator_column_type.bounds();
    max as u64
}

fn injected_auto_increment_prepare_options(
    table: &AutoIncrementTable,
    statement: Stmt,
) -> PrepareOptions {
    PrepareOptions::default()
        .with_reprepare_parser(Arc::new(FrozenInjectedAutoIncrementInsertParser {
            statement,
        }))
        .with_assignment_validator(Arc::new(InjectedAutoIncrementAssignmentValidator {
            table_name: table.name.clone(),
            table_sql: table.stored_sql.clone(),
            allocator_column_ordinal: table.definition.allocator_column_ordinal,
        }))
}

struct InjectedAutoIncrementAssignmentValidator {
    table_name: String,
    table_sql: String,
    allocator_column_ordinal: usize,
}

impl AssignmentValidator for InjectedAutoIncrementAssignmentValidator {
    fn validate_assignment(
        &self,
        table_name: &str,
        table_sql: Option<&str>,
        operation: AssignmentOperation,
        values: &[Value],
    ) -> Result<()> {
        if operation != AssignmentOperation::Insert {
            return Err(LimboError::Corrupt(
                "AUTO_INCREMENT injected insert did not execute as an INSERT".to_string(),
            ));
        }
        if !table_name.eq_ignore_ascii_case(&self.table_name)
            || table_sql != Some(self.table_sql.as_str())
        {
            return Err(LimboError::Corrupt(
                "AUTO_INCREMENT injected insert reached a different table or schema".to_string(),
            ));
        }
        crate::dialect::validate_mysql_assignment(
            table_name,
            table_sql,
            operation,
            values,
            Some(self.allocator_column_ordinal),
        )?;
        Ok(())
    }
}

struct FrozenInjectedAutoIncrementInsertParser {
    statement: Stmt,
}

fn run_checked_write_statement(statement: &mut Statement, timeout: Option<Duration>) -> Result<()> {
    if let Some(timeout) = timeout {
        statement.set_query_timeout_override(Some(Some(timeout)));
    }
    statement.run_with_row_callback(|_| Ok(()))
}

/// The name MySQL gives an index.
///
/// An index the engine created for an inline UNIQUE carries a generated
/// `sqlite_autoindex_` name; MySQL names such an index after its first column.
fn mysql_index_name(index: &turso_core::schema::Index) -> String {
    if index.name.starts_with("sqlite_autoindex_") {
        if let Some(first) = index.columns.first() {
            return first.name.clone();
        }
    }
    index.name.clone()
}

fn checked_insert_target(statement: &Stmt) -> Result<Option<CheckedInsertTarget>> {
    let Stmt::Insert {
        tbl_name,
        columns,
        body,
        ..
    } = statement
    else {
        return Ok(None);
    };
    let table = MySqlTableName::parse(tbl_name.name.as_str())
        .map_err(|error| LimboError::ParseError(error.to_string()))?;
    match body {
        InsertBody::DefaultValues => Ok(Some(CheckedInsertTarget::DefaultValues(table))),
        // The upsert clause changes what happens to a row that collides, not
        // what the row being offered is, so the required-column check reads the
        // same VALUES either way.
        InsertBody::Select(select, _) if !columns.is_empty() => {
            let OneSelect::Values(values) = &select.body.select else {
                return Err(LimboError::InternalError(
                    "checked INSERT source is not VALUES".into(),
                ));
            };
            Ok(Some(CheckedInsertTarget::Listed(ListedInsert {
                table,
                columns: columns
                    .iter()
                    .map(|name| name.as_str().to_owned())
                    .collect(),
                rows: values
                    .iter()
                    .map(|row| row.iter().map(|value| inserted_value(value)).collect())
                    .collect(),
            })))
        }
        _ => Err(LimboError::InternalError(
            "checked INSERT has an unexpected body".into(),
        )),
    }
}

fn inserted_value(expr: &Expr) -> InsertedValue {
    match expr {
        Expr::Literal(Literal::Null) => InsertedValue::Null,
        Expr::Variable(variable) if variable.name.is_none() => {
            InsertedValue::Marker(variable.index.get() as usize - 1)
        }
        // The checked INSERT grammar keeps parentheses and a unary plus, so
        // `(NULL)` and `(+?)` still hand the column a NULL.
        Expr::Parenthesized(inner) if inner.len() == 1 => inserted_value(&inner[0]),
        Expr::Unary(UnaryOperator::Positive, inner) => inserted_value(inner),
        _ => InsertedValue::Value,
    }
}

fn mysql_query_parse_error(error: MySqlParseError) -> MySqlQueryError {
    if matches!(error, MySqlParseError::Unsupported { .. }) {
        MySqlQueryError::Unsupported(error.to_string())
    } else {
        MySqlQueryError::Syntax(error.to_string())
    }
}

impl ReprepareParser for FrozenInjectedAutoIncrementInsertParser {
    fn parse(&self, sql: &str, _context: &ReprepareContext<'_>) -> Result<(Option<Cmd>, usize)> {
        Ok((Some(Cmd::Stmt(self.statement.clone())), sql.len()))
    }
}

impl ReprepareParser for FrozenDmlParser {
    fn parse(&self, sql: &str, context: &ReprepareContext<'_>) -> Result<(Option<Cmd>, usize)> {
        let translated =
            parse_dml(sql, self.mode).map_err(|error| LimboError::ParseError(error.to_string()))?;
        validate_dml_comparison_columns(context.schema, &translated)?;
        let stmt = translated
            .parse_ast()
            .map_err(|error| LimboError::ParseError(error.to_string()))?;
        Ok((Some(Cmd::Stmt(stmt)), sql.len()))
    }
}

impl ReprepareParser for FrozenSelectParser {
    fn parse(&self, sql: &str, context: &ReprepareContext<'_>) -> Result<(Option<Cmd>, usize)> {
        let translated = parse_select(sql, self.mode)
            .map_err(|error| LimboError::ParseError(error.to_string()))?;
        validate_frozen_select_comparison_columns(
            context.schema,
            self.source_table.as_deref(),
            &self.checked_comparisons,
        )?;
        let stmt = translated
            .parse_ast()
            .map_err(|error| LimboError::ParseError(error.to_string()))?;
        Ok((Some(Cmd::Stmt(stmt)), sql.len()))
    }
}

impl ReprepareParser for FrozenSchemaDdlParser {
    fn parse(&self, sql: &str, _context: &ReprepareContext<'_>) -> Result<(Option<Cmd>, usize)> {
        let stmt = parse_schema_ddl_ast(sql, self.mode)
            .map_err(|error| LimboError::ParseError(error.to_string()))?;
        Ok((Some(Cmd::Stmt(stmt)), sql.len()))
    }
}

impl ReprepareParser for FrozenAutoIncrementDdlParser {
    fn parse(&self, sql: &str, _context: &ReprepareContext<'_>) -> Result<(Option<Cmd>, usize)> {
        let checked = parse_auto_increment_create_table(sql, self.mode)
            .map_err(|error| LimboError::ParseError(error.to_string()))?;
        Ok((Some(Cmd::Stmt(checked.sqlite_statement)), sql.len()))
    }
}

struct AutoIncrementSchemaSqlFormatter {
    context: SchemaSqlSessionContext,
    metadata: SchemaSqlV2Metadata,
    normalized_mysql_ddl: String,
    sqlite_statement: Stmt,
}

impl SchemaSqlFormatter for AutoIncrementSchemaSqlFormatter {
    fn format_schema_sql(&self, kind: SchemaSqlKind, input: &str, stmt: &Stmt) -> Result<String> {
        if kind != SchemaSqlKind::Table
            || input != self.normalized_mysql_ddl
            || stmt != &self.sqlite_statement
        {
            return Err(LimboError::InternalError(
                "AUTO_INCREMENT schema formatter received a different statement".to_string(),
            ));
        }
        encode_schema_sql_v2(
            self.context.for_kind(SchemaSqlKind::Table),
            self.metadata,
            &self.normalized_mysql_ddl,
        )
        .map_err(|error| LimboError::InternalError(error.to_string()))
    }

    fn format_rewritten_schema_sql(
        &self,
        _kind: SchemaSqlKind,
        _previous_sql: &str,
        _stmt: &Stmt,
    ) -> Result<String> {
        Err(LimboError::ParseError(
            "AUTO_INCREMENT schema rewrites are not supported".to_string(),
        ))
    }
}

fn new_allocator_identity() -> Result<[u8; 16]> {
    loop {
        let mut identity = [0; 16];
        getrandom::fill(&mut identity).map_err(|_| {
            LimboError::InternalError(
                "failed to generate an AUTO_INCREMENT allocator identity".to_string(),
            )
        })?;
        if identity.iter().any(|byte| *byte != 0) {
            return Ok(identity);
        }
    }
}

#[cfg(test)]
mod tests;
