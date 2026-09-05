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
    SchemaSqlFormatter, SchemaSqlKind, Statement, Value,
    storage::auto_increment::{AutoIncrementKey, DurableRangeAllocator},
};
use turso_mysql_parser::{
    CheckedAutoIncrementCreateTable, CheckedAutoIncrementInsert, CheckedPrimaryKeyCreateTable,
    CheckedUpdateAssignmentValue, MySqlDropTableCommand, MySqlTableName, MySqlTransactionCommand,
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
    AlterTableBody, Cmd, ColumnConstraint, CreateTableBody, Expr, Literal, OneSelect, ResultColumn,
    SelectTable, Stmt, UnaryOperator,
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
    prepared_statements: Arc<Mutex<PreparedStatementRegistry>>,
    prepared_statement_authority: MySqlPreparedStatementAuthority,
}

#[derive(Clone)]
pub(crate) struct AutoIncrementExecutionCapability {
    allocator: DurableRangeAllocator,
    io: Arc<dyn IO>,
}

/// Failure stage for a checked MySQL query prepare.
///
/// Keeping parser rejection separate from a core prepare failure lets protocol
/// adapters return a syntax error only when the MySQL parser actually rejected
/// the statement. Core currently uses `LimboError::ParseError` for some schema
/// lookup failures too, so flattening both stages would mislabel missing objects
/// as malformed SQL.
#[derive(Debug)]
pub enum MySqlQueryError {
    /// An omitted required column has no default in a checked empty INSERT.
    MissingRequiredDefault(String),
    /// The MySQL parser or checked translator rejected the query text.
    Syntax(String),
    /// Valid MySQL syntax lies outside the implemented compatibility surface.
    Unsupported(String),
    /// The checked Turso AST reached core, which then failed to prepare it.
    Engine(LimboError),
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
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MySqlColumnKey {
    /// The column has no supported key declaration.
    None,
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
}

enum PreparedExecutionPlan {
    Select { reads_table: bool },
    OrdinaryWrite {
        is_update: bool,
        default_insert_table: Option<MySqlTableName>,
    },
    AutoIncrementInsert(Box<PreparedAutoIncrementInsert>),
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
            Self::Syntax(error) => f.write_str(error),
            Self::Unsupported(error) => f.write_str(error),
            Self::Engine(error) => error.fmt(f),
        }
    }
}

impl Error for MySqlQueryError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::MissingRequiredDefault(_) => None,
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
        Ok(Self {
            inner,
            schema_context,
            auto_increment: None,
            session_autocommit: Arc::new(Mutex::new(true)),
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

    /// Lists user-visible tables and views from the current database catalog.
    ///
    /// This reads the persisted schema directly through the trusted Core
    /// connection. SQLite and Turso internal tables are deliberately omitted.
    pub fn list_tables(&self) -> Result<Vec<MySqlTable>> {
        let sql = format!(
            "SELECT name, type FROM sqlite_schema \
             WHERE type IN ('table', 'view') \
             AND lower(name) NOT LIKE 'sqlite\\_%' ESCAPE '\\' \
             AND lower(name) NOT LIKE '\\_\\_turso\\_internal\\_%' ESCAPE '\\' \
             LIMIT {TABLE_LIST_SCAN_LIMIT}"
        );
        let rows = self
            .inner
            .prepare(&sql)?
            .run_collect_rows()?;
        if Self::table_list_is_truncated(rows.len()) {
            return Err(LimboError::TooBig);
        }
        let mut tables = Vec::with_capacity(rows.len());
        for row in rows {
            let [name, kind] = row.as_slice() else {
                return Err(LimboError::Corrupt(
                    "sqlite_schema table listing row has an invalid shape".to_string(),
                ));
            };
            let name = name.to_text().ok_or_else(|| {
                LimboError::Corrupt("sqlite_schema table name is not text".to_string())
            })?;
            assert!(
                !turso_core::schema::is_system_table(name),
                "fixed table-list query must exclude internal tables"
            );
            let kind = match kind.to_text() {
                Some("table") => MySqlTableKind::BaseTable,
                Some("view") => MySqlTableKind::View,
                _ => {
                    return Err(LimboError::Corrupt(
                        "sqlite_schema table listing kind is invalid".to_string(),
                    ));
                }
            };
            tables.push(MySqlTable {
                name: name.to_owned(),
                kind,
            });
        }
        tables.sort_unstable_by(|left, right| left.name.cmp(&right.name));
        Ok(tables)
    }

    /// Reconstructs the initial MySQL column metadata surface for one table or view.
    ///
    /// The normalized MySQL DDL is the source for MySQL-only fields. Core is
    /// used only to verify that the marked catalog row still describes the
    /// loaded table and its columns in the same order.
    pub fn list_columns(
        &self,
        table: &MySqlTableName,
    ) -> std::result::Result<Vec<MySqlColumnMetadata>, MySqlColumnMetadataError> {
        let table_name = table.as_str();
        if turso_core::schema::is_system_table(table_name) {
            return Err(MySqlColumnMetadataError::TableNotFound);
        }
        let sql = format!(
            "SELECT name, type, sql, rootpage FROM sqlite_schema \
             WHERE type IN ('table', 'view') AND lower(name) = '{table_name}' LIMIT 2"
        );
        let rows = self
            .inner
            .prepare_internal(&sql)
            .map_err(MySqlColumnMetadataError::Engine)?
            .run_collect_rows()
            .map_err(MySqlColumnMetadataError::Engine)?;
        let row = match rows.as_slice() {
            [] => return Err(MySqlColumnMetadataError::TableNotFound),
            [row] => row,
            _ => return Err(MySqlColumnMetadataError::CorruptDefinition),
        };
        let [catalog_name, object_type, stored_sql, root_page] = row.as_slice() else {
            return Err(MySqlColumnMetadataError::CorruptDefinition);
        };
        let catalog_name = catalog_name
            .to_text()
            .ok_or(MySqlColumnMetadataError::CorruptDefinition)?;
        let object_type = object_type
            .to_text()
            .ok_or(MySqlColumnMetadataError::CorruptDefinition)?;
        let stored_sql = stored_sql
            .to_text()
            .ok_or(MySqlColumnMetadataError::CorruptDefinition)?;
        if object_type.eq_ignore_ascii_case("view") {
            // Core stores rootpage as integer zero for views, not SQL NULL.
            if root_page.as_int() != Some(0) {
                return Err(MySqlColumnMetadataError::CorruptDefinition);
            }
            return self.list_view_columns(table_name, catalog_name, stored_sql);
        }
        let root_page = root_page
            .as_int()
            .filter(|root_page| *root_page > 0)
            .ok_or(MySqlColumnMetadataError::CorruptDefinition)?;
        if !catalog_name.eq_ignore_ascii_case(table_name)
            || !object_type.eq_ignore_ascii_case("table")
        {
            return Err(MySqlColumnMetadataError::CorruptDefinition);
        }

        let decoded = decode_schema_sql(SchemaSqlKind::Table, stored_sql)
            .map_err(|_| MySqlColumnMetadataError::CorruptDefinition)?
            .ok_or(MySqlColumnMetadataError::UnsupportedDefinition)?;
        let mode = SessionSqlMode {
            ansi_quotes: decoded.context.sql_mode.ansi_quotes,
            no_backslash_escapes: decoded.context.sql_mode.no_backslash_escapes,
        };
        let (statement, auto_increment_column_ordinal) = match decoded.v2_metadata() {
            Some(metadata) => {
                let Some(validation_context) = self.inner.schema_catalog_validation_context()
                else {
                    return Err(MySqlColumnMetadataError::CorruptDefinition);
                };
                if metadata.database_id.into_bytes() != *validation_context.database_identity() {
                    return Err(MySqlColumnMetadataError::CorruptDefinition);
                }
                let checked = parse_auto_increment_create_table(decoded.normalized_ddl, mode)
                    .map_err(|_| MySqlColumnMetadataError::CorruptDefinition)?;
                if checked.normalized_mysql_ddl != decoded.normalized_ddl {
                    return Err(MySqlColumnMetadataError::CorruptDefinition);
                }

                // The checked parser proves the marker belongs to the
                // allocator column. Remove it from the canonical copy so the
                // general metadata parser can retain the original INT/INTEGER
                // spelling while the checked ordinal supplies the key/extra.
                const AUTO_INCREMENT_PRIMARY_KEY: &str = " AUTO_INCREMENT PRIMARY KEY";
                let mut normalized_without_auto_increment = checked.normalized_mysql_ddl.clone();
                let Some(marker_start) = find_unquoted_sql_fragment(
                    &normalized_without_auto_increment,
                    AUTO_INCREMENT_PRIMARY_KEY,
                    mode.no_backslash_escapes,
                ) else {
                    return Err(MySqlColumnMetadataError::CorruptDefinition);
                };
                if find_unquoted_sql_fragment(
                    &normalized_without_auto_increment
                        [marker_start + AUTO_INCREMENT_PRIMARY_KEY.len()..],
                    AUTO_INCREMENT_PRIMARY_KEY,
                    mode.no_backslash_escapes,
                )
                .is_some()
                {
                    return Err(MySqlColumnMetadataError::CorruptDefinition);
                }
                normalized_without_auto_increment.replace_range(
                    marker_start..marker_start + AUTO_INCREMENT_PRIMARY_KEY.len(),
                    "",
                );
                let statement = parse_create_table_ast(&normalized_without_auto_increment, mode)
                    .map_err(|_| MySqlColumnMetadataError::CorruptDefinition)?;
                (statement, Some(checked.allocator_column_ordinal))
            }
            None => (
                parse_create_table_ast(decoded.normalized_ddl, mode)
                    .map_err(mysql_metadata_parse_error)?,
                None,
            ),
        };
        let Stmt::CreateTable {
            temporary,
            tbl_name,
            body,
            ..
        } = statement
        else {
            return Err(MySqlColumnMetadataError::CorruptDefinition);
        };
        if temporary
            || tbl_name.db_name.is_some()
            || tbl_name.alias.is_some()
            || !tbl_name.name.as_str().eq_ignore_ascii_case(catalog_name)
        {
            return Err(MySqlColumnMetadataError::CorruptDefinition);
        }
        let CreateTableBody::ColumnsAndConstraints {
            columns,
            constraints,
            options,
        } = body
        else {
            return Err(MySqlColumnMetadataError::UnsupportedDefinition);
        };
        if columns.is_empty()
            || options.without_rowid_text.is_some()
            || options.strict_text.is_some()
            || constraints.iter().any(|constraint| {
                !matches!(
                    constraint.constraint,
                    turso_parser::ast::TableConstraint::Check { .. }
                        | turso_parser::ast::TableConstraint::ForeignKey { .. }
                )
            })
        {
            return Err(MySqlColumnMetadataError::UnsupportedDefinition);
        }

        let schema = self.inner.current_schema();
        let core_table = schema
            .get_table(catalog_name)
            .ok_or(MySqlColumnMetadataError::CorruptDefinition)?;
        if core_table
            .get_root_page()
            .map_err(|_| MySqlColumnMetadataError::CorruptDefinition)?
            != root_page
            || schema.table_sql(catalog_name) != Some(stored_sql)
        {
            return Err(MySqlColumnMetadataError::CorruptDefinition);
        }
        let core_columns = core_table.columns();
        if core_columns.len() != columns.len() {
            return Err(MySqlColumnMetadataError::CorruptDefinition);
        }
        if columns
            .iter()
            .zip(core_columns)
            .any(|(column, core_column)| {
                core_column.name.as_deref() != Some(column.col_name.as_str())
            })
        {
            return Err(MySqlColumnMetadataError::CorruptDefinition);
        }

        let mut metadata = columns
            .iter()
            .map(mysql_column_metadata)
            .collect::<std::result::Result<Vec<_>, _>>()?;
        if let Some(ordinal) = auto_increment_column_ordinal {
            let column = metadata
                .get_mut(ordinal)
                .ok_or(MySqlColumnMetadataError::CorruptDefinition)?;
            if column.key != MySqlColumnKey::None || column.nullable {
                return Err(MySqlColumnMetadataError::CorruptDefinition);
            }
            column.key = MySqlColumnKey::Primary;
            "AUTO_INCREMENT".clone_into(&mut column.extra);
        }
        let rowid_alias_ordinal = core_table
            .btree()
            .and_then(|table| table.get_rowid_alias_column().map(|(ordinal, _)| ordinal));
        if auto_increment_column_ordinal.is_none() && rowid_alias_ordinal.is_some() {
            return Err(MySqlColumnMetadataError::CorruptDefinition);
        }
        self.verify_column_indexes(table_name, &metadata, rowid_alias_ordinal)?;
        if core_columns
            .iter()
            .zip(&metadata)
            .enumerate()
            .any(|(ordinal, (core_column, column))| {
                (Some(ordinal) != auto_increment_column_ordinal
                    && core_column.notnull() == column.nullable)
                    || core_column.unique() != (column.key == MySqlColumnKey::Unique)
                    || core_column.primary_key() != (column.key == MySqlColumnKey::Primary)
            })
        {
            return Err(MySqlColumnMetadataError::CorruptDefinition);
        }
        Ok(metadata)
    }

    fn list_view_columns(
        &self,
        requested_name: &str,
        catalog_name: &str,
        stored_sql: &str,
    ) -> std::result::Result<Vec<MySqlColumnMetadata>, MySqlColumnMetadataError> {
        if !catalog_name.eq_ignore_ascii_case(requested_name) {
            return Err(MySqlColumnMetadataError::CorruptDefinition);
        }
        let decoded = decode_schema_sql(SchemaSqlKind::View, stored_sql)
            .map_err(|_| MySqlColumnMetadataError::CorruptDefinition)?
            .ok_or(MySqlColumnMetadataError::UnsupportedDefinition)?;
        let mode = SessionSqlMode {
            ansi_quotes: decoded.context.sql_mode.ansi_quotes,
            no_backslash_escapes: decoded.context.sql_mode.no_backslash_escapes,
        };
        let statement = parse_create_view_ast(decoded.normalized_ddl, mode)
            .map_err(mysql_metadata_parse_error)?;
        let canonical = render_create_view_mysql_with_mode(&statement, mode)
            .map_err(|_| MySqlColumnMetadataError::UnsupportedDefinition)?;
        if canonical != decoded.normalized_ddl {
            return Err(MySqlColumnMetadataError::CorruptDefinition);
        }
        let Stmt::CreateView {
            temporary,
            if_not_exists,
            view_name,
            columns,
            select,
        } = &statement
        else {
            return Err(MySqlColumnMetadataError::CorruptDefinition);
        };
        if *temporary
            || *if_not_exists
            || view_name.db_name.is_some()
            || view_name.alias.is_some()
            || !columns.is_empty()
            || !view_name.name.as_str().eq_ignore_ascii_case(catalog_name)
        {
            return Err(MySqlColumnMetadataError::CorruptDefinition);
        }
        let (source_table, projected_columns) = Self::view_projection(select)?;
        if turso_core::schema::is_system_table(source_table.as_str()) {
            return Err(MySqlColumnMetadataError::UnsupportedDefinition);
        }
        let schema = self.inner.current_schema();
        if schema.get_view(source_table.as_str()).is_some() {
            return Err(MySqlColumnMetadataError::UnsupportedDefinition);
        }
        if schema.get_table(source_table.as_str()).is_none() {
            return Err(MySqlColumnMetadataError::CorruptDefinition);
        }
        let core_view = schema
            .get_view(catalog_name)
            .ok_or(MySqlColumnMetadataError::CorruptDefinition)?;
        if core_view.sql != stored_sql {
            return Err(MySqlColumnMetadataError::CorruptDefinition);
        }
        let source_columns = self
            .list_columns(&source_table)
            .map_err(|error| match error {
                MySqlColumnMetadataError::TableNotFound
                | MySqlColumnMetadataError::CorruptDefinition => {
                    MySqlColumnMetadataError::CorruptDefinition
                }
                MySqlColumnMetadataError::UnsupportedDefinition => {
                    MySqlColumnMetadataError::UnsupportedDefinition
                }
                MySqlColumnMetadataError::Engine(error) => MySqlColumnMetadataError::Engine(error),
            })?;
        let mut metadata = Vec::with_capacity(projected_columns.len());
        for projected_name in projected_columns {
            if metadata.iter().any(|column: &MySqlColumnMetadata| {
                column.name.eq_ignore_ascii_case(&projected_name)
            }) {
                return Err(MySqlColumnMetadataError::CorruptDefinition);
            }
            let source = source_columns
                .iter()
                .find(|column| column.name.eq_ignore_ascii_case(&projected_name))
                .ok_or(MySqlColumnMetadataError::CorruptDefinition)?;
            let mut column = source.clone();
            column.name = projected_name;
            column.key = MySqlColumnKey::None;
            column.default_sql = None;
            column.default_value = None;
            column.extra.clear();
            metadata.push(column);
        }
        if core_view.columns.len() != metadata.len() {
            return Err(MySqlColumnMetadataError::CorruptDefinition);
        }
        for (core_column, column) in core_view.columns.iter().zip(&mut metadata) {
            if core_column.name.as_deref() != Some(column.name.as_str())
                || core_column.ty_str != column.type_name
            {
                return Err(MySqlColumnMetadataError::CorruptDefinition);
            }
        }
        Ok(metadata)
    }

    fn view_projection(
        select: &turso_parser::ast::Select,
    ) -> std::result::Result<(MySqlTableName, Vec<String>), MySqlColumnMetadataError> {
        if select.with.is_some() || !select.order_by.is_empty() || select.limit.is_some() {
            return Err(MySqlColumnMetadataError::UnsupportedDefinition);
        }
        if !select.body.compounds.is_empty() {
            return Err(MySqlColumnMetadataError::UnsupportedDefinition);
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
            return Err(MySqlColumnMetadataError::UnsupportedDefinition);
        };
        if distinctness.is_some()
            || where_clause.is_some()
            || group_by.is_some()
            || !window_clause.is_empty()
        {
            return Err(MySqlColumnMetadataError::UnsupportedDefinition);
        }
        let Some(from) = from else {
            return Err(MySqlColumnMetadataError::UnsupportedDefinition);
        };
        if !from.joins.is_empty() {
            return Err(MySqlColumnMetadataError::UnsupportedDefinition);
        }
        let SelectTable::Table(table_name, alias, indexed) = from.select.as_ref() else {
            return Err(MySqlColumnMetadataError::UnsupportedDefinition);
        };
        if table_name.db_name.is_some()
            || table_name.alias.is_some()
            || alias.is_some()
            || indexed.is_some()
        {
            return Err(MySqlColumnMetadataError::UnsupportedDefinition);
        }
        let source_table = MySqlTableName::parse(table_name.name.as_str())
            .map_err(|_| MySqlColumnMetadataError::UnsupportedDefinition)?;
        let mut projected_columns = Vec::with_capacity(columns.len());
        for column in columns {
            let ResultColumn::Expr(expr, alias) = column else {
                return Err(MySqlColumnMetadataError::UnsupportedDefinition);
            };
            if alias.as_ref().is_some_and(|alias| alias.is_explicit()) {
                return Err(MySqlColumnMetadataError::UnsupportedDefinition);
            }
            let projected_name = match expr.as_ref() {
                Expr::Name(name) | Expr::Id(name) => name.as_str().to_owned(),
                _ => return Err(MySqlColumnMetadataError::UnsupportedDefinition),
            };
            projected_columns.push(projected_name);
        }
        if projected_columns.is_empty() {
            return Err(MySqlColumnMetadataError::UnsupportedDefinition);
        }
        Ok((source_table, projected_columns))
    }

    fn verify_column_indexes(
        &self,
        table_name: &str,
        columns: &[MySqlColumnMetadata],
        rowid_alias_ordinal: Option<usize>,
    ) -> std::result::Result<(), MySqlColumnMetadataError> {
        let sql = format!(
            "SELECT name FROM sqlite_schema \
             WHERE type = 'index' AND lower(tbl_name) = '{table_name}' \
             LIMIT {COLUMN_INDEX_SCAN_LIMIT}"
        );
        let rows = self
            .inner
            .prepare_internal(&sql)
            .map_err(MySqlColumnMetadataError::Engine)?
            .run_collect_rows()
            .map_err(MySqlColumnMetadataError::Engine)?;
        if Self::column_index_scan_is_truncated(rows.len()) {
            return Err(MySqlColumnMetadataError::UnsupportedDefinition);
        }
        let mut automatic_index_count = 0;
        for row in rows {
            let [name] = row.as_slice() else {
                return Err(MySqlColumnMetadataError::CorruptDefinition);
            };
            let name = name
                .to_text()
                .ok_or(MySqlColumnMetadataError::CorruptDefinition)?;
            if !name.starts_with("sqlite_autoindex_") {
                return Err(MySqlColumnMetadataError::UnsupportedDefinition);
            }
            automatic_index_count += 1;
        }
        let inline_unique_count = columns
            .iter()
            .filter(|column| column.key == MySqlColumnKey::Unique)
            .count();
        let inline_primary_index_count = columns
            .iter()
            .enumerate()
            .filter(|(ordinal, column)| {
                column.key == MySqlColumnKey::Primary
                    && column.extra.is_empty()
                    && Some(*ordinal) != rowid_alias_ordinal
            })
            .count();
        if automatic_index_count != inline_unique_count + inline_primary_index_count {
            return Err(MySqlColumnMetadataError::CorruptDefinition);
        }
        Ok(())
    }

    fn column_index_scan_is_truncated(row_count: usize) -> bool {
        row_count == COLUMN_INDEX_SCAN_LIMIT
    }

    fn table_list_is_truncated(row_count: usize) -> bool {
        row_count == TABLE_LIST_SCAN_LIMIT
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
        let (statement, execution_plan) = match parse_select(sql, self.parser_mode()) {
            Ok(translated) => {
                Self::reject_internal_catalog_select(&translated)
                    .map_err(MySqlPreparedStatementError::Prepare)?;
                static_result_metadata = translated.static_result_metadata().to_vec();
                let statement = translated.parse_ast().map_err(|error| {
                    MySqlPreparedStatementError::Prepare(MySqlQueryError::Syntax(error.to_string()))
                })?;
                let reads_table = translated.reads_table();
                let statement = self
                    .inner
                    .prepare_translated_stmt(statement, translated.as_sql())
                    .map_err(|error| {
                        MySqlPreparedStatementError::Prepare(MySqlQueryError::Engine(error))
                    })?;
                (
                    Some(statement),
                    PreparedExecutionPlan::Select { reads_table },
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
        let statement = translated.parse_ast().map_err(|error| {
            MySqlPreparedStatementError::Prepare(MySqlQueryError::Syntax(error.to_string()))
        })?;
        let is_update = matches!(statement, Stmt::Update(_));
        let default_insert_table = default_insert_table(&statement)
            .map_err(MySqlPreparedStatementError::Engine)?;
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
                default_insert_table,
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

        if let Some(statement) = prepared.statement.as_mut() {
            statement
                .reset()
                .map_err(MySqlPreparedStatementError::Engine)?;
        }
        let timeout = if let PreparedExecutionPlan::OrdinaryWrite {
            default_insert_table: Some(table),
            ..
        } = &prepared.execution_plan
        {
            let deadline = self.write_deadline(timeout);
            self.check_write_deadline(deadline)
                .map_err(|error| MySqlPreparedStatementError::Engine(error.into()))?;
            self.begin_implicit_transaction_for_write()
                .map_err(|error| MySqlPreparedStatementError::Engine(error.into()))?;
            let missing = self
                .missing_insert_default(table)
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
        let values = values
            .iter()
            .map(mysql_prepared_value_to_core)
            .collect::<Result<Vec<_>>>()?;

        match &prepared.execution_plan {
            PreparedExecutionPlan::Select { reads_table } => {
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
        if range.first() == 0 || range.last() != expected_last || range.last() > i32::MAX as u64 {
            return Err(LimboError::Corrupt(
                "AUTO_INCREMENT allocator returned an invalid signed INT range".to_string(),
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
            MySqlTransactionCommand::Begin if !self.inner.get_auto_commit() => {
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
            _ => {}
        }
        let statement = match command {
            MySqlTransactionCommand::Begin => Stmt::Begin {
                typ: None,
                name: None,
            },
            MySqlTransactionCommand::Commit => Stmt::Commit { name: None },
            MySqlTransactionCommand::Rollback => Stmt::Rollback {
                tx_name: None,
                savepoint_name: None,
            },
        };
        self.inner
            .prepare_translated_stmt(statement, sql)
            .and_then(|mut statement| statement.run_ignore_rows())
            .map_err(MySqlQueryError::Engine)
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
        let translated = parse_select(sql, self.parser_mode())
            .map_err(|error| MySqlQueryError::Syntax(error.to_string()))?;
        Self::reject_internal_catalog_select(&translated)?;
        if translated.reads_table() {
            self.begin_implicit_transaction_for_table_read()?;
        }
        let stmt = translated
            .parse_ast()
            .map_err(|error| MySqlQueryError::Syntax(error.to_string()))?;
        let stmt = self
            .inner
            .prepare_translated_stmt(stmt, translated.as_sql())
            .map_err(MySqlQueryError::Engine)?;
        let static_result_metadata =
            aligned_static_result_metadata(&stmt, translated.static_result_metadata());
        Ok((stmt, static_result_metadata))
    }

    fn prepare_non_schema(&self, sql: &str) -> Result<Statement> {
        let mode = self.parser_mode();
        match parse_dml(sql, mode) {
            Ok(translated) => {
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
            .source_table()
            .is_some_and(turso_core::schema::is_system_table)
        {
            return Err(MySqlQueryError::Unsupported(
                "SELECT from an internal catalog is unsupported".to_string(),
            ));
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
        let default_insert_table =
            default_insert_table(&statement).map_err(MySqlQueryError::Engine)?;
        let options =
            PrepareOptions::default().with_reprepare_parser(Arc::new(FrozenDmlParser { mode }));
        let mut statement = self
            .inner
            .prepare_translated_stmt_with_options(statement, sql, &options)
            .map_err(MySqlQueryError::Engine)?;
        if let Some(table) = default_insert_table {
            self.check_write_deadline(deadline)?;
            let missing = self
                .missing_insert_default(&table)
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
        if high_water > i32::MAX as u64 {
            return Err(MySqlQueryError::Engine(LimboError::Constraint(
                "AUTO_INCREMENT value is outside signed INT range".to_string(),
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
        if range.first() == 0 || range.last() != expected_last || range.last() > i32::MAX as u64 {
            return Err(LimboError::Corrupt(
                "AUTO_INCREMENT allocator returned an invalid signed INT range".to_string(),
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
    let result_column_type_metadata =
        prepared_result_column_type_metadata(statement, &prepared.static_result_projections);
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
    if data_type.size.is_some() || data_type.array_dimensions != 0 {
        return Err(MySqlColumnMetadataError::UnsupportedDefinition);
    }
    let type_name = match data_type.name.as_str() {
        "TINYINT" => "TINYINT",
        "SMALLINT" => "SMALLINT",
        "MEDIUMINT" => "MEDIUMINT",
        "INT" => "INT",
        "INTEGER" => "INTEGER",
        "BIGINT" => "BIGINT",
        "TEXT" => "TEXT",
        "BLOB" => "BLOB",
        _ => return Err(MySqlColumnMetadataError::UnsupportedDefinition),
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
        name: column.col_name.as_str().to_owned(),
        type_name: type_name.to_owned(),
        nullable,
        key,
        default_sql,
        default_value,
        extra: String::new(),
    })
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

struct AutoIncrementTable {
    name: String,
    definition: CheckedAutoIncrementCreateTable,
    key: AutoIncrementKey,
    stored_sql: String,
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

fn default_insert_table(statement: &Stmt) -> Result<Option<MySqlTableName>> {
    let Stmt::Insert {
        tbl_name,
        body: turso_parser::ast::InsertBody::DefaultValues,
        ..
    } = statement
    else {
        return Ok(None);
    };
    MySqlTableName::parse(tbl_name.name.as_str())
        .map(Some)
        .map_err(|error| LimboError::ParseError(error.to_string()))
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
    fn parse(&self, sql: &str, _context: &ReprepareContext<'_>) -> Result<(Option<Cmd>, usize)> {
        let translated =
            parse_dml(sql, self.mode).map_err(|error| LimboError::ParseError(error.to_string()))?;
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
mod tests {
    use super::*;
    use crate::{
        MySqlDialect,
        schema_sql::{CharacterSet, Collation, SchemaSqlKind, SchemaSqlMode, decode_schema_sql},
    };
    use turso_core::{
        AssignmentError, Database, DatabaseOpts, IO, MemoryIO, OpenFlags, OpenOptions, PlatformIO,
        SchemaCatalogValidationContext, Value,
        io::FileSyncType,
        storage::auto_increment::{AllocatorDatabaseIdentity, AllocatorOpenMode},
        storage::database::DatabaseFile,
    };
    use turso_parser::parser::Parser;

    fn binary_context() -> SchemaSqlSessionContext {
        SchemaSqlSessionContext {
            sql_mode: SchemaSqlMode {
                ansi_quotes: false,
                no_backslash_escapes: false,
            },
            character_set_client: CharacterSet::Binary,
            collation_connection: Collation::Binary,
            default_character_set: CharacterSet::Binary,
            default_collation: Collation::Binary,
        }
    }

    fn open_database(io: Arc<dyn IO>, path: &str, flags: OpenFlags) -> Result<Arc<Database>> {
        let file = io.open_file(path, flags, true)?;
        Database::open(
            io,
            path,
            OpenOptions::new(Arc::new(MySqlDialect))
                .storage(Arc::new(DatabaseFile::new(file)))
                .flags(flags)
                .db_opts(DatabaseOpts::new().with_vacuum(true).with_views(true)),
        )
    }

    fn open_database_with_identity(
        io: Arc<dyn IO>,
        path: &str,
        flags: OpenFlags,
        database_identity: [u8; 16],
    ) -> Result<Arc<Database>> {
        let file = io.open_file(path, flags, true)?;
        Database::open(
            io,
            path,
            OpenOptions::new(Arc::new(MySqlDialect))
                .storage(Arc::new(DatabaseFile::new(file)))
                .flags(flags)
                .schema_catalog_validation_context(SchemaCatalogValidationContext::new(
                    database_identity,
                ))
                .db_opts(DatabaseOpts::new().with_vacuum(true).with_views(true)),
        )
    }

    fn open_allocator_connection(
        path: &str,
        database_identity: [u8; 16],
    ) -> Result<(MySqlConnection, DurableRangeAllocator, Arc<dyn IO>)> {
        let io: Arc<dyn IO> = Arc::new(MemoryIO::new());
        let database = open_database_with_identity(
            Arc::clone(&io),
            path,
            OpenFlags::Create,
            database_identity,
        )?;
        let allocator = DurableRangeAllocator::open(
            io.as_ref(),
            &format!("{path}.auto-increment"),
            AllocatorDatabaseIdentity::new(database_identity)?,
            AllocatorOpenMode::Create,
            FileSyncType::Fsync,
        )?;
        let mut initialization = allocator.initialize()?;
        io.block(|| initialization.step())?;
        let connection = MySqlConnection::new_with_auto_increment_and_prepared_statement_authority(
            database.connect()?,
            binary_context(),
            allocator.clone(),
            Arc::clone(&io),
            MySqlPreparedStatementAuthority::default(),
        )?;
        Ok((connection, allocator, io))
    }

    fn auto_increment_key(connection: &MySqlConnection, table: &str) -> Result<AutoIncrementKey> {
        let rows = connection
            .inner()
            .prepare(format!(
                "SELECT sql FROM sqlite_schema WHERE name = '{table}'"
            ))?
            .run_collect_rows()?;
        let stored = rows
            .first()
            .and_then(|row| row.first())
            .ok_or_else(|| {
                LimboError::InternalError("AUTO_INCREMENT table is missing".to_string())
            })?
            .to_string();
        let decoded = decode_schema_sql(SchemaSqlKind::Table, stored.trim_matches('\''))
            .map_err(|error| LimboError::Corrupt(error.to_string()))?
            .ok_or_else(|| {
                LimboError::Corrupt("AUTO_INCREMENT table has no envelope".to_string())
            })?;
        AutoIncrementKey::new(
            decoded
                .v2_metadata()
                .ok_or_else(|| {
                    LimboError::Corrupt("AUTO_INCREMENT table has no v2 metadata".to_string())
                })?
                .allocator_id
                .into_bytes(),
        )
    }

    #[test]
    fn auto_increment_execute_reserves_and_injects_one_range_per_values_batch() -> Result<()> {
        let (connection, _allocator, _io) =
            open_allocator_connection("mysql-session-auto-increment-execute.db", [0x51; 16])?;
        connection.execute(
            "CREATE TABLE users (id INT NOT NULL AUTO_INCREMENT PRIMARY KEY, name TEXT)",
        )?;

        connection.execute("INSERT INTO users (name) VALUES ('Ada')")?;
        connection.execute("INSERT INTO users (name) VALUES ('Grace'), ('Linus')")?;
        connection.execute("CREATE TABLE notes (id INT, body TEXT)")?;
        connection.execute("INSERT INTO notes (id, body) VALUES (9, 'ordinary')")?;

        assert_eq!(
            connection
                .prepare_select("SELECT id, name FROM users")?
                .run_collect_rows()?,
            vec![
                vec![Value::from_i64(1), Value::from_text("Ada")],
                vec![Value::from_i64(2), Value::from_text("Grace")],
                vec![Value::from_i64(3), Value::from_text("Linus")],
            ]
        );
        assert_eq!(
            connection
                .prepare_select("SELECT id, body FROM notes")?
                .run_collect_rows()?,
            vec![vec![Value::from_i64(9), Value::from_text("ordinary")]]
        );
        connection.close()?;
        Ok(())
    }

    #[test]
    fn auto_increment_prepare_never_reserves_and_unsupported_marked_insert_fails_closed()
    -> Result<()> {
        let (connection, allocator, io) =
            open_allocator_connection("mysql-session-auto-increment-prepare.db", [0x52; 16])?;
        connection.execute(
            "CREATE TABLE users (id INT NOT NULL AUTO_INCREMENT PRIMARY KEY, name TEXT)",
        )?;
        connection.prepare("INSERT INTO users (name) VALUES ('Ada')")?;
        assert!(
            connection
                .execute("INSERT INTO users (name) VALUES (upper('Ada'))")
                .is_err()
        );

        let mut reservation = allocator.reserve(auto_increment_key(&connection, "users")?, 1)?;
        assert_eq!(io.block(|| reservation.step())?.first(), 1);
        connection.execute("INSERT INTO users (name) VALUES ('Grace')")?;
        assert_eq!(
            connection
                .prepare_select("SELECT id FROM users")?
                .run_collect_rows()?,
            vec![vec![Value::from_i64(2)]]
        );
        connection.close()?;
        Ok(())
    }

    #[test]
    fn auto_increment_insert_with_a_target_trigger_fails_before_reservation() -> Result<()> {
        let (connection, allocator, io) =
            open_allocator_connection("mysql-session-auto-increment-trigger.db", [0x55; 16])?;
        connection.execute(
            "CREATE TABLE users (id INT NOT NULL AUTO_INCREMENT PRIMARY KEY, name TEXT)",
        )?;
        connection.execute("CREATE TABLE audit (name TEXT)")?;
        connection.execute(
            "CREATE TRIGGER copy_user AFTER INSERT ON users FOR EACH ROW BEGIN INSERT INTO audit (name) VALUES (NEW.name); END",
        )?;

        assert!(matches!(
            connection.execute("INSERT INTO users (name) VALUES ('Ada')"),
            Err(LimboError::ParseError(_))
        ));
        let mut reservation = allocator.reserve(auto_increment_key(&connection, "users")?, 1)?;
        assert_eq!(io.block(|| reservation.step())?.first(), 1);
        connection.close()?;
        Ok(())
    }

    #[test]
    fn auto_increment_legacy_constructor_has_no_allocator_capability() -> Result<()> {
        let io: Arc<dyn IO> = Arc::new(MemoryIO::new());
        let database = open_database_with_identity(
            io,
            "mysql-session-auto-increment-no-capability.db",
            OpenFlags::Create,
            [0x53; 16],
        )?;
        let connection = MySqlConnection::new(database.connect()?, binary_context())?;
        connection.execute(
            "CREATE TABLE users (id INT NOT NULL AUTO_INCREMENT PRIMARY KEY, name TEXT)",
        )?;
        assert!(matches!(
            connection.execute("INSERT INTO users (name) VALUES ('Ada')"),
            Err(LimboError::ParseError(_))
        ));
        connection.close()?;
        Ok(())
    }

    #[test]
    fn auto_increment_reservation_is_not_rolled_back_with_the_row() -> Result<()> {
        let (connection, _allocator, _io) =
            open_allocator_connection("mysql-session-auto-increment-rollback.db", [0x54; 16])?;
        connection.execute(
            "CREATE TABLE users (id INT NOT NULL AUTO_INCREMENT PRIMARY KEY, name TEXT)",
        )?;
        connection.inner().execute("BEGIN")?;
        connection.execute("INSERT INTO users (name) VALUES ('rolled back')")?;
        connection.inner().execute("ROLLBACK")?;
        connection.execute("INSERT INTO users (name) VALUES ('kept')")?;
        assert_eq!(
            connection
                .prepare_select("SELECT id, name FROM users")?
                .run_collect_rows()?,
            vec![vec![Value::from_i64(2), Value::from_text("kept")]]
        );
        connection.close()?;
        Ok(())
    }

    #[test]
    fn last_insert_id_tracks_only_successful_generated_inserts() -> Result<()> {
        let (connection, _allocator, _io) =
            open_allocator_connection("mysql-session-last-insert-id.db", [0x56; 16])?;
        let clone = connection.clone();
        assert_eq!(connection.last_insert_id(), 0);
        let mut prepared = connection.prepare_select("SELECT LAST_INSERT_ID()")?;

        connection.execute(
            "CREATE TABLE users (id INT NOT NULL AUTO_INCREMENT PRIMARY KEY, name TEXT)",
        )?;
        connection.execute("INSERT INTO users (name) VALUES ('Ada')")?;
        assert_eq!(connection.last_insert_id(), 1);
        assert_eq!(clone.last_insert_id(), 1);
        assert_eq!(prepared.run_collect_rows()?, vec![vec![Value::from_i64(1)]]);
        prepared.reset()?;

        connection.execute("INSERT INTO users (name) VALUES ('Grace'), ('Linus')")?;
        assert_eq!(connection.last_insert_id(), 2);
        assert_eq!(prepared.run_collect_rows()?, vec![vec![Value::from_i64(2)]]);
        prepared.reset()?;

        assert!(
            connection
                .execute("INSERT INTO users (name) VALUES (upper('failed'))")
                .is_err()
        );
        assert_eq!(connection.last_insert_id(), 2);

        connection.execute("CREATE TABLE notes (id INT, body TEXT)")?;
        connection.execute("INSERT INTO notes (id, body) VALUES (9, 'ordinary')")?;
        assert_eq!(connection.last_insert_id(), 2);

        connection.inner().execute("BEGIN")?;
        connection.execute("INSERT INTO users (name) VALUES ('rolled back')")?;
        assert_eq!(connection.last_insert_id(), 4);
        connection.inner().execute("ROLLBACK")?;
        assert_eq!(connection.last_insert_id(), 4);
        assert_eq!(prepared.run_collect_rows()?, vec![vec![Value::from_i64(4)]]);
        connection.close()?;
        Ok(())
    }

    #[test]
    fn checked_write_reports_insert_delete_and_generated_results() -> Result<()> {
        let (connection, _allocator, _io) =
            open_allocator_connection("mysql-session-checked-write.db", [0x57; 16])?;
        connection.execute("CREATE TABLE notes (id INT, body TEXT)")?;

        let inserted = connection
            .execute_checked_write("INSERT INTO notes (id, body) VALUES (1, 'kept')", None)
            .unwrap();
        assert_eq!(inserted.affected_rows, 1);
        assert_eq!(inserted.last_insert_id, 0);

        let deleted = connection
            .execute_checked_write("DELETE FROM notes WHERE id IS NOT NULL", None)
            .unwrap();
        assert_eq!(deleted.affected_rows, 1);
        assert_eq!(deleted.last_insert_id, 0);
        let deleted_again = connection
            .execute_checked_write("DELETE FROM notes WHERE id IS NOT NULL", None)
            .unwrap();
        assert_eq!(deleted_again.affected_rows, 0);

        connection.execute(
            "CREATE TABLE users (id INT NOT NULL AUTO_INCREMENT PRIMARY KEY, name TEXT)",
        )?;
        let generated = connection
            .execute_checked_write("INSERT INTO users (name) VALUES ('Ada'), ('Grace')", None)
            .unwrap();
        assert_eq!(generated.affected_rows, 2);
        assert_eq!(generated.last_insert_id, 1);
        assert_eq!(connection.last_insert_id(), 1);

        assert!(matches!(
            connection.execute_checked_write("DELETE FROM missing", None),
            Err(MySqlQueryError::Engine(_))
        ));
        connection.close()?;
        Ok(())
    }

    #[test]
    fn checked_update_reports_zero_changed_rows_for_no_op_values() -> Result<()> {
        let (connection, _allocator, _io) =
            open_allocator_connection("mysql-session-checked-update-no-op.db", [0x59; 16])?;
        connection.execute("CREATE TABLE notes (id INT, body TEXT)")?;
        connection.execute("INSERT INTO notes (id, body) VALUES (1, 'kept'), (2, 'kept')")?;

        let changed = connection
            .execute_checked_write_with_affected_rows_mode(
                "UPDATE notes SET body = 'kept' WHERE TRUE",
                None,
                MySqlAffectedRowsMode::Changed,
            )
            .unwrap();
        assert_eq!(changed.affected_rows, 0);

        let matched = connection
            .execute_checked_write_with_affected_rows_mode(
                "UPDATE notes SET body = 'kept' WHERE TRUE",
                None,
                MySqlAffectedRowsMode::Matched,
            )
            .unwrap();
        assert_eq!(matched.affected_rows, 2);
        connection.close()?;
        Ok(())
    }

    #[test]
    fn explicit_transaction_commands_commit_rollback_and_no_op_when_idle() -> Result<()> {
        let (connection, _allocator, _io) =
            open_allocator_connection("mysql-session-explicit-transaction.db", [0x61; 16])?;
        connection.execute("CREATE TABLE notes (id INT, body TEXT)")?;

        connection.execute_transaction_command("COMMIT").unwrap();
        connection.execute_transaction_command("ROLLBACK").unwrap();
        assert!(connection.is_auto_commit());

        connection.execute_transaction_command("BEGIN").unwrap();
        assert!(!connection.is_auto_commit());
        connection.execute("INSERT INTO notes (id, body) VALUES (1, 'discarded')")?;
        connection.execute_transaction_command("ROLLBACK").unwrap();
        assert!(connection.is_auto_commit());
        assert!(
            connection
                .prepare_select("SELECT id FROM notes")?
                .run_collect_rows()?
                .is_empty()
        );

        connection
            .execute_transaction_command("START TRANSACTION")
            .unwrap();
        connection.execute("INSERT INTO notes (id, body) VALUES (2, 'kept')")?;
        connection.execute_transaction_command("COMMIT").unwrap();
        assert_eq!(
            connection
                .prepare_select("SELECT body FROM notes")?
                .run_collect_rows()?,
            vec![vec![Value::from_text("kept")]]
        );
        connection.close()?;
        Ok(())
    }

    #[test]
    fn autocommit_off_opens_on_write_and_survives_transaction_end() -> Result<()> {
        let (connection, _allocator, _io) =
            open_allocator_connection("mysql-session-autocommit-off.db", [0x62; 16])?;
        connection.execute("CREATE TABLE notes (id INT, body TEXT)")?;

        connection
            .execute_autocommit_setting("SET SESSION autocommit = 0")
            .unwrap();
        assert!(!connection.session_autocommit());
        assert!(connection.is_auto_commit());

        connection
            .execute_checked_write("INSERT INTO notes (id, body) VALUES (1, 'discarded')", None)
            .unwrap();
        assert!(!connection.is_auto_commit());
        connection.execute_transaction_command("ROLLBACK").unwrap();
        assert!(!connection.session_autocommit());
        assert!(connection.is_auto_commit());

        connection
            .execute_checked_write("INSERT INTO notes (id, body) VALUES (2, 'kept')", None)
            .unwrap();
        connection
            .execute_autocommit_setting("SET autocommit = 1")
            .unwrap();
        assert!(connection.session_autocommit());
        assert!(connection.is_auto_commit());
        assert_eq!(
            connection
                .prepare_select("SELECT id FROM notes")?
                .run_collect_rows()?,
            vec![vec![Value::from_i64(2)]]
        );
        connection.close()?;
        Ok(())
    }

    #[test]
    fn schema_ddl_commits_prior_work_even_when_the_ddl_fails() -> Result<()> {
        let (connection, _allocator, _io) =
            open_allocator_connection("mysql-session-ddl-implicit-commit.db", [0x65; 16])?;
        connection.execute("CREATE TABLE notes (id INT)")?;
        connection
            .execute_autocommit_setting("SET autocommit = 0")
            .unwrap();
        connection
            .execute_checked_write("INSERT INTO notes (id) VALUES (1)", None)
            .unwrap();

        assert!(
            connection
                .execute_schema_ddl("CREATE TABLE notes (id INT)")
                .is_err()
        );
        assert!(connection.is_auto_commit());
        assert!(!connection.session_autocommit());

        connection.execute_transaction_command("ROLLBACK").unwrap();
        assert_eq!(
            connection
                .prepare_select("SELECT id FROM notes")?
                .run_collect_rows()?,
            vec![vec![Value::from_i64(1)]]
        );
        connection.execute_transaction_command("ROLLBACK").unwrap();
        connection.close()?;
        Ok(())
    }

    #[test]
    fn schema_ddl_commits_prior_work_and_returns_idle_after_success() -> Result<()> {
        for (suffix, setup, ddl, schema_name, schema_kind) in [
            (
                "table",
                None,
                "CREATE TABLE created_table (id INT)",
                "created_table",
                "table",
            ),
            (
                "index",
                Some("CREATE TABLE indexed_notes (body TEXT)"),
                "CREATE INDEX idx_notes_body ON indexed_notes (body)",
                "idx_notes_body",
                "index",
            ),
            (
                "view",
                Some("CREATE TABLE viewed_notes (body TEXT)"),
                "CREATE VIEW notes_view AS SELECT body FROM viewed_notes",
                "notes_view",
                "view",
            ),
            (
                "trigger",
                Some("CREATE TABLE triggered_notes (body TEXT)"),
                "CREATE TRIGGER copy_note AFTER INSERT ON triggered_notes FOR EACH ROW BEGIN INSERT INTO committed_notes (id) VALUES (NEW.rowid); END",
                "copy_note",
                "trigger",
            ),
            (
                "alter",
                Some("CREATE TABLE altered_notes (id INT)"),
                "ALTER TABLE altered_notes ADD COLUMN body TEXT",
                "altered_notes",
                "table",
            ),
        ] {
            let path = format!("mysql-session-ddl-implicit-commit-{suffix}.db");
            let (connection, _allocator, _io) = open_allocator_connection(&path, [0x66; 16])?;
            connection.execute("CREATE TABLE committed_notes (id INT)")?;
            if let Some(setup) = setup {
                connection.execute(setup)?;
            }
            connection
                .execute_autocommit_setting("SET autocommit = 0")
                .unwrap();
            connection
                .execute_checked_write("INSERT INTO committed_notes (id) VALUES (1)", None)
                .unwrap();

            connection.execute_schema_ddl(ddl).unwrap();
            assert!(
                connection.is_auto_commit(),
                "DDL left a transaction active: {ddl}"
            );
            assert!(!connection.session_autocommit());
            assert_eq!(
                connection
                    .inner()
                    .prepare(format!(
                        "SELECT type FROM sqlite_schema WHERE name = '{schema_name}'"
                    ))?
                    .run_collect_rows()?,
                vec![vec![Value::from_text(schema_kind)]],
                "DDL did not create its schema object: {ddl}"
            );

            connection.execute_transaction_command("ROLLBACK").unwrap();
            assert_eq!(
                connection
                    .prepare_select("SELECT id FROM committed_notes")?
                    .run_collect_rows()?,
                vec![vec![Value::from_i64(1)]],
                "DDL did not commit prior work: {ddl}"
            );
            connection.execute_transaction_command("ROLLBACK").unwrap();
            connection.close()?;
        }
        Ok(())
    }

    #[test]
    fn autocommit_off_opens_on_table_select_but_not_constant_select() -> Result<()> {
        let (connection, _allocator, _io) =
            open_allocator_connection("mysql-session-autocommit-select.db", [0x63; 16])?;
        connection.execute("CREATE TABLE notes (id INT)")?;
        connection
            .execute_autocommit_setting("SET autocommit = 0")
            .unwrap();

        connection.prepare_select("SELECT 1")?.run_collect_rows()?;
        assert!(connection.is_auto_commit());

        connection
            .prepare_select("SELECT id FROM notes")?
            .run_collect_rows()?;
        assert!(!connection.is_auto_commit());

        connection.execute_transaction_command("ROLLBACK").unwrap();
        connection.close()?;
        Ok(())
    }

    #[test]
    fn autocommit_off_table_select_starts_before_engine_prepare_error() -> Result<()> {
        let (connection, _allocator, _io) =
            open_allocator_connection("mysql-session-autocommit-select-error.db", [0x64; 16])?;
        connection
            .execute_autocommit_setting("SET autocommit = 0")
            .unwrap();

        assert!(matches!(
            connection.prepare_select("SELECT id FROM missing_table"),
            Err(MySqlQueryError::Engine(_))
        ));
        assert!(!connection.is_auto_commit());

        connection.execute_transaction_command("ROLLBACK").unwrap();
        connection.close()?;
        Ok(())
    }

    #[test]
    fn checked_update_reports_changed_rows_for_actual_values() -> Result<()> {
        let (connection, _allocator, _io) =
            open_allocator_connection("mysql-session-checked-update-actual.db", [0x5a; 16])?;
        connection.execute("CREATE TABLE notes (id INT, body TEXT)")?;
        connection.execute("INSERT INTO notes (id, body) VALUES (1, 'before'), (2, 'before')")?;

        let changed = connection
            .execute_checked_write("UPDATE notes SET body = 'after' WHERE TRUE", None)
            .unwrap();
        assert_eq!(changed.affected_rows, 2);

        let matched = connection
            .execute_checked_write_with_affected_rows_mode(
                "UPDATE notes SET body = 'again' WHERE TRUE",
                None,
                MySqlAffectedRowsMode::Matched,
            )
            .unwrap();
        assert_eq!(matched.affected_rows, 2);
        connection.close()?;
        Ok(())
    }

    #[test]
    fn checked_update_allows_auto_increment_tables_but_uninjected_inserts_stay_rejected()
    -> Result<()> {
        let (connection, _allocator, _io) = open_allocator_connection(
            "mysql-session-checked-update-auto-increment.db",
            [0x60; 16],
        )?;
        connection.execute(
            "CREATE TABLE users (id INT NOT NULL AUTO_INCREMENT PRIMARY KEY, name TEXT)",
        )?;
        connection.execute("INSERT INTO users (name) VALUES ('Ada')")?;

        let updated = connection
            .execute_checked_write("UPDATE users SET name = 'Grace' WHERE TRUE", None)
            .unwrap();
        assert_eq!(updated.affected_rows, 1);
        assert_eq!(
            connection
                .prepare_select("SELECT id, name FROM users")?
                .run_collect_rows()?,
            vec![vec![Value::from_i64(1), Value::from_text("Grace")]]
        );

        let key_updated = connection
            .execute_checked_write("UPDATE users SET id = 7 WHERE TRUE", None)
            .unwrap();
        assert_eq!(key_updated.affected_rows, 1);
        assert!(matches!(
            connection.execute_checked_write(
                "UPDATE users SET name = 'unsafe', id = (id) WHERE TRUE",
                None,
            ),
            Err(MySqlQueryError::Unsupported(_))
        ));
        let unchanged = connection
            .execute_checked_write("UPDATE users SET id = ID WHERE TRUE", None)
            .unwrap();
        assert_eq!(unchanged.affected_rows, 0);

        let next = connection
            .execute_checked_write("INSERT INTO users (name) VALUES ('Linus')", None)
            .unwrap();
        assert_eq!(next.last_insert_id, 8);

        assert!(matches!(
            connection
                .prepare("INSERT INTO users (id, name) VALUES (10, 'unmanaged')")?
                .run_ignore_rows(),
            Err(LimboError::ParseError(message)) if message == "MySQL AUTO_INCREMENT inserts are not enabled"
        ));
        connection.close()?;
        Ok(())
    }

    #[test]
    fn rolled_back_auto_increment_key_update_burns_the_advanced_value() -> Result<()> {
        let (connection, _allocator, _io) = open_allocator_connection(
            "mysql-session-checked-update-auto-increment-rollback.db",
            [0x67; 16],
        )?;
        connection.execute(
            "CREATE TABLE users (id INT NOT NULL AUTO_INCREMENT PRIMARY KEY, name TEXT)",
        )?;
        connection.execute("INSERT INTO users (name) VALUES ('Ada')")?;

        connection.execute_transaction_command("BEGIN").unwrap();
        connection
            .execute_checked_write("UPDATE users SET id = 20 WHERE TRUE", None)
            .unwrap();
        connection.execute_transaction_command("ROLLBACK").unwrap();

        assert_eq!(
            connection
                .prepare_select("SELECT id FROM users")?
                .run_collect_rows()?,
            vec![vec![Value::from_i64(1)]]
        );
        let generated = connection
            .execute_checked_write("INSERT INTO users (name) VALUES ('Grace')", None)
            .unwrap();
        assert_eq!(generated.last_insert_id, 21);
        connection.close()?;
        Ok(())
    }

    #[test]
    fn failed_auto_increment_key_update_burns_the_advanced_value() -> Result<()> {
        let (connection, _allocator, _io) = open_allocator_connection(
            "mysql-session-checked-update-auto-increment-failure.db",
            [0x68; 16],
        )?;
        connection.execute(
            "CREATE TABLE users (id INT NOT NULL AUTO_INCREMENT PRIMARY KEY, name TEXT)",
        )?;
        connection.execute("INSERT INTO users (name) VALUES ('Ada'), ('Grace')")?;

        assert!(
            connection
                .execute_checked_write("UPDATE users SET id = 30 WHERE TRUE", None)
                .is_err()
        );
        let generated = connection
            .execute_checked_write("INSERT INTO users (name) VALUES ('Linus')", None)
            .unwrap();
        assert_eq!(generated.last_insert_id, 31);
        connection.close()?;
        Ok(())
    }

    #[test]
    fn checked_update_distinguishes_mixed_changed_and_matched_rows() -> Result<()> {
        let (connection, _allocator, _io) =
            open_allocator_connection("mysql-session-checked-update-mixed.db", [0x5b; 16])?;
        connection.execute("CREATE TABLE notes (id INT, body TEXT)")?;
        connection.execute(
            "INSERT INTO notes (id, body) VALUES (1, 'kept'), (2, 'replace'), (3, 'replace')",
        )?;

        let changed = connection
            .execute_checked_write_with_affected_rows_mode(
                "UPDATE notes SET body = 'kept' WHERE TRUE",
                None,
                MySqlAffectedRowsMode::Changed,
            )
            .unwrap();
        assert_eq!(changed.affected_rows, 2);

        connection.execute("UPDATE notes SET body = 'replace' WHERE TRUE")?;
        connection.execute("UPDATE notes SET body = 'kept' WHERE TRUE")?;
        let matched = connection
            .execute_checked_write_with_affected_rows_mode(
                "UPDATE notes SET body = 'kept' WHERE TRUE",
                None,
                MySqlAffectedRowsMode::Matched,
            )
            .unwrap();
        assert_eq!(matched.affected_rows, 3);
        connection.close()?;
        Ok(())
    }

    #[test]
    fn checked_update_counts_null_assignments_by_stored_value_change() -> Result<()> {
        let (connection, _allocator, _io) =
            open_allocator_connection("mysql-session-checked-update-null.db", [0x5c; 16])?;
        connection.execute("CREATE TABLE notes (id INT, body TEXT)")?;
        connection.execute("INSERT INTO notes (id, body) VALUES (1, NULL), (2, 'present')")?;

        let changed = connection
            .execute_checked_write_with_affected_rows_mode(
                "UPDATE notes SET body = NULL WHERE TRUE",
                None,
                MySqlAffectedRowsMode::Changed,
            )
            .unwrap();
        assert_eq!(changed.affected_rows, 1);

        let matched = connection
            .execute_checked_write_with_affected_rows_mode(
                "UPDATE notes SET body = NULL WHERE TRUE",
                None,
                MySqlAffectedRowsMode::Matched,
            )
            .unwrap();
        assert_eq!(matched.affected_rows, 2);
        connection.close()?;
        Ok(())
    }

    #[test]
    fn failed_checked_update_does_not_return_an_affected_row_count() -> Result<()> {
        let (connection, _allocator, _io) =
            open_allocator_connection("mysql-session-checked-update-failed.db", [0x5d; 16])?;
        connection.execute("CREATE TABLE notes (id INTEGER UNIQUE, label TEXT, body TEXT)")?;
        connection.execute(
            "INSERT INTO notes (id, label, body) VALUES (1, 'first', 'kept'), (2, 'second', 'kept')",
        )?;

        assert!(matches!(
            connection.execute_checked_write_with_affected_rows_mode(
                "UPDATE notes SET id = 1 WHERE TRUE",
                None,
                MySqlAffectedRowsMode::Changed,
            ),
            Err(MySqlQueryError::Engine(_))
        ));
        assert_eq!(
            connection
                .inner()
                .prepare("SELECT label FROM notes ORDER BY id")?
                .run_collect_rows()?,
            vec![
                vec![Value::from_text("first")],
                vec![Value::from_text("second")],
            ]
        );
        let no_op = connection
            .execute_checked_write_with_affected_rows_mode(
                "UPDATE notes SET body = 'kept' WHERE TRUE",
                None,
                MySqlAffectedRowsMode::Changed,
            )
            .unwrap();
        assert_eq!(no_op.affected_rows, 0);
        connection.close()?;
        Ok(())
    }

    #[test]
    fn checked_update_deadline_interrupts_before_mutating_rows() -> Result<()> {
        let (connection, _allocator, _io) =
            open_allocator_connection("mysql-session-checked-update-timeout.db", [0x5e; 16])?;
        connection.execute("CREATE TABLE notes (id INT, body TEXT)")?;
        connection.execute("INSERT INTO notes (id, body) VALUES (1, 'kept')")?;

        assert!(matches!(
            connection.execute_checked_write_with_affected_rows_mode(
                "UPDATE notes SET body = 'late' WHERE TRUE",
                Some(Duration::ZERO),
                MySqlAffectedRowsMode::Changed,
            ),
            Err(MySqlQueryError::Engine(LimboError::Interrupt))
        ));
        assert_eq!(
            connection
                .inner()
                .prepare("SELECT body FROM notes")?
                .run_collect_rows()?,
            vec![vec![Value::from_text("kept")]]
        );
        connection.close()?;
        Ok(())
    }

    #[test]
    fn empty_default_insert_rejects_auto_increment_without_consuming_ids() -> Result<()> {
        let (connection, _allocator, _io) =
            open_allocator_connection("mysql-session-empty-default-auto.db", [0xa2; 16])?;
        connection.execute(
            "CREATE TABLE users (id INT NOT NULL AUTO_INCREMENT PRIMARY KEY, value INT DEFAULT 7)",
        )?;
        assert!(connection
            .execute_checked_write("INSERT INTO users () VALUES ()", None)
            .is_err());
        assert!(connection
            .prepare_checked_statement("INSERT INTO users () VALUES ()")
            .is_err());
        assert!(connection
            .prepare("INSERT INTO users () VALUES ()")
            .and_then(|mut statement| statement.run_ignore_rows())
            .is_err());
        connection
            .execute_checked_write("INSERT INTO users (value) VALUES (8)", None)
            .unwrap();
        assert_eq!(
            connection
                .prepare_select("SELECT id, value FROM users")?
                .run_collect_rows()?,
            vec![vec![Value::from_i64(1), Value::from_i64(8)]]
        );
        connection.close()?;
        Ok(())
    }

    #[test]
    fn checked_write_zero_timeout_changes_nothing() -> Result<()> {
        let (connection, allocator, io) =
            open_allocator_connection("mysql-session-checked-write-timeout.db", [0x58; 16])?;
        connection.execute("CREATE TABLE notes (id INT, body TEXT)")?;
        connection.execute(
            "CREATE TABLE users (id INT NOT NULL AUTO_INCREMENT PRIMARY KEY, name TEXT)",
        )?;

        assert!(matches!(
            connection.execute_checked_write(
                "INSERT INTO notes (id, body) VALUES (1, 'late')",
                Some(Duration::ZERO),
            ),
            Err(MySqlQueryError::Engine(LimboError::Interrupt))
        ));
        assert!(
            connection
                .inner()
                .prepare("SELECT id FROM notes")?
                .run_collect_rows()?
                .is_empty()
        );

        assert!(matches!(
            connection.execute_checked_write(
                "INSERT INTO users (name) VALUES ('late')",
                Some(Duration::ZERO),
            ),
            Err(MySqlQueryError::Engine(LimboError::Interrupt))
        ));
        let mut reservation = allocator.reserve(auto_increment_key(&connection, "users")?, 1)?;
        let range = io.block(|| reservation.step())?;
        assert_eq!(range.first(), 1);
        connection.close()?;
        Ok(())
    }

    #[test]
    fn empty_mysql_database_persists_the_format_v2_policy_marker() -> Result<()> {
        let io: Arc<dyn IO> = Arc::new(MemoryIO::new());
        let path = "mysql-session-empty-format-v2.db";
        let marker = DatabaseFileOwner::mysql_application_id(
            DatabaseFileOwner::MYSQL_LOWER_CASE_TABLE_NAMES,
        ) as i64;

        {
            let db = open_database(io.clone(), path, OpenFlags::Create)?;
            let connection = db.connect()?;
            assert_eq!(
                connection
                    .prepare("PRAGMA application_id")?
                    .run_collect_rows()?,
                vec![vec![Value::from_i64(marker)]]
            );
            connection.close()?;
        }

        let db = open_database(io, path, OpenFlags::None)?;
        let connection = db.connect()?;
        assert_eq!(
            connection
                .prepare("PRAGMA application_id")?
                .run_collect_rows()?,
            vec![vec![Value::from_i64(marker)]]
        );
        connection.close()?;
        Ok(())
    }

    #[test]
    fn auto_increment_ddl_persists_trusted_identities_and_reopens_fail_closed() -> Result<()> {
        let io: Arc<dyn IO> = Arc::new(MemoryIO::new());
        let path = "mysql-session-auto-increment-v2.db";
        let database_identity = [0x31; 16];
        let db =
            open_database_with_identity(io.clone(), path, OpenFlags::Create, database_identity)?;
        let connection = MySqlConnection::new(db.connect()?, binary_context())?;
        connection.execute(
            "CREATE TABLE `users` (`id` INT NOT NULL AUTO_INCREMENT PRIMARY KEY, `name` TEXT)",
        )?;

        let rows = connection
            .inner()
            .prepare("SELECT sql FROM sqlite_schema WHERE name = 'users'")?
            .run_collect_rows()?;
        let stored = rows[0][0].to_string();
        let decoded = decode_schema_sql(SchemaSqlKind::Table, stored.trim_matches('\''))
            .map_err(|error| LimboError::Corrupt(error.to_string()))?
            .expect("AUTO_INCREMENT DDL must use a v2 schema envelope");
        let metadata = decoded
            .v2_metadata()
            .expect("AUTO_INCREMENT DDL must persist both identities");
        assert_eq!(metadata.database_id.into_bytes(), database_identity);
        assert_ne!(metadata.allocator_id.into_bytes(), [0; 16]);

        let insert_error = connection
            .inner()
            .execute("INSERT INTO users(name) VALUES ('Ada')")
            .unwrap_err();
        assert!(matches!(insert_error, LimboError::ParseError(_)));
        assert!(
            connection
                .prepare("ALTER TABLE users ADD COLUMN email TEXT")
                .is_err()
        );
        connection.close()?;
        drop(connection);
        drop(db);

        let wrong_identity =
            open_database_with_identity(io.clone(), path, OpenFlags::None, [0x32; 16]);
        let Err(wrong_identity) = wrong_identity else {
            panic!("a v2 schema must reject a different durable database identity");
        };
        assert!(matches!(wrong_identity, LimboError::Corrupt(_)));

        let db = open_database_with_identity(io, path, OpenFlags::None, database_identity)?;
        let connection = db.connect()?;
        assert_eq!(
            connection
                .prepare("SELECT count(*) FROM sqlite_schema WHERE name = 'users'")?
                .run_collect_rows()?,
            vec![vec![Value::from_i64(1)]]
        );
        connection.close()?;
        Ok(())
    }

    #[test]
    fn auto_increment_ddl_requires_a_durable_database_identity() -> Result<()> {
        let io: Arc<dyn IO> = Arc::new(MemoryIO::new());
        let db = open_database(
            io,
            "mysql-session-auto-increment-no-id.db",
            OpenFlags::Create,
        )?;
        let connection = MySqlConnection::new(db.connect()?, binary_context())?;
        let error = connection
            .prepare("CREATE TABLE users (id INT NOT NULL AUTO_INCREMENT PRIMARY KEY, name TEXT)")
            .unwrap_err();
        assert!(matches!(error, LimboError::ParseError(_)));
        assert!(
            connection
                .inner()
                .prepare("SELECT 1 FROM sqlite_schema WHERE name = 'users'")?
                .run_collect_rows()?
                .is_empty()
        );
        connection.close()?;
        Ok(())
    }

    #[test]
    fn auto_increment_ddl_rejects_non_main_catalog_targets() -> Result<()> {
        let io: Arc<dyn IO> = Arc::new(MemoryIO::new());
        let db = open_database_with_identity(
            io,
            "mysql-session-auto-increment-main-only.db",
            OpenFlags::Create,
            [0x41; 16],
        )?;
        let connection = MySqlConnection::new(db.connect()?, binary_context())?;

        for sql in [
            "CREATE TABLE app.users (id INT NOT NULL AUTO_INCREMENT PRIMARY KEY)",
            "CREATE TEMPORARY TABLE users (id INT NOT NULL AUTO_INCREMENT PRIMARY KEY)",
        ] {
            assert!(
                connection.prepare(sql).is_err(),
                "expected AUTO_INCREMENT target to be rejected: {sql}"
            );
        }
        assert!(
            connection
                .inner()
                .prepare("SELECT 1 FROM sqlite_schema WHERE name = 'users'")?
                .run_collect_rows()?
                .is_empty()
        );
        connection.close()?;
        Ok(())
    }

    #[test]
    fn create_table_persists_marker_and_reopens_with_mysql_dialect() -> Result<()> {
        let io: Arc<dyn IO> = Arc::new(MemoryIO::new());
        let path = "mysql-session-create-reopen.db";
        {
            let db = open_database(io.clone(), path, OpenFlags::Create)?;
            let connection = MySqlConnection::new(db.connect()?, binary_context())?;
            connection
                .execute("CREATE TABLE `users` (`id` INTEGER NOT NULL UNIQUE, `name` TEXT)")?;

            let rows = connection
                .inner()
                .prepare("SELECT sql FROM sqlite_schema WHERE name = 'users'")?
                .run_collect_rows()?;
            assert_eq!(rows.len(), 1);
            assert!(
                rows[0][0]
                    .to_string()
                    .trim_matches('\'')
                    .starts_with("/*@turso:mysql-schema:v1:")
            );
            connection.inner().close()?;
        }

        {
            let db = open_database(io.clone(), path, OpenFlags::None)?;
            let connection = db.connect()?;
            connection.execute("INSERT INTO users VALUES (1, 'Ada')")?;
            connection.execute("VACUUM")?;
            connection.close()?;
        }

        let db = open_database(io, path, OpenFlags::None)?;
        let connection = db.connect()?;
        assert_eq!(
            connection
                .prepare("SELECT name FROM users WHERE id = 1")?
                .run_collect_rows()?,
            vec![vec![Value::build_text("Ada")]]
        );
        connection.close()?;
        Ok(())
    }

    #[test]
    fn ordinary_integer_primary_keys_keep_mysql_metadata_without_a_rowid_alias() -> Result<()> {
        let io: Arc<dyn IO> = Arc::new(MemoryIO::new());
        let path = "mysql-session-ordinary-primary-key-reopen.db";
        {
            let db = open_database(io.clone(), path, OpenFlags::Create)?;
            let connection = MySqlConnection::new(db.connect()?, binary_context())?;
            for (table_name, type_name) in [("int_keys", "INT"), ("integer_keys", "INTEGER")] {
                let ddl = format!(
                    "CREATE TABLE `{table_name}` (`id` {type_name} PRIMARY KEY, `name` TEXT) ENGINE = InnoDB"
                );
                connection.execute(&ddl)?;

                let columns = connection
                    .list_columns(&MySqlTableName::parse(table_name).unwrap())
                    .map_err(|error| LimboError::InternalError(error.to_string()))?;
                assert_eq!(columns[0].type_name(), "INT");
                assert!(!columns[0].nullable());
                assert_eq!(columns[0].key(), MySqlColumnKey::Primary);
                assert!(columns[0].extra.is_empty());

                let table = connection
                    .inner
                    .current_schema()
                    .get_table(table_name)
                    .ok_or_else(|| {
                        LimboError::InternalError(format!("missing table {table_name}"))
                    })?;
                let btree = table.btree().ok_or_else(|| {
                    LimboError::InternalError(format!("missing btree for {table_name}"))
                })?;
                assert!(btree.get_rowid_alias_column().is_none());
                assert!(btree.unique_sets.iter().any(|set| set.is_primary_key));

                let rows = connection
                    .inner()
                    .prepare(format!(
                        "SELECT sql FROM sqlite_schema WHERE name = '{table_name}'"
                    ))?
                    .run_collect_rows()?;
                let [row] = rows.as_slice() else {
                    return Err(LimboError::InternalError(format!(
                        "expected one sqlite_schema row for {table_name}"
                    )));
                };
                let stored = row[0].to_string();
                let decoded = decode_schema_sql(SchemaSqlKind::Table, stored.trim_matches('\''))
                    .map_err(|error| LimboError::InternalError(error.to_string()))?
                    .ok_or_else(|| {
                        LimboError::InternalError(format!(
                            "missing MySQL marker for {table_name}"
                        ))
                    })?;
                assert!(decoded.normalized_ddl.contains(&format!("`id` {type_name}")));
                assert!(decoded.normalized_ddl.ends_with("ENGINE = InnoDB"));

                let insert = format!(
                    "INSERT INTO `{table_name}` (`id`, `name`) VALUES (1, 'first')"
                );
                connection.execute(&insert)?;
                let duplicate = format!(
                    "INSERT INTO `{table_name}` (`id`, `name`) VALUES (1, 'duplicate')"
                );
                assert!(connection.execute(&duplicate).is_err());
            }
            connection.inner().close()?;
        }

        let db = open_database(io, path, OpenFlags::None)?;
        let connection = MySqlConnection::new(db.connect()?, binary_context())?;
        for table_name in ["int_keys", "integer_keys"] {
            let columns = connection
                .list_columns(&MySqlTableName::parse(table_name).unwrap())
                .map_err(|error| LimboError::InternalError(error.to_string()))?;
            assert_eq!(columns[0].type_name(), "INT");
            assert_eq!(columns[0].key(), MySqlColumnKey::Primary);
        }
        connection.inner().execute("VACUUM")?;
        for table_name in ["int_keys", "integer_keys"] {
            let insert = format!(
                "INSERT INTO `{table_name}` (`id`, `name`) VALUES (2, 'second')"
            );
            connection.execute(&insert)?;
        }
        connection.inner().close()?;
        Ok(())
    }

    #[test]
    fn alter_table_preserves_marker_context_through_reopen_and_vacuum() -> Result<()> {
        let io: Arc<dyn IO> = Arc::new(MemoryIO::new());
        let path = "mysql-session-alter-reopen.db";
        {
            let db = open_database(io.clone(), path, OpenFlags::Create)?;
            let connection = MySqlConnection::new(db.connect()?, binary_context())?;
            connection.execute("CREATE TABLE `users` (`id` INTEGER, `old_name` TEXT)")?;
            let expected_context = stored_schema_context(&connection, "users")?;

            connection.execute("ALTER TABLE `users` ADD COLUMN `email` TEXT")?;
            assert_eq!(
                stored_schema_context(&connection, "users")?,
                expected_context
            );
            connection.execute("ALTER TABLE `users` RENAME COLUMN `old_name` TO `name`")?;
            assert_eq!(
                stored_schema_context(&connection, "users")?,
                expected_context
            );
            connection.execute("ALTER TABLE `users` DROP COLUMN `email`")?;
            assert_eq!(
                stored_schema_context(&connection, "users")?,
                expected_context
            );
            connection.execute("ALTER TABLE `users` RENAME TO `accounts`")?;
            assert_eq!(
                stored_schema_context(&connection, "accounts")?,
                expected_context
            );
            connection.inner().close()?;
        }

        {
            let db = open_database(io.clone(), path, OpenFlags::None)?;
            let connection = db.connect()?;
            connection.execute("INSERT INTO accounts VALUES (1, 'Ada')")?;
            connection.execute("VACUUM")?;
            connection.close()?;
        }

        let db = open_database(io, path, OpenFlags::None)?;
        let connection = db.connect()?;
        assert_eq!(
            connection
                .prepare("SELECT name FROM accounts WHERE id = 1")?
                .run_collect_rows()?,
            vec![vec![Value::build_text("Ada")]]
        );
        connection.close()?;
        Ok(())
    }

    #[test]
    fn create_then_alter_in_transaction_preserves_marker_context_through_reopen() -> Result<()> {
        let io: Arc<dyn IO> = Arc::new(MemoryIO::new());
        let path = "mysql-session-transaction-reopen.db";
        let expected_context;
        {
            let db = open_database(io.clone(), path, OpenFlags::Create)?;
            let connection = MySqlConnection::new(db.connect()?, binary_context())?;
            connection.inner().execute("BEGIN")?;
            connection.execute("CREATE TABLE `users` (`id` INTEGER)")?;
            expected_context = stored_schema_context(&connection, "users")?;
            connection.execute("ALTER TABLE `users` ADD COLUMN `name` TEXT")?;
            assert_eq!(
                stored_schema_context(&connection, "users")?,
                expected_context
            );
            connection.inner().execute("COMMIT")?;
            connection.inner().close()?;
        }

        let db = open_database(io, path, OpenFlags::None)?;
        let connection = MySqlConnection::new(db.connect()?, binary_context())?;
        assert_eq!(
            stored_schema_context(&connection, "users")?,
            expected_context
        );
        assert_eq!(
            connection
                .inner()
                .prepare("SELECT name FROM users WHERE id = 1")?
                .run_collect_rows()?,
            Vec::<Vec<Value>>::new()
        );
        connection.inner().close()?;
        Ok(())
    }

    #[test]
    fn vacuum_into_preserves_mysql_marker_and_reopens_with_mysql_dialect() -> Result<()> {
        let temp_dir = tempfile::tempdir().map_err(|error| {
            LimboError::InternalError(format!("failed to create vacuum output directory: {error}"))
        })?;
        let output_path = temp_dir.path().join("mysql-vacuum-into-output.db");
        let output_path = output_path.to_str().ok_or_else(|| {
            LimboError::InternalError("vacuum output path is not valid UTF-8".to_string())
        })?;
        let io: Arc<dyn IO> = Arc::new(MemoryIO::new());
        let expected_context;
        {
            let db = open_database(io, "mysql-session-vacuum-into-source.db", OpenFlags::Create)?;
            let connection = MySqlConnection::new(db.connect()?, binary_context())?;
            connection.execute("CREATE TABLE `users` (`id` INTEGER, `name` TEXT)")?;
            expected_context = stored_schema_context(&connection, "users")?;
            connection
                .inner()
                .execute(format!("VACUUM INTO '{output_path}'"))?;
            connection.inner().close()?;
        }

        let output_io: Arc<dyn IO> = Arc::new(PlatformIO::new()?);
        let db = open_database(output_io, output_path, OpenFlags::None)?;
        let connection = MySqlConnection::new(db.connect()?, binary_context())?;
        assert_eq!(
            stored_schema_context(&connection, "users")?,
            expected_context
        );
        connection.inner().close()?;
        Ok(())
    }

    #[test]
    fn create_index_preserves_its_marker_through_schema_rewrites_and_vacuum() -> Result<()> {
        let io: Arc<dyn IO> = Arc::new(MemoryIO::new());
        let path = "mysql-session-index-reopen.db";
        let expected_context = binary_context().for_kind(SchemaSqlKind::Index);
        {
            let db = open_database(io.clone(), path, OpenFlags::Create)?;
            let connection = MySqlConnection::new(db.connect()?, binary_context())?;
            connection.execute("CREATE TABLE `users` (`id` INTEGER, `name` TEXT)")?;
            connection
                .execute("CREATE INDEX `idx_users_name` ON `users` (`name`)")
                .map_err(|error| {
                    LimboError::InternalError(format!("create marked index failed: {error}"))
                })?;
            assert_eq!(
                stored_schema_context_for_kind(
                    &connection,
                    "idx_users_name",
                    SchemaSqlKind::Index,
                )?,
                expected_context
            );

            connection
                .execute("ALTER TABLE `users` RENAME COLUMN `name` TO `display_name`")
                .map_err(|error| {
                    LimboError::InternalError(format!("rename marked index column failed: {error}"))
                })?;
            assert_eq!(
                stored_schema_context_for_kind(
                    &connection,
                    "idx_users_name",
                    SchemaSqlKind::Index,
                )?,
                expected_context
            );
            connection
                .execute("ALTER TABLE `users` RENAME TO `accounts`")
                .map_err(|error| {
                    LimboError::InternalError(format!("rename marked index table failed: {error}"))
                })?;
            assert_eq!(
                stored_schema_context_for_kind(
                    &connection,
                    "idx_users_name",
                    SchemaSqlKind::Index,
                )?,
                expected_context
            );
            connection.inner().close()?;
        }

        {
            let db = open_database(io.clone(), path, OpenFlags::None)?;
            let connection = db.connect()?;
            connection.execute("INSERT INTO accounts VALUES (1, 'Ada')")?;
            let plan = connection
                .prepare("EXPLAIN QUERY PLAN SELECT id FROM accounts WHERE display_name = 'Ada'")?
                .run_collect_rows()?;
            assert!(
                plan.iter()
                    .flat_map(|row| row.iter())
                    .any(|value| value.to_string().contains("idx_users_name")),
                "expected index lookup plan, got {plan:?}"
            );
            connection.execute("VACUUM")?;
            connection.close()?;
        }

        let db = open_database(io, path, OpenFlags::None)?;
        let connection = MySqlConnection::new(db.connect()?, binary_context())?;
        assert_eq!(
            stored_schema_context_for_kind(&connection, "idx_users_name", SchemaSqlKind::Index)?,
            expected_context
        );
        assert_eq!(
            connection
                .inner()
                .prepare("SELECT id FROM accounts WHERE display_name = 'Ada'")?
                .run_collect_rows()?,
            vec![vec![Value::from_i64(1)]]
        );
        connection.inner().close()?;
        Ok(())
    }

    #[test]
    fn create_view_preserves_its_marker_through_reopen_and_vacuum() -> Result<()> {
        let io: Arc<dyn IO> = Arc::new(MemoryIO::new());
        let path = "mysql-session-view-reopen.db";
        let expected_context = binary_context().for_kind(SchemaSqlKind::View);
        {
            let db = open_database(io.clone(), path, OpenFlags::Create)?;
            let connection = MySqlConnection::new(db.connect()?, binary_context())?;
            connection.execute("CREATE TABLE `users` (`id` INTEGER, `name` TEXT)")?;
            connection
                .inner()
                .execute("INSERT INTO users VALUES (1, 'Ada')")?;
            connection.execute("CREATE VIEW `users_view` AS SELECT `name` FROM `users`")?;
            assert_eq!(
                stored_schema_context_for_kind(&connection, "users_view", SchemaSqlKind::View)?,
                expected_context
            );
            assert_eq!(
                connection
                    .list_columns(&MySqlTableName::parse("users_view").unwrap())
                    .map_err(|error| LimboError::InternalError(error.to_string()))?,
                vec![MySqlColumnMetadata {
                    name: "name".to_owned(),
                    type_name: "TEXT".to_owned(),
                    nullable: true,
                    key: MySqlColumnKey::None,
                    default_sql: None,
                    default_value: None,
                    extra: String::new(),
                }]
            );
            assert_eq!(
                connection
                    .inner()
                    .prepare("SELECT name FROM users_view")?
                    .run_collect_rows()?,
                vec![vec![Value::build_text("Ada")]]
            );
            connection.execute("ALTER TABLE `users` ADD COLUMN `email` TEXT")?;
            assert_eq!(
                stored_schema_context(&connection, "users")?,
                binary_context().for_kind(SchemaSqlKind::Table)
            );
            assert!(matches!(
                connection.execute("ALTER TABLE `users` DROP COLUMN `email`"),
                Err(LimboError::ParseError(_))
            ));
            assert!(matches!(
                connection.execute("ALTER TABLE `users` DROP COLUMN `name`"),
                Err(LimboError::ParseError(_))
            ));
            assert!(matches!(
                connection.execute("ALTER TABLE `users` RENAME COLUMN `name` TO `display_name`"),
                Err(LimboError::ParseError(_))
            ));
            assert!(matches!(
                connection.execute("ALTER TABLE `users` RENAME TO `accounts`"),
                Err(LimboError::ParseError(_))
            ));
            assert_eq!(
                connection
                    .inner()
                    .prepare("SELECT name FROM users_view")?
                    .run_collect_rows()?,
                vec![vec![Value::build_text("Ada")]]
            );
            connection.inner().close()?;
        }

        {
            let db = open_database(io.clone(), path, OpenFlags::None)?;
            let connection = db.connect()?;
            assert_eq!(
                connection
                    .prepare("SELECT name FROM users_view")?
                    .run_collect_rows()?,
                vec![vec![Value::build_text("Ada")]]
            );
            connection.execute("VACUUM")?;
            connection.close()?;
        }

        let db = open_database(io, path, OpenFlags::None)?;
        let connection = MySqlConnection::new(db.connect()?, binary_context())?;
        assert_eq!(
            stored_schema_context_for_kind(&connection, "users_view", SchemaSqlKind::View)?,
            expected_context
        );
        assert_eq!(
            connection
                .list_columns(&MySqlTableName::parse("users_view").unwrap())
                .map_err(|error| LimboError::InternalError(error.to_string()))?
                .len(),
            1
        );
        assert_eq!(
            connection
                .inner()
                .prepare("SELECT name FROM users_view")?
                .run_collect_rows()?,
            vec![vec![Value::build_text("Ada")]]
        );
        connection.inner().close()?;
        Ok(())
    }

    #[test]
    fn create_trigger_fires_and_preserves_its_marker_through_reopen_and_vacuum() -> Result<()> {
        let io: Arc<dyn IO> = Arc::new(MemoryIO::new());
        let path = "mysql-session-trigger-reopen.db";
        let expected_context = binary_context().for_kind(SchemaSqlKind::Trigger);
        {
            let db = open_database(io.clone(), path, OpenFlags::Create)?;
            let connection = MySqlConnection::new(db.connect()?, binary_context())?;
            connection.execute("CREATE TABLE `users` (`name` TEXT)")?;
            connection.execute("CREATE TABLE `audit` (`name` TEXT, `kind` TEXT)")?;
            connection.execute(
                "CREATE TRIGGER `copy_user` AFTER INSERT ON `users` FOR EACH ROW BEGIN INSERT INTO `audit` (`name`, `kind`) VALUES (NEW.`name`, 'created'); END",
            )?;
            assert_eq!(
                stored_schema_context_for_kind(&connection, "copy_user", SchemaSqlKind::Trigger)?,
                expected_context
            );
            connection
                .inner()
                .execute("INSERT INTO users VALUES ('Ada')")?;
            assert_eq!(
                connection
                    .inner()
                    .prepare("SELECT name, kind FROM audit")?
                    .run_collect_rows()?,
                vec![vec![Value::build_text("Ada"), Value::build_text("created")]]
            );
            assert!(matches!(
                connection.execute(
                    "CREATE TRIGGER `copy_user_again` AFTER INSERT ON `users` FOR EACH ROW BEGIN INSERT INTO `audit` (`name`, `kind`) VALUES (NEW.`name`, 'again'); END"
                ),
                Err(LimboError::ParseError(_))
            ));
            let duplicate_rows = connection
                .inner()
                .prepare("SELECT name FROM sqlite_schema WHERE name = 'copy_user_again'")?
                .run_collect_rows()?;
            assert!(duplicate_rows.is_empty());
            let table_context = stored_schema_context(&connection, "users")?;
            assert!(matches!(
                connection.execute("ALTER TABLE `users` ADD COLUMN `email` TEXT"),
                Err(LimboError::ParseError(_))
            ));
            assert_eq!(stored_schema_context(&connection, "users")?, table_context);
            connection.inner().close()?;
        }

        {
            let db = open_database(io.clone(), path, OpenFlags::None)?;
            let connection = db.connect()?;
            connection.execute("INSERT INTO users VALUES ('Grace')")?;
            assert_eq!(
                connection
                    .prepare("SELECT name, kind FROM audit ORDER BY rowid")?
                    .run_collect_rows()?,
                vec![
                    vec![Value::build_text("Ada"), Value::build_text("created")],
                    vec![Value::build_text("Grace"), Value::build_text("created")],
                ]
            );
            connection.execute("VACUUM")?;
            connection.close()?;
        }

        let db = open_database(io, path, OpenFlags::None)?;
        let connection = MySqlConnection::new(db.connect()?, binary_context())?;
        assert_eq!(
            stored_schema_context_for_kind(&connection, "copy_user", SchemaSqlKind::Trigger)?,
            expected_context
        );
        connection
            .inner()
            .execute("INSERT INTO users VALUES ('Lin')")?;
        assert_eq!(
            connection
                .inner()
                .prepare("SELECT name, kind FROM audit ORDER BY rowid")?
                .run_collect_rows()?,
            vec![
                vec![Value::build_text("Ada"), Value::build_text("created")],
                vec![Value::build_text("Grace"), Value::build_text("created")],
                vec![Value::build_text("Lin"), Value::build_text("created")],
            ]
        );
        connection.inner().close()?;
        Ok(())
    }

    #[test]
    fn strict_signed_integer_assignments_use_durable_mysql_ddl() -> Result<()> {
        use std::num::NonZeroUsize;

        let io: Arc<dyn IO> = Arc::new(MemoryIO::new());
        let path = "mysql-session-strict-signed-integers.db";
        {
            let db = open_database(io.clone(), path, OpenFlags::Create)?;
            let connection = MySqlConnection::new(db.connect()?, binary_context())?;
            connection
                .execute("CREATE TABLE `numbers` (`tiny` TINYINT, `wide` INTEGER, `label` TEXT)")?;
            let stored = connection
                .inner()
                .prepare("SELECT sql FROM sqlite_schema WHERE name = 'numbers'")?
                .run_collect_rows()?[0][0]
                .to_string();
            assert!(stored.contains("`tiny` TINYINT"));
            assert!(stored.contains("`wide` INTEGER"));

            connection.execute(
                "INSERT INTO `numbers` (`tiny`, `wide`, `label`) VALUES (-128, -2147483648, 'low'), (127, 2147483647, 'high')",
            )?;

            let mut parameterized = connection.prepare(
                "INSERT INTO `numbers` (`tiny`, `wide`, `label`) VALUES (?, ?, 'bound')",
            )?;
            parameterized.bind_at(NonZeroUsize::new(1).unwrap(), Value::from_i64(0))?;
            parameterized.bind_at(NonZeroUsize::new(2).unwrap(), Value::from_i64(1))?;
            parameterized.run_ignore_rows()?;

            let error = connection
                .execute(
                    "INSERT INTO `numbers` (`tiny`, `wide`, `label`) VALUES (0, 2147483648, 'wide-overflow')",
                )
                .unwrap_err();
            assert!(matches!(
                error,
                LimboError::Assignment(error)
                    if matches!(error.as_ref(), AssignmentError::OutOfRange { type_name, .. } if type_name == "INT")
            ));

            let error = connection
                .execute(
                    "INSERT INTO `numbers` (`tiny`, `wide`, `label`) VALUES (0, 0, 'kept'), (128, 0, 'rollback')",
                )
                .unwrap_err();
            assert!(matches!(
                error,
                LimboError::Assignment(error)
                    if matches!(error.as_ref(), AssignmentError::OutOfRange { .. })
            ));
            assert_eq!(
                connection
                    .inner()
                    .prepare("SELECT label FROM numbers ORDER BY rowid")?
                    .run_collect_rows()?,
                vec![
                    vec![Value::build_text("low")],
                    vec![Value::build_text("high")],
                    vec![Value::build_text("bound")],
                ]
            );

            let error = connection
                .execute("INSERT INTO `numbers` (`tiny`, `wide`, `label`) VALUES ('bad', 0, 'bad')")
                .unwrap_err();
            assert!(matches!(
                error,
                LimboError::Assignment(error)
                    if matches!(error.as_ref(), AssignmentError::IncorrectType { .. })
            ));

            let error = connection
                .inner()
                .execute("INSERT INTO numbers (tiny, wide, label) VALUES (128, 0, 'raw')")
                .unwrap_err();
            assert!(matches!(
                error,
                LimboError::Assignment(error)
                    if matches!(error.as_ref(), AssignmentError::OutOfRange { .. })
            ));

            connection.execute("CREATE TABLE `source` (`wide` INT)")?;
            connection.execute(
                "CREATE TRIGGER `copy_source` AFTER INSERT ON `source` FOR EACH ROW BEGIN INSERT INTO `numbers` (`tiny`, `wide`, `label`) VALUES (NEW.`wide`, 0, 'trigger'); END",
            )?;
            let error = connection
                .execute("INSERT INTO `source` (`wide`) VALUES (128)")
                .unwrap_err();
            assert!(matches!(
                error,
                LimboError::Assignment(error)
                    if matches!(error.as_ref(), AssignmentError::OutOfRange { .. })
            ));
            assert_eq!(
                connection
                    .inner()
                    .prepare("SELECT COUNT(*) FROM source")?
                    .run_collect_rows()?,
                vec![vec![Value::from_i64(0)]]
            );

            connection.execute(
                "CREATE TEMPORARY TABLE `temp_numbers` (`tiny` TINYINT, `wide` INTEGER)",
            )?;
            let error = connection
                .execute("INSERT INTO `temp_numbers` (`tiny`, `wide`) VALUES (128, 0)")
                .unwrap_err();
            assert!(matches!(
                error,
                LimboError::Assignment(error)
                    if matches!(error.as_ref(), AssignmentError::OutOfRange { type_name, .. } if type_name == "TINYINT")
            ));
            connection.execute("INSERT INTO `temp_numbers` (`tiny`, `wide`) VALUES (0, 0)")?;
            let error = connection
                .execute("UPDATE `temp_numbers` SET `wide` = 2147483648 WHERE TRUE")
                .unwrap_err();
            assert!(matches!(
                error,
                LimboError::Assignment(error)
                    if matches!(error.as_ref(), AssignmentError::OutOfRange { type_name, .. } if type_name == "INT")
            ));

            let error = connection
                .execute("UPDATE `numbers` SET `tiny` = 128 WHERE TRUE")
                .unwrap_err();
            assert!(matches!(
                error,
                LimboError::Assignment(error)
                    if matches!(error.as_ref(), AssignmentError::OutOfRange { .. })
            ));
            assert_eq!(
                connection
                    .inner()
                    .prepare("SELECT tiny FROM numbers WHERE label = 'bound'")?
                    .run_collect_rows()?,
                vec![vec![Value::from_i64(0)]]
            );
            connection.inner().close()?;
        }

        {
            let db = open_database(io.clone(), path, OpenFlags::None)?;
            let connection = MySqlConnection::new(db.connect()?, binary_context())?;
            connection.inner().execute("VACUUM")?;
            let error = connection
                .execute("UPDATE `numbers` SET `wide` = 2147483648 WHERE TRUE")
                .unwrap_err();
            assert!(matches!(
                error,
                LimboError::Assignment(error)
                    if matches!(error.as_ref(), AssignmentError::OutOfRange { type_name, .. } if type_name == "INT")
            ));
            connection.inner().close()?;
        }

        let db = open_database(io, path, OpenFlags::None)?;
        let connection = MySqlConnection::new(db.connect()?, binary_context())?;
        assert_eq!(
            connection
                .inner()
                .prepare("SELECT wide FROM numbers WHERE label = 'low'")?
                .run_collect_rows()?,
            vec![vec![Value::from_i64(-2_147_483_648)]]
        );
        connection.inner().close()?;
        Ok(())
    }

    fn stored_schema_context(
        connection: &MySqlConnection,
        name: &str,
    ) -> Result<crate::schema_sql::SchemaSqlContext> {
        stored_schema_context_for_kind(connection, name, SchemaSqlKind::Table)
    }

    fn stored_schema_context_for_kind(
        connection: &MySqlConnection,
        name: &str,
        kind: SchemaSqlKind,
    ) -> Result<crate::schema_sql::SchemaSqlContext> {
        let rows = connection
            .inner()
            .prepare(format!(
                "SELECT sql FROM sqlite_schema WHERE name = '{name}'"
            ))?
            .run_collect_rows()?;
        let [row] = rows.as_slice() else {
            return Err(LimboError::InternalError(format!(
                "expected one sqlite_schema row for {name}"
            )));
        };
        let stored = row[0].to_string();
        Ok(decode_schema_sql(kind, stored.trim_matches('\''))
            .map_err(|error| LimboError::InternalError(error.to_string()))?
            .ok_or_else(|| LimboError::InternalError(format!("missing MySQL marker for {name}")))?
            .context)
    }

    #[test]
    fn rejects_a_context_the_loader_cannot_preserve() {
        let mut context = binary_context();
        context.default_character_set = CharacterSet::Utf8mb4;
        context.default_collation = Collation::Utf8mb4_0900AiCi;
        let io: Arc<dyn IO> = Arc::new(MemoryIO::new());
        let db = open_database(io, "mysql-session-invalid-context.db", OpenFlags::Create).unwrap();

        assert!(matches!(
            MySqlConnection::new(db.connect().unwrap(), context),
            Err(LimboError::ParseError(_))
        ));
    }

    #[test]
    fn rejects_a_connection_opened_with_another_dialect() {
        let io: Arc<dyn IO> = Arc::new(MemoryIO::new());
        let path = "mysql-session-wrong-dialect.db";
        let file = io.open_file(path, OpenFlags::Create, true).unwrap();
        let db = Database::open(
            io,
            path,
            OpenOptions::new(Arc::new(turso_core::SqliteDialect))
                .storage(Arc::new(DatabaseFile::new(file)))
                .flags(OpenFlags::Create)
                .db_opts(DatabaseOpts::new()),
        )
        .unwrap();

        assert!(matches!(
            MySqlConnection::new(db.connect().unwrap(), binary_context()),
            Err(LimboError::InvalidArgument(_))
        ));
    }

    #[test]
    fn prepares_checked_selects_with_parameters_and_aliases() -> Result<()> {
        let io: Arc<dyn IO> = Arc::new(MemoryIO::new());
        let db = open_database(io, "mysql-session-select.db", OpenFlags::Create)?;
        let connection = MySqlConnection::new(db.connect()?, binary_context())?;
        connection.execute("CREATE TABLE `users` (`id` INTEGER UNIQUE, `name` TEXT)")?;
        connection
            .inner()
            .execute("INSERT INTO users VALUES (1, 'Ada'), (2, 'Grace')")?;

        let mut statement = connection.prepare(
            "SELECT u.`name` AS `display name`, ? AS marker FROM `users` AS u WHERE u.`name` IS NOT NULL",
        )?;
        assert_eq!(statement.parameters_count(), 1);
        statement.bind_at(1.try_into().unwrap(), Value::build_text("matched"))?;
        connection.execute("CREATE INDEX `idx_users_name` ON `users` (`name`)")?;
        assert_eq!(
            statement.run_collect_rows()?,
            vec![
                vec![Value::build_text("Ada"), Value::build_text("matched")],
                vec![Value::build_text("Grace"), Value::build_text("matched")]
            ]
        );
        connection.inner().close()?;
        Ok(())
    }

    #[test]
    fn query_entry_does_not_fall_back_to_unchecked_sqlite_syntax() -> Result<()> {
        let io: Arc<dyn IO> = Arc::new(MemoryIO::new());
        let db = open_database(io, "mysql-session-select-fail-closed.db", OpenFlags::Create)?;
        let connection = MySqlConnection::new(db.connect()?, binary_context())?;

        for sql in [
            "SELECT 3 / 2",
            "SELECT 1 + 2",
            "SELECT '1' = 1",
            "SELECT random()",
            "SELECT 1 UNION SELECT 2",
            "INSERT INTO t VALUES (1)",
        ] {
            assert!(
                matches!(connection.prepare(sql), Err(LimboError::ParseError(_))),
                "expected rejection for {sql}"
            );
        }
        connection.inner().close()?;
        Ok(())
    }

    #[test]
    fn dropped_view_stays_absent_after_reopen() -> Result<()> {
        let io: Arc<dyn IO> = Arc::new(MemoryIO::new());
        let path = "mysql-session-drop-view.db";
        let db = open_database(io.clone(), path, OpenFlags::Create)?;
        let connection = MySqlConnection::new(db.connect()?, binary_context())?;
        connection.execute("CREATE TABLE records (id INT)")?;
        connection.execute("CREATE VIEW records_view AS SELECT id FROM records")?;
        connection.drop_view(&MySqlTableName::parse("records_view").unwrap()).unwrap();
        connection.close()?;
        drop(connection);
        drop(db);
        let db = open_database(io, path, OpenFlags::Create)?;
        let connection = MySqlConnection::new(db.connect()?, binary_context())?;
        let tables = connection.list_tables()?;
        assert_eq!(tables.len(), 1);
        assert_eq!(tables[0].name(), "records");
        connection.execute("CREATE VIEW records_view AS SELECT id FROM records")?;
        connection.close()?;
        Ok(())
    }

    #[test]
    fn dropped_table_stays_absent_after_reopen() -> Result<()> {
        let io: Arc<dyn IO> = Arc::new(MemoryIO::new());
        let path = "mysql-session-drop-table.db";
        let db = open_database(io.clone(), path, OpenFlags::Create)?;
        let connection = MySqlConnection::new(db.connect()?, binary_context())?;
        connection.execute("CREATE TABLE records (id INT)")?;
        let command = turso_mysql_parser::parse_optional_drop_table(
            "DROP TABLE records",
            SessionSqlMode::default(),
        )
        .unwrap()
        .expect("DROP TABLE must be recognized");
        assert!(connection
            .drop_table(&command)
            .map_err(|error| LimboError::InternalError(error.to_string()))?
            .dropped);
        connection.close()?;
        drop(connection);
        drop(db);
        let db = open_database(io, path, OpenFlags::Create)?;
        let connection = MySqlConnection::new(db.connect()?, binary_context())?;
        assert!(connection.list_tables()?.is_empty());
        connection.close()?;
        Ok(())
    }

    #[test]
    fn dropping_and_recreating_auto_increment_table_gets_new_identity_and_starts_at_one()
    -> Result<()> {
        let (connection, _allocator, _io) =
            open_allocator_connection("mysql-session-drop-recreate-auto-increment.db", [0x57; 16])?;
        let ddl = "CREATE TABLE generated_records (id INT NOT NULL AUTO_INCREMENT PRIMARY KEY, label TEXT)";
        connection.execute(ddl)?;
        let first_key = auto_increment_key(&connection, "generated_records")?;
        connection.execute("INSERT INTO generated_records (label) VALUES ('first')")?;
        assert_eq!(connection.last_insert_id(), 1);

        let command = turso_mysql_parser::parse_optional_drop_table(
            "DROP TABLE generated_records",
            SessionSqlMode::default(),
        )
        .unwrap()
        .expect("DROP TABLE must be recognized");
        connection
            .drop_table(&command)
            .map_err(|error| LimboError::InternalError(error.to_string()))?;
        connection.execute(ddl)?;
        let second_key = auto_increment_key(&connection, "generated_records")?;
        assert_ne!(first_key, second_key);
        connection.execute("INSERT INTO generated_records (label) VALUES ('second')")?;
        assert_eq!(connection.last_insert_id(), 1);
        assert_eq!(
            connection
                .prepare_select("SELECT id FROM generated_records")?
                .run_collect_rows()?,
            vec![vec![Value::from_i64(1)]]
        );
        connection.close()?;
        Ok(())
    }

    #[test]
    fn lists_user_tables_and_views_in_name_order() -> Result<()> {
        let io: Arc<dyn IO> = Arc::new(MemoryIO::new());
        let path = "mysql-session-table-listing.db";
        let db = open_database(io.clone(), path, OpenFlags::Create)?;
        let connection = MySqlConnection::new(db.connect()?, binary_context())?;

        connection.execute("CREATE TABLE notes (id INT)")?;
        connection.execute("CREATE TABLE accounts (id INT)")?;
        connection.execute("CREATE VIEW active_accounts AS SELECT id FROM accounts")?;

        let expected = vec![
            MySqlTable {
                name: "accounts".to_owned(),
                kind: MySqlTableKind::BaseTable,
            },
            MySqlTable {
                name: "active_accounts".to_owned(),
                kind: MySqlTableKind::View,
            },
            MySqlTable {
                name: "notes".to_owned(),
                kind: MySqlTableKind::BaseTable,
            },
        ];
        assert_eq!(connection.list_tables()?, expected);
        connection.close()?;
        drop(connection);
        drop(db);
        let db = open_database(io, path, OpenFlags::Create)?;
        let connection = MySqlConnection::new(db.connect()?, binary_context())?;
        assert_eq!(connection.list_tables()?, expected);
        connection.close()?;
        Ok(())
    }

    #[test]
    fn lists_supported_columns_from_durable_mysql_ddl() -> Result<()> {
        let io: Arc<dyn IO> = Arc::new(MemoryIO::new());
        let path = "mysql-session-column-listing.db";
        let db = open_database(io.clone(), path, OpenFlags::Create)?;
        let connection = MySqlConnection::new(db.connect()?, binary_context())?;
        connection.execute(
            "CREATE TABLE records (id INT NOT NULL UNIQUE DEFAULT 1, name TEXT DEFAULT 'guest', payload BLOB, tiny TINYINT, small SMALLINT, maybe MEDIUMINT NULL DEFAULT NULL, `Camel` TEXT DEFAULT 'camel')",
        )?;
        connection.execute("CREATE VIEW record_view AS SELECT id, name FROM records")?;
        assert_eq!(
            connection
                .list_columns(&MySqlTableName::parse("record_view").unwrap())
                .map_err(|error| LimboError::InternalError(error.to_string()))?,
            vec![
                MySqlColumnMetadata {
                    name: "id".to_owned(),
                    type_name: "INT".to_owned(),
                    nullable: false,
                    key: MySqlColumnKey::None,
                    default_sql: None,
                    default_value: None,
                    extra: String::new(),
                },
                MySqlColumnMetadata {
                    name: "name".to_owned(),
                    type_name: "TEXT".to_owned(),
                    nullable: true,
                    key: MySqlColumnKey::None,
                    default_sql: None,
                    default_value: None,
                    extra: String::new(),
                },
            ]
        );

        assert_eq!(
            connection
                .list_columns(&MySqlTableName::parse("RECORDS").unwrap())
                .map_err(|error| LimboError::InternalError(error.to_string()))?,
            vec![
                MySqlColumnMetadata {
                    name: "id".to_owned(),
                    type_name: "INT".to_owned(),
                    nullable: false,
                    key: MySqlColumnKey::Unique,
                    default_sql: Some("1".to_owned()),
                    default_value: Some(MySqlColumnDefault::Integer {
                        text: "1".to_owned(),
                        value: 1,
                    }),
                    extra: String::new(),
                },
                MySqlColumnMetadata {
                    name: "name".to_owned(),
                    type_name: "TEXT".to_owned(),
                    nullable: true,
                    key: MySqlColumnKey::None,
                    default_sql: Some("'guest'".to_owned()),
                    default_value: Some(MySqlColumnDefault::Text("guest".to_owned())),
                    extra: String::new(),
                },
                MySqlColumnMetadata {
                    name: "payload".to_owned(),
                    type_name: "BLOB".to_owned(),
                    nullable: true,
                    key: MySqlColumnKey::None,
                    default_sql: None,
                    default_value: None,
                    extra: String::new(),
                },
                MySqlColumnMetadata {
                    name: "tiny".to_owned(),
                    type_name: "TINYINT".to_owned(),
                    nullable: true,
                    key: MySqlColumnKey::None,
                    default_sql: None,
                    default_value: None,
                    extra: String::new(),
                },
                MySqlColumnMetadata {
                    name: "small".to_owned(),
                    type_name: "SMALLINT".to_owned(),
                    nullable: true,
                    key: MySqlColumnKey::None,
                    default_sql: None,
                    default_value: None,
                    extra: String::new(),
                },
                MySqlColumnMetadata {
                    name: "maybe".to_owned(),
                    type_name: "MEDIUMINT".to_owned(),
                    nullable: true,
                    key: MySqlColumnKey::None,
                    default_sql: Some("NULL".to_owned()),
                    default_value: Some(MySqlColumnDefault::Null),
                    extra: String::new(),
                },
                MySqlColumnMetadata {
                    name: "Camel".to_owned(),
                    type_name: "TEXT".to_owned(),
                    nullable: true,
                    key: MySqlColumnKey::None,
                    default_sql: Some("'camel'".to_owned()),
                    default_value: Some(MySqlColumnDefault::Text("camel".to_owned())),
                    extra: String::new(),
                },
            ]
        );
        assert!(matches!(
            connection.list_columns(&MySqlTableName::parse("missing").unwrap()),
            Err(MySqlColumnMetadataError::TableNotFound)
        ));
        connection.execute("ALTER TABLE records ADD COLUMN added TEXT DEFAULT 'added'")?;
        let columns = connection
            .list_columns(&MySqlTableName::parse("records").unwrap())
            .map_err(|error| LimboError::InternalError(error.to_string()))?;
        assert_eq!(
            columns.last().and_then(MySqlColumnMetadata::default_sql),
            Some("'added'")
        );
        connection.inner().execute("VACUUM")?;
        assert_eq!(
            connection
                .list_columns(&MySqlTableName::parse("records").unwrap())
                .map_err(|error| LimboError::InternalError(error.to_string()))?
                .last()
                .and_then(MySqlColumnMetadata::default_sql),
            Some("'added'")
        );
        connection.close()?;
        drop(connection);
        drop(db);

        let db = open_database(io, path, OpenFlags::None)?;
        let connection = MySqlConnection::new(db.connect()?, binary_context())?;
        assert_eq!(
            connection
                .list_columns(&MySqlTableName::parse("records").unwrap())
                .map_err(|error| LimboError::InternalError(error.to_string()))?
                .len(),
            8
        );
        connection.close()?;
        Ok(())
    }

    #[test]
    fn lists_mediumint_columns_for_tables_and_direct_views() -> Result<()> {
        let io: Arc<dyn IO> = Arc::new(MemoryIO::new());
        let db = open_database(
            io,
            "mysql-session-mediumint-column-metadata.db",
            OpenFlags::Create,
        )?;
        let connection = MySqlConnection::new(db.connect()?, binary_context())?;
        connection.execute("CREATE TABLE records (id INT, value MEDIUMINT NULL)")?;
        connection.execute("CREATE VIEW record_view AS SELECT value FROM records")?;

        let table_columns = connection
            .list_columns(&MySqlTableName::parse("records").unwrap())
            .map_err(|error| LimboError::InternalError(error.to_string()))?;
        assert_eq!(table_columns[1].name(), "value");
        assert_eq!(table_columns[1].type_name(), "MEDIUMINT");
        assert!(table_columns[1].nullable());

        let view_columns = connection
            .list_columns(&MySqlTableName::parse("record_view").unwrap())
            .map_err(|error| LimboError::InternalError(error.to_string()))?;
        assert_eq!(view_columns.len(), 1);
        assert_eq!(view_columns[0].name(), "value");
        assert_eq!(view_columns[0].type_name(), "MEDIUMINT");
        assert!(view_columns[0].nullable());
        assert_eq!(view_columns[0].key(), MySqlColumnKey::None);
        assert_eq!(view_columns[0].default_value(), None);
        assert_eq!(view_columns[0].extra(), "");

        connection.close()?;
        Ok(())
    }

    #[test]
    fn rejects_view_chains_for_column_metadata() -> Result<()> {
        let io: Arc<dyn IO> = Arc::new(MemoryIO::new());
        let db = open_database(
            io,
            "mysql-session-view-chain-metadata.db",
            OpenFlags::Create,
        )?;
        let connection = MySqlConnection::new(db.connect()?, binary_context())?;
        connection.execute("CREATE TABLE records (id INT, name TEXT)")?;
        connection.execute("CREATE VIEW records_view AS SELECT id, name FROM records")?;
        connection.execute("CREATE VIEW chained_view AS SELECT id FROM records_view")?;

        assert!(matches!(
            connection.list_columns(&MySqlTableName::parse("records_view").unwrap()),
            Ok(columns) if columns.len() == 2
        ));
        assert!(matches!(
            connection.list_columns(&MySqlTableName::parse("chained_view").unwrap()),
            Err(MySqlColumnMetadataError::UnsupportedDefinition)
        ));
        connection.close()?;
        Ok(())
    }

    #[test]
    fn rejects_duplicate_view_projection_names_as_corrupt() -> Result<()> {
        let io: Arc<dyn IO> = Arc::new(MemoryIO::new());
        let db = open_database(
            io,
            "mysql-session-view-duplicate-metadata.db",
            OpenFlags::Create,
        )?;
        let connection = MySqlConnection::new(db.connect()?, binary_context())?;
        connection.execute("CREATE TABLE records (id INT, name TEXT)")?;
        connection.execute("CREATE VIEW duplicate_view AS SELECT id, ID FROM records")?;

        assert!(matches!(
            connection.list_columns(&MySqlTableName::parse("duplicate_view").unwrap()),
            Err(MySqlColumnMetadataError::CorruptDefinition)
        ));
        connection.close()?;
        Ok(())
    }

    #[test]
    fn view_projection_rejects_non_column_shapes() {
        for sql in [
            "CREATE VIEW v AS SELECT id AS renamed FROM records",
            "CREATE VIEW v AS SELECT id + 1 FROM records",
            "CREATE VIEW v AS SELECT records.id FROM records",
            "CREATE VIEW v AS SELECT records.id FROM records JOIN other ON records.id = other.id",
            "CREATE VIEW v AS SELECT id FROM records AS source",
        ] {
            let mut parser = Parser::new(sql.as_bytes());
            let Ok(Some(Cmd::Stmt(Stmt::CreateView { select, .. }))) = parser.next_cmd() else {
                panic!("expected CREATE VIEW statement for {sql:?}");
            };
            assert!(
                matches!(
                    MySqlConnection::view_projection(&select),
                    Err(MySqlColumnMetadataError::UnsupportedDefinition)
                ),
                "expected unsupported view projection for {sql:?}"
            );
        }
    }

    #[test]
    fn view_columns_survive_reopen_and_vacuum_into() -> Result<()> {
        let temp_dir = tempfile::tempdir().map_err(|error| {
            LimboError::InternalError(format!("failed to create vacuum output directory: {error}"))
        })?;
        let output_path = temp_dir.path().join("view-metadata-vacuum.db");
        let output_path = output_path.to_str().ok_or_else(|| {
            LimboError::InternalError("vacuum output path is not valid UTF-8".to_string())
        })?;
        let io: Arc<dyn IO> = Arc::new(MemoryIO::new());
        let path = "mysql-session-view-metadata.db";
        let expected = vec![
            MySqlColumnMetadata {
                name: "id".to_owned(),
                type_name: "INT".to_owned(),
                nullable: false,
                key: MySqlColumnKey::None,
                default_sql: None,
                default_value: None,
                extra: String::new(),
            },
            MySqlColumnMetadata {
                name: "name".to_owned(),
                type_name: "TEXT".to_owned(),
                nullable: true,
                key: MySqlColumnKey::None,
                default_sql: None,
                default_value: None,
                extra: String::new(),
            },
        ];
        {
            let db = open_database(io, path, OpenFlags::Create)?;
            let connection = MySqlConnection::new(db.connect()?, binary_context())?;
            connection.execute("CREATE TABLE records (id INT NOT NULL UNIQUE DEFAULT 1, name TEXT DEFAULT 'guest')")?;
            connection.execute("CREATE VIEW records_view AS SELECT id, name FROM records")?;
            assert_eq!(
                connection
                    .list_columns(&MySqlTableName::parse("records_view").unwrap())
                    .map_err(|error| LimboError::InternalError(error.to_string()))?,
                expected
            );
            connection
                .inner()
                .execute(format!("VACUUM INTO '{output_path}'"))?;
            connection.inner().close()?;
        }

        let output_io: Arc<dyn IO> = Arc::new(PlatformIO::new()?);
        let db = open_database(output_io, output_path, OpenFlags::None)?;
        let connection = MySqlConnection::new(db.connect()?, binary_context())?;
        assert_eq!(
            connection
                .list_columns(&MySqlTableName::parse("records_view").unwrap())
                .map_err(|error| LimboError::InternalError(error.to_string()))?,
            expected
        );
        connection.inner().close()?;
        Ok(())
    }

    #[test]
    fn lists_primary_and_auto_increment_metadata_after_reopen_and_vacuum_into() -> Result<()> {
        let path = "mysql-session-column-keys.db";
        let database_identity = [0x91; 16];
        let temp_dir = tempfile::tempdir().map_err(|error| {
            LimboError::InternalError(format!("failed to create vacuum output directory: {error}"))
        })?;
        let output_path = temp_dir.path().join("column-keys-vacuum.db");
        let output_path = output_path.to_str().ok_or_else(|| {
            LimboError::InternalError("vacuum output path is not valid UTF-8".to_string())
        })?;
        let (connection, _allocator, io) = open_allocator_connection(path, database_identity)?;
        connection.execute(
            "CREATE TABLE numbers_int (id INT NOT NULL AUTO_INCREMENT PRIMARY KEY, label TEXT DEFAULT ' AUTO_INCREMENT PRIMARY KEY')",
        )?;
        connection.execute(
            "CREATE TABLE numbers_integer (id INTEGER NOT NULL AUTO_INCREMENT PRIMARY KEY, label TEXT)",
        )?;
        let assert_columns = |connection: &MySqlConnection| -> Result<()> {
            for (table, type_name) in [("numbers_int", "INT"), ("numbers_integer", "INTEGER")] {
                let columns = connection
                    .list_columns(&MySqlTableName::parse(table).unwrap())
                    .map_err(|error| LimboError::InternalError(error.to_string()))?;
                assert_eq!(columns[0].name(), "id");
                assert_eq!(columns[0].type_name(), type_name);
                assert!(!columns[0].nullable());
                assert_eq!(columns[0].key(), MySqlColumnKey::Primary);
                assert_eq!(columns[0].extra(), "AUTO_INCREMENT");
                assert_eq!(columns[1].key(), MySqlColumnKey::None);
                assert_eq!(columns[1].extra(), "");
            }
            Ok(())
        };

        assert_columns(&connection)?;
        connection
            .inner()
            .execute(format!("VACUUM INTO '{output_path}'"))?;
        connection.close()?;
        drop(connection);

        let database = open_database_with_identity(io, path, OpenFlags::None, database_identity)?;
        let connection = MySqlConnection::new(database.connect()?, binary_context())?;
        assert_columns(&connection)?;
        connection.close()?;

        let output_io: Arc<dyn IO> = Arc::new(PlatformIO::new()?);
        let database = open_database_with_identity(
            output_io,
            output_path,
            OpenFlags::None,
            database_identity,
        )?;
        let connection = MySqlConnection::new(database.connect()?, binary_context())?;
        assert_columns(&connection)?;
        connection.close()?;
        Ok(())
    }

    #[test]
    fn mysql_column_metadata_accepts_plain_inline_primary_key() {
        let mut parser = Parser::new(b"CREATE TABLE keys (id INT PRIMARY KEY)");
        let Some(Cmd::Stmt(Stmt::CreateTable {
            body: CreateTableBody::ColumnsAndConstraints { columns, .. },
            ..
        })) = parser.next_cmd().unwrap()
        else {
            panic!("expected CREATE TABLE statement");
        };
        let metadata = mysql_column_metadata(&columns[0]).unwrap();
        assert_eq!(metadata.name(), "id");
        assert_eq!(metadata.type_name(), "INT");
        assert!(!metadata.nullable());
        assert_eq!(metadata.key(), MySqlColumnKey::Primary);
        assert_eq!(metadata.extra(), "");

        for sql in [
            b"CREATE TABLE keys (id INT PRIMARY KEY DESC)" as &[u8],
            b"CREATE TABLE keys (id INTEGER PRIMARY KEY AUTOINCREMENT)",
        ] {
            let mut parser = Parser::new(sql);
            let Some(Cmd::Stmt(Stmt::CreateTable {
                body: CreateTableBody::ColumnsAndConstraints { columns, .. },
                ..
            })) = parser.next_cmd().unwrap()
            else {
                panic!("expected CREATE TABLE statement");
            };
            assert!(matches!(
                mysql_column_metadata(&columns[0]),
                Err(MySqlColumnMetadataError::UnsupportedDefinition)
            ));
        }
    }

    #[test]
    fn persisted_column_defaults_decode_with_stored_sql_mode() -> Result<()> {
        let io: Arc<dyn IO> = Arc::new(MemoryIO::new());
        let path = "mysql-session-column-defaults.db";
        let db = open_database(io.clone(), path, OpenFlags::Create)?;
        let connection = MySqlConnection::new(db.connect()?, binary_context())?;
        assert!(connection
            .execute(r"CREATE TABLE unsupported_escape (value TEXT DEFAULT '\a')")
            .is_err());
        assert!(connection
            .execute("CREATE TABLE trailing_escape (value TEXT DEFAULT 'bad\\")
            .is_err());
        connection.execute(
            r"CREATE TABLE defaults (escaped TEXT DEFAULT 'line\nnext', literal_slash TEXT DEFAULT 'line\\nnext', quoted TEXT DEFAULT 'it''s', escaped_quote TEXT DEFAULT 'it\'s', integer INT DEFAULT 42, negative BIGINT DEFAULT -42, positive BIGINT DEFAULT +42, truth INT DEFAULT TRUE, falsehood INT DEFAULT FALSE, explicit_null INT DEFAULT NULL, omitted TEXT)",
        )?;

        let assert_defaults = |connection: &MySqlConnection| -> Result<()> {
            let columns = connection
                .list_columns(&MySqlTableName::parse("defaults").unwrap())
                .map_err(|error| LimboError::InternalError(error.to_string()))?;
            let default_for = |name: &str| {
                columns
                    .iter()
                    .find(|column| column.name() == name)
                    .and_then(MySqlColumnMetadata::default_value)
            };
            assert_eq!(
                default_for("escaped"),
                Some(&MySqlColumnDefault::Text("line\nnext".to_owned()))
            );
            assert_eq!(
                default_for("literal_slash"),
                Some(&MySqlColumnDefault::Text(r"line\nnext".to_owned()))
            );
            assert_eq!(
                default_for("quoted"),
                Some(&MySqlColumnDefault::Text("it's".to_owned()))
            );
            assert_eq!(
                default_for("escaped_quote"),
                Some(&MySqlColumnDefault::Text("it's".to_owned()))
            );
            assert_eq!(
                default_for("integer"),
                Some(&MySqlColumnDefault::Integer {
                    text: "42".to_owned(),
                    value: 42,
                })
            );
            assert_eq!(
                default_for("negative"),
                Some(&MySqlColumnDefault::Integer {
                    text: "-42".to_owned(),
                    value: -42,
                })
            );
            assert_eq!(
                default_for("positive"),
                Some(&MySqlColumnDefault::Integer {
                    text: "42".to_owned(),
                    value: 42,
                })
            );
            assert_eq!(
                default_for("truth"),
                Some(&MySqlColumnDefault::Boolean(true))
            );
            assert_eq!(
                default_for("falsehood"),
                Some(&MySqlColumnDefault::Boolean(false))
            );
            assert_eq!(
                default_for("explicit_null"),
                Some(&MySqlColumnDefault::Null)
            );
            assert_eq!(default_for("omitted"), None);
            Ok(())
        };
        assert_defaults(&connection)?;
        connection.close()?;
        drop(connection);
        drop(db);

        let db = open_database(io.clone(), path, OpenFlags::None)?;
        let connection = MySqlConnection::new(db.connect()?, binary_context())?;
        assert_defaults(&connection)?;
        connection.close()?;
        drop(connection);
        drop(db);

        let path = "mysql-session-column-defaults-no-backslash.db";
        let db = open_database(io.clone(), path, OpenFlags::Create)?;
        let mut context = binary_context();
        context.sql_mode.no_backslash_escapes = true;
        let connection = MySqlConnection::new(db.connect()?, context)?;
        connection.execute(
            r"CREATE TABLE defaults (literal_slash TEXT DEFAULT 'line\nnext', unrecognized TEXT DEFAULT '\a', quoted TEXT DEFAULT 'it''s')",
        )?;
        let columns = connection
            .list_columns(&MySqlTableName::parse("defaults").unwrap())
            .map_err(|error| LimboError::InternalError(error.to_string()))?;
        assert_eq!(
            columns
                .iter()
                .find(|column| column.name() == "literal_slash")
                .and_then(MySqlColumnMetadata::default_value),
            Some(&MySqlColumnDefault::Text(r"line\nnext".to_owned()))
        );
        assert_eq!(
            columns
                .iter()
                .find(|column| column.name() == "quoted")
                .and_then(MySqlColumnMetadata::default_value),
            Some(&MySqlColumnDefault::Text("it's".to_owned()))
        );
        assert_eq!(
            columns
                .iter()
                .find(|column| column.name() == "unrecognized")
                .and_then(MySqlColumnMetadata::default_value),
            Some(&MySqlColumnDefault::Text(r"\a".to_owned()))
        );
        connection.close()?;
        drop(connection);
        drop(db);

        let db = open_database(io, path, OpenFlags::None)?;
        let connection = MySqlConnection::new(db.connect()?, binary_context())?;
        let columns = connection
            .list_columns(&MySqlTableName::parse("defaults").unwrap())
            .map_err(|error| LimboError::InternalError(error.to_string()))?;
        assert_eq!(
            columns
                .iter()
                .find(|column| column.name() == "literal_slash")
                .and_then(MySqlColumnMetadata::default_value),
            Some(&MySqlColumnDefault::Text(r"line\nnext".to_owned()))
        );
        assert_eq!(
            columns
                .iter()
                .find(|column| column.name() == "quoted")
                .and_then(MySqlColumnMetadata::default_value),
            Some(&MySqlColumnDefault::Text("it's".to_owned()))
        );
        assert_eq!(
            columns
                .iter()
                .find(|column| column.name() == "unrecognized")
                .and_then(MySqlColumnMetadata::default_value),
            Some(&MySqlColumnDefault::Text(r"\a".to_owned()))
        );
        connection.close()?;
        Ok(())
    }

    #[test]
    fn column_metadata_fails_closed_for_unrepresentable_table_keys() -> Result<()> {
        let io: Arc<dyn IO> = Arc::new(MemoryIO::new());
        let db = open_database(
            io,
            "mysql-session-column-listing-unsupported.db",
            OpenFlags::Create,
        )?;
        let connection = MySqlConnection::new(db.connect()?, binary_context())?;

        for name in ["sqlite_sequence", "__turso_internal_seq_records"] {
            assert!(matches!(
                connection.list_columns(&MySqlTableName::parse(name).unwrap()),
                Err(MySqlColumnMetadataError::TableNotFound)
            ));
        }

        connection.execute("CREATE TABLE records (id INT, name TEXT)")?;
        connection.execute("CREATE INDEX records_name_idx ON records (name)")?;

        assert!(matches!(
            connection.list_columns(&MySqlTableName::parse("records").unwrap()),
            Err(MySqlColumnMetadataError::UnsupportedDefinition)
        ));

        connection.execute("CREATE TABLE keyed (id INT, name TEXT, UNIQUE (id, name))")?;
        assert!(matches!(
            connection.list_columns(&MySqlTableName::parse("keyed").unwrap()),
            Err(MySqlColumnMetadataError::UnsupportedDefinition)
        ));

        connection.execute("CREATE TABLE decimal_default (value INT DEFAULT 1.25)")?;
        assert!(matches!(
            connection.list_columns(&MySqlTableName::parse("decimal_default").unwrap()),
            Err(MySqlColumnMetadataError::UnsupportedDefinition)
        ));
        connection
            .execute("CREATE TABLE integer_overflow (value BIGINT DEFAULT 9223372036854775808)")?;
        assert!(matches!(
            connection.list_columns(&MySqlTableName::parse("integer_overflow").unwrap()),
            Err(MySqlColumnMetadataError::UnsupportedDefinition)
        ));
        connection.close()?;
        Ok(())
    }

    #[test]
    fn column_metadata_distinguishes_corrupt_and_unsupported_ddl() {
        let malformed = parse_create_table_ast(
            "CREATE TABLE records (id INT",
            SessionSqlMode::default(),
        )
        .unwrap_err();
        assert!(matches!(
            mysql_metadata_parse_error(malformed),
            MySqlColumnMetadataError::CorruptDefinition
        ));

        let unsupported = parse_create_table_ast(
            "CREATE TABLE records (id TEXT PRIMARY KEY)",
            SessionSqlMode::default(),
        )
        .unwrap_err();
        assert!(matches!(
            mysql_metadata_parse_error(unsupported),
            MySqlColumnMetadataError::UnsupportedDefinition
        ));

        let malformed = parse_create_view_ast(
            "CREATE VIEW records_view AS SELECT",
            SessionSqlMode::default(),
        )
        .unwrap_err();
        assert!(matches!(
            mysql_metadata_parse_error(malformed),
            MySqlColumnMetadataError::CorruptDefinition
        ));

        let unsupported = parse_create_view_ast(
            "CREATE VIEW records_view AS SELECT id FROM records WHERE id > 1",
            SessionSqlMode::default(),
        )
        .unwrap_err();
        assert!(matches!(
            mysql_metadata_parse_error(unsupported),
            MySqlColumnMetadataError::UnsupportedDefinition
        ));
    }

    #[test]
    fn table_listing_detects_the_limit_sentinel() {
        assert!(!MySqlConnection::table_list_is_truncated(
            TABLE_LIST_SCAN_LIMIT - 1
        ));
        assert!(MySqlConnection::table_list_is_truncated(
            TABLE_LIST_SCAN_LIMIT
        ));
        assert!(!MySqlConnection::column_index_scan_is_truncated(
            COLUMN_INDEX_SCAN_LIMIT - 1
        ));
        assert!(MySqlConnection::column_index_scan_is_truncated(
            COLUMN_INDEX_SCAN_LIMIT
        ));
    }

    #[test]
    fn select_prepare_preserves_parser_and_engine_error_stages() -> Result<()> {
        let io: Arc<dyn IO> = Arc::new(MemoryIO::new());
        let db = open_database(io, "mysql-session-select-errors.db", OpenFlags::Create)?;
        let connection = MySqlConnection::new(db.connect()?, binary_context())?;

        assert!(matches!(
            connection.prepare_select("SELECT FROM"),
            Err(MySqlQueryError::Syntax(_))
        ));
        assert!(matches!(
            connection.prepare_select("SELECT id FROM missing_table"),
            Err(MySqlQueryError::Engine(_))
        ));

        connection.close()?;
        Ok(())
    }

    #[test]
    fn system_catalog_selects_fail_closed_before_transaction_or_core_prepare() -> Result<()> {
        let io: Arc<dyn IO> = Arc::new(MemoryIO::new());
        let db = open_database(
            io,
            "mysql-session-internal-catalog-guard.db",
            OpenFlags::Create,
        )?;
        let connection = MySqlConnection::new(db.connect()?, binary_context())?;
        let queries = [
            "SELECT name FROM sqlite_schema",
            "SELECT name FROM SQLITE_MASTER",
            "SELECT name FROM sqlite_sequence",
            "SELECT name FROM `SQLite_Sequence`",
            "SELECT name FROM \"sqlite_schema\"",
            "SELECT name FROM 'sqlite_master'",
            "SELECT name FROM __turso_internal_types",
            "SELECT name FROM `__TURSO_INTERNAL_seq_records`",
            "/* leading */ SELECT name FROM sqlite_schema AS catalog /* trailing */",
        ];
        let expected = "SELECT from an internal catalog is unsupported";

        for sql in queries {
            assert!(
                matches!(
                    connection.prepare_select(sql),
                    Err(MySqlQueryError::Unsupported(message)) if message == expected
                ),
                "unexpected prepare_select result for {sql:?}"
            );
            assert!(
                connection.is_auto_commit(),
                "catalog rejection must not start a transaction for {sql:?}"
            );
            assert!(
                matches!(
                    connection.prepare_checked_statement(sql),
                    Err(MySqlPreparedStatementError::Prepare(
                        MySqlQueryError::Unsupported(message)
                    )) if message == expected
                ),
                "unexpected prepare_checked_statement result for {sql:?}"
            );
        }

        assert_eq!(connection.prepared_statement_metadata(1), None);
        connection.close()?;
        Ok(())
    }

    #[test]
    fn generic_core_create_index_requires_mysql_schema_context() -> Result<()> {
        let io: Arc<dyn IO> = Arc::new(MemoryIO::new());
        let db = open_database(
            io,
            "mysql-session-generic-create-index.db",
            OpenFlags::Create,
        )?;
        let connection = MySqlConnection::new(db.connect()?, binary_context())?;
        connection.execute("CREATE TABLE `users` (`name` TEXT)")?;

        let error = connection
            .inner()
            .execute("CREATE INDEX idx_users_name ON users (name)")
            .unwrap_err();
        assert!(matches!(error, LimboError::ParseError(_)));

        let rows = connection
            .inner()
            .prepare("SELECT name FROM sqlite_schema WHERE name = 'idx_users_name'")?
            .run_collect_rows()?;
        assert!(rows.is_empty());
        connection.inner().close()?;
        Ok(())
    }

    #[test]
    fn generic_core_create_view_requires_mysql_schema_context() -> Result<()> {
        let io: Arc<dyn IO> = Arc::new(MemoryIO::new());
        let db = open_database(
            io,
            "mysql-session-generic-create-view.db",
            OpenFlags::Create,
        )?;
        let connection = MySqlConnection::new(db.connect()?, binary_context())?;
        connection.execute("CREATE TABLE `users` (`name` TEXT)")?;

        let error = connection
            .inner()
            .execute("CREATE VIEW users_view AS SELECT name FROM users")
            .unwrap_err();
        assert!(matches!(error, LimboError::ParseError(_)));

        let rows = connection
            .inner()
            .prepare("SELECT name FROM sqlite_schema WHERE name = 'users_view'")?
            .run_collect_rows()?;
        assert!(rows.is_empty());
        connection.inner().close()?;
        Ok(())
    }

    #[test]
    fn generic_core_create_materialized_view_is_rejected_without_a_schema_row() -> Result<()> {
        let io: Arc<dyn IO> = Arc::new(MemoryIO::new());
        let db = open_database(
            io,
            "mysql-session-generic-create-materialized-view.db",
            OpenFlags::Create,
        )?;
        let connection = MySqlConnection::new(db.connect()?, binary_context())?;
        connection.execute("CREATE TABLE `users` (`name` TEXT)")?;

        let error = connection
            .inner()
            .execute("CREATE MATERIALIZED VIEW users_view AS SELECT name FROM users")
            .unwrap_err();
        assert!(matches!(error, LimboError::ParseError(_)));
        assert!(
            error
                .to_string()
                .contains("MySQL schema formatter supports only CREATE VIEW"),
            "unexpected error: {error}"
        );

        let rows = connection
            .inner()
            .prepare("SELECT name FROM sqlite_schema WHERE name = 'users_view'")?
            .run_collect_rows()?;
        assert!(rows.is_empty());
        connection.inner().close()?;
        Ok(())
    }

    #[test]
    fn generic_core_create_trigger_requires_mysql_schema_context() -> Result<()> {
        let io: Arc<dyn IO> = Arc::new(MemoryIO::new());
        let db = open_database(
            io,
            "mysql-session-generic-create-trigger.db",
            OpenFlags::Create,
        )?;
        let connection = MySqlConnection::new(db.connect()?, binary_context())?;
        connection.execute("CREATE TABLE `users` (`name` TEXT)")?;
        connection.execute("CREATE TABLE `audit` (`name` TEXT)")?;

        let error = connection
            .inner()
            .execute(
                "CREATE TRIGGER copy_user AFTER INSERT ON users FOR EACH ROW BEGIN INSERT INTO audit (name) VALUES (NEW.name); END",
            )
            .unwrap_err();
        assert!(matches!(error, LimboError::ParseError(_)));
        assert!(
            error
                .to_string()
                .contains("MySQL CREATE TRIGGER requires SchemaSqlSessionContext"),
            "unexpected error: {error}"
        );

        let rows = connection
            .inner()
            .prepare("SELECT name FROM sqlite_schema WHERE name = 'copy_user'")?
            .run_collect_rows()?;
        assert!(rows.is_empty());
        connection.inner().close()?;
        Ok(())
    }

    #[test]
    fn prepared_select_stores_metadata_without_starting_a_transaction() -> Result<()> {
        let (connection, _allocator, _io) =
            open_allocator_connection("mysql-session-prepared-select-metadata.db", [0x69; 16])?;
        connection.execute("CREATE TABLE users (id INT, name TEXT)")?;
        connection.execute("INSERT INTO users (id, name) VALUES (7, 'Ada')")?;

        assert!(connection.is_auto_commit());
        let metadata = connection
            .prepare_checked_statement("SELECT id, ? AS input, 'ready' AS status FROM users")
            .unwrap();

        assert_eq!(metadata.statement_id, 1);
        assert_eq!(metadata.parameter_count, 1);
        assert_eq!(
            metadata.result_columns,
            vec![
                MySqlPreparedResultColumn {
                    name: "id".to_string(),
                    type_name: Some("INTEGER".to_string()),
                },
                MySqlPreparedResultColumn {
                    name: "input".to_string(),
                    type_name: None,
                },
                MySqlPreparedResultColumn {
                    name: "status".to_string(),
                    type_name: Some("TEXT".to_string()),
                },
            ]
        );
        assert!(connection.is_auto_commit());
        assert_eq!(
            connection.prepared_statement_metadata(metadata.statement_id),
            Some(metadata.clone())
        );

        let rows = connection
            .with_prepared_statement(metadata.statement_id, |statement| {
                statement.bind_at(std::num::NonZero::new(1).unwrap(), Value::from_i64(7))?;
                statement.run_collect_rows()
            })
            .unwrap();
        assert_eq!(
            rows,
            vec![vec![
                Value::from_i64(7),
                Value::from_i64(7),
                Value::from_text("ready"),
            ]]
        );
        connection.close()?;
        Ok(())
    }

    #[test]
    fn prepared_metadata_preserves_declared_integer_widths_for_empty_and_null_rows() -> Result<()> {
        let path = "mysql-session-prepared-declared-types.db";
        let database_identity = [0x9a; 16];
        let (connection, _allocator, io) = open_allocator_connection(path, database_identity)?;
        connection.execute(
            "CREATE TABLE widths (tiny TINYINT, small SMALLINT, int_value INT, integer_value INTEGER, big BIGINT)",
        )?;
        let query = "SELECT tiny, small, int_value, integer_value, big FROM widths";
        let expected_types = ["INTEGER"; 5];
        let expected_declared_types = ["TINYINT", "SMALLINT", "INT", "INTEGER", "BIGINT"];
        let assert_metadata = |connection: &MySqlConnection| -> Result<u32> {
            let metadata = connection.prepare_checked_statement(query).unwrap();
            assert_eq!(
                metadata
                    .result_columns
                    .iter()
                    .map(|column| column.type_name.as_deref())
                    .collect::<Vec<_>>(),
                expected_types
                    .iter()
                    .map(|type_name| Some(*type_name))
                    .collect::<Vec<_>>()
            );
            let type_metadata = connection
                .prepared_statement_result_column_type_metadata(metadata.statement_id)
                .unwrap();
            assert_eq!(
                type_metadata
                    .iter()
                    .map(|column| column.declared_type_name())
                    .collect::<Vec<_>>(),
                expected_declared_types
                    .iter()
                    .map(|type_name| Some(*type_name))
                    .collect::<Vec<_>>()
            );
            assert_eq!(
                type_metadata
                    .iter()
                    .map(|column| column.source_reference())
                    .collect::<Vec<_>>(),
                vec![
                    Some(("widths", 0)),
                    Some(("widths", 1)),
                    Some(("widths", 2)),
                    Some(("widths", 3)),
                    Some(("widths", 4)),
                ]
            );
            Ok(metadata.statement_id)
        };

        let statement_id = assert_metadata(&connection)?;
        assert!(connection
            .execute_prepared_select(statement_id, &[], None)
            .map_err(|error| LimboError::InternalError(error.to_string()))?
            .is_empty());
        connection
            .inner()
            .execute("INSERT INTO widths VALUES (NULL, NULL, NULL, NULL, NULL)")?;
        assert_eq!(
            connection
                .execute_prepared_select(statement_id, &[], None)
                .map_err(|error| LimboError::InternalError(error.to_string()))?,
            vec![vec![
                MySqlPreparedValue::Null,
                MySqlPreparedValue::Null,
                MySqlPreparedValue::Null,
                MySqlPreparedValue::Null,
                MySqlPreparedValue::Null,
            ]]
        );

        let metadata = connection
            .prepare_checked_statement(
                "SELECT tiny AS tiny_alias, 1 AS literal_expression, NULL AS null_expression FROM widths",
            )
            .unwrap();
        assert_eq!(
            metadata
                .result_columns
                .iter()
                .map(|column| column.type_name.as_deref())
                .collect::<Vec<_>>(),
            vec![Some("INTEGER"), Some("INTEGER"), None]
        );
        let type_metadata = connection
            .prepared_statement_result_column_type_metadata(metadata.statement_id)
            .unwrap();
        assert_eq!(
            type_metadata
                .iter()
                .map(|column| column.declared_type_name())
                .collect::<Vec<_>>(),
            vec![Some("TINYINT"), None, None]
        );
        assert_eq!(
            type_metadata
                .iter()
                .map(|column| column.source_reference())
                .collect::<Vec<_>>(),
            vec![Some(("widths", 0)), None, None]
        );
        let alias_metadata = connection
            .prepare_checked_statement("SELECT tiny AS tiny_alias FROM widths AS source")
            .unwrap();
        let alias_type_metadata = connection
            .prepared_statement_result_column_type_metadata(alias_metadata.statement_id)
            .unwrap();
        assert_eq!(
            alias_type_metadata[0].source_reference(),
            Some(("source", 0))
        );
        connection.close()?;
        drop(connection);

        let database = open_database_with_identity(io, path, OpenFlags::None, database_identity)?;
        let connection = MySqlConnection::new(database.connect()?, binary_context())?;
        assert_metadata(&connection)?;
        connection.close()?;
        Ok(())
    }

    #[test]
    fn prepared_metadata_refreshes_after_wildcard_reprepare() -> Result<()> {
        let (connection, _allocator, _io) =
            open_allocator_connection("mysql-session-prepared-wildcard-reprepare.db", [0xa1; 16])?;
        connection.execute("CREATE TABLE reprepare_metadata (id INT)")?;
        let metadata = connection
            .prepare_checked_statement(
                "SELECT *, 0001 AS literal_value FROM reprepare_metadata LIMIT 0",
            )
            .unwrap();
        assert_eq!(metadata.result_columns.len(), 2);
        connection.execute("ALTER TABLE reprepare_metadata ADD COLUMN ignored TEXT")?;
        assert!(connection
            .execute_prepared_select(metadata.statement_id, &[], None)
            .map_err(|error| LimboError::InternalError(error.to_string()))?
            .is_empty());
        let refreshed = connection
            .prepared_statement_metadata(metadata.statement_id)
            .expect("prepared metadata must remain registered after execution");
        let type_metadata = connection
            .prepared_statement_result_column_type_metadata(metadata.statement_id)
            .expect("prepared type metadata must remain registered after execution");
        assert_eq!(refreshed.result_columns.len(), 3);
        assert_eq!(
            type_metadata
                .iter()
                .map(|column| column.source_reference())
                .collect::<Vec<_>>(),
            vec![
                Some(("reprepare_metadata", 0)),
                Some(("reprepare_metadata", 1)),
                None,
            ]
        );
        assert!(matches!(
            type_metadata[2].static_metadata(),
            Some(StaticSelectMetadata::Integer { digit_count, .. }) if *digit_count == 4
        ));
        connection.close()?;
        Ok(())
    }

    #[test]
    fn executes_prepared_select_values_and_reuses_the_statement() -> Result<()> {
        let (connection, _allocator, _io) =
            open_allocator_connection("mysql-session-prepared-select-execute.db", [0x6c; 16])?;
        let metadata = connection
            .prepare_checked_statement("SELECT ?, ?, ?, ?, ?")
            .unwrap();
        let first = vec![
            MySqlPreparedValue::Null,
            MySqlPreparedValue::Integer(-7),
            MySqlPreparedValue::Real(1.5),
            MySqlPreparedValue::Text("Ada".to_string()),
            MySqlPreparedValue::Blob(vec![0x01, 0x02]),
        ];
        assert_eq!(
            connection
                .execute_prepared_select(metadata.statement_id, &first, None)
                .unwrap(),
            vec![vec![
                MySqlPreparedValue::Null,
                MySqlPreparedValue::Integer(-7),
                MySqlPreparedValue::Real(1.5),
                MySqlPreparedValue::Text("Ada".to_string()),
                MySqlPreparedValue::Blob(vec![0x01, 0x02]),
            ]]
        );

        let count_error = connection
            .execute_prepared_select(metadata.statement_id, &[], None)
            .unwrap_err();
        assert!(matches!(
            count_error,
            MySqlPreparedStatementError::ParameterCountMismatch {
                expected: 5,
                actual: 0
            }
        ));

        let second = vec![
            MySqlPreparedValue::Integer(42),
            MySqlPreparedValue::Real(-2.25),
            MySqlPreparedValue::Text("Grace".to_string()),
            MySqlPreparedValue::Blob(vec![0xff]),
            MySqlPreparedValue::Null,
        ];
        assert_eq!(
            connection
                .execute_prepared_select(metadata.statement_id, &second, None)
                .unwrap(),
            vec![vec![
                MySqlPreparedValue::Integer(42),
                MySqlPreparedValue::Real(-2.25),
                MySqlPreparedValue::Text("Grace".to_string()),
                MySqlPreparedValue::Blob(vec![0xff]),
                MySqlPreparedValue::Null,
            ]]
        );
        let expanded = connection
            .with_prepared_statement(metadata.statement_id, |statement| {
                Ok(statement.expanded_sql())
            })
            .unwrap();
        assert!(
            expanded.contains("'Grace'"),
            "unexpected expanded SQL: {expanded}"
        );
        connection.close()?;
        Ok(())
    }

    #[test]
    fn prepared_select_table_read_starts_implicit_transaction_at_execute() -> Result<()> {
        let (connection, _allocator, _io) =
            open_allocator_connection("mysql-session-prepared-select-read-txn.db", [0x6d; 16])?;
        connection.execute("CREATE TABLE users (id INTEGER)")?;
        connection.execute("INSERT INTO users (id) VALUES (7)")?;

        let metadata = connection
            .prepare_checked_statement("SELECT id, ? FROM users")
            .unwrap();
        assert!(connection.is_auto_commit());

        connection.set_autocommit(false).unwrap();
        assert!(!connection.session_autocommit());
        assert!(connection.is_auto_commit());
        assert_eq!(
            connection
                .execute_prepared_select(
                    metadata.statement_id,
                    &[MySqlPreparedValue::Text("bound".to_string())],
                    None,
                )
                .unwrap(),
            vec![vec![
                MySqlPreparedValue::Integer(7),
                MySqlPreparedValue::Text("bound".to_string()),
            ]]
        );
        assert!(!connection.is_auto_commit());

        connection.set_autocommit(true).unwrap();
        assert!(connection.is_auto_commit());
        connection.close()?;
        Ok(())
    }

    #[test]
    fn prepared_select_timeout_resets_statement_for_reuse() -> Result<()> {
        let (connection, _allocator, _io) =
            open_allocator_connection("mysql-session-prepared-select-timeout.db", [0x6e; 16])?;
        let metadata = connection.prepare_checked_statement("SELECT ?").unwrap();

        assert!(matches!(
            connection.execute_prepared_select(
                metadata.statement_id,
                &[MySqlPreparedValue::Integer(1)],
                Some(Duration::ZERO),
            ),
            Err(MySqlPreparedStatementError::Engine(_))
        ));
        assert_eq!(
            connection
                .execute_prepared_select(
                    metadata.statement_id,
                    &[MySqlPreparedValue::Integer(2)],
                    None,
                )
                .unwrap(),
            vec![vec![MySqlPreparedValue::Integer(2)]]
        );
        connection.close()?;
        Ok(())
    }

    #[test]
    fn prepared_select_callback_error_resets_statement_for_reuse() -> Result<()> {
        let (connection, _allocator, _io) = open_allocator_connection(
            "mysql-session-prepared-select-callback-error.db",
            [0x6f; 16],
        )?;
        let metadata = connection.prepare_checked_statement("SELECT ?").unwrap();

        assert!(matches!(
            connection.execute_prepared_select_with_row_callback(
                metadata.statement_id,
                &[MySqlPreparedValue::Integer(1)],
                None,
                |_| Err(LimboError::TooBig),
            ),
            Err(MySqlPreparedStatementError::Engine(LimboError::TooBig))
        ));
        assert_eq!(
            connection
                .execute_prepared_select(
                    metadata.statement_id,
                    &[MySqlPreparedValue::Integer(2)],
                    None,
                )
                .unwrap(),
            vec![vec![MySqlPreparedValue::Integer(2)]]
        );
        connection.close()?;
        Ok(())
    }

    #[test]
    fn prepared_insert_reuses_bindings_without_starting_a_transaction_at_prepare() -> Result<()> {
        let (connection, _allocator, _io) =
            open_allocator_connection("mysql-session-prepared-insert-reuse.db", [0x70; 16])?;
        connection.execute("CREATE TABLE notes (id INT, body TEXT)")?;
        connection.set_autocommit(false)?;

        let metadata = connection
            .prepare_checked_statement("INSERT INTO notes (id, body) VALUES (?, ?)")
            .unwrap();
        assert!(metadata.result_columns.is_empty());
        assert!(connection.is_auto_commit());

        let first = connection
            .execute_prepared_statement(
                metadata.statement_id,
                &[
                    MySqlPreparedValue::Integer(1),
                    MySqlPreparedValue::Text("Ada".to_string()),
                ],
                None,
                MySqlAffectedRowsMode::Changed,
            )
            .unwrap();
        assert_eq!(
            first,
            MySqlPreparedExecutionResult::Write(MySqlWriteResult {
                affected_rows: 1,
                last_insert_id: 0,
            })
        );
        assert!(!connection.is_auto_commit());
        connection.execute_transaction_command("COMMIT")?;

        let second = connection
            .execute_prepared_statement(
                metadata.statement_id,
                &[
                    MySqlPreparedValue::Integer(2),
                    MySqlPreparedValue::Text("Grace".to_string()),
                ],
                None,
                MySqlAffectedRowsMode::Changed,
            )
            .unwrap();
        assert_eq!(
            second,
            MySqlPreparedExecutionResult::Write(MySqlWriteResult {
                affected_rows: 1,
                last_insert_id: 0,
            })
        );
        connection.set_autocommit(true)?;
        assert_eq!(
            connection
                .prepare_select("SELECT id, body FROM notes")?
                .run_collect_rows()?,
            vec![
                vec![Value::from_i64(1), Value::from_text("Ada")],
                vec![Value::from_i64(2), Value::from_text("Grace")],
            ]
        );
        connection.close()?;
        Ok(())
    }

    #[test]
    fn prepared_update_uses_requested_affected_rows_mode_and_prepared_delete_returns_ok()
    -> Result<()> {
        let (connection, _allocator, _io) =
            open_allocator_connection("mysql-session-prepared-update-delete.db", [0x71; 16])?;
        connection.execute("CREATE TABLE notes (id INT, body TEXT)")?;
        connection.execute("INSERT INTO notes (id, body) VALUES (1, 'kept'), (2, 'kept')")?;

        let update = connection
            .prepare_checked_statement("UPDATE notes SET body = ? WHERE TRUE")
            .unwrap();
        assert_eq!(
            connection
                .execute_prepared_statement(
                    update.statement_id,
                    &[MySqlPreparedValue::Text("kept".to_string())],
                    None,
                    MySqlAffectedRowsMode::Changed,
                )
                .unwrap(),
            MySqlPreparedExecutionResult::Write(MySqlWriteResult {
                affected_rows: 0,
                last_insert_id: 0,
            })
        );
        assert_eq!(
            connection
                .execute_prepared_statement(
                    update.statement_id,
                    &[MySqlPreparedValue::Text("kept".to_string())],
                    None,
                    MySqlAffectedRowsMode::Matched,
                )
                .unwrap(),
            MySqlPreparedExecutionResult::Write(MySqlWriteResult {
                affected_rows: 2,
                last_insert_id: 0,
            })
        );

        let delete = connection
            .prepare_checked_statement("DELETE FROM notes WHERE TRUE")
            .unwrap();
        assert_eq!(
            connection
                .execute_prepared_statement(
                    delete.statement_id,
                    &[],
                    None,
                    MySqlAffectedRowsMode::Changed,
                )
                .unwrap(),
            MySqlPreparedExecutionResult::Write(MySqlWriteResult {
                affected_rows: 2,
                last_insert_id: 0,
            })
        );
        connection.close()?;
        Ok(())
    }

    #[test]
    fn failed_prepared_write_resets_the_statement_for_reuse() -> Result<()> {
        let (connection, _allocator, _io) =
            open_allocator_connection("mysql-session-prepared-write-reset.db", [0x72; 16])?;
        connection.execute("CREATE TABLE notes (id INTEGER UNIQUE, body TEXT)")?;
        connection.execute("INSERT INTO notes (id, body) VALUES (1, 'kept')")?;
        let metadata = connection
            .prepare_checked_statement("INSERT INTO notes (id, body) VALUES (?, ?)")
            .unwrap();

        assert!(matches!(
            connection.execute_prepared_statement(
                metadata.statement_id,
                &[
                    MySqlPreparedValue::Integer(1),
                    MySqlPreparedValue::Text("duplicate".to_string()),
                ],
                None,
                MySqlAffectedRowsMode::Changed,
            ),
            Err(MySqlPreparedStatementError::Engine(_))
        ));
        assert_eq!(
            connection
                .execute_prepared_statement(
                    metadata.statement_id,
                    &[
                        MySqlPreparedValue::Integer(2),
                        MySqlPreparedValue::Text("reused".to_string()),
                    ],
                    None,
                    MySqlAffectedRowsMode::Changed,
                )
                .unwrap(),
            MySqlPreparedExecutionResult::Write(MySqlWriteResult {
                affected_rows: 1,
                last_insert_id: 0,
            })
        );
        connection.close()?;
        Ok(())
    }

    #[test]
    fn prepared_write_timeout_resets_the_statement_without_mutating_rows() -> Result<()> {
        let (connection, _allocator, _io) =
            open_allocator_connection("mysql-session-prepared-write-timeout.db", [0x74; 16])?;
        connection.execute("CREATE TABLE notes (id INT, body TEXT)")?;
        connection.execute("INSERT INTO notes (id, body) VALUES (1, 'kept')")?;
        let metadata = connection
            .prepare_checked_statement("UPDATE notes SET body = ? WHERE TRUE")
            .unwrap();

        assert!(matches!(
            connection.execute_prepared_statement(
                metadata.statement_id,
                &[MySqlPreparedValue::Text("late".to_string())],
                Some(Duration::ZERO),
                MySqlAffectedRowsMode::Changed,
            ),
            Err(MySqlPreparedStatementError::Engine(LimboError::Interrupt))
        ));
        assert_eq!(
            connection
                .prepare_select("SELECT body FROM notes")?
                .run_collect_rows()?,
            vec![vec![Value::from_text("kept")]]
        );
        assert_eq!(
            connection
                .execute_prepared_statement(
                    metadata.statement_id,
                    &[MySqlPreparedValue::Text("updated".to_string())],
                    None,
                    MySqlAffectedRowsMode::Changed,
                )
                .unwrap(),
            MySqlPreparedExecutionResult::Write(MySqlWriteResult {
                affected_rows: 1,
                last_insert_id: 0,
            })
        );
        connection.close()?;
        Ok(())
    }

    #[test]
    fn prepared_auto_increment_insert_does_not_reserve_or_expose_a_prototype() -> Result<()> {
        let (connection, _allocator, _io) = open_allocator_connection(
            "mysql-session-prepared-auto-increment-rejected.db",
            [0x73; 16],
        )?;
        connection.execute(
            "CREATE TABLE users (id INT NOT NULL AUTO_INCREMENT PRIMARY KEY, name TEXT)",
        )?;

        let metadata = connection
            .prepare_checked_statement("INSERT INTO users (name) VALUES (?)")
            .unwrap();
        assert_eq!(metadata.parameter_count, 1);
        assert!(metadata.result_columns.is_empty());
        assert!(matches!(
            connection.with_prepared_statement(metadata.statement_id, |_| Ok(())),
            Err(MySqlPreparedStatementError::Prepare(
                MySqlQueryError::Unsupported(_)
            ))
        ));
        connection
            .reset_prepared_statement(metadata.statement_id)
            .unwrap();

        let mut reservation = _allocator.reserve(auto_increment_key(&connection, "users")?, 1)?;
        assert_eq!(_io.block(|| reservation.step())?.first(), 1);
        connection.close()?;
        Ok(())
    }

    #[test]
    fn prepared_auto_increment_insert_reuses_multirow_parameters_in_source_order() -> Result<()> {
        let (connection, _allocator, _io) = open_allocator_connection(
            "mysql-session-prepared-auto-increment-reuse.db",
            [0x74; 16],
        )?;
        connection.execute(
            "CREATE TABLE users (id INT NOT NULL AUTO_INCREMENT PRIMARY KEY, name TEXT, value INT)",
        )?;
        let metadata = connection
            .prepare_checked_statement("INSERT INTO users (name, value) VALUES (?, ?), (?, ?)")
            .unwrap();
        assert_eq!(metadata.parameter_count, 4);

        assert_eq!(
            connection
                .execute_prepared_statement(
                    metadata.statement_id,
                    &[
                        MySqlPreparedValue::Text("Ada".to_string()),
                        MySqlPreparedValue::Integer(10),
                        MySqlPreparedValue::Text("Grace".to_string()),
                        MySqlPreparedValue::Integer(20),
                    ],
                    None,
                    MySqlAffectedRowsMode::Changed,
                )
                .unwrap(),
            MySqlPreparedExecutionResult::Write(MySqlWriteResult {
                affected_rows: 2,
                last_insert_id: 1,
            })
        );
        assert_eq!(
            connection
                .execute_prepared_statement(
                    metadata.statement_id,
                    &[
                        MySqlPreparedValue::Text("Linus".to_string()),
                        MySqlPreparedValue::Integer(30),
                        MySqlPreparedValue::Text("Marie".to_string()),
                        MySqlPreparedValue::Integer(40),
                    ],
                    None,
                    MySqlAffectedRowsMode::Changed,
                )
                .unwrap(),
            MySqlPreparedExecutionResult::Write(MySqlWriteResult {
                affected_rows: 2,
                last_insert_id: 3,
            })
        );
        assert_eq!(connection.last_insert_id(), 3);
        assert_eq!(
            connection
                .prepare_select("SELECT id, name, value FROM users")?
                .run_collect_rows()?,
            vec![
                vec![
                    Value::from_i64(1),
                    Value::from_text("Ada"),
                    Value::from_i64(10),
                ],
                vec![
                    Value::from_i64(2),
                    Value::from_text("Grace"),
                    Value::from_i64(20),
                ],
                vec![
                    Value::from_i64(3),
                    Value::from_text("Linus"),
                    Value::from_i64(30),
                ],
                vec![
                    Value::from_i64(4),
                    Value::from_text("Marie"),
                    Value::from_i64(40),
                ],
            ]
        );
        connection.close()?;
        Ok(())
    }

    #[test]
    fn failed_prepared_auto_increment_insert_burns_its_range_without_changing_last_id() -> Result<()>
    {
        let (connection, _allocator, _io) = open_allocator_connection(
            "mysql-session-prepared-auto-increment-failure.db",
            [0x75; 16],
        )?;
        connection.execute(
            "CREATE TABLE users (id INT NOT NULL AUTO_INCREMENT PRIMARY KEY, name TEXT UNIQUE)",
        )?;
        let metadata = connection
            .prepare_checked_statement("INSERT INTO users (name) VALUES (?)")
            .unwrap();
        assert_eq!(
            connection
                .execute_prepared_statement(
                    metadata.statement_id,
                    &[MySqlPreparedValue::Text("Ada".to_string())],
                    None,
                    MySqlAffectedRowsMode::Changed,
                )
                .unwrap(),
            MySqlPreparedExecutionResult::Write(MySqlWriteResult {
                affected_rows: 1,
                last_insert_id: 1,
            })
        );
        assert!(matches!(
            connection.execute_prepared_statement(
                metadata.statement_id,
                &[MySqlPreparedValue::Text("Ada".to_string())],
                None,
                MySqlAffectedRowsMode::Changed,
            ),
            Err(MySqlPreparedStatementError::Engine(_))
        ));
        assert_eq!(connection.last_insert_id(), 1);
        assert_eq!(
            connection
                .execute_prepared_statement(
                    metadata.statement_id,
                    &[MySqlPreparedValue::Text("Grace".to_string())],
                    None,
                    MySqlAffectedRowsMode::Changed,
                )
                .unwrap(),
            MySqlPreparedExecutionResult::Write(MySqlWriteResult {
                affected_rows: 1,
                last_insert_id: 3,
            })
        );
        assert_eq!(
            connection
                .prepare_select("SELECT id, name FROM users")?
                .run_collect_rows()?,
            vec![
                vec![Value::from_i64(1), Value::from_text("Ada")],
                vec![Value::from_i64(3), Value::from_text("Grace")],
            ]
        );
        connection.close()?;
        Ok(())
    }

    #[test]
    fn rolled_back_prepared_auto_increment_insert_does_not_reuse_its_id() -> Result<()> {
        let (connection, _allocator, _io) = open_allocator_connection(
            "mysql-session-prepared-auto-increment-rollback.db",
            [0x78; 16],
        )?;
        connection.execute(
            "CREATE TABLE users (id INT NOT NULL AUTO_INCREMENT PRIMARY KEY, name TEXT)",
        )?;
        let metadata = connection
            .prepare_checked_statement("INSERT INTO users (name) VALUES (?)")
            .unwrap();

        connection.execute_transaction_command("BEGIN").unwrap();
        assert_eq!(
            connection
                .execute_prepared_statement(
                    metadata.statement_id,
                    &[MySqlPreparedValue::Text("discarded".to_string())],
                    None,
                    MySqlAffectedRowsMode::Changed,
                )
                .unwrap(),
            MySqlPreparedExecutionResult::Write(MySqlWriteResult {
                affected_rows: 1,
                last_insert_id: 1,
            })
        );
        connection.execute_transaction_command("ROLLBACK").unwrap();

        assert_eq!(
            connection
                .execute_prepared_statement(
                    metadata.statement_id,
                    &[MySqlPreparedValue::Text("kept".to_string())],
                    None,
                    MySqlAffectedRowsMode::Changed,
                )
                .unwrap(),
            MySqlPreparedExecutionResult::Write(MySqlWriteResult {
                affected_rows: 1,
                last_insert_id: 2,
            })
        );
        assert_eq!(
            connection
                .prepare_select("SELECT id, name FROM users")?
                .run_collect_rows()?,
            vec![vec![Value::from_i64(2), Value::from_text("kept")]]
        );
        connection.close()?;
        Ok(())
    }

    #[test]
    fn prepared_auto_increment_insert_zero_timeout_does_not_reserve() -> Result<()> {
        let (connection, allocator, io) = open_allocator_connection(
            "mysql-session-prepared-auto-increment-timeout.db",
            [0x76; 16],
        )?;
        connection.execute(
            "CREATE TABLE users (id INT NOT NULL AUTO_INCREMENT PRIMARY KEY, name TEXT)",
        )?;
        let metadata = connection
            .prepare_checked_statement("INSERT INTO users (name) VALUES (?)")
            .unwrap();
        assert!(matches!(
            connection.execute_prepared_statement(
                metadata.statement_id,
                &[MySqlPreparedValue::Text("late".to_string())],
                Some(Duration::ZERO),
                MySqlAffectedRowsMode::Changed,
            ),
            Err(MySqlPreparedStatementError::Engine(LimboError::Interrupt))
        ));
        let mut reservation = allocator.reserve(auto_increment_key(&connection, "users")?, 1)?;
        assert_eq!(io.block(|| reservation.step())?.first(), 1);
        connection.close()?;
        Ok(())
    }

    #[test]
    fn prepared_auto_increment_allocator_mutations_fail_closed() -> Result<()> {
        let (connection, _allocator, _io) = open_allocator_connection(
            "mysql-session-prepared-auto-increment-allocator-rejected.db",
            [0x77; 16],
        )?;
        connection.execute(
            "CREATE TABLE users (id INT NOT NULL AUTO_INCREMENT PRIMARY KEY, name TEXT)",
        )?;

        assert!(matches!(
            connection.prepare_checked_statement("INSERT INTO users (id, name) VALUES (?, ?)"),
            Err(MySqlPreparedStatementError::Prepare(MySqlQueryError::Unsupported(message)))
                if message.contains("explicitly names the AUTO_INCREMENT column")
        ));
        assert!(matches!(
            connection.prepare_checked_statement("UPDATE users SET id = ? WHERE TRUE"),
            Err(MySqlPreparedStatementError::Prepare(MySqlQueryError::Unsupported(message)))
                if message == "prepared AUTO_INCREMENT column updates are not supported"
        ));
        connection.close()?;
        Ok(())
    }

    #[test]
    fn prepared_statement_ids_are_monotonic_across_removal_and_clear() -> Result<()> {
        let (connection, _allocator, _io) =
            open_allocator_connection("mysql-session-prepared-statement-ids.db", [0x6a; 16])?;

        let first = connection.prepare_checked_statement("SELECT 1").unwrap();
        let second = connection.prepare_checked_statement("SELECT 2").unwrap();
        assert_eq!((first.statement_id, second.statement_id), (1, 2));

        assert!(connection.remove_prepared_statement(first.statement_id));
        assert!(!connection.remove_prepared_statement(first.statement_id));
        assert_eq!(
            connection.prepared_statement_metadata(first.statement_id),
            None
        );

        let third = connection.prepare_checked_statement("SELECT 3").unwrap();
        assert_eq!(third.statement_id, 3);
        connection.clear_prepared_statements();
        assert_eq!(
            connection.prepared_statement_metadata(second.statement_id),
            None
        );
        assert_eq!(
            connection.prepared_statement_metadata(third.statement_id),
            None
        );

        let fourth = connection.prepare_checked_statement("SELECT 4").unwrap();
        assert_eq!(fourth.statement_id, 4);
        connection.close()?;
        Ok(())
    }

    #[test]
    fn prepared_statement_authority_enforces_limits_and_returns_failed_reservations(
    ) -> std::result::Result<(), Box<dyn Error>> {
        let io: Arc<dyn IO> = Arc::new(MemoryIO::new());
        let database = open_database(
            Arc::clone(&io),
            "mysql-session-prepared-quota.db",
            OpenFlags::Create,
        )?;
        let authority = MySqlPreparedStatementAuthority::new(1).unwrap();
        let first = MySqlConnection::new_with_prepared_statement_authority(
            database.connect()?,
            binary_context(),
            authority.clone(),
        )?;
        let second = MySqlConnection::new_with_prepared_statement_authority(
            database.connect()?,
            binary_context(),
            authority.clone(),
        )?;

        assert_eq!(authority.active_count(), 0);
        assert!(matches!(
            first.prepare_checked_statement("not a prepared statement"),
            Err(MySqlPreparedStatementError::Prepare(_))
        ));
        assert_eq!(authority.active_count(), 0);

        let prepared = first.prepare_checked_statement("SELECT 1")?;
        assert_eq!(authority.active_count(), 1);
        assert!(matches!(
            second.prepare_checked_statement("SELECT 2"),
            Err(MySqlPreparedStatementError::PreparedStatementLimitReached { maximum: 1 })
        ));
        assert_eq!(authority.active_count(), 1);
        assert!(first.remove_prepared_statement(prepared.statement_id));
        assert_eq!(authority.active_count(), 0);
        second.prepare_checked_statement("SELECT 2")?;
        assert_eq!(authority.active_count(), 1);
        second.clear_prepared_statements();
        assert_eq!(authority.active_count(), 0);
        let closed = MySqlConnection::new_with_prepared_statement_authority(
            database.connect()?,
            binary_context(),
            authority.clone(),
        )?;
        let closed_statement = closed.prepare_checked_statement("SELECT 3")?;
        let closed_clone = closed.clone();
        assert_eq!(authority.active_count(), 1);
        closed.close()?;
        assert_eq!(authority.active_count(), 0);
        assert!(closed
            .prepared_statement_metadata(closed_statement.statement_id)
            .is_none());
        drop(closed_clone);
        let dropped = MySqlConnection::new_with_prepared_statement_authority(
            database.connect()?,
            binary_context(),
            authority.clone(),
        )?;
        dropped.prepare_checked_statement("SELECT 4")?;
        assert_eq!(authority.active_count(), 1);
        drop(dropped);
        assert_eq!(authority.active_count(), 0);
        first.close()?;
        second.close()?;
        Ok(())
    }

    #[test]
    fn prepared_statement_authority_supports_zero_and_dynamic_lowering(
    ) -> std::result::Result<(), Box<dyn Error>> {
        assert_eq!(
            MySqlPreparedStatementAuthority::default().maximum(),
            DEFAULT_MAX_PREPARED_STMT_COUNT
        );
        assert!(matches!(
            MySqlPreparedStatementAuthority::new(MAX_PREPARED_STMT_COUNT + 1),
            Err(MySqlPreparedStatementAuthorityError::MaximumOutOfRange { maximum })
                if maximum == MAX_PREPARED_STMT_COUNT + 1
        ));
        let io: Arc<dyn IO> = Arc::new(MemoryIO::new());
        let database = open_database(
            Arc::clone(&io),
            "mysql-session-prepared-quota-lowering.db",
            OpenFlags::Create,
        )?;
        let zero = MySqlPreparedStatementAuthority::new(0).unwrap();
        let disabled = MySqlConnection::new_with_prepared_statement_authority(
            database.connect()?,
            binary_context(),
            zero.clone(),
        )?;
        assert!(matches!(
            disabled.prepare_checked_statement("SELECT 1"),
            Err(MySqlPreparedStatementError::PreparedStatementLimitReached { maximum: 0 })
        ));
        assert_eq!(zero.active_count(), 0);
        disabled.close()?;

        let authority = MySqlPreparedStatementAuthority::new(2).unwrap();
        let connection = MySqlConnection::new_with_prepared_statement_authority(
            database.connect()?,
            binary_context(),
            authority.clone(),
        )?;
        let first = connection.prepare_checked_statement("SELECT 1")?;
        let second = connection.prepare_checked_statement("SELECT 2")?;
        authority.set_maximum(1).unwrap();
        assert!(matches!(
            connection.prepare_checked_statement("SELECT 3"),
            Err(MySqlPreparedStatementError::PreparedStatementLimitReached { maximum: 1 })
        ));
        connection.remove_prepared_statement(first.statement_id);
        assert_eq!(authority.active_count(), 1);
        assert!(matches!(
            connection.prepare_checked_statement("SELECT 3"),
            Err(MySqlPreparedStatementError::PreparedStatementLimitReached { maximum: 1 })
        ));
        connection.remove_prepared_statement(second.statement_id);
        assert_eq!(authority.active_count(), 0);
        connection.prepare_checked_statement("SELECT 3")?;
        connection.close()?;
        Ok(())
    }

    #[test]
    fn prepared_statement_authority_returns_permits_for_prepare_failures_and_id_exhaustion(
    ) -> std::result::Result<(), Box<dyn Error>> {
        let io: Arc<dyn IO> = Arc::new(MemoryIO::new());
        let database = open_database(
            Arc::clone(&io),
            "mysql-session-prepared-quota-failure.db",
            OpenFlags::Create,
        )?;
        let authority = MySqlPreparedStatementAuthority::new(2).unwrap();
        let connection = MySqlConnection::new_with_prepared_statement_authority(
            database.connect()?,
            binary_context(),
            authority.clone(),
        )?;

        assert!(matches!(
            connection.prepare_checked_statement("SELECT * FROM missing_table"),
            Err(MySqlPreparedStatementError::Prepare(
                MySqlQueryError::Engine(_)
            ))
        ));
        assert_eq!(authority.active_count(), 0);

        let first_success = connection.prepare_checked_statement("SELECT 1")?;
        assert_eq!(first_success.statement_id, 1);
        connection.remove_prepared_statement(first_success.statement_id);
        assert_eq!(authority.active_count(), 0);

        {
            let mut registry = connection
                .prepared_statements
                .lock()
                .expect("MySQL prepared statement registry mutex poisoned");
            registry.next_id = Some(u32::MAX);
        }
        let prepared = connection.prepare_checked_statement("SELECT 1")?;
        assert_eq!(prepared.statement_id, u32::MAX);
        assert_eq!(authority.active_count(), 1);
        assert!(matches!(
            connection.prepare_checked_statement("SELECT 2"),
            Err(MySqlPreparedStatementError::StatementIdExhausted)
        ));
        assert_eq!(authority.active_count(), 1);
        assert!(connection.remove_prepared_statement(u32::MAX));
        assert_eq!(authority.active_count(), 0);
        assert!(matches!(
            connection.prepare_checked_statement("SELECT 3"),
            Err(MySqlPreparedStatementError::StatementIdExhausted)
        ));
        connection.close()?;
        Ok(())
    }

    #[test]
    fn prepared_statement_authority_never_exceeds_its_limit_during_concurrent_reserve() {
        use std::sync::{Arc, Barrier};

        let authority = MySqlPreparedStatementAuthority::new(2).unwrap();
        let barrier = Arc::new(Barrier::new(8));
        let workers = (0..8)
            .map(|_| {
                let authority = authority.clone();
                let barrier = Arc::clone(&barrier);
                std::thread::spawn(move || {
                    barrier.wait();
                    authority.reserve().ok()
                })
            })
            .collect::<Vec<_>>();
        let permits = workers
            .into_iter()
            .map(|worker| worker.join().expect("quota worker panicked"))
            .collect::<Vec<_>>();
        assert_eq!(permits.iter().filter(|permit| permit.is_some()).count(), 2);
        assert_eq!(authority.active_count(), 2);
        drop(permits);
        assert_eq!(authority.active_count(), 0);
    }

    #[test]
    fn concurrent_connection_prepares_never_exceed_the_shared_limit(
    ) -> std::result::Result<(), Box<dyn Error>> {
        use std::sync::{Arc, Barrier};

        let io: Arc<dyn IO> = Arc::new(MemoryIO::new());
        let database = open_database(
            Arc::clone(&io),
            "mysql-session-prepared-quota-concurrent.db",
            OpenFlags::Create,
        )?;
        let authority = MySqlPreparedStatementAuthority::new(1).unwrap();
        let first = MySqlConnection::new_with_prepared_statement_authority(
            database.connect()?,
            binary_context(),
            authority.clone(),
        )?;
        let second = MySqlConnection::new_with_prepared_statement_authority(
            database.connect()?,
            binary_context(),
            authority.clone(),
        )?;
        let barrier = Arc::new(Barrier::new(2));
        let workers = [first, second]
            .into_iter()
            .map(|connection| {
                let barrier = Arc::clone(&barrier);
                std::thread::spawn(move || {
                    barrier.wait();
                    let result = connection.prepare_checked_statement("SELECT 1");
                    (connection, result)
                })
            })
            .collect::<Vec<_>>();
        let results = workers
            .into_iter()
            .map(|worker| worker.join().expect("prepare worker panicked"))
            .collect::<Vec<_>>();
        assert_eq!(
            results.iter().filter(|(_, result)| result.is_ok()).count(),
            1
        );
        assert_eq!(authority.active_count(), 1);
        drop(results);
        assert_eq!(authority.active_count(), 0);
        Ok(())
    }

    #[test]
    fn cleared_or_reset_in_flight_prepares_cannot_resurrect_a_statement(
    ) -> std::result::Result<(), Box<dyn Error>> {
        let io: Arc<dyn IO> = Arc::new(MemoryIO::new());
        let database = open_database(
            Arc::clone(&io),
            "mysql-session-prepared-quota-generation.db",
            OpenFlags::Create,
        )?;
        let authority = MySqlPreparedStatementAuthority::new(2).unwrap();
        let connection = MySqlConnection::new_with_prepared_statement_authority(
            database.connect()?,
            binary_context(),
            authority.clone(),
        )?;

        let reservation = connection.reserve_prepared_statement().unwrap();
        let statement_id = reservation.statement_id;
        connection.clear_prepared_statements();
        assert!(matches!(
            connection.commit_prepared_statement(
                reservation,
                None,
                MySqlPreparedStatementMetadata {
                    statement_id,
                    parameter_count: 0,
                    result_columns: Vec::new(),
                },
                Vec::new(),
                Vec::new(),
                PreparedExecutionPlan::Select { reads_table: false },
            ),
            Err(MySqlPreparedStatementError::Prepare(
                MySqlQueryError::Unsupported(message)
            )) if message == "prepared statement was cleared during prepare"
        ));
        assert_eq!(authority.active_count(), 0);
        assert!(connection
            .prepared_statement_metadata(statement_id)
            .is_none());

        let reservation = connection.reserve_prepared_statement().unwrap();
        let statement_id = reservation.statement_id;
        connection.reset_connection().unwrap();
        assert!(matches!(
            connection.commit_prepared_statement(
                reservation,
                None,
                MySqlPreparedStatementMetadata {
                    statement_id,
                    parameter_count: 0,
                    result_columns: Vec::new(),
                },
                Vec::new(),
                Vec::new(),
                PreparedExecutionPlan::Select { reads_table: false },
            ),
            Err(MySqlPreparedStatementError::Prepare(
                MySqlQueryError::Unsupported(message)
            )) if message == "prepared statement was cleared during prepare"
        ));
        assert_eq!(authority.active_count(), 0);
        connection.close()?;
        Ok(())
    }

    #[test]
    fn prepared_statement_reset_clears_bindings_and_schema_sql_is_unsupported() -> Result<()> {
        let (connection, _allocator, _io) =
            open_allocator_connection("mysql-session-prepared-statement-reset.db", [0x6b; 16])?;
        let metadata = connection.prepare_checked_statement("SELECT ?").unwrap();
        connection
            .with_prepared_statement(metadata.statement_id, |statement| {
                statement.bind_at(std::num::NonZero::new(1).unwrap(), Value::from_i64(7))?;
                Ok(())
            })
            .unwrap();
        connection
            .reset_prepared_statement(metadata.statement_id)
            .unwrap();
        connection
            .with_prepared_statement(metadata.statement_id, |statement| {
                assert_eq!(statement.expanded_sql(), "SELECT NULL");
                Ok(())
            })
            .unwrap();
        assert!(matches!(
            connection.prepare_checked_statement("CREATE TABLE users (id INT)"),
            Err(MySqlPreparedStatementError::Prepare(MySqlQueryError::Unsupported(message)))
                if message == "prepared statements support only SELECT, INSERT, UPDATE, and DELETE"
        ));
        assert!(matches!(
            connection.reset_prepared_statement(0),
            Err(MySqlPreparedStatementError::UnknownStatement { statement_id: 0 })
        ));
        connection.close()?;
        Ok(())
    }

    #[test]
    fn strict_smallint_assignments_use_durable_mysql_ddl() -> Result<()> {
        use std::num::NonZeroUsize;

        let io: Arc<dyn IO> = Arc::new(MemoryIO::new());
        let path = "mysql-session-strict-smallint.db";
        {
            let db = open_database(io.clone(), path, OpenFlags::Create)?;
            let connection = MySqlConnection::new(db.connect()?, binary_context())?;
            connection.execute("CREATE TABLE numbers (value SMALLINT, label TEXT)")?;
            connection.execute(
                "INSERT INTO numbers (value, label) VALUES (-32768, 'low'), (32767, 'high')",
            )?;

            let mut statement =
                connection.prepare("INSERT INTO numbers (value, label) VALUES (?, 'bound')")?;
            statement.bind_at(NonZeroUsize::new(1).unwrap(), Value::from_i64(-1))?;
            statement.run_ignore_rows()?;

            let mut statement = connection
                .prepare("INSERT INTO numbers (value, label) VALUES (?, 'prepared-overflow')")?;
            statement.bind_at(NonZeroUsize::new(1).unwrap(), Value::from_i64(32768))?;
            let error = statement.run_ignore_rows().unwrap_err();
            assert!(matches!(
                error,
                LimboError::Assignment(error)
                    if matches!(error.as_ref(), AssignmentError::OutOfRange { type_name, .. } if type_name == "SMALLINT")
            ));

            let error = connection
                .execute("INSERT INTO numbers (value, label) VALUES (-32769, 'underflow')")
                .unwrap_err();
            assert!(matches!(
                error,
                LimboError::Assignment(error)
                    if matches!(error.as_ref(), AssignmentError::OutOfRange { type_name, .. } if type_name == "SMALLINT")
            ));

            let error = connection
                .execute(
                    "INSERT INTO numbers (value, label) VALUES (0, 'kept'), (32768, 'rollback')",
                )
                .unwrap_err();
            assert!(matches!(
                error,
                LimboError::Assignment(error)
                    if matches!(error.as_ref(), AssignmentError::OutOfRange { type_name, .. } if type_name == "SMALLINT")
            ));
            assert_eq!(
                connection
                    .inner()
                    .prepare("SELECT value, label FROM numbers ORDER BY rowid")?
                    .run_collect_rows()?,
                vec![
                    vec![Value::from_i64(-32768), Value::from_text("low")],
                    vec![Value::from_i64(32767), Value::from_text("high")],
                    vec![Value::from_i64(-1), Value::from_text("bound")],
                ]
            );

            let error = connection
                .inner()
                .execute("INSERT INTO numbers (value, label) VALUES (32768, 'raw')")
                .unwrap_err();
            assert!(matches!(
                error,
                LimboError::Assignment(error)
                    if matches!(error.as_ref(), AssignmentError::OutOfRange { type_name, .. } if type_name == "SMALLINT")
            ));

            connection.execute("CREATE TABLE source (value INT)")?;
            connection.execute(
                "CREATE TRIGGER copy_source AFTER INSERT ON source FOR EACH ROW BEGIN INSERT INTO numbers (value, label) VALUES (NEW.value, 'trigger'); END",
            )?;
            let error = connection
                .execute("INSERT INTO source (value) VALUES (32768)")
                .unwrap_err();
            assert!(matches!(
                error,
                LimboError::Assignment(error)
                    if matches!(error.as_ref(), AssignmentError::OutOfRange { type_name, .. } if type_name == "SMALLINT")
            ));
            assert_eq!(
                connection
                    .inner()
                    .prepare("SELECT COUNT(*) FROM source")?
                    .run_collect_rows()?,
                vec![vec![Value::from_i64(0)]]
            );

            connection.execute("CREATE TEMPORARY TABLE temp_numbers (value SMALLINT)")?;
            let error = connection
                .execute("INSERT INTO temp_numbers (value) VALUES (32768)")
                .unwrap_err();
            assert!(matches!(
                error,
                LimboError::Assignment(error)
                    if matches!(error.as_ref(), AssignmentError::OutOfRange { type_name, .. } if type_name == "SMALLINT")
            ));
            connection.execute("INSERT INTO temp_numbers (value) VALUES (0)")?;
            let error = connection
                .execute("UPDATE temp_numbers SET value = 32768 WHERE TRUE")
                .unwrap_err();
            assert!(matches!(
                error,
                LimboError::Assignment(error)
                    if matches!(error.as_ref(), AssignmentError::OutOfRange { type_name, .. } if type_name == "SMALLINT")
            ));
            connection.inner().close()?;
        }

        {
            let db = open_database(io.clone(), path, OpenFlags::None)?;
            let connection = MySqlConnection::new(db.connect()?, binary_context())?;
            connection.inner().execute("VACUUM")?;
            let error = connection
                .execute("UPDATE numbers SET value = 32768 WHERE TRUE")
                .unwrap_err();
            assert!(matches!(
                error,
                LimboError::Assignment(error)
                    if matches!(error.as_ref(), AssignmentError::OutOfRange { type_name, .. } if type_name == "SMALLINT")
            ));
            connection.inner().close()?;
        }

        let db = open_database(io, path, OpenFlags::None)?;
        let connection = MySqlConnection::new(db.connect()?, binary_context())?;
        assert_eq!(
            connection
                .inner()
                .prepare("SELECT value FROM numbers WHERE label = 'low'")?
                .run_collect_rows()?,
            vec![vec![Value::from_i64(-32768)]]
        );
        connection.inner().close()?;
        Ok(())
    }

    #[test]
    fn strict_bigint_assignments_use_durable_mysql_ddl() -> Result<()> {
        use std::num::NonZeroUsize;

        let io: Arc<dyn IO> = Arc::new(MemoryIO::new());
        let path = "mysql-session-strict-bigint.db";
        {
            let db = open_database(io.clone(), path, OpenFlags::Create)?;
            let connection = MySqlConnection::new(db.connect()?, binary_context())?;
            connection.execute("CREATE TABLE `numbers` (`value` BIGINT, `label` TEXT)")?;
            let stored = connection
                .inner()
                .prepare("SELECT sql FROM sqlite_schema WHERE name = 'numbers'")?
                .run_collect_rows()?[0][0]
                .to_string();
            assert!(stored.contains("`value` BIGINT"));

            connection.execute(
                "INSERT INTO `numbers` (`value`, `label`) VALUES (-9223372036854775808, 'low'), (9223372036854775807, 'high')",
            )?;

            let mut parameterized = connection
                .prepare("INSERT INTO `numbers` (`value`, `label`) VALUES (?, 'prepared-low')")?;
            parameterized.bind_at(NonZeroUsize::new(1).unwrap(), Value::from_i64(i64::MIN))?;
            parameterized.run_ignore_rows()?;

            let mut parameterized = connection
                .prepare("INSERT INTO `numbers` (`value`, `label`) VALUES (?, 'prepared-high')")?;
            parameterized.bind_at(NonZeroUsize::new(1).unwrap(), Value::from_i64(i64::MAX))?;
            parameterized.run_ignore_rows()?;

            let error = connection
                .execute(
                    "INSERT INTO `numbers` (`value`, `label`) VALUES (0, 'kept'), ('bad', 'rollback')",
                )
                .unwrap_err();
            assert!(matches!(
                error,
                LimboError::Assignment(error)
                    if matches!(error.as_ref(), AssignmentError::IncorrectType { type_name, .. } if type_name == "BIGINT")
            ));
            assert_eq!(
                connection
                    .inner()
                    .prepare("SELECT label FROM numbers ORDER BY rowid")?
                    .run_collect_rows()?,
                vec![
                    vec![Value::build_text("low")],
                    vec![Value::build_text("high")],
                    vec![Value::build_text("prepared-low")],
                    vec![Value::build_text("prepared-high")],
                ]
            );
            connection.inner().close()?;
        }

        {
            let db = open_database(io.clone(), path, OpenFlags::None)?;
            let connection = MySqlConnection::new(db.connect()?, binary_context())?;
            connection.inner().execute("VACUUM")?;
            let columns = connection
                .list_columns(&MySqlTableName::parse("numbers").unwrap())
                .map_err(|error| LimboError::InternalError(error.to_string()))?;
            assert_eq!(columns[0].type_name(), "BIGINT");

            let error = connection
                .execute("UPDATE `numbers` SET `value` = 'bad' WHERE TRUE")
                .unwrap_err();
            assert!(matches!(
                error,
                LimboError::Assignment(error)
                    if matches!(error.as_ref(), AssignmentError::IncorrectType { type_name, .. } if type_name == "BIGINT")
            ));
            connection.inner().close()?;
        }

        let db = open_database(io, path, OpenFlags::None)?;
        let connection = MySqlConnection::new(db.connect()?, binary_context())?;
        assert_eq!(
            connection
                .inner()
                .prepare("SELECT value FROM numbers WHERE label = 'low'")?
                .run_collect_rows()?,
            vec![vec![Value::from_i64(i64::MIN)]]
        );
        assert_eq!(
            connection
                .inner()
                .prepare("SELECT value FROM numbers WHERE label = 'high'")?
                .run_collect_rows()?,
            vec![vec![Value::from_i64(i64::MAX)]]
        );
        connection.inner().close()?;
        Ok(())
    }
}
