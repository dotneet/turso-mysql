mod catalog;
mod table_rewrite;
mod transaction_isolation;
mod trigger_body;

use std::{
    collections::{HashMap, HashSet},
    error::Error,
    fmt,
    sync::{Arc, Mutex},
    time::Duration,
};

use turso_core::{
    storage::auto_increment::{AutoIncrementKey, DurableRangeAllocator, InsertAutoIncrementValue},
    AssignmentOperation, AssignmentValidator, Connection, DatabaseFileOwner, IOExt as _,
    LimboError, MetadataLockMode, Numeric, PrepareOptions, ReprepareContext, ReprepareParser,
    Result, SchemaSqlFormatter, SchemaSqlKind, Statement, StatementStatusCounter,
    TriggerRowidSupplier, Value, IO,
};
use turso_mysql_parser::{
    parse_auto_increment_create_table, parse_auto_increment_insert,
    parse_auto_increment_insert_target, parse_autocommit_setting,
    parse_checked_primary_key_create_table, parse_create_table_ast, parse_create_view_ast,
    parse_dml, parse_insert_values_written_into, parse_optional_autocommit_setting,
    parse_prepared_auto_increment_insert, parse_schema_ddl_ast, parse_select,
    parse_transaction_command, render_create_index_mysql_with_mode,
    render_create_table_mysql_with_mode, render_create_view_mysql_with_mode, AutoIncrementRowValue,
    BoundAutoIncrementInsert, CheckedAutoIncrementCreateTable, CheckedAutoIncrementInsert,
    CheckedComparisonAnswer, CheckedComparisonNow, CheckedComparisonOperand, CheckedInsertValue,
    CheckedPrimaryKeyCreateTable, CheckedSelectComparison, CheckedSelectComparisonOperator,
    CheckedSelectComparisonRhs, CheckedSubqueryComparison, CheckedUpdateAssignmentValue,
    ColumnLiteral, MySqlAlterTableIndexOperation, MySqlAlterTableIndexes, MySqlCreateTableAsSelect,
    MySqlCreateTableAsSelectSource, MySqlCreateTableWithKeys, MySqlDropTableCommand,
    MySqlDropViewCommand, MySqlLockedTable, MySqlLockingRead, MySqlRowLockWait, MySqlSelectSource,
    MySqlTableName, MySqlTransactionCommand, MySqlTruncateTableCommand, MySqlViewReplacement,
    OfferedValue, ParseError as MySqlParseError, SessionSqlMode, StaticSelectMetadata,
    StaticSelectProjectionMetadata, TranslatedDml, WrittenZero,
};
use turso_parser::ast::{
    AlterTableBody, Cmd, ColumnConstraint, CreateTableBody, Expr, InsertBody, Literal, OneSelect,
    ResultColumn, SelectTable, SortedColumn, Stmt, UnaryOperator,
};

use crate::alter_table_indexes::MySqlAlterTableIndexError;
use crate::create_table_as_select::MySqlCreateTableAsSelectError;
use crate::database_users::{DatabaseUser, MySqlStatementNotStarted, DEFAULT_METADATA_LOCK_WAIT};
use crate::drop_table::{MySqlDropTableError, MySqlDropTableResult};
use crate::schema_sql::{
    decode_schema_sql, decode_schema_sql_any, encode_schema_sql_v3, CreatorSchemaSqlFormatter,
    SchemaSqlCreator, SchemaSqlSessionContext, SchemaSqlV2Metadata,
};
use crate::truncate_table::MySqlTruncateTableError;
use crate::wal_keeper::WalKeeperHandle;
pub(crate) use catalog::trigger_metadata;
use transaction_isolation::TransactionIsolation;
pub use transaction_isolation::{MySqlIsolationLevel, MySqlTransactionOutcome};

/// MySQL statement entry for one connection and immutable schema parsing context.
#[derive(Clone)]
pub struct MySqlConnection {
    inner: Arc<Connection>,
    schema_context: SchemaSqlSessionContext,
    auto_increment: Option<AutoIncrementExecutionCapability>,
    session_autocommit: Arc<Mutex<bool>>,
    session_time_zone_offset: Arc<Mutex<i32>>,
    explained_error: Arc<Mutex<Option<String>>>,
    /// Set while a `START TRANSACTION READ ONLY` is open. MySQL answers 1792 to
    /// a write inside one, so this frontend has to know it is in one to answer
    /// the same rather than accept a transaction whose promise it does not keep.
    read_only_transaction: Arc<Mutex<bool>>,
    /// Whether the session asked for read-only transactions with `SET SESSION
    /// TRANSACTION READ ONLY`. A transaction begun without saying which it is
    /// takes this, and so does a statement outside any transaction.
    session_read_only: Arc<Mutex<bool>>,
    /// Set while this session holds the lock `LOCK TABLES` took, which is
    /// the write transaction it opened.
    tables_locked: Arc<Mutex<bool>>,
    transaction_isolation: Arc<Mutex<TransactionIsolation>>,
    /// What a 0 written into a counted column means under the session's
    /// `sql_mode`.
    written_zero: Arc<Mutex<WrittenZero>>,
    prepared_statements: Arc<Mutex<PreparedStatementRegistry>>,
    prepared_statement_authority: MySqlPreparedStatementAuthority,
    schema_readings: Arc<Mutex<SchemaReadings>>,
    prepared_counted_rows_statements: Arc<Mutex<HashMap<&'static str, turso_core::Statement>>>,
    prepared_transaction_statements: Arc<Mutex<Vec<(Stmt, String, turso_core::Statement)>>>,
    /// The catalog's WAL keeper and this connection's database, when the
    /// connection belongs to a catalog.
    wal_keeper: Option<(WalKeeperHandle, std::sync::Weak<turso_core::Database>)>,
    /// The collation this connection's database gives a new table that names
    /// none, shared with every other connection to it so that an `ALTER
    /// DATABASE` one of them runs reaches the next `CREATE TABLE` of all.
    database_collation: Option<SharedDatabaseCollation>,
    /// This connection's use of its database, when the connection belongs to
    /// a catalog, which a `DROP DATABASE` waits for and then refuses.
    database_user: Option<Arc<DatabaseUser>>,
    /// MySQL's `lock_wait_timeout`: how long a statement waits for a table or
    /// a database another session is using.
    metadata_lock_wait: Arc<Mutex<Duration>>,
    /// The transaction command the statement running now ran, if any, which
    /// decides whether its transaction keeps the database from a drop.
    transaction_command_ran: Arc<Mutex<TransactionCommandRan>>,
    /// How many counted inserts were written with numbers another session
    /// took first and had to be written again.
    #[cfg(test)]
    pub(crate) counted_rows_written_again: Arc<std::sync::atomic::AtomicUsize>,
    /// Whether this is an empty database standing in for one another session
    /// dropped, which the statements reading no table run on.
    stands_in_for_a_dropped_database: bool,
    /// Closes the engine connection once the last clone lets go. Declared
    /// last so that everything else a clone shares is gone first.
    _closes_on_last_drop: Arc<CloseOnLastDrop>,
}

/// What [`MySqlConnection::list_tables`] and [`MySqlConnection::list_columns`]
/// read, for the one schema it was read from.
///
/// A schema is never changed in place while this holds it, so any change
/// makes a new one and what is kept here stops being found.
#[derive(Default)]
struct SchemaReadings {
    schema: Option<Arc<turso_core::schema::Schema>>,
    tables: Option<Arc<Vec<MySqlTable>>>,
    columns: HashMap<String, Arc<Vec<MySqlColumnMetadata>>>,
    counted_tables_by_lowercase_name: HashMap<String, Option<AutoIncrementTable>>,
    /// How many times a table's columns were read rather than found here.
    #[cfg(test)]
    column_reads: usize,
    #[cfg(test)]
    counted_table_catalog_reads: usize,
}

impl SchemaReadings {
    fn tables(&self, schema: &Arc<turso_core::schema::Schema>) -> Option<Arc<Vec<MySqlTable>>> {
        self.reads(schema).and_then(|kept| kept.tables.clone())
    }

    fn columns(
        &self,
        schema: &Arc<turso_core::schema::Schema>,
        table: &str,
    ) -> Option<Arc<Vec<MySqlColumnMetadata>>> {
        self.reads(schema)
            .and_then(|kept| kept.columns.get(table).cloned())
    }

    fn keep_tables(
        &mut self,
        schema: Arc<turso_core::schema::Schema>,
        tables: &Arc<Vec<MySqlTable>>,
    ) {
        self.for_schema(schema).tables = Some(Arc::clone(tables));
    }

    fn keep_columns(
        &mut self,
        schema: Arc<turso_core::schema::Schema>,
        table: &str,
        columns: &Arc<Vec<MySqlColumnMetadata>>,
    ) {
        let kept = self.for_schema(schema);
        kept.columns.insert(table.to_owned(), Arc::clone(columns));
        #[cfg(test)]
        {
            kept.column_reads += 1;
        }
    }

    fn counted_table(
        &self,
        schema: &Arc<turso_core::schema::Schema>,
        name: &str,
    ) -> Option<Option<AutoIncrementTable>> {
        self.reads(schema).and_then(|kept| {
            kept.counted_tables_by_lowercase_name
                .get(&name.to_ascii_lowercase())
                .cloned()
        })
    }

    fn keep_counted_table(
        &mut self,
        schema: Arc<turso_core::schema::Schema>,
        name: &str,
        table: &Option<AutoIncrementTable>,
    ) {
        let kept = self.for_schema(schema);
        kept.counted_tables_by_lowercase_name
            .insert(name.to_ascii_lowercase(), table.clone());
        #[cfg(test)]
        {
            kept.counted_table_catalog_reads += 1;
        }
    }

    fn reads(&self, schema: &Arc<turso_core::schema::Schema>) -> Option<&Self> {
        self.schema
            .as_ref()
            .is_some_and(|kept| Arc::ptr_eq(kept, schema))
            .then_some(self)
    }

    fn for_schema(&mut self, schema: Arc<turso_core::schema::Schema>) -> &mut Self {
        if self.reads(&schema).is_none() {
            self.tables = None;
            self.columns.clear();
            self.counted_tables_by_lowercase_name.clear();
            self.schema = Some(schema);
        }
        self
    }
}

/// Closes an engine connection when dropped.
///
/// Only a closed connection runs the engine's closing checkpoint, which
/// empties the WAL. A connection that is merely dropped leaves the WAL as it
/// was, however large a write made it, and the next open reads every frame of
/// it again: measured, 6.7 GB after one UPDATE of a million rows, and over a
/// second added to the next start.
struct CloseOnLastDrop {
    connection: Arc<Connection>,
    /// The connection's use of its database, when it belongs to a catalog
    /// that may drop the database while the connection still holds it.
    database_user: std::sync::OnceLock<Arc<DatabaseUser>>,
}

/// A transaction command a statement ran, which [`MySqlConnection::finish_a_statement`]
/// reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum TransactionCommandRan {
    #[default]
    None,
    /// `BEGIN`, `START TRANSACTION`, `COMMIT` or `ROLLBACK`, and their
    /// chaining forms: whatever transaction is open after it has read nothing
    /// yet, even when `WITH CONSISTENT SNAPSHOT` took its snapshot.
    BeganATransaction,
    /// `SAVEPOINT`, `ROLLBACK TO` or `RELEASE`, which read nothing.
    Savepoint,
}

/// Gives a connection back the lock wait it had once a statement that waited
/// longer is done, however it ends.
struct RestoresLockWait<'a> {
    connection: &'a Connection,
    wait: Duration,
}

impl Drop for RestoresLockWait<'_> {
    fn drop(&mut self) {
        self.connection.set_busy_timeout(self.wait);
    }
}

impl Drop for CloseOnLastDrop {
    fn drop(&mut self) {
        // Closing runs the engine's closing checkpoint, which writes the
        // database file. A dropped database's files are written by nobody,
        // so a connection to one, or to one a drop is under way on, is let go
        // without closing; the checkpoint is not needed, the next open reading
        // the WAL either way. A closing counts as using the database, so that
        // a drop never runs beside it.
        let Some(user) = self.database_user.get() else {
            let _ = self.connection.close();
            return;
        };
        if user.start_using(Duration::ZERO).is_err() {
            return;
        }
        // A drop has nowhere to report a failure to. A closing checkpoint that
        // fails leaves the WAL for the next open, which recovers it.
        let _ = self.connection.close();
        user.stop_using();
        user.note_a_transaction_ended();
    }
}

/// A database's collation, held once by the catalog and shared with every
/// connection to the database.
#[derive(Clone, Debug, Default)]
pub(crate) struct SharedDatabaseCollation(Arc<Mutex<turso_mysql_parser::MySqlTableCollation>>);

impl SharedDatabaseCollation {
    pub(crate) fn new(collation: turso_mysql_parser::MySqlTableCollation) -> Self {
        Self(Arc::new(Mutex::new(collation)))
    }

    pub(crate) fn get(&self) -> turso_mysql_parser::MySqlTableCollation {
        *self
            .0
            .lock()
            .expect("MySQL database collation mutex poisoned")
    }

    pub(crate) fn set(&self, collation: turso_mysql_parser::MySqlTableCollation) {
        *self
            .0
            .lock()
            .expect("MySQL database collation mutex poisoned") = collation;
    }
}

struct StoredIndexStatement {
    sql: String,
    /// The name the engine holds the index under.
    stored_name: String,
    implicit: bool,
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
    /// A `ROLLBACK TO` or `RELEASE` named a savepoint that is not there.
    NoSuchSavepoint,
    /// An omitted required column has no default in a checked empty INSERT.
    MissingRequiredDefault(String),
    /// A `CHANGE COLUMN` renamed a column onto a name the table already has.
    DuplicateColumn(String),
    /// An index with this name already exists on the same table.
    DuplicateIndex,
    /// An index named for removal does not exist on this table.
    MissingIndex,
    /// The table an index operation names does not exist.
    MissingTable,
    /// Dropping this index would leave a foreign key without a child index.
    RequiredByForeignKey,
    /// A JSON value cannot be indexed directly.
    JsonIndex,
    /// A JSON column cannot have a literal default.
    JsonLiteralDefault,
    /// The MySQL parser or checked translator rejected the query text.
    Syntax(String),
    /// Valid MySQL syntax lies outside the implemented compatibility surface.
    Unsupported(String),
    /// An `ALTER TABLE` named a `CHECK` its table has not got.
    NoSuchCheck(String),
    /// A `CHECK` was given a name another constraint already has.
    DuplicateCheckName(String),
    ForeignKeyDefinition(MySqlForeignKeyDefinitionError),
    /// An `ALTER TABLE` changing a table's key asked for one MySQL refuses.
    KeyChange(MySqlKeyChangeError),
    /// The checked Turso AST reached core, which then failed to prepare it.
    Engine(LimboError),
}

/// Why MySQL refuses an `ALTER TABLE` that changes a table's primary key or
/// the column the table counts on. Each was measured on MySQL 8.4.11.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MySqlKeyChangeError {
    /// `ADD PRIMARY KEY` over a column the table has not got: 1072.
    KeyColumnMissing(String),
    /// `DROP PRIMARY KEY` on a table without one: 1091.
    NoKeyToDrop,
    /// A key added beside the one the table has: 1068.
    SecondKey,
    /// A key column declared `NULL` or `DEFAULT NULL`: 1171.
    KeyColumnMayBeNull,
    /// A column made `NOT NULL`, or a key over one, holding NULL in some row:
    /// 1138.
    NullInANotNullColumn,
    /// A word too long for the narrower column of words it is copied into:
    /// 1265.
    WordCutShort,
    /// A counted column no key starts with: 1075.
    CountedColumnNotAKey,
    /// A column of the table's own foreign key would start or stop counting
    /// while foreign key checks are on: 1832.
    ForeignKeyColumnCountingChanges,
    /// A column another table's foreign key names would start or stop
    /// counting while foreign key checks are on: 1833.
    ReferencedColumnCountingChanges,
    /// A column a foreign key is over, on either side, renamed by a change
    /// MySQL makes by copying the rows: 1846.
    ForeignKeyColumnRenamedInACopy,
    /// `DROP COLUMN` of a column the table has not got: 1091.
    NoColumnToDrop(String),
    /// `DROP COLUMN` of every column the table has: 1090.
    EveryColumnDropped,
    /// `DROP COLUMN` of a column the table's own foreign key is over: 1828.
    ColumnOfAForeignKeyDropped { column: String, constraint: String },
    /// `DROP COLUMN` of a column another table's foreign key names: 1829.
    ReferencedColumnDropped {
        column: String,
        constraint: String,
        table: String,
    },
}

impl MySqlKeyChangeError {
    pub fn message(&self) -> String {
        match self {
            Self::KeyColumnMissing(column) => {
                format!("Key column '{column}' doesn't exist in table")
            }
            Self::NoKeyToDrop => "Can't DROP 'PRIMARY'; check that column/key exists".to_string(),
            Self::SecondKey => "Multiple primary key defined".to_string(),
            Self::KeyColumnMayBeNull => "All parts of a PRIMARY KEY must be NOT NULL".to_string(),
            Self::NullInANotNullColumn => "Invalid use of NULL value".to_string(),
            Self::WordCutShort => "Data truncated for column".to_string(),
            Self::CountedColumnNotAKey => {
                "there can be only one auto column and it must be defined as a key".to_string()
            }
            Self::ForeignKeyColumnCountingChanges => {
                "Cannot change column used in a foreign key constraint".to_string()
            }
            Self::ReferencedColumnCountingChanges => {
                "Cannot change column used in a foreign key constraint of another table".to_string()
            }
            Self::ForeignKeyColumnRenamedInACopy => "ALGORITHM=COPY is not supported. Reason: Columns participating in a foreign key are renamed. Try ALGORITHM=INPLACE.".to_string(),
            Self::NoColumnToDrop(column) => {
                format!("Can't DROP '{column}'; check that column/key exists")
            }
            Self::EveryColumnDropped => {
                "You can't delete all columns with ALTER TABLE; use DROP TABLE instead".to_string()
            }
            Self::ColumnOfAForeignKeyDropped { column, constraint } => format!(
                "Cannot drop column '{column}': needed in a foreign key constraint '{constraint}'"
            ),
            Self::ReferencedColumnDropped {
                column,
                constraint,
                table,
            } => format!(
                "Cannot drop column '{column}': needed in a foreign key constraint '{constraint}' of table '{table}'"
            ),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MySqlForeignKeyDefinitionError {
    ChildColumnMissing {
        column: String,
    },
    ColumnCountMismatch {
        constraint: Option<String>,
    },
    ParentTableMissing {
        table: String,
    },
    ParentColumnMissing {
        column: String,
        constraint: String,
        table: String,
    },
    IncompatibleColumns {
        child: String,
        parent: String,
        constraint: String,
    },
    NoUniqueKeyInParent {
        constraint: String,
        table: String,
    },
    DuplicateName {
        name: String,
    },
}

impl MySqlForeignKeyDefinitionError {
    pub fn message(&self) -> String {
        match self {
            Self::ChildColumnMissing { column } => {
                format!("Key column '{column}' doesn't exist in table")
            }
            Self::ColumnCountMismatch { constraint } => format!(
                "Incorrect foreign key definition for '{}': Key reference and table reference don't match",
                constraint.as_deref().unwrap_or("foreign key without name")
            ),
            Self::ParentTableMissing { table } => {
                format!("Failed to open the referenced table '{table}'")
            }
            Self::ParentColumnMissing {
                column,
                constraint,
                table,
            } => format!(
                "Failed to add the foreign key constraint. Missing column '{column}' for constraint '{constraint}' in the referenced table '{table}'"
            ),
            Self::IncompatibleColumns {
                child,
                parent,
                constraint,
            } => format!(
                "Referencing column '{child}' and referenced column '{parent}' in foreign key constraint '{constraint}' are incompatible."
            ),
            Self::NoUniqueKeyInParent { constraint, table } => format!(
                "Failed to add the foreign key constraint. Missing unique key for constraint '{constraint}' in the referenced table '{table}'"
            ),
            Self::DuplicateName { name } => {
                format!("Duplicate foreign key constraint name '{name}'")
            }
        }
    }
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

/// Persisted view attributes exposed to schema dump clients.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MySqlViewMetadata {
    pub name: String,
    pub create_statement: String,
    pub creator: SchemaSqlCreator,
}

/// Persisted trigger attributes exposed to schema dump clients.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MySqlTriggerMetadata {
    pub name: String,
    pub table: String,
    pub event: turso_mysql_parser::MySqlTriggerEvent,
    pub timing: turso_mysql_parser::MySqlTriggerTiming,
    /// The body as it was written.
    pub statement: String,
    pub create_statement: String,
    pub creator: SchemaSqlCreator,
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

/// Failure while dropping checked MySQL views.
#[derive(Debug)]
pub enum MySqlDropViewError {
    MissingView,
    NotView,
    /// One statement named the same view twice, which MySQL answers with 1066.
    NamedTwice,
    Engine(LimboError),
}

/// Failure while writing a view again with `CREATE OR REPLACE VIEW` or
/// `ALTER VIEW`.
#[derive(Debug)]
pub enum MySqlReplaceViewError {
    /// A table has the name: MySQL answers 1347.
    NotView,
    /// `ALTER VIEW` named nothing: MySQL answers 1146.
    MissingView,
    Query(MySqlQueryError),
}

/// A name a `DROP VIEW IF EXISTS` passed over, each noted by MySQL.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MySqlSkippedView {
    /// Nothing of that name is there: note 1051.
    Missing(String),
    /// A table is there: note 1347.
    NotView(String),
}

/// Failure while renaming tables with one `RENAME TABLE`.
#[derive(Debug)]
pub enum MySqlRenameTableError {
    /// A pair names a table that is not there when its turn comes.
    MissingTable,
    /// A pair names a new name a table or view already has when its turn
    /// comes.
    NameTaken,
    Query(MySqlQueryError),
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

/// A row `IGNORE` skipped because a key of its table already held its value,
/// which MySQL warns 1062 about.
#[derive(Debug, Clone, PartialEq)]
pub struct MySqlIgnoredDuplicate {
    pub table: String,
    /// The key as MySQL names it: `PRIMARY`, or the index's own name.
    pub key_name: String,
    /// The row's value for each column of the key.
    pub key: Vec<Value>,
}

/// One user-visible table or view from the selected MySQL database.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MySqlTable {
    name: String,
    kind: MySqlTableKind,
    /// The collation and comment a base table was declared with. A view
    /// has neither.
    options: Option<turso_mysql_parser::MySqlTableOptions>,
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
    /// A written number that is not a whole one, kept as its digits.
    ///
    /// A `DECIMAL` column holds its default at its own scale and MySQL prints
    /// it that way, so what is kept here is what the statement wrote and the
    /// scale is put on where the column is known.
    Number(String),
    /// A decoded single-quoted string DEFAULT literal.
    Text(String),
    /// A `TRUE` or `FALSE` DEFAULT literal.
    Boolean(bool),
    /// `DEFAULT CURRENT_TIMESTAMP`, which stores the moment the row is
    /// written rather than a value written into the statement.
    Moment,
    /// `DEFAULT (now())`, an expression default storing the same moment,
    /// which MySQL reports as the call it is rather than as
    /// `CURRENT_TIMESTAMP`.
    MomentCall,
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
    temporal_precision: Option<u8>,
    collation_name: Option<&'static str>,
    nullable: bool,
    key: MySqlColumnKey,
    default_sql: Option<String>,
    default_value: Option<MySqlColumnDefault>,
    extra: String,
    comment: String,
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

    /// Returns the fractional-second precision of TIME, DATETIME, or TIMESTAMP.
    pub const fn temporal_precision(&self) -> Option<u8> {
        self.temporal_precision
    }

    /// Returns the collation used by a character column.
    pub const fn collation_name(&self) -> Option<&'static str> {
        self.collation_name
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

    /// Returns the text the column's `COMMENT` holds, empty where it has none.
    pub fn comment(&self) -> &str {
        &self.comment
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

    /// Returns the collation a base table was declared with, or `None` for a
    /// view.
    pub fn collation(&self) -> Option<turso_mysql_parser::MySqlTableCollation> {
        self.options.as_ref().map(|options| options.collation)
    }

    /// Returns the comment a base table was declared with, empty where it has
    /// none, or `None` for a view.
    pub fn comment(&self) -> Option<&str> {
        self.options
            .as_ref()
            .map(|options| options.comment.as_deref().unwrap_or_default())
    }

    /// Returns whether a base table was declared `ROW_FORMAT=DYNAMIC`.
    pub fn dynamic_row_format(&self) -> bool {
        self.options
            .as_ref()
            .is_some_and(|options| options.dynamic_row_format)
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
    last_insert_id_result: bool,
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

    pub const fn is_last_insert_id_result(&self) -> bool {
        self.last_insert_id_result
    }
}

/// An owned value accepted by a checked MySQL prepared `SELECT`.
#[derive(Debug, Clone, PartialEq)]
pub enum MySqlPreparedValue {
    /// SQL NULL.
    Null,
    /// A signed integer value.
    Integer(i64),
    /// An unsigned binary-protocol integer above the signed 64-bit range.
    UnsignedInteger(u64),
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
                write!(f, "Field '{column}' doesn't have a default value")
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

    /// Takes one place in the quota for a statement prepared on no
    /// database's connection, which gives it back when it is dropped.
    pub fn take_a_place(
        &self,
    ) -> std::result::Result<MySqlPreparedStatementPlace, MySqlPreparedStatementError> {
        self.reserve()
            .map(|permit| MySqlPreparedStatementPlace { _permit: permit })
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

/// One place in the server's prepared-statement quota, held by a statement
/// the server answers itself rather than on a database's connection.
pub struct MySqlPreparedStatementPlace {
    _permit: MySqlPreparedStatementPermit,
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
    time_zone_offset_at_prepare: i32,
    /// How many times core had reprepared this statement when its metadata was
    /// last rebuilt. Core only ever adds to this, so a change means a schema
    /// reprepare happened, which is where MySQL returns a `?` column to its
    /// generic type.
    reprepares_at_last_refresh: u64,
    /// Whether an execution has bound a number where a JSON reading is
    /// compared with a word or looked in for a document.
    ///
    /// Measured on MySQL 8.4.11: once one has, MySQL prepares the statement
    /// again reading that parameter as a number, and goes on reading it so —
    /// `doc->>'$.a' = ?` bound `'1.0'` after it was bound 1 compares `'1.0'`
    /// as a number, and `JSON_CONTAINS(doc, ?)` refuses every word after it
    /// was bound one. Every later word is refused here rather than read
    /// either way.
    bound_a_number_to_a_json_reading: bool,
    select_parameter_readings: Option<SelectParameterReadings>,
    #[cfg(test)]
    metadata_rebuilds: usize,
}

struct SelectParameterReadings {
    schema: Arc<turso_core::schema::Schema>,
    bound_temporal: Vec<BoundTemporalParameter>,
    bound_decimal: Vec<usize>,
    whole_number: Vec<usize>,
    word: Vec<usize>,
    byte: Vec<usize>,
}

enum PreparedExecutionPlan {
    Select {
        reads_table: bool,
        /// Whether the statement locks the rows it reads, as `FOR UPDATE`
        /// does, which MySQL does not count as the transaction's first
        /// consistent read.
        locks_rows: bool,
        locking_read: Option<MySqlLockingRead>,
        /// Every table the statement reads, which is what says where a
        /// comparison's qualified column comes from.
        source_tables: Vec<MySqlSelectSource>,
        checked_comparisons: Vec<CheckedSelectComparison>,
        /// Which parameters stand where a row count is written, so what a
        /// `LIMIT ?` binds can be held to a row count.
        row_count_parameters: Vec<usize>,
    },
    OrdinaryWrite {
        is_update: bool,
        insert_target: Option<CheckedInsertTarget>,
        written_table: Option<String>,
        /// The tables the statement reads, which a trigger it sets off may
        /// not write.
        read_tables: Vec<String>,
        /// What an `INSERT`'s `SELECT` compares its bound values with.
        copied_select: Option<CopiedSelect>,
        /// The parameters compared with a column of words, which bind a word.
        word_parameters: Vec<usize>,
        /// The parameters compared with a column of bytes, which bind a word
        /// or bytes.
        byte_parameters: Vec<usize>,
        /// Each `?` an `UPDATE` does arithmetic with, and how MySQL reads
        /// what binds there.
        bound_operands: Vec<(usize, BoundOperandKind)>,
    },
    AutoIncrementInsert(Box<PreparedAutoIncrementInsert>),
    CountedInsertSelect(Box<PreparedCountedInsertSelect>),
}

impl PreparedExecutionPlan {
    /// The tables, comparisons and row counts a `SELECT` holds its bound
    /// values to — a bare one, or the one an `INSERT ... SELECT` copies from.
    fn select_comparisons(&self) -> Option<SelectComparisons<'_>> {
        match self {
            Self::Select {
                source_tables,
                checked_comparisons,
                row_count_parameters,
                ..
            } => Some(SelectComparisons {
                source_tables,
                checked_comparisons,
                row_count_parameters,
            }),
            Self::OrdinaryWrite {
                copied_select: Some(copied),
                ..
            } => Some(copied.comparisons()),
            Self::CountedInsertSelect(copy) => Some(copy.copied_select.comparisons()),
            Self::OrdinaryWrite { .. } | Self::AutoIncrementInsert(_) => None,
        }
    }
}

/// How MySQL reads a value bound as one side of `+`, `-` or `*` in an
/// `UPDATE`, which depends on the column the answer is written into.
///
/// Measured on 8.4.11 with go-sql-driver's binary types: `balance - ?` into
/// a `DECIMAL(10,2)` reads a bound whole number and a bound word exactly —
/// 90.49 less `'0.005'` is 90.485, stored as 90.49 — while a bound double
/// makes the arithmetic a double's, so 1.97 plus 0.145 is 2.1149999... and
/// stores 2.11 where exact arithmetic stores 2.12. `views + ?` into a `BIGINT`
/// adds a bound whole number exactly and rounds anything else into the
/// column, 8 plus 1.5 storing 10. A word naming no number fails the statement
/// with 1292. So a whole number, a word naming one, and NULL are taken
/// against either, a word naming a decimal against a `DECIMAL` too, and
/// everything else is refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BoundOperandKind {
    WholeNumber,
    ExactNumber,
}

struct SelectComparisons<'a> {
    source_tables: &'a [MySqlSelectSource],
    checked_comparisons: &'a [CheckedSelectComparison],
    row_count_parameters: &'a [usize],
}

/// What the `SELECT` of a prepared `INSERT ... SELECT` holds its bound values
/// to, which is what a bare prepared `SELECT` holds them to.
struct CopiedSelect {
    source_tables: Vec<MySqlSelectSource>,
    checked_comparisons: Vec<CheckedSelectComparison>,
    row_count_parameters: Vec<usize>,
}

impl CopiedSelect {
    fn of(translated: &TranslatedDml) -> Self {
        Self {
            source_tables: translated.read_tables().to_vec(),
            checked_comparisons: translated.checked_comparisons().to_vec(),
            row_count_parameters: translated.row_count_parameters().to_vec(),
        }
    }

    fn comparisons(&self) -> SelectComparisons<'_> {
        SelectComparisons {
            source_tables: &self.source_tables,
            checked_comparisons: &self.checked_comparisons,
            row_count_parameters: &self.row_count_parameters,
        }
    }
}

/// The type an `information_schema` table the statement reads declares for
/// the column one comparison names. Such a table's columns are named by the
/// table itself rather than by stored DDL.
fn catalog_column_type(
    source_tables: &[MySqlSelectSource],
    comparison: &CheckedSelectComparison,
) -> Option<&'static str> {
    source_tables.iter().find_map(|source| {
        source
            .catalog()
            .and_then(|catalog| catalog.column_type(comparison.column_name()))
    })
}

/// Returns the tables a comparison's column may belong to, nearest first.
///
/// A qualified comparison names one; an unqualified one written inside a
/// subquery is the subquery's column when it has one and the outer
/// statement's when it does not, which is how MySQL reads it — measured on
/// 8.4.11, `EXISTS (SELECT 1 FROM b WHERE name = 'one')` reads `a.name` where
/// `b` carries no `name`.
fn comparison_tables(
    source_tables: &[MySqlSelectSource],
    comparison: &CheckedSelectComparison,
) -> Result<Vec<MySqlTableName>> {
    column_tables(
        source_tables,
        comparison.qualifier(),
        comparison.inner_sources(),
    )
}

/// Returns the tables a column named with this qualifier, or none, may belong
/// to, nearest first. See `comparison_tables`.
fn column_tables(
    source_tables: &[MySqlSelectSource],
    qualifier: Option<&str>,
    inner_sources: &[String],
) -> Result<Vec<MySqlTableName>> {
    let named = |reference: &str| {
        source_tables
            .iter()
            .find(|source| source.reference().eq_ignore_ascii_case(reference))
            .map(|source| source.table().clone())
    };
    if let Some(qualifier) = qualifier {
        return named(qualifier).map(|table| vec![table]).ok_or_else(|| {
            LimboError::InvalidArgument(
                "SELECT comparison names no table the statement reads".to_string(),
            )
        });
    }
    let mut candidates = inner_sources
        .iter()
        .filter_map(|inner| named(inner))
        .collect::<Vec<_>>();
    let readable = source_tables
        .iter()
        .filter(|source| !source.subquery() && source.branch() == 0)
        .collect::<Vec<_>>();
    match readable.as_slice() {
        [source] => candidates.push(source.table().clone()),
        // An unqualified name in a join is the column of whichever joined
        // table has one — Gitea's `INNER JOIN issue_assignees ON assignee_id
        // = user.id`. The first that has it is taken; where two have it the
        // engine refuses the name as ambiguous, as MySQL answers 1052. A
        // derived table or a CTE shows fewer columns than its table has, so a
        // join over one is left out.
        joined
            if joined.iter().all(|source| {
                source.derived().is_none()
                    && source.catalog().is_none()
                    && source.projected_columns().is_empty()
            }) =>
        {
            candidates.extend(joined.iter().map(|source| source.table().clone()));
        }
        _ => {}
    }
    if candidates.is_empty() {
        return Err(LimboError::InvalidArgument(
            "SELECT comparison requires a table column as its left operand".to_string(),
        ));
    }
    Ok(candidates)
}

/// Which columns a checked INSERT fills in, so the caller can report the NOT
/// NULL error MySQL would report.
enum CheckedInsertTarget {
    /// `INSERT INTO t DEFAULT VALUES` names no columns at all.
    DefaultValues(MySqlTableName),
    /// `INSERT INTO t (c1, ..., cn) VALUES (...)`.
    Listed(ListedInsert),
}

impl CheckedInsertTarget {
    fn table(&self) -> &MySqlTableName {
        match self {
            Self::DefaultValues(table) => table,
            Self::Listed(insert) => &insert.table,
        }
    }
}

struct ListedInsert {
    table: MySqlTableName,
    /// Whether the statement was written `INSERT IGNORE`.
    ignores: bool,
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
            is_null
                && not_null
                    .iter()
                    .any(|name| name.eq_ignore_ascii_case(column))
        })
    }

    /// Whether any row puts a NULL bound for a `?` in a NOT NULL column.
    fn binds_null_to_a_not_null_column(
        &self,
        not_null: &[String],
        bound: &[MySqlPreparedValue],
    ) -> bool {
        self.rows.iter().any(|row| {
            row.iter().zip(&self.columns).any(|(value, column)| {
                matches!(value, InsertedValue::Marker(index)
                    if matches!(bound.get(*index), Some(MySqlPreparedValue::Null)))
                    && not_null
                        .iter()
                        .any(|name| name.eq_ignore_ascii_case(column))
            })
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
    last_engine_statement: Mutex<Option<(Stmt, Statement)>>,
}

/// A prepared `INSERT ... SELECT` into a table that counts its own ids,
/// Laravel's `insertUsing` with its bindings.
struct PreparedCountedInsertSelect {
    sql: String,
    copy: turso_mysql_parser::MySqlInsertSelect,
    table: AutoIncrementTable,
    copied_select: CopiedSelect,
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
                write!(f, "Field '{column}' doesn't have a default value")
            }
            Self::DuplicateColumn(column) => {
                write!(f, "Duplicate column name '{column}'")
            }
            Self::DuplicateIndex => f.write_str("Duplicate key name"),
            Self::MissingIndex => f.write_str("unknown index"),
            Self::MissingTable => f.write_str("unknown table"),
            Self::RequiredByForeignKey => f.write_str("cannot drop index needed by a foreign key"),
            Self::JsonIndex => {
                f.write_str("JSON column supports indexing only via generated columns")
            }
            Self::JsonLiteralDefault => f.write_str("JSON column cannot have a literal default"),
            Self::ReadOnlyTransaction => {
                f.write_str("cannot execute statement in a READ ONLY transaction")
            }
            Self::NoSuchSavepoint => f.write_str("savepoint does not exist"),
            Self::NoSuchCheck(name) => {
                write!(f, "Check constraint '{name}' is not found in the table")
            }
            Self::DuplicateCheckName(name) => {
                write!(f, "Duplicate check constraint name '{name}'")
            }
            Self::ForeignKeyDefinition(error) => f.write_str(&error.message()),
            Self::KeyChange(error) => f.write_str(&error.message()),
            Self::Syntax(error) => f.write_str(error),
            Self::Unsupported(error) => f.write_str(error),
            Self::Engine(error) => error.fmt(f),
        }
    }
}

impl Error for MySqlQueryError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::MissingRequiredDefault(_)
            | Self::DuplicateColumn(_)
            | Self::DuplicateIndex
            | Self::MissingIndex
            | Self::MissingTable
            | Self::RequiredByForeignKey
            | Self::JsonIndex
            | Self::JsonLiteralDefault
            | Self::ReadOnlyTransaction
            | Self::NoSuchSavepoint
            | Self::NoSuchCheck(_)
            | Self::DuplicateCheckName(_)
            | Self::ForeignKeyDefinition(_)
            | Self::KeyChange(_) => None,
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
            MySqlQueryError::ForeignKeyDefinition(error) => Self::ParseError(error.message()),
            MySqlQueryError::KeyChange(error) => Self::ParseError(error.message()),
            MySqlQueryError::DuplicateColumn(column) => {
                Self::ParseError(format!("Duplicate column name '{column}'"))
            }
            MySqlQueryError::DuplicateIndex => Self::ParseError("Duplicate key name".to_string()),
            MySqlQueryError::MissingIndex => Self::ParseError("unknown index".to_string()),
            MySqlQueryError::MissingTable => Self::ParseError("unknown table".to_string()),
            MySqlQueryError::RequiredByForeignKey => {
                Self::ParseError("cannot drop index needed by a foreign key".to_string())
            }
            MySqlQueryError::JsonIndex => {
                Self::ParseError("JSON column cannot be indexed directly".to_string())
            }
            MySqlQueryError::JsonLiteralDefault => {
                Self::ParseError("JSON column cannot have a literal default".to_string())
            }
            MySqlQueryError::ReadOnlyTransaction => Self::ReadOnly,
            MySqlQueryError::NoSuchSavepoint => Self::TxError("no such savepoint".to_string()),
            MySqlQueryError::NoSuchCheck(name) => Self::ParseError(format!(
                "Check constraint '{name}' is not found in the table"
            )),
            MySqlQueryError::DuplicateCheckName(name) => {
                Self::ParseError(format!("Duplicate check constraint name '{name}'"))
            }
            MySqlQueryError::Syntax(error) => Self::ParseError(error),
            MySqlQueryError::Unsupported(error) => Self::ParseError(error),
            MySqlQueryError::Engine(error) => error,
        }
    }
}

type CheckedDmlTranslation = (TranslatedDml, DmlColumnTypes, Option<(String, String)>);

/// What the table a DML statement writes says about its columns, which the
/// statement is read a second time knowing. A reprepare has no connection to
/// read them again from, so they are kept with the statement.
#[derive(Default)]
struct DmlColumnTypes {
    /// The columns an `UPDATE` rewrites to the moment it runs at.
    rewritten_on_update: Vec<(String, u8)>,
    decimal: Vec<(String, u32)>,
    integer: Vec<String>,
    /// The columns holding words, which a `?` compared with one is held to
    /// binding a word against.
    text: Vec<String>,
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
        reject_incompatible_legacy_tables(&inner)?;
        // MySQL has no SQLite DQS misfeature. Left on, an identifier that does
        // not resolve becomes a string literal, so `SELECT id, nosuchcolumn
        // FROM t` answers with a fabricated `nosuchcolumn` beside a real value
        // instead of MySQL's 1054. Measured on MySQL 8.4.11: `SELECT $`, which
        // is what a real client's `select $$` probe reduces to, is 1054, and
        // `select $$` itself is 1064.
        inner.set_dqs_dml(false);
        // MySQL makes a session wait for a lock another session holds rather
        // than answering straight away, and answers 1205 once the wait runs
        // out. The engine waits the same way for the one write lock it holds
        // over the database, so the wait is set to the one MySQL starts with.
        inner.set_busy_timeout(Self::DEFAULT_LOCK_WAIT);
        // A transaction that read and then writes is given up with 1213 only
        // when another session's commit changed a page it read; see
        // `Connection::set_write_after_unrelated_commits`.
        inner.set_write_after_unrelated_commits(true);
        // MySQL's InnoDB enforces a foreign key, and the engine enforces one
        // only with this on. Left off, a `FOREIGN KEY` written by a client is
        // stored and never checked, which is a guarantee handed over and not
        // kept — measured on MySQL 8.4.11, a child row naming a parent that
        // does not exist answers 1452. Turning it on here changes nothing for
        // a table already stored: the constraint was refused until now, so no
        // durable MySQL table carries one.
        inner.set_foreign_keys_enabled(true);
        inner.set_foreign_keys_checked_row_by_row(true);
        // Measured on MySQL 8.4.11: `IGNORE` warns 1062 for each row it skips
        // over a key it collides with, so the engine notes each.
        inner.set_ignored_duplicates_noted(true);
        Ok(Self {
            _closes_on_last_drop: Arc::new(CloseOnLastDrop {
                connection: Arc::clone(&inner),
                database_user: std::sync::OnceLock::new(),
            }),
            inner,
            schema_context,
            auto_increment: None,
            session_autocommit: Arc::new(Mutex::new(true)),
            session_time_zone_offset: Arc::new(Mutex::new(0)),
            explained_error: Arc::new(Mutex::new(None)),
            read_only_transaction: Arc::new(Mutex::new(false)),
            session_read_only: Arc::new(Mutex::new(false)),
            tables_locked: Arc::new(Mutex::new(false)),
            transaction_isolation: Arc::new(Mutex::new(TransactionIsolation::default())),
            written_zero: Arc::new(Mutex::new(WrittenZero::AsksForTheNextNumber)),
            prepared_statements: Arc::new(Mutex::new(PreparedStatementRegistry::default())),
            prepared_statement_authority,
            schema_readings: Arc::default(),
            prepared_counted_rows_statements: Arc::default(),
            prepared_transaction_statements: Arc::default(),
            wal_keeper: None,
            #[cfg(test)]
            counted_rows_written_again: Arc::default(),
            database_collation: None,
            database_user: None,
            metadata_lock_wait: Arc::new(Mutex::new(DEFAULT_METADATA_LOCK_WAIT)),
            transaction_command_ran: Arc::default(),
            stands_in_for_a_dropped_database: false,
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
        connection
            .inner
            .set_trigger_rowid_supplier(Some(Arc::new(CountedTriggerRowSupplier {
                allocator: allocator.clone(),
                io: Arc::clone(&io),
                database_identity: connection
                    .inner
                    .schema_catalog_validation_context()
                    .map(|context| *context.database_identity()),
            })));
        connection.auto_increment = Some(AutoIncrementExecutionCapability { allocator, io });
        Ok(connection)
    }

    #[cfg(test)]
    pub(crate) fn inner(&self) -> &Arc<Connection> {
        &self.inner
    }

    /// Records the tables this session may see, or `None` for all of them.
    ///
    /// The `information_schema` tables are registered on the database rather
    /// than on one connection, so a session that may see only some of them
    /// leaves the list here for those tables to read.
    pub fn set_visible_tables(&self, visible: Option<Vec<String>>) {
        self.inner.set_mysql_visible_tables(visible);
    }

    /// Leaves the rows one `information_schema` table answers, for a table
    /// whose rows only the session can work out.
    ///
    /// A row may stop short of the table's width, holding only the columns
    /// that could be worked out; a statement reading one past them fails.
    pub fn set_catalog_rows(
        &self,
        table: turso_mysql_parser::MySqlCatalogTable,
        rows: Vec<Vec<Value>>,
    ) {
        self.inner.set_mysql_catalog_rows(table.engine_name(), rows);
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

    /// Has the WAL emptied once it holds more than
    /// [`Self::WAL_FRAMES_BEFORE_TRUNCATING`] frames, between transactions.
    ///
    /// The engine's own checkpoint copies the WAL into the database after a
    /// write but leaves the file at its size, and a pooled connection may stay
    /// open for as long as the server runs. So a large write would otherwise
    /// keep its whole size on disk, and a start after a crash would read all
    /// of it again. A session of a catalog asks the catalog's WAL keeper,
    /// which empties it on a thread of its own; one opened on its own empties
    /// it here. Another session reading at the same moment keeps the WAL busy;
    /// the attempt is left to a later statement then.
    pub fn keep_the_wal_small(&self) -> Result<()> {
        if self.inner.mvcc_enabled() {
            return self.keep_the_mvcc_log_small();
        }
        self.truncate_the_wal_past(Self::WAL_FRAMES_BEFORE_TRUNCATING)
    }

    /// Asks the keeper to checkpoint an MVCC database whose logical log grew
    /// past twice the engine's own bound, or whose WAL did.
    ///
    /// The engine checkpoints the log at a commit only when no other
    /// transaction is open, which under a steady load of overlapping
    /// transactions never happens. Measured with eight sysbench sessions
    /// running `oltp_read_write`, the log grew from 2 MB to 20 MB in 30 s
    /// without one checkpoint, and the checkpoint that ran once the load
    /// stopped held a reader back for 150 ms, 500 ms after 120 s. The keeper
    /// holds new transactions back until the running ones end, so the log
    /// stays near the bound and each checkpoint stays short.
    fn keep_the_mvcc_log_small(&self) -> Result<()> {
        let Some((keeper, database)) = &self.wal_keeper else {
            return Ok(());
        };
        let log_past_its_bound = self.inner.mv_store().as_ref().is_some_and(|store| {
            u64::try_from(store.checkpoint_threshold())
                .is_ok_and(|threshold| store.logical_log_offset() >= threshold.saturating_mul(2))
        });
        if !log_past_its_bound
            && self.inner.wal_state()?.max_frame <= Self::WAL_FRAMES_BEFORE_TRUNCATING
        {
            return Ok(());
        }
        keeper.ask_to_truncate(
            database,
            self.database_user
                .as_ref()
                .map(|user| user.another_on_the_same_database()),
        )
    }

    /// 64 MiB of 4 KiB pages.
    const WAL_FRAMES_BEFORE_TRUNCATING: u64 = 16_384;

    /// Hands emptying this connection's WAL to a catalog's keeper.
    pub(crate) fn with_wal_keeper(
        mut self,
        keeper: WalKeeperHandle,
        database: std::sync::Weak<turso_core::Database>,
    ) -> Self {
        self.wal_keeper = Some((keeper, database));
        self
    }

    /// Gives this connection its database's collation, which a catalog keeps.
    pub(crate) fn with_database_collation(mut self, collation: SharedDatabaseCollation) -> Self {
        self.database_collation = Some(collation);
        self
    }

    /// Lets a catalog's `DROP DATABASE` know when this connection uses its
    /// database.
    pub(crate) fn with_database_user(mut self, user: DatabaseUser) -> Self {
        let user = Arc::new(user);
        self._closes_on_last_drop
            .database_user
            .set(Arc::clone(&user))
            .unwrap_or_else(|_| panic!("a connection belongs to one database"));
        self.database_user = Some(user);
        self
    }

    /// Opens an empty database named `name`, held in memory and refusing
    /// every write, for a session whose database another session dropped to
    /// run what reads none of its tables on, carrying over the session's
    /// settings and its open transaction from `dropped`.
    ///
    /// Measured on MySQL 8.4.11: such a session still runs `SELECT 1`,
    /// `BEGIN` and `COMMIT`, and its `information_schema` reads find no table
    /// of the database. Nothing may run on the dropped database's files.
    pub(crate) fn stand_in_for_a_dropped_database(
        name: &str,
        schema_context: SchemaSqlSessionContext,
        prepared_statement_authority: MySqlPreparedStatementAuthority,
        dropped: &MySqlConnection,
    ) -> Result<Self> {
        let database = turso_core::Database::open_file_with_flags(
            Arc::new(turso_core::MemoryIO::new()),
            ":memory:",
            turso_core::OpenFlags::Create,
            turso_core::DatabaseOpts::new().with_views(true),
            None,
            Arc::new(crate::MySqlDialect),
        )?;
        crate::catalog_tables::register_catalog_tables(&database, name)?;
        let mut stand_in = Self::new_with_prepared_statement_authority(
            database.connect()?,
            schema_context,
            prepared_statement_authority,
        )?;
        stand_in.stands_in_for_a_dropped_database = true;
        stand_in.set_time_zone_offset_seconds(dropped.time_zone_offset_seconds());
        stand_in.set_last_insert_id(dropped.last_insert_id());
        stand_in.set_metadata_lock_wait(dropped.metadata_lock_wait());
        *stand_in.session_autocommit.lock().unwrap() = dropped.session_autocommit();
        if !dropped.is_auto_commit() {
            stand_in
                .inner
                .prepare("BEGIN")
                .and_then(|mut statement| statement.run_ignore_rows())?;
        }
        stand_in.inner.set_query_only(true);
        Ok(stand_in)
    }

    /// Whether this is the empty database a session whose database another
    /// session dropped runs what reads no table on.
    pub fn stands_in_for_a_dropped_database(&self) -> bool {
        self.stands_in_for_a_dropped_database
    }

    /// Whether `other` is a clone of this connection, running on the same
    /// engine connection.
    pub fn is_the_same_connection_as(&self, other: &MySqlConnection) -> bool {
        Arc::ptr_eq(&self.inner, &other.inner)
    }

    /// Marks a statement starting on this connection's database, which a
    /// `DROP DATABASE` then waits for until [`Self::finish_a_statement`].
    ///
    /// Answers the database's name when another session dropped it: nothing
    /// may run on the files of a database that is gone. A statement that
    /// starts while a drop waits waits behind it, as MySQL's does.
    pub fn start_a_statement(&self) -> std::result::Result<(), MySqlStatementNotStarted> {
        *self.transaction_command_ran.lock().unwrap() = TransactionCommandRan::None;
        self.inner.take_main_database_was_used();
        match &self.database_user {
            Some(user) => user.start_using(self.metadata_lock_wait()),
            None => Ok(()),
        }
    }

    /// Marks the statement [`Self::start_a_statement`] started as finished.
    ///
    /// A transaction left open that has read keeps the database in use until
    /// it ends. Measured on MySQL 8.4.11: a drop waits for a transaction that
    /// read one of the database's tables, or listed them with `SHOW TABLES`,
    /// and not for one that has only begun — `WITH CONSISTENT SNAPSHOT`
    /// among them — taken a savepoint or run `SELECT 1`. The engine takes a
    /// transaction's snapshot at its first read, so a snapshot left by a
    /// statement other than a transaction command is such a read. In MVCC
    /// mode `BEGIN CONCURRENT` holds a snapshot from the start, and a
    /// statement that used the database is the read instead.
    pub fn finish_a_statement(&self) {
        let Some(user) = &self.database_user else {
            return;
        };
        if self.inner.get_auto_commit() {
            user.stop_using();
            user.note_a_transaction_ended();
            return;
        }
        let ran = std::mem::take(&mut *self.transaction_command_ran.lock().unwrap());
        match ran {
            TransactionCommandRan::BeganATransaction => user.stop_using(),
            TransactionCommandRan::Savepoint => user.stop_using_unless_the_transaction_keeps_it(),
            TransactionCommandRan::None if self.the_transaction_read_the_database() => {
                user.keep_for_the_transaction();
            }
            TransactionCommandRan::None => user.stop_using_unless_the_transaction_keeps_it(),
        }
    }

    /// How many transactions on this connection's database have ended, for
    /// [`Self::wait_for_another_transaction_to_end`].
    pub fn transactions_ended_on_the_database(&self) -> u64 {
        self.database_user
            .as_ref()
            .map_or(0, |user| user.transactions_ended())
    }

    /// Waits for a transaction on this connection's database to end after
    /// `ended_before` was read, for a statement that met another
    /// transaction's write and runs again once that one is over. Answers
    /// false when `deadline` passes first.
    pub fn wait_for_another_transaction_to_end(
        &self,
        ended_before: u64,
        deadline: Option<std::time::Instant>,
    ) -> bool {
        match &self.database_user {
            Some(user) => user.wait_for_a_transaction_to_end(ended_before, deadline),
            None => {
                std::thread::sleep(Duration::from_millis(1));
                deadline.is_none_or(|deadline| std::time::Instant::now() < deadline)
            }
        }
    }

    /// How long a statement waits for a lock another session holds, which is
    /// MySQL's `innodb_lock_wait_timeout`.
    pub fn lock_wait(&self) -> Duration {
        self.inner.get_busy_timeout()
    }

    fn the_transaction_read_the_database(&self) -> bool {
        let used = self.inner.take_main_database_was_used();
        if self.inner.mvcc_enabled() {
            return used;
        }
        self.inner.has_read_snapshot()
    }

    /// Lets the database go whatever this connection still has open, for a
    /// session dropping its own database once its transaction has ended.
    pub(crate) fn stop_using_the_database(&self) {
        if let Some(user) = &self.database_user {
            user.stop_using();
        }
    }

    /// Whether another session dropped this connection's database.
    pub fn database_was_dropped(&self) -> bool {
        self.database_user
            .as_ref()
            .is_some_and(|user| user.database_was_dropped())
    }

    /// The collation a table made through this connection takes when it names
    /// neither a character set nor a collation: its database's, and
    /// `utf8mb4_0900_ai_ci` for a connection that belongs to no catalog.
    pub fn database_collation(&self) -> turso_mysql_parser::MySqlTableCollation {
        self.database_collation
            .as_ref()
            .map(SharedDatabaseCollation::get)
            .unwrap_or_default()
    }

    pub(crate) fn truncate_the_wal_past(&self, frames: u64) -> Result<()> {
        if !self.inner.get_auto_commit() || self.inner.wal_state()?.max_frame <= frames {
            return Ok(());
        }
        if let Some((keeper, database)) = &self.wal_keeper {
            return keeper.ask_to_truncate(
                database,
                self.database_user
                    .as_ref()
                    .map(|user| user.another_on_the_same_database()),
            );
        }
        match self.inner.checkpoint(turso_core::CheckpointMode::Truncate {
            upper_bound_inclusive: None,
        }) {
            Ok(_)
            | Err(LimboError::Busy)
            | Err(LimboError::BusySnapshot)
            | Err(LimboError::StatementsInProgress(_)) => Ok(()),
            Err(error) => Err(error),
        }
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
            Ok((translated, rendered_differently)) => {
                Self::reject_internal_catalog_select(&translated)
                    .map_err(MySqlPreparedStatementError::Prepare)?;
                self.reject_binary_scalar_collation(&translated)
                    .map_err(MySqlPreparedStatementError::Prepare)?;
                self.refuse_select_json_readings_of_other_columns(&translated)
                    .map_err(MySqlPreparedStatementError::Prepare)?;
                self.hold_the_projection_to_what_the_keys_decide(&translated)
                    .map_err(MySqlPreparedStatementError::Prepare)?;
                self.hold_bare_names_in_result_subqueries_to_their_tables(&translated)
                    .map_err(MySqlPreparedStatementError::Prepare)?;
                self.validate_select_comparison_columns(
                    translated.source_tables(),
                    translated.checked_comparisons(),
                )
                .map_err(|error| {
                    MySqlPreparedStatementError::Prepare(MySqlQueryError::Unsupported(
                        error.to_string(),
                    ))
                })?;
                self.validate_select_subquery_comparison_columns(
                    translated.source_table(),
                    translated.source_tables(),
                    translated.checked_subquery_comparisons(),
                )
                .map_err(|error| {
                    MySqlPreparedStatementError::Prepare(MySqlQueryError::Unsupported(
                        error.to_string(),
                    ))
                })?;
                // The count it notes is read after a statement run as text,
                // and a prepared one would leave it unread.
                if translated.calculates_found_rows() {
                    return Err(MySqlPreparedStatementError::Prepare(
                        MySqlQueryError::Unsupported(
                            "SQL_CALC_FOUND_ROWS in a prepared statement".to_string(),
                        ),
                    ));
                }
                static_result_metadata = translated.static_result_metadata().to_vec();
                let statement = translated.parse_ast().map_err(|error| {
                    MySqlPreparedStatementError::Prepare(MySqlQueryError::Syntax(error.to_string()))
                })?;
                self.validate_session_timestamp_select(&translated, &statement)
                    .map_err(MySqlPreparedStatementError::Prepare)?;
                let reads_table = translated.reads_table();
                let locks_rows = translated.locks_rows();
                let locking_read = translated
                    .locking_read()
                    .filter(|_| self.inner.mvcc_enabled());
                let row_count_parameters = translated.row_count_parameters().to_vec();
                let source_tables = translated.source_tables().to_vec();
                let checked_comparisons = translated.checked_comparisons().to_vec();
                let frozen =
                    self.frozen_select_parser(&translated, rendered_differently, &statement);
                let options = PrepareOptions::default().with_reprepare_parser(Arc::new(frozen));
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
                        locks_rows,
                        locking_read,
                        source_tables,
                        checked_comparisons,
                        row_count_parameters,
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
                time_zone_offset_at_prepare: self.time_zone_offset_seconds(),
                bound_a_number_to_a_json_reading: false,
                select_parameter_readings: None,
                #[cfg(test)]
                metadata_rebuilds: 0,
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

    /// Parses one checked DML statement, telling the parser which of the
    /// table's columns an `UPDATE` rewrites to the moment it runs at.
    ///
    /// Only the frontend can see that, so the statement is read once to learn
    /// which table it writes and again knowing that table's columns — the same
    /// two passes a `SELECT` over a text column takes. A table this cannot
    /// describe cannot carry the attribute, only this frontend writing one
    /// putting it there, so the first reading stands for those.
    fn parse_checked_dml_translation(
        &self,
        sql: &str,
        mode: SessionSqlMode,
    ) -> std::result::Result<CheckedDmlTranslation, MySqlParseError> {
        if self.time_zone_offset_seconds() != 0
            && (uses_session_local_clock(sql) || self.a_trigger_reads_the_clock())
        {
            return Err(MySqlParseError::Unsupported {
                feature: "session-local clock functions in a non-UTC time zone",
            });
        }
        let translated = match parse_dml(sql, mode) {
            Err(MySqlParseError::Unsupported { feature })
                if feature == turso_mysql_parser::INSERT_SELECT_NEEDING_COLUMN_TYPES =>
            {
                self.parse_insert_select_knowing_its_select(sql, mode)?
            }
            translated => translated?,
        };
        self.refuse_an_upsert_answered_otherwise(sql, mode)?;
        self.refuse_literals_their_columns_store_otherwise(sql, mode)?;
        if translated
            .parse_ast()
            .is_ok_and(|statement| self.writes_a_value_a_trigger_replaces(&statement))
        {
            return Err(MySqlParseError::Unsupported {
                feature: "a statement writing a value a BEFORE trigger writes over",
            });
        }
        let insert_target = translated
            .parse_ast()
            .ok()
            .and_then(|statement| checked_insert_target(&statement).ok().flatten());
        if let Some(target) = &insert_target {
            if !translated.read_tables().is_empty() {
                let mut has_decimal = false;
                for source in std::iter::once(target.table()).chain(
                    translated
                        .read_tables()
                        .iter()
                        .map(MySqlSelectSource::table),
                ) {
                    let columns = self.list_shared_columns(source).map_err(|_| {
                        MySqlParseError::Unsupported {
                            feature: "INSERT SELECT table metadata",
                        }
                    })?;
                    if columns.iter().any(|column| column.decimal_size().is_some()) {
                        has_decimal = true;
                    }
                }
                if has_decimal
                    && !self.insert_select_copies_decimal_columns(sql, mode, target, &translated)
                    && !self.insert_select_ignores_decimal_columns(sql, mode, target, &translated)
                {
                    return Err(MySqlParseError::Unsupported {
                        feature: "INSERT SELECT with DECIMAL source or target columns",
                    });
                }
            }
        }
        let inserts = insert_target.is_some();
        let table = if let Some(update) = translated.checked_update() {
            MySqlTableName::parse(update.table_name()).ok()
        } else {
            insert_target
                .map(|target| target.table().clone())
                .or_else(|| {
                    translated
                        .source_table()
                        .and_then(|name| MySqlTableName::parse(name).ok())
                })
        };
        let refuse_an_unknown_fallback = |translated: &TranslatedDml| {
            if translated.falls_back_in_a_set() {
                return Err(MySqlParseError::Unsupported {
                    feature: "COALESCE in a SET over a table whose columns are not known",
                });
            }
            Ok(())
        };
        let Some(table) = table else {
            refuse_an_unknown_fallback(&translated)?;
            return Ok((translated, DmlColumnTypes::default(), None));
        };
        let table_definition = self
            .inner
            .current_schema()
            .get_btree_table(table.as_str())
            .map(|stored| (table.as_str().to_owned(), stored.to_sql()));
        let Ok(columns) = self.list_shared_columns(&table) else {
            refuse_an_unknown_fallback(&translated)?;
            return Ok((translated, DmlColumnTypes::default(), table_definition));
        };
        let rewritten = if translated.checked_update().is_some() {
            columns
                .iter()
                .filter(|column| column.extra().contains("on update CURRENT_TIMESTAMP"))
                .map(|column| {
                    (
                        column.name().to_owned(),
                        column.temporal_precision().unwrap_or(0),
                    )
                })
                .collect::<Vec<_>>()
        } else {
            Vec::new()
        };
        // The engine holds a `BIGINT UNSIGNED` as a blob of its own, which a
        // bound number meets only through the comparison a `DECIMAL` is read
        // with, as a `SELECT` reads it. An `INSERT` compares nothing of the
        // table it writes.
        let decimal_columns = columns
            .iter()
            .filter_map(|column| {
                column
                    .decimal_size()
                    .map(|(_, scale)| (column.name().to_owned(), scale))
                    .or_else(|| {
                        (!inserts && column.type_name() == "BIGINT UNSIGNED")
                            .then(|| (column.name().to_owned(), 0))
                    })
            })
            .collect::<Vec<_>>();
        let integer_columns = columns
            .iter()
            .filter(|column| is_integer_type(column.type_name()))
            .map(|column| column.name().to_owned())
            .collect::<Vec<_>>();
        let text_columns = if inserts {
            Vec::new()
        } else {
            columns
                .iter()
                .filter(|column| is_text_type(column.type_name()))
                .map(|column| column.name().to_owned())
                .collect::<Vec<_>>()
        };
        let compares_a_placeholder = translated.checked_comparisons().iter().any(|comparison| {
            matches!(
                comparison.rhs(),
                CheckedSelectComparisonRhs::Placeholder { .. }
            )
        });
        let column_types = DmlColumnTypes {
            rewritten_on_update: rewritten,
            decimal: decimal_columns,
            integer: integer_columns,
            text: text_columns,
        };
        // The `SELECT` a copy reads was rendered knowing its own columns'
        // types; the table it writes changes nothing in how it renders.
        if translated.copies_a_select_rendered_knowing_its_types()
            || (column_types.rewritten_on_update.is_empty()
                && column_types.decimal.is_empty()
                && !translated.compares_a_written_number()
                && !translated.falls_back_in_a_set()
                && (column_types.text.is_empty() || !compares_a_placeholder))
        {
            return Ok((translated, column_types, table_definition));
        }
        let translated = turso_mysql_parser::parse_dml_knowing_column_types(
            sql,
            mode,
            &column_types.rewritten_on_update,
            &column_types.decimal,
            &column_types.integer,
            &column_types.text,
        )?;
        Ok((translated, column_types, table_definition))
    }

    /// Refuses a literal written into a column that would store it otherwise
    /// than MySQL stores it.
    ///
    /// A number with a fraction reaches the engine as the digits it was
    /// written with, which a column of bytes keeps as they stand, where MySQL
    /// keeps its own spelling of the number: measured on 8.4.11, `1e3` into a
    /// `BLOB` stores `1000`. A string of bytes into any other column is read
    /// as a number or as text by rules not measured here — `X'41'` into an
    /// `INT` stores 65 — so it is taken only by a column of bytes.
    fn refuse_literals_their_columns_store_otherwise(
        &self,
        sql: &str,
        mode: SessionSqlMode,
    ) -> std::result::Result<(), MySqlParseError> {
        let Ok(Some(turso_mysql_parser::WrittenLiterals {
            tables,
            columns: written,
        })) = turso_mysql_parser::literals_written_into_columns(sql, mode)
        else {
            return Ok(());
        };
        for table in tables {
            let Ok(table) = MySqlTableName::parse(&table) else {
                continue;
            };
            let Ok(columns) = self.list_shared_columns(&table) else {
                continue;
            };
            for (written_column, literal) in &written {
                let column = match written_column {
                    turso_mysql_parser::WrittenColumn::Named(name) => columns
                        .iter()
                        .find(|column| column.name().eq_ignore_ascii_case(name)),
                    turso_mysql_parser::WrittenColumn::AtPlace(place) => columns.get(*place),
                };
                let Some(column) = column else {
                    continue;
                };
                let holds_bytes = turso_mysql_parser::holds_bytes(column.type_name());
                match literal {
                    ColumnLiteral::NumberWithAFraction if holds_bytes => {
                        return Err(MySqlParseError::Unsupported {
                            feature: "a number with a fraction written into a column of bytes",
                        });
                    }
                    ColumnLiteral::Bytes if !holds_bytes => {
                        return Err(MySqlParseError::Unsupported {
                            feature: "a string of bytes written into a column of another kind",
                        });
                    }
                    ColumnLiteral::Moment
                        if !matches!(column.type_name(), "DATETIME" | "TIMESTAMP") =>
                    {
                        return Err(MySqlParseError::Unsupported {
                            feature: "TIMESTAMP('...') written into a column holding no moment",
                        });
                    }
                    _ => {}
                }
            }
        }
        Ok(())
    }

    /// Refuses an upsert the engine would answer otherwise than MySQL.
    ///
    /// A table with an `ON UPDATE CURRENT_TIMESTAMP` column the clause does not
    /// assign is refused: measured on 8.4.11, MySQL writes the moment into it
    /// whenever the upsert changes the row, and the engine's upsert does not.
    ///
    /// A column the clause compares between the row already there and the row
    /// offered — Rails' `upsert_all` writes `t.name <=> offered.name` — is held
    /// to the pairs that answer alike. MySQL puts the offered value into the
    /// column's type before it compares, and the engine compares it as it was
    /// written: measured, `'2026-01-01'` offered for a `DATETIME` holding that
    /// midnight is the same moment in MySQL and a different word here. A word
    /// offered for a column of words, compared under the column's collation in
    /// both, a whole number for a column of them, and a written number for a
    /// `DECIMAL`, which the engine puts into the column's form before it
    /// compares, answer alike.
    fn refuse_an_upsert_answered_otherwise(
        &self,
        sql: &str,
        mode: SessionSqlMode,
    ) -> std::result::Result<(), MySqlParseError> {
        let upsert = match turso_mysql_parser::parse_optional_upsert(sql, mode) {
            Ok(Some(upsert)) => upsert,
            // A statement that is no upsert, or that does not parse, is
            // answered by the path that runs it.
            Ok(None) | Err(_) => return Ok(()),
        };
        let table = MySqlTableName::parse(&upsert.table).map_err(|_| UPSERT_REFUSED)?;
        // Only a counted table writes an upsert's rows one at a time, which
        // is what leaves out the columns each row gives `DEFAULT`.
        if upsert.defaults_in_some_rows
            && !matches!(self.load_auto_increment_table(&upsert.table), Ok(Some(_)))
        {
            return Err(MySqlParseError::Unsupported {
                feature: "INSERT DEFAULT in some rows only",
            });
        }
        let Ok(columns) = self.list_shared_columns(&table) else {
            return Ok(());
        };
        if !moments_the_clause_leaves(&columns, &upsert.assigned).is_empty()
            && !self.stamps_the_rows_an_upsert_changes(sql, mode, &upsert.table)
        {
            return Err(MySqlParseError::Unsupported {
                feature: "an upsert on a table with an ON UPDATE CURRENT_TIMESTAMP column it does not assign",
            });
        }
        for comparison in upsert.comparisons {
            let column = columns
                .iter()
                .find(|column| column.name().eq_ignore_ascii_case(&comparison.column))
                .ok_or(UPSERT_REFUSED)?;
            let type_name = column.type_name();
            let offered_alike = |taken: &[OfferedValue]| {
                comparison
                    .offered
                    .iter()
                    .all(|offered| *offered == OfferedValue::Null || taken.contains(offered))
            };
            let alike = if matches!(
                type_name,
                "VARCHAR" | "TEXT" | "TINYTEXT" | "MEDIUMTEXT" | "LONGTEXT"
            ) {
                offered_alike(&[OfferedValue::Word])
            } else if is_decimal_type(type_name) {
                offered_alike(&[OfferedValue::WholeNumber, OfferedValue::NumberWithAPoint])
            } else if is_integer_type(type_name) && type_name != "BIGINT UNSIGNED" {
                offered_alike(&[OfferedValue::WholeNumber])
            } else {
                false
            };
            if !alike {
                return Err(UPSERT_REFUSED);
            }
        }
        Ok(())
    }

    /// Whether this upsert is written where each row it changes is stamped
    /// with the moment in the `ON UPDATE CURRENT_TIMESTAMP` columns its clause
    /// leaves: an upsert on a counted table asking the counter for every id,
    /// or written a row at a time, in a session reading the clock in UTC, on
    /// a table carrying no trigger, which writing a changed row a second time
    /// would set off twice.
    fn stamps_the_rows_an_upsert_changes(
        &self,
        sql: &str,
        mode: SessionSqlMode,
        table: &str,
    ) -> bool {
        if self.time_zone_offset_seconds() != 0
            || self
                .inner
                .current_schema()
                .get_triggers_for_table(table)
                .next()
                .is_some()
        {
            return false;
        }
        let Ok(Some(counted)) = self.load_auto_increment_table(table) else {
            return false;
        };
        let Ok(insert) = parse_prepared_auto_increment_insert(sql, mode) else {
            return false;
        };
        let Ok(bound) = insert.bind_allocator_table_with(&counted.definition, self.written_zero())
        else {
            return false;
        };
        bound.rowwise_conflicts()
            || bound
                .row_values()
                .iter()
                .all(|value| *value == AutoIncrementRowValue::Generated)
    }

    /// Renders an `INSERT ... SELECT` whose `SELECT` has to know its columns'
    /// types, the way a bare `SELECT` is rendered: read once, and again
    /// knowing the types of the columns it orders by and compares with a `?`.
    ///
    /// Measured on MySQL 8.4.11, the `SELECT` of `INSERT INTO t (a) SELECT a
    /// FROM u WHERE name = ?` finds the rows a bare `SELECT` binding the same
    /// value finds, a word matching without regard to case, and one ordering
    /// by a text column copies the rows in the order the bare one answers.
    fn parse_insert_select_knowing_its_select(
        &self,
        sql: &str,
        mode: SessionSqlMode,
    ) -> std::result::Result<TranslatedDml, MySqlParseError> {
        const REFUSED: MySqlParseError = MySqlParseError::Unsupported {
            feature: "INSERT SELECT whose SELECT is refused on its own",
        };
        let select_sql = turso_mysql_parser::insert_select_source_sql(sql, mode)?.ok_or(
            MySqlParseError::Unsupported {
                feature: turso_mysql_parser::INSERT_SELECT_NEEDING_COLUMN_TYPES,
            },
        )?;
        let (select, _) = self
            .parse_select_knowing_column_types(&select_sql)
            .map_err(|_| REFUSED)?;
        Self::reject_internal_catalog_select(&select).map_err(|_| REFUSED)?;
        self.reject_binary_scalar_collation(&select)
            .map_err(|_| REFUSED)?;
        self.refuse_select_json_readings_of_other_columns(&select)
            .map_err(|_| REFUSED)?;
        turso_mysql_parser::parse_insert_select_knowing_its_select(sql, mode, &select)
    }

    fn insert_select_copies_decimal_columns(
        &self,
        sql: &str,
        mode: SessionSqlMode,
        target: &CheckedInsertTarget,
        translated: &TranslatedDml,
    ) -> bool {
        let CheckedInsertTarget::Listed(target) = target else {
            return false;
        };
        let [source] = translated.read_tables() else {
            return false;
        };
        if source.subquery()
            || source.catalog().is_some()
            || self
                .inner
                .current_schema()
                .get_btree_table(source.table().as_str())
                .is_none()
        {
            return false;
        }
        let Some(projection) = turso_mysql_parser::direct_insert_select_projection(sql, mode)
        else {
            return false;
        };
        let Ok(source_columns) = self.list_shared_columns(source.table()) else {
            return false;
        };
        let Ok(target_columns) = self.list_shared_columns(&target.table) else {
            return false;
        };
        let projected = match projection {
            turso_mysql_parser::MySqlDirectInsertSelectProjection::All => source_columns
                .iter()
                .map(|column| Some(column.name().to_owned()))
                .collect::<Vec<_>>(),
            turso_mysql_parser::MySqlDirectInsertSelectProjection::Columns(columns) => {
                columns.into_iter().map(Some).collect()
            }
            turso_mysql_parser::MySqlDirectInsertSelectProjection::ColumnsAndLiterals(values) => {
                values
            }
            turso_mysql_parser::MySqlDirectInsertSelectProjection::IntegerArithmetic(_) => {
                return false;
            }
        };
        projected.len() == target.columns.len()
            && projected
                .iter()
                .zip(&target.columns)
                .all(|(read, written)| {
                    let target = target_columns
                        .iter()
                        .find(|column| column.name().eq_ignore_ascii_case(written));
                    target.is_some_and(|target| match read {
                        Some(read) => source_columns
                            .iter()
                            .find(|column| column.name().eq_ignore_ascii_case(read))
                            .is_some_and(|source| {
                                source.decimal_size() == target.decimal_size()
                                    || (is_integer_type(target.type_name())
                                        && target.type_name() != "BIGINT UNSIGNED")
                            }),
                        None => target.decimal_size().is_none(),
                    })
                })
    }

    fn insert_select_ignores_decimal_columns(
        &self,
        sql: &str,
        mode: SessionSqlMode,
        target: &CheckedInsertTarget,
        translated: &TranslatedDml,
    ) -> bool {
        let CheckedInsertTarget::Listed(target) = target else {
            return false;
        };
        let [source] = translated.read_tables() else {
            return false;
        };
        if source.subquery()
            || source.catalog().is_some()
            || self
                .inner
                .current_schema()
                .get_btree_table(source.table().as_str())
                .is_none()
        {
            return false;
        }
        let Some(projection) = turso_mysql_parser::filtered_insert_select_projection(sql, mode)
        else {
            return false;
        };
        let (Ok(source_columns), Ok(target_columns)) = (
            self.list_shared_columns(source.table()),
            self.list_shared_columns(&target.table),
        ) else {
            return false;
        };
        if source_columns
            .iter()
            .filter(|column| column.decimal_size().is_some())
            .any(|column| sql_mentions_column(sql, column.name()))
        {
            return false;
        }
        if let turso_mysql_parser::MySqlDirectInsertSelectProjection::IntegerArithmetic(
            expressions,
        ) = &projection
        {
            return expressions.len() == target.columns.len()
                && expressions.iter().all(|columns| {
                    columns.iter().all(|read| {
                        source_columns.iter().any(|column| {
                            column.name().eq_ignore_ascii_case(read)
                                && is_integer_type(column.type_name())
                        })
                    })
                })
                && target.columns.iter().all(|written| {
                    target_columns.iter().any(|column| {
                        column.name().eq_ignore_ascii_case(written)
                            && is_integer_type(column.type_name())
                    })
                });
        }
        // A value reading nothing of the source — a written whole number,
        // NULL, a `?` — lands in a column that holds no `DECIMAL`, as every
        // column written here has to.
        let projected = match projection {
            turso_mysql_parser::MySqlDirectInsertSelectProjection::All => return false,
            turso_mysql_parser::MySqlDirectInsertSelectProjection::Columns(columns) => {
                columns.into_iter().map(Some).collect::<Vec<_>>()
            }
            turso_mysql_parser::MySqlDirectInsertSelectProjection::ColumnsAndLiterals(values) => {
                values
            }
            turso_mysql_parser::MySqlDirectInsertSelectProjection::IntegerArithmetic(_) => {
                unreachable!()
            }
        };
        projected.len() == target.columns.len()
            && projected.iter().flatten().all(|read| {
                source_columns
                    .iter()
                    .any(|column| column.name().eq_ignore_ascii_case(read))
            })
            && target.columns.iter().all(|written| {
                target_columns.iter().any(|column| {
                    column.name().eq_ignore_ascii_case(written) && column.decimal_size().is_none()
                })
            })
    }

    fn frozen_dml_parser(
        &self,
        mode: SessionSqlMode,
        column_types: DmlColumnTypes,
        table_definition: Option<(String, String)>,
        translated: &TranslatedDml,
    ) -> FrozenDmlParser {
        let mut read_table_definitions = Vec::new();
        let mut untracked_read_source = false;
        for source in translated.read_tables() {
            if let Some(table) = self
                .inner
                .current_schema()
                .get_btree_table(source.table().as_str())
            {
                read_table_definitions.push((source.table().as_str().to_owned(), table.to_sql()));
            } else {
                untracked_read_source = true;
            }
        }
        FrozenDmlParser {
            mode,
            column_types,
            table_definition,
            read_table_definitions,
            untracked_read_source,
            shifted_timestamp_insert: None,
            typed_copy: None,
        }
    }

    fn prepare_checked_dml_statement(
        &self,
        sql: &str,
    ) -> std::result::Result<(Option<Statement>, PreparedExecutionPlan), MySqlPreparedStatementError>
    {
        let mode = self.parser_mode();
        let (translated, column_types, table_definition) =
            match self.parse_checked_dml_translation(sql, mode) {
                Ok(read) => read,
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
        self.validate_dml_comparison_columns(&translated)
            .map_err(|error| {
                MySqlPreparedStatementError::Prepare(MySqlQueryError::Unsupported(
                    error.to_string(),
                ))
            })?;
        self.validate_dml_ordered_columns(translated.source_table(), translated.ordered_columns())
            .map_err(|error| {
                MySqlPreparedStatementError::Prepare(MySqlQueryError::Unsupported(
                    error.to_string(),
                ))
            })?;
        self.reject_non_utc_timestamp_dml_source(&translated)
            .map_err(MySqlPreparedStatementError::Prepare)?;
        let mut statement = translated.parse_ast().map_err(|error| {
            MySqlPreparedStatementError::Prepare(MySqlQueryError::Syntax(error.to_string()))
        })?;
        let shifted_timestamp_insert = self
            .shift_timestamp_insert_literals(&mut statement)
            .map_err(MySqlPreparedStatementError::Prepare)?;
        let is_update = matches!(statement, Stmt::Update(_));
        let insert_target =
            checked_insert_target(&statement).map_err(MySqlPreparedStatementError::Engine)?;
        if matches!(statement, Stmt::Insert { .. }) {
            if let Some(table) = self.prepared_auto_increment_insert_table(sql, mode)? {
                if let Some(copy) = turso_mysql_parser::parse_optional_insert_select(sql, mode)
                    .map_err(|error| {
                        MySqlPreparedStatementError::Prepare(mysql_query_parse_error(error))
                    })?
                {
                    return self.prepare_counted_insert_select(sql, copy, table, &translated);
                }
                let target = insert_target
                    .as_ref()
                    .expect("a checked INSERT has a target");
                let writes_timestamp = self
                    .list_shared_columns(target.table())
                    .map_err(|error| {
                        MySqlPreparedStatementError::Prepare(MySqlQueryError::Unsupported(
                            error.to_string(),
                        ))
                    })?
                    .iter()
                    .any(|column| {
                        column.type_name() == "TIMESTAMP"
                            && matches!(target, CheckedInsertTarget::Listed(insert)
                                if insert.lists(column.name()))
                    });
                if self.time_zone_offset_seconds() != 0 && writes_timestamp {
                    return Err(MySqlPreparedStatementError::Prepare(
                        MySqlQueryError::Unsupported(
                            "non-UTC TIMESTAMP INSERT with AUTO_INCREMENT is unsupported"
                                .to_owned(),
                        ),
                    ));
                }
                return self.prepare_checked_auto_increment_insert(sql, mode, table);
            }
        }
        if is_update {
            self.reject_prepared_auto_increment_update(translated.checked_update())?;
        }
        let mut frozen = self.frozen_dml_parser(mode, column_types, table_definition, &translated);
        if shifted_timestamp_insert {
            frozen.shifted_timestamp_insert = Some(statement.clone());
        }
        if translated.copies_a_select_rendered_knowing_its_types() {
            frozen.typed_copy = Some(statement.clone());
        }
        let options = PrepareOptions::default()
            .with_reprepare_parser(Arc::new(frozen))
            .with_rows_foreign_keys_refuse_skipped(turso_mysql_parser::deletes_ignoring_errors(
                sql, mode,
            ));
        let statement = self
            .inner
            .prepare_translated_stmt_with_options(statement, sql, &options)
            .map_err(|error| {
                MySqlPreparedStatementError::Prepare(MySqlQueryError::Engine(error))
            })?;
        let written_table = insert_target
            .as_ref()
            .map(|target| target.table().as_str().to_owned())
            .or_else(|| translated.source_table().map(str::to_owned));
        let word_parameters = self
            .dml_word_parameters(&translated)
            .map_err(MySqlPreparedStatementError::Engine)?;
        let byte_parameters = self
            .dml_byte_parameters(&translated)
            .map_err(MySqlPreparedStatementError::Engine)?;
        let bound_operands = self
            .bound_operand_kinds(&translated)
            .map_err(MySqlPreparedStatementError::Prepare)?;
        Ok((
            Some(statement),
            PreparedExecutionPlan::OrdinaryWrite {
                is_update,
                copied_select: (insert_target.is_some() && !translated.read_tables().is_empty())
                    .then(|| CopiedSelect::of(&translated)),
                insert_target,
                written_table,
                read_tables: read_table_names(&translated),
                word_parameters,
                byte_parameters,
                bound_operands,
            },
        ))
    }

    /// Reads how MySQL takes each `?` an `UPDATE` does arithmetic with, from
    /// the type of the column the answer is written into.
    fn bound_operand_kinds(
        &self,
        translated: &TranslatedDml,
    ) -> std::result::Result<Vec<(usize, BoundOperandKind)>, MySqlQueryError> {
        let operands = translated.bound_arithmetic_operands();
        let Some(update) = translated.checked_update().filter(|_| !operands.is_empty()) else {
            return Ok(Vec::new());
        };
        let table = MySqlTableName::parse(update.table_name())
            .map_err(|error| MySqlQueryError::Syntax(error.to_string()))?;
        let columns = self
            .list_shared_columns(&table)
            .map_err(|error| MySqlQueryError::Unsupported(error.to_string()))?;
        operands
            .iter()
            .map(|operand| {
                let written = operand.written_column();
                let column = columns
                    .iter()
                    .find(|column| column.name().eq_ignore_ascii_case(written))
                    .ok_or(MySqlQueryError::Engine(LimboError::SchemaUpdated))?;
                let kind = if column.decimal_size().is_some() {
                    BoundOperandKind::ExactNumber
                } else if is_integer_type(column.type_name())
                    && column.type_name() != "BIGINT UNSIGNED"
                {
                    BoundOperandKind::WholeNumber
                } else {
                    return Err(MySqlQueryError::Unsupported(format!(
                        "a ? in arithmetic written into a {} column, where what MySQL does with the value bound there has not been measured",
                        column.type_name()
                    )));
                };
                Ok((operand.ordinal(), kind))
            })
            .collect()
    }

    /// Prepares an `INSERT ... SELECT` into a table that counts its own ids.
    ///
    /// Nothing is reserved here: the rows are read and numbered when it runs,
    /// the `SELECT` binding the values the statement is run with, exactly as
    /// the text statement copies them.
    fn prepare_counted_insert_select(
        &self,
        sql: &str,
        copy: turso_mysql_parser::MySqlInsertSelect,
        table: AutoIncrementTable,
        translated: &TranslatedDml,
    ) -> std::result::Result<(Option<Statement>, PreparedExecutionPlan), MySqlPreparedStatementError>
    {
        let Stmt::Insert {
            body: InsertBody::Select(source, None),
            ..
        } = translated.parse_ast().map_err(|error| {
            MySqlPreparedStatementError::Prepare(MySqlQueryError::Syntax(error.to_string()))
        })?
        else {
            return Err(MySqlPreparedStatementError::Prepare(
                MySqlQueryError::Unsupported(
                    "INSERT SELECT into an AUTO_INCREMENT table".to_string(),
                ),
            ));
        };
        let reading = Stmt::Select(source);
        let options = PrepareOptions::default().with_reprepare_parser(Arc::new(
            FrozenInjectedAutoIncrementInsertParser {
                statement: reading.clone(),
            },
        ));
        let reading = self
            .inner
            .prepare_translated_stmt_with_options(reading, sql, &options)
            .map_err(|error| {
                MySqlPreparedStatementError::Prepare(MySqlQueryError::Engine(error))
            })?;
        Ok((
            None,
            PreparedExecutionPlan::CountedInsertSelect(Box::new(PreparedCountedInsertSelect {
                sql: sql.to_owned(),
                copy,
                table,
                copied_select: CopiedSelect::of(translated),
                parameter_count: reading.parameters_count(),
            })),
        ))
    }

    fn shift_timestamp_insert_literals(
        &self,
        statement: &mut Stmt,
    ) -> std::result::Result<bool, MySqlQueryError> {
        if self.time_zone_offset_seconds() == 0 {
            return Ok(false);
        }
        let table_name = match statement {
            Stmt::Insert { tbl_name, .. } => tbl_name.name.as_str(),
            Stmt::Update(update) => update.tbl_name.name.as_str(),
            Stmt::Delete { tbl_name, .. } => tbl_name.name.as_str(),
            _ => return Ok(false),
        };
        let table = MySqlTableName::parse(table_name)
            .map_err(|error| MySqlQueryError::Syntax(error.to_string()))?;
        let metadata = self
            .list_shared_columns(&table)
            .map_err(|error| MySqlQueryError::Unsupported(error.to_string()))?;
        if matches!(statement, Stmt::Insert { .. })
            && metadata.iter().any(|column| {
                column.type_name() == "DATETIME"
                    && matches!(
                        column.default_value(),
                        Some(MySqlColumnDefault::Moment | MySqlColumnDefault::MomentCall)
                    )
            })
        {
            return Err(MySqlQueryError::Unsupported(
                "non-UTC DATETIME clock defaults are unsupported".to_owned(),
            ));
        }
        if matches!(statement, Stmt::Update(_))
            && metadata.iter().any(|column| {
                column.type_name() == "DATETIME"
                    && column
                        .extra()
                        .to_ascii_lowercase()
                        .contains("on update current_timestamp")
            })
        {
            return Err(MySqlQueryError::Unsupported(
                "non-UTC DATETIME automatic updates are unsupported".to_owned(),
            ));
        }
        if !metadata
            .iter()
            .any(|column| column.type_name() == "TIMESTAMP")
        {
            return Ok(false);
        }
        let Stmt::Insert { columns, body, .. } = statement else {
            return Err(MySqlQueryError::Unsupported(
                "non-UTC TIMESTAMP UPDATE and DELETE are unsupported".to_owned(),
            ));
        };
        if columns.is_empty() {
            return Err(MySqlQueryError::Unsupported(
                "non-UTC TIMESTAMP INSERT requires an explicit column list".to_owned(),
            ));
        }
        let InsertBody::Select(select, None) = body else {
            return Err(MySqlQueryError::Unsupported(
                "non-UTC TIMESTAMP INSERT requires direct VALUES".to_owned(),
            ));
        };
        let OneSelect::Values(rows) = &mut select.body.select else {
            return Err(MySqlQueryError::Unsupported(
                "non-UTC TIMESTAMP INSERT requires direct VALUES".to_owned(),
            ));
        };
        let mut changed = false;
        for row in rows {
            for (index, expression) in row.iter_mut().enumerate() {
                let Some(column) = metadata.iter().find(|column| {
                    columns
                        .get(index)
                        .is_some_and(|name| column.name().eq_ignore_ascii_case(name.as_str()))
                }) else {
                    return Err(MySqlQueryError::Unsupported(
                        "INSERT column mismatch".to_owned(),
                    ));
                };
                if column.type_name() != "TIMESTAMP" {
                    continue;
                }
                match &mut **expression {
                    Expr::Literal(Literal::String(value)) => {
                        let decoded = mysql_text_default(value).map_err(|_| {
                            MySqlQueryError::Unsupported("invalid TIMESTAMP literal".to_owned())
                        })?;
                        let normalized = turso_mysql_parser::normalize_datetime_with_precision(
                            &decoded,
                            column.temporal_precision().unwrap_or(0),
                        )
                        .ok_or_else(|| {
                            MySqlQueryError::Unsupported("invalid TIMESTAMP value".to_owned())
                        })?;
                        let shifted = crate::temporal_zone::shift_timestamp(
                            &normalized,
                            -self.time_zone_offset_seconds(),
                        )
                        .ok_or_else(|| {
                            MySqlQueryError::Unsupported(
                                "TIMESTAMP leaves supported range".to_owned(),
                            )
                        })?;
                        *value = format!("'{shifted}'");
                        changed = true;
                    }
                    Expr::Literal(Literal::Null) | Expr::Variable(_) => {}
                    _ => {
                        return Err(MySqlQueryError::Unsupported(
                            "non-UTC TIMESTAMP requires a direct string or parameter".to_owned(),
                        ));
                    }
                }
            }
        }
        Ok(changed)
    }

    fn reject_non_utc_timestamp_dml_source(
        &self,
        translated: &TranslatedDml,
    ) -> std::result::Result<(), MySqlQueryError> {
        if self.time_zone_offset_seconds() == 0 {
            return Ok(());
        }
        for source in translated.read_tables() {
            let columns = self
                .list_shared_columns(source.table())
                .map_err(|error| MySqlQueryError::Unsupported(error.to_string()))?;
            if columns
                .iter()
                .any(|column| column.type_name() == "TIMESTAMP")
            {
                return Err(MySqlQueryError::Unsupported(
                    "non-UTC TIMESTAMP reads during a write are unsupported".to_owned(),
                ));
            }
        }
        Ok(())
    }

    fn timestamp_insert_parameters(
        &self,
        target: &CheckedInsertTarget,
    ) -> Result<Vec<(usize, u8)>> {
        if self.time_zone_offset_seconds() == 0 {
            return Ok(Vec::new());
        }
        let CheckedInsertTarget::Listed(insert) = target else {
            return Ok(Vec::new());
        };
        let columns = self
            .list_shared_columns(&insert.table)
            .map_err(|error| LimboError::ParseError(error.to_string()))?;
        let mut parameters = Vec::new();
        for row in &insert.rows {
            for (column_name, value) in insert.columns.iter().zip(row) {
                let Some(column) = columns.iter().find(|column| {
                    column.name().eq_ignore_ascii_case(column_name)
                        && column.type_name() == "TIMESTAMP"
                }) else {
                    continue;
                };
                if let InsertedValue::Marker(ordinal) = value {
                    parameters.push((*ordinal, column.temporal_precision().unwrap_or(0)));
                }
            }
        }
        Ok(parameters)
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
            .bind_allocator_table_with(&table.definition, self.written_zero())
            .map_err(|error| {
                MySqlPreparedStatementError::Prepare(MySqlQueryError::Unsupported(
                    error.to_string(),
                ))
            })?;
        let prototype_ids = bound
            .row_values()
            .iter()
            .enumerate()
            .map(|(offset, value)| {
                (*value == AutoIncrementRowValue::Generated).then_some(offset as u64 + 1)
            })
            .collect::<Vec<_>>();
        let prototype = bound.inject_row_ids(&prototype_ids).map_err(|error| {
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
                last_engine_statement: Mutex::new(None),
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
        callback: impl FnMut(&[MySqlPreparedValue]) -> Result<()>,
    ) -> std::result::Result<MySqlPreparedExecutionResult, MySqlPreparedStatementError> {
        let writes = self
            .prepared_statements
            .lock()
            .expect("MySQL prepared statement registry mutex poisoned")
            .statements
            .get(&statement_id)
            .is_some_and(|prepared| {
                !matches!(
                    prepared.execution_plan,
                    PreparedExecutionPlan::Select { .. }
                )
            });
        if !writes {
            return self.execute_prepared_statement_in_its_transaction(
                statement_id,
                values,
                timeout,
                affected_rows_mode,
                callback,
            );
        }
        self.in_a_concurrent_statement_transaction(
            || {
                self.execute_prepared_statement_in_its_transaction(
                    statement_id,
                    values,
                    timeout,
                    affected_rows_mode,
                    callback,
                )
            },
            MySqlPreparedStatementError::Engine,
        )
    }

    fn execute_prepared_statement_in_its_transaction(
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
        if prepared.time_zone_offset_at_prepare != self.time_zone_offset_seconds() {
            return Err(MySqlPreparedStatementError::Prepare(
                MySqlQueryError::Unsupported(
                    "time zone changed after statement prepare; prepare it again".to_owned(),
                ),
            ));
        }
        let expected = usize::from(prepared.metadata.parameter_count);
        if values.len() != expected {
            return Err(MySqlPreparedStatementError::ParameterCountMismatch {
                expected,
                actual: values.len(),
            });
        }
        observe_parameter_markers(&mut prepared.result_column_type_metadata, values);
        let written_values;
        let values = if matches!(
            prepared.execution_plan,
            PreparedExecutionPlan::Select { .. }
        ) {
            values
        } else {
            written_values = utf8_bytes_as_words(values);
            &written_values
        };

        if let Some(statement) = prepared.statement.as_mut() {
            statement
                .reset()
                .map_err(MySqlPreparedStatementError::Engine)?;
        }
        if let PreparedExecutionPlan::CountedInsertSelect(copy) = &prepared.execution_plan {
            let values = self
                .core_values_for(
                    &prepared.execution_plan,
                    &mut prepared.select_parameter_readings,
                    values,
                )
                .map_err(MySqlPreparedStatementError::Engine)?;
            return self
                .execute_prepared_counted_insert_select(copy, &values, timeout, affected_rows_mode)
                .map(MySqlPreparedExecutionResult::Write)
                .map_err(|error| match error {
                    MySqlQueryError::MissingRequiredDefault(column) => {
                        MySqlPreparedStatementError::MissingRequiredDefault(column)
                    }
                    error => MySqlPreparedStatementError::Prepare(error),
                });
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
            ) {
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
            checked_comparisons,
            ..
        } = &prepared.execution_plan
        {
            let binds_a_number = binds_a_number_to_a_json_reading(checked_comparisons, values);
            let held = hold_json_reading_parameters(
                checked_comparisons,
                values,
                prepared.bound_a_number_to_a_json_reading,
            );
            prepared.bound_a_number_to_a_json_reading |= binds_a_number;
            held?;
        }
        let values = self.core_values_for(
            &prepared.execution_plan,
            &mut prepared.select_parameter_readings,
            values,
        )?;

        match &prepared.execution_plan {
            PreparedExecutionPlan::Select {
                reads_table,
                locks_rows,
                locking_read,
                source_tables,
                ..
            } => {
                if *reads_table {
                    self.begin_implicit_transaction_for_table_read()?;
                }
                let serializable_read = (!*locks_rows)
                    .then(|| self.serializable_read_of_a_table(*reads_table))
                    .flatten();
                if *reads_table && !*locks_rows && serializable_read.is_none() {
                    self.note_consistent_read();
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
                match (locking_read, serializable_read) {
                    (Some(locking_read), _) => {
                        lock_the_rows_a_select_reads(statement, *locking_read, source_tables)?;
                    }
                    (None, Some(read)) => {
                        lock_the_rows_a_serializable_select_reads(statement, read, source_tables)?
                    }
                    (None, None) => statement.read_without_locking_rows(),
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
            PreparedExecutionPlan::OrdinaryWrite {
                is_update,
                written_table,
                insert_target,
                read_tables,
                ..
            } => {
                if let Some(target) = insert_target {
                    self.check_the_triggers_an_insert_sets_off(
                        target.table().as_str(),
                        read_tables,
                    )?;
                }
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
                run_checked_write_statement(statement, timeout).map_err(|error| {
                    self.map_unsigned_decimal_write_error(error, written_table.as_deref())
                })?;
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
            PreparedExecutionPlan::CountedInsertSelect(_) => Err(LimboError::InternalError(
                "a prepared counted copy runs before the statement is bound".to_string(),
            )),
        }
    }

    fn execute_prepared_counted_insert_select(
        &self,
        copy: &PreparedCountedInsertSelect,
        values: &[Value],
        timeout: Option<Duration>,
        affected_rows_mode: MySqlAffectedRowsMode,
    ) -> std::result::Result<MySqlWriteResult, MySqlQueryError> {
        let deadline = self.write_deadline(timeout);
        self.check_write_deadline(deadline)?;
        self.begin_implicit_transaction_for_write()?;
        let table = self
            .load_auto_increment_table(copy.table.name.as_str())
            .map_err(MySqlQueryError::Engine)?
            .ok_or(MySqlQueryError::Engine(LimboError::SchemaUpdated))?;
        if table.key != copy.table.key || table.stored_sql != copy.table.stored_sql {
            return Err(MySqlQueryError::Engine(LimboError::SchemaUpdated));
        }
        self.execute_counted_insert_select(
            &copy.sql,
            &copy.copy,
            table,
            values,
            deadline,
            affected_rows_mode,
        )
    }

    /// Checks the values a prepared statement is run with and puts each into
    /// the form the engine binds.
    ///
    /// A value a `SELECT` compares with a column is held to that column as a
    /// bare prepared `SELECT` holds it, the `SELECT` of an `INSERT ... SELECT`
    /// included.
    fn core_values_for(
        &self,
        plan: &PreparedExecutionPlan,
        select_parameter_readings: &mut Option<SelectParameterReadings>,
        values: &[MySqlPreparedValue],
    ) -> Result<Vec<Value>> {
        let mut bound_temporal = Vec::new();
        let mut whole_number_parameters = Vec::new();
        if let Some(SelectComparisons {
            source_tables,
            checked_comparisons,
            row_count_parameters,
        }) = plan.select_comparisons()
        {
            let readings = self.read_select_parameters(
                source_tables,
                checked_comparisons,
                select_parameter_readings,
            )?;
            Self::validate_select_comparison_values(
                checked_comparisons,
                values,
                &readings.bound_temporal,
                &readings.bound_decimal,
                &readings.whole_number,
                &readings.word,
                &readings.byte,
            )?;
            Self::validate_row_count_values(row_count_parameters, values)?;
            Self::refuse_untyped_wide_integer_select_parameters(values, &readings.bound_decimal)?;
            bound_temporal.clone_from(&readings.bound_temporal);
            whole_number_parameters.clone_from(&readings.whole_number);
        }
        if let PreparedExecutionPlan::OrdinaryWrite {
            insert_target,
            word_parameters,
            byte_parameters,
            bound_operands,
            ..
        } = plan
        {
            self.refuse_untyped_wide_integer_write_parameters(insert_target.as_ref(), values)?;
            refuse_a_word_parameter_bound_otherwise(word_parameters, values)?;
            refuse_a_byte_parameter_bound_otherwise(byte_parameters, values)?;
            hold_bound_operands(bound_operands, values)?;
            whole_number_parameters.extend(
                bound_operands
                    .iter()
                    .filter(|(_, kind)| *kind == BoundOperandKind::WholeNumber)
                    .map(|(ordinal, _)| *ordinal),
            );
        }
        let timestamp_parameters = match plan {
            PreparedExecutionPlan::OrdinaryWrite {
                insert_target: Some(target),
                ..
            } => self.timestamp_insert_parameters(target)?,
            _ => Vec::new(),
        };
        // A value meeting a column that holds a day or a moment is put into
        // that column's own form first, which is what MySQL reads it as.
        values
            .iter()
            .enumerate()
            .map(|(ordinal, value)| {
                if let Some((_, precision)) = timestamp_parameters
                    .iter()
                    .find(|(index, _)| *index == ordinal)
                {
                    return match value {
                        MySqlPreparedValue::Text(written) => {
                            let normalized = turso_mysql_parser::normalize_datetime_with_precision(
                                written, *precision,
                            )
                            .ok_or_else(|| {
                                LimboError::InvalidArgument("invalid TIMESTAMP value".to_owned())
                            })?;
                            let shifted = crate::temporal_zone::shift_timestamp(
                                &normalized,
                                -self.time_zone_offset_seconds(),
                            )
                            .ok_or_else(|| {
                                LimboError::InvalidArgument(
                                    "TIMESTAMP leaves supported range".to_owned(),
                                )
                            })?;
                            Ok(Value::from_text(shifted))
                        }
                        MySqlPreparedValue::Null => Ok(Value::Null),
                        _ => Err(LimboError::InvalidArgument(
                            "TIMESTAMP parameter must be a string".to_owned(),
                        )),
                    };
                }
                if let MySqlPreparedValue::Text(written) = value {
                    if whole_number_parameters.contains(&ordinal) {
                        return Ok(Value::from_i64(bound_whole_number(written).ok_or_else(
                            || {
                                LimboError::InternalError(
                                    "a bound word was checked to name a whole number".to_owned(),
                                )
                            },
                        )?));
                    }
                }
                match bound_temporal
                    .iter()
                    .find(|parameter| parameter.ordinal == ordinal)
                {
                    Some(parameter) => temporal_value_in_its_stored_form(value, parameter.form),
                    None => mysql_prepared_value_to_core(value),
                }
            })
            .collect::<Result<Vec<_>>>()
    }

    fn read_select_parameters<'a>(
        &self,
        source_tables: &[MySqlSelectSource],
        comparisons: &[CheckedSelectComparison],
        kept: &'a mut Option<SelectParameterReadings>,
    ) -> Result<&'a SelectParameterReadings> {
        self.inner.maybe_update_schema();
        let schema = self.inner.current_schema();
        if kept
            .as_ref()
            .is_some_and(|readings| Arc::ptr_eq(&readings.schema, &schema))
        {
            return Ok(kept.as_ref().expect("readings were just found"));
        }
        let readings = SelectParameterReadings {
            bound_temporal: self.validate_select_comparison_columns(source_tables, comparisons)?,
            bound_decimal: self.decimal_comparison_parameters(source_tables, comparisons)?,
            whole_number: self.whole_number_comparison_parameters(source_tables, comparisons)?,
            word: self.word_comparison_parameters(source_tables, comparisons)?,
            byte: self.byte_comparison_parameters(source_tables, comparisons)?,
            schema,
        };
        Ok(kept.insert(readings))
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
        self.check_the_triggers_an_insert_sets_off(&table.name, &[])?;
        let bound = insert
            .insert
            .clone()
            .bind_allocator_table_with(&table.definition, self.written_zero())
            .map_err(|error| LimboError::ParseError(error.to_string()))?;
        if insert
            .insert
            .binds_null_where_the_table_refuses_one(&table.definition, |ordinal| {
                matches!(values.get(ordinal), Some(Value::Null))
            })
        {
            return Err(LimboError::InvalidArgument(format!(
                "IGNORE writing a bound NULL into a NOT NULL column of {}",
                table.name
            )));
        }
        let rounded = ids_bound_as_whole_numbers(&bound, values);
        let values = rounded.as_deref().unwrap_or(values);
        if bound.rowwise_conflicts() {
            return self
                .execute_auto_increment_conflict_rows(
                    &insert.sql,
                    insert.insert.clone(),
                    table,
                    values,
                    deadline,
                    affected_rows_mode,
                )
                .map(MySqlPreparedExecutionResult::Write);
        }
        if let Some(result) = self.execute_high_water_mixed_insert(
            &insert.sql,
            &bound,
            &table,
            values,
            deadline,
            affected_rows_mode,
        )? {
            return Ok(MySqlPreparedExecutionResult::Write(result));
        }
        let stamped = if insert.insert.upserts() {
            self.moments_an_upsert_stamps(&insert.sql)?
        } else {
            Vec::new()
        };
        let reserved = self.write_counted_rows(
            &insert.sql,
            &bound,
            &table,
            values,
            deadline,
            |reserved, take_numbers| {
                self.check_write_deadline(deadline)?;
                let take_numbers = std::cell::Cell::new(take_numbers);
                let statement = bound
                    .inject_row_ids(&reserved.ids)
                    .map_err(|error| LimboError::ParseError(error.to_string()))?;
                self.write_stamping_the_row_an_upsert_changes(statement, &stamped, |statement| {
                    let reusable = stamped.is_empty() && !self.inner.is_closed();
                    let last = reusable
                        .then(|| {
                            insert
                                .last_engine_statement
                                .lock()
                                .expect("prepared counted INSERT statement mutex poisoned")
                                .take()
                        })
                        .flatten()
                        .filter(|(last_statement, _)| *last_statement == statement);
                    let (statement, mut engine_statement) = match last {
                        Some((statement, mut engine_statement)) => {
                            engine_statement.clear_bindings();
                            (statement, engine_statement)
                        }
                        None => {
                            let options =
                                injected_auto_increment_prepare_options(&table, statement.clone());
                            let engine_statement =
                                self.inner.prepare_translated_stmt_with_options(
                                    statement.clone(),
                                    &insert.sql,
                                    &options,
                                )?;
                            (statement, engine_statement)
                        }
                    };
                    if engine_statement.parameters_count() != insert.parameter_count {
                        return Err(LimboError::InternalError(
                            "prepared AUTO_INCREMENT INSERT changed its parameter count"
                                .to_string(),
                        ));
                    }
                    bind_prepared_values(&mut engine_statement, &reserved.bound_values)?;
                    if let Some(take_numbers) = take_numbers.take() {
                        engine_statement.run_before_writing(&table.name, take_numbers);
                    }
                    let result = (|| -> Result<()> {
                        let timeout = self
                            .remaining_write_timeout(deadline)
                            .map_err(Into::<LimboError>::into)?;
                        run_checked_write_statement(&mut engine_statement, timeout).map_err(
                            |error| self.map_unsigned_decimal_write_error(error, Some(&table.name)),
                        )
                    })();
                    let reset_result = engine_statement.reset();
                    if reusable && result.is_ok() && reset_result.is_ok() {
                        *insert
                            .last_engine_statement
                            .lock()
                            .expect("prepared counted INSERT statement mutex poisoned") =
                            Some((statement, engine_statement));
                    }
                    result.and(reset_result)
                })
            },
        );
        match reserved {
            Err(error) => Err(error),
            Ok(reserved) => {
                let upserted = self.inner.mysql_upserted_rowid();
                let inserted = self.inner.changes() != 0 && upserted == 0;
                if inserted {
                    if let Some(id) = reserved.first_generated {
                        self.inner.set_mysql_last_insert_id(id);
                    }
                }
                let reported_id = if upserted > 0 {
                    if self.inner.mysql_changed_rows() == 0 {
                        0
                    } else {
                        self.id_of_counted_row(&table, upserted)?
                    }
                } else if inserted {
                    reserved
                        .first_generated
                        .or(reserved.last_explicit)
                        .unwrap_or(0)
                } else {
                    0
                };
                Ok(MySqlPreparedExecutionResult::Write(MySqlWriteResult {
                    affected_rows: self.affected_rows(false, affected_rows_mode)?,
                    last_insert_id: reported_id,
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
        if prepared
            .execution_plan
            .select_comparisons()
            .is_some_and(|comparisons| !comparisons.checked_comparisons.is_empty())
        {
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

    /// How long a session waits for a lock another session holds.
    ///
    /// MySQL's `innodb_lock_wait_timeout` defaults to fifty seconds, and a
    /// session that waits that long without getting the lock answers 1205.
    /// The engine waits the same way for the one write lock it holds over the
    /// database.
    pub const DEFAULT_LOCK_WAIT: Duration = Duration::from_secs(50);

    /// Sets how long this session waits for a lock before giving up.
    pub fn set_lock_wait(&self, wait: Duration) {
        self.inner.set_busy_timeout(wait);
    }

    /// Sets how long a statement waits for a table or a database another
    /// session is using, which is MySQL's `lock_wait_timeout`.
    pub fn set_metadata_lock_wait(&self, wait: Duration) {
        *self.metadata_lock_wait.lock().unwrap() = wait;
        self.inner.set_metadata_lock_wait(wait);
    }

    pub fn metadata_lock_wait(&self) -> Duration {
        *self.metadata_lock_wait.lock().unwrap()
    }

    /// Runs a statement that changes a table's definition, waiting for the
    /// tables it changes as long as `lock_wait_timeout` says rather than
    /// `innodb_lock_wait_timeout`.
    ///
    /// Measured on MySQL 8.4.11 with `lock_wait_timeout = 1` and
    /// `innodb_lock_wait_timeout = 30`: `ALTER TABLE`, `DROP TABLE`,
    /// `TRUNCATE TABLE`, `CREATE INDEX` and `RENAME TABLE` on a table another
    /// open transaction read or wrote each answer 1205 after one second, and
    /// go ahead at once while that transaction used only other tables. Under
    /// MVCC the statement first commits what came before it and takes each
    /// table's exclusive metadata lock; under WAL the engine's one write lock
    /// stands for them.
    pub fn waiting_for_metadata_locks<T>(
        &self,
        sql: &str,
        run: impl FnOnce() -> T,
    ) -> std::result::Result<T, MySqlQueryError> {
        let ran = self.with_the_metadata_lock_wait(|| {
            self.lock_the_tables_a_definition_changes(sql)
                .map(|()| run())
        });
        self.inner.release_metadata_locks_outside_a_transaction();
        ran
    }

    fn lock_the_tables_a_definition_changes(
        &self,
        sql: &str,
    ) -> std::result::Result<(), MySqlQueryError> {
        if !self.inner.mvcc_enabled() || self.tables_are_locked() {
            return Ok(());
        }
        let targets = turso_mysql_parser::tables_a_definition_changes(sql, self.parser_mode());
        let mut tables: Vec<String> = targets
            .tables
            .into_iter()
            .map(|named| named.table.to_ascii_lowercase())
            .collect();
        for trigger in &targets.dropped_triggers {
            tables.extend(self.table_of_trigger(&trigger.table)?);
        }
        if tables.is_empty() {
            return Ok(());
        }
        if !self.inner.get_auto_commit() {
            self.run_internal("COMMIT")?;
        }
        let requests: Vec<(&str, MetadataLockMode)> = tables
            .iter()
            .map(|table| (table.as_str(), MetadataLockMode::Exclusive))
            .collect();
        self.inner
            .lock_tables_metadata(&requests)
            .map_err(MySqlQueryError::Engine)
    }

    fn table_of_trigger(
        &self,
        trigger: &str,
    ) -> std::result::Result<Option<String>, MySqlQueryError> {
        let mut statement = self
            .inner
            .prepare("SELECT tbl_name FROM sqlite_schema WHERE type = 'trigger' AND lower(name) = lower(?)")
            .map_err(MySqlQueryError::Engine)?;
        statement
            .bind_at(
                std::num::NonZeroUsize::MIN,
                turso_core::Value::build_text(trigger.to_string()),
            )
            .map_err(MySqlQueryError::Engine)?;
        let rows = statement
            .run_collect_rows()
            .map_err(MySqlQueryError::Engine)?;
        Ok(rows
            .first()
            .and_then(|row| row.first())
            .and_then(|table| table.to_text().map(str::to_ascii_lowercase)))
    }

    fn with_the_metadata_lock_wait<T>(&self, run: impl FnOnce() -> T) -> T {
        let row_lock_wait = self.inner.get_busy_timeout();
        self.inner.set_busy_timeout(self.metadata_lock_wait());
        let restores = RestoresLockWait {
            connection: &self.inner,
            wait: row_lock_wait,
        };
        let result = run();
        drop(restores);
        result
    }

    /// Says whether a row this connection writes has to name a parent that is
    /// there.
    ///
    /// Measured on MySQL 8.4.11: turning it off lets a child row name a parent
    /// that is not there, and turning it back on leaves that row where it is
    /// rather than looking at it again. Both are what the engine's own switch
    /// does.
    pub fn set_foreign_key_checks(&self, enabled: bool) {
        self.inner.set_foreign_keys_enabled(enabled);
    }

    pub fn set_time_zone_offset_seconds(&self, offset: i32) {
        *self.session_time_zone_offset.lock().unwrap() = offset;
    }

    pub fn time_zone_offset_seconds(&self) -> i32 {
        *self.session_time_zone_offset.lock().unwrap()
    }

    /// Sets how many bytes a `GROUP_CONCAT` answers before it is cut, which is
    /// what the session's `group_concat_max_len` says.
    pub fn set_group_concat_max_len(&self, max_len: u64) {
        crate::group_concat::set_max_len(&self.inner, max_len);
    }

    pub fn group_concat_max_len(&self) -> u64 {
        crate::group_concat::max_len(&self.inner)
    }

    /// Forgets what the last statement's `GROUP_CONCAT` calls joined, before
    /// the next one runs. MySQL counts the row a cut names within one
    /// statement.
    pub fn forget_group_concat_cuts(&self) {
        crate::group_concat::forget_progress(&self.inner);
    }

    /// The rows the statement that ran cut a `GROUP_CONCAT` at, in the order
    /// MySQL warns about them, each the row its warning 1260 names.
    pub fn take_group_concat_cuts(&self) -> Vec<u64> {
        crate::group_concat::take_cut_rows(&self.inner)
    }

    pub fn take_foreign_key_refusals(&self) -> Vec<turso_core::ForeignKeyRefusal> {
        self.inner.take_foreign_key_refusals()
    }

    /// Each row the statement that ran let `IGNORE` skip over a key it
    /// collides with, in the order it skipped them, with the key named as
    /// MySQL names it and a `DECIMAL` or `BIGINT UNSIGNED` value read out of
    /// the form the engine keeps it in.
    pub fn take_ignored_duplicates(&self) -> Vec<MySqlIgnoredDuplicate> {
        let duplicates = self.inner.take_ignored_duplicates();
        if duplicates.is_empty() {
            return Vec::new();
        }
        let schema = self.inner.current_schema();
        duplicates
            .into_iter()
            .map(|duplicate| {
                let table = schema.get_btree_table(&duplicate.table);
                let index = duplicate.index.as_deref().and_then(|name| {
                    schema
                        .get_indices(&duplicate.table)
                        .find(|index| index.name == name)
                        .cloned()
                });
                let key_name = match &index {
                    Some(index)
                        if !table.as_ref().is_some_and(|table| {
                            is_the_primary_keys_own_index(index, &table.primary_key_columns)
                        }) =>
                    {
                        mysql_index_name(index)
                    }
                    _ => "PRIMARY".to_owned(),
                };
                let key = duplicate
                    .key
                    .into_iter()
                    .enumerate()
                    .map(|(at, value)| {
                        let declared = index
                            .as_ref()
                            .and_then(|index| index.columns.get(at))
                            .zip(table.as_ref())
                            .and_then(|(column, table)| table.columns().get(column.pos_in_table))
                            .map(|column| column.ty_str.to_ascii_lowercase());
                        key_value_as_mysql_reads_it(value, declared.as_deref())
                    })
                    .collect();
                MySqlIgnoredDuplicate {
                    table: duplicate.table,
                    key_name,
                    key,
                }
            })
            .collect()
    }

    pub fn foreign_key_refusal_message(
        &self,
        database: &str,
        refusal: &turso_core::ForeignKeyRefusal,
    ) -> String {
        let keys_of_the_child_table = self
            .inner
            .current_schema()
            .get_btree_table(&refusal.child_table)
            .map(|table| table.foreign_keys.clone())
            .unwrap_or_else(|| vec![refusal.foreign_key.clone()]);
        crate::show_create_table::foreign_key_refusal_message(
            database,
            refusal,
            &keys_of_the_child_table,
        )
    }

    pub fn take_explained_error(&self) -> Option<String> {
        self.explained_error.lock().unwrap().take()
    }

    /// How many rows the last `SQL_CALC_FOUND_ROWS` statement would have
    /// answered without its `LIMIT`, if the last statement was one. Reading
    /// it forgets it, so a statement that is not one leaves nothing behind.
    pub fn take_found_rows_before_the_limit(&self) -> Option<u64> {
        crate::found_rows::take(&self.inner)
    }

    /// Sets what `ROW_COUNT()` reads in the statements that run next, `None`
    /// where the session does not know it.
    pub fn set_row_count(&self, count: Option<i64>) {
        crate::row_count::set(&self.inner, count);
    }

    /// Whether the statement that ran noted the rows it answers without its
    /// `LIMIT`, left for `take_found_rows_before_the_limit` to read.
    pub fn noted_found_rows_before_the_limit(&self) -> bool {
        crate::found_rows::noted(&self.inner)
    }

    /// Takes the locks `LOCK TABLES` asks for and holds them until `UNLOCK
    /// TABLES`.
    ///
    /// Under MVCC each named table gets MySQL's metadata lock: `READ` (and
    /// `READ LOCAL`, which on InnoDB blocks writers too) the shared read-only
    /// lock, which lets other sessions read the table and makes their writes
    /// wait, and `WRITE` the shared no-read-write lock, which makes their
    /// reads and writes wait. Other tables stay open to everyone. MySQL
    /// commits an open transaction before it locks, and so does this; the
    /// locks belong to a transaction this opens and ends at `UNLOCK TABLES`.
    ///
    /// Under WAL the engine holds one write lock over the whole database, so
    /// a `BEGIN IMMEDIATE` takes it for every table, which locks more than was
    /// asked for.
    pub fn lock_tables(
        &self,
        tables: &[MySqlLockedTable],
    ) -> std::result::Result<(), MySqlQueryError> {
        self.unlock_tables()?;
        if !self.inner.mvcc_enabled() {
            self.with_the_metadata_lock_wait(|| self.run_engine_statement("BEGIN IMMEDIATE"))?;
            *self.tables_locked.lock().unwrap() = true;
            return Ok(());
        }
        if !self.inner.get_auto_commit() {
            self.run_internal("COMMIT")?;
        }
        self.run_engine_statement("BEGIN CONCURRENT")?;
        let requests: Vec<(&str, MetadataLockMode)> = tables
            .iter()
            .map(|locked| {
                let mode = if locked.write {
                    MetadataLockMode::SharedNoReadWrite
                } else {
                    MetadataLockMode::SharedReadOnly
                };
                (locked.table.as_str(), mode)
            })
            .collect();
        if let Err(error) = self.inner.lock_tables_metadata(&requests) {
            self.run_engine_statement("ROLLBACK")?;
            return Err(MySqlQueryError::Engine(error));
        }
        *self.tables_locked.lock().unwrap() = true;
        Ok(())
    }

    /// Lets go of the lock `LOCK TABLES` took.
    ///
    /// MySQL answers an `UNLOCK TABLES` that holds nothing with an OK, which
    /// is what this does.
    pub fn unlock_tables(&self) -> std::result::Result<(), MySqlQueryError> {
        if !std::mem::replace(&mut *self.tables_locked.lock().unwrap(), false) {
            return Ok(());
        }
        self.run_engine_statement("COMMIT")
    }

    /// Reports whether this session is holding the lock `LOCK TABLES` took.
    pub fn tables_are_locked(&self) -> bool {
        *self.tables_locked.lock().unwrap()
    }

    fn run_engine_statement(&self, sql: &str) -> std::result::Result<(), MySqlQueryError> {
        self.inner
            .prepare(sql)
            .and_then(|mut statement| statement.run_ignore_rows())
            .map_err(MySqlQueryError::Engine)
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
    ) -> std::result::Result<MySqlTransactionOutcome, MySqlQueryError> {
        let command =
            parse_transaction_command(sql, self.parser_mode()).map_err(mysql_query_parse_error)?;
        *self.transaction_command_ran.lock().unwrap() = match command {
            MySqlTransactionCommand::Savepoint(_)
            | MySqlTransactionCommand::RollbackToSavepoint(_)
            | MySqlTransactionCommand::ReleaseSavepoint(_) => TransactionCommandRan::Savepoint,
            _ => TransactionCommandRan::BeganATransaction,
        };
        // The lock `LOCK TABLES` took is held by the transaction it opened, so
        // ending that transaction would let go of a lock the client believes
        // it still holds. MySQL keeps the two apart; this keeps them together,
        // and says so rather than dropping the lock quietly.
        if self.tables_are_locked() {
            return Err(MySqlQueryError::Unsupported(
                "a transaction cannot be started or ended while tables are locked".to_string(),
            ));
        }
        if let MySqlTransactionCommand::Savepoint(_)
        | MySqlTransactionCommand::RollbackToSavepoint(_)
        | MySqlTransactionCommand::ReleaseSavepoint(_) = &command
        {
            self.execute_savepoint_command(&command, sql)?;
            return Ok(MySqlTransactionOutcome::default());
        }
        match command {
            MySqlTransactionCommand::Begin
            | MySqlTransactionCommand::BeginReadOnly
            | MySqlTransactionCommand::BeginReadWrite
            | MySqlTransactionCommand::BeginWithConsistentSnapshot
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
                return Ok(MySqlTransactionOutcome::default());
            }
            // The chaining forms end a transaction and begin another at once.
            // Measured on MySQL 8.4.11: they leave the session in a transaction
            // even when autocommit is on and there was none to end, so the
            // ending half is skipped rather than the whole statement.
            MySqlTransactionCommand::CommitAndChain | MySqlTransactionCommand::RollbackAndChain
                if self.inner.get_auto_commit() =>
            {
                *self.read_only_transaction.lock().unwrap() = self.session_read_only();
                self.begin_transaction_isolation();
                self.run_transaction_statement(self.engine_begin(), sql)?;
                return Ok(MySqlTransactionOutcome::default());
            }
            _ => {}
        }
        // Measured on MySQL 8.4.11: a transaction begun without saying takes
        // the session's access mode, `READ ONLY` and `READ WRITE` override
        // it, and a chained transaction keeps the mode of the one it follows.
        let read_only = match command {
            MySqlTransactionCommand::BeginReadOnly => true,
            MySqlTransactionCommand::BeginReadWrite => false,
            MySqlTransactionCommand::Begin
            | MySqlTransactionCommand::BeginWithConsistentSnapshot => self.session_read_only(),
            MySqlTransactionCommand::CommitAndChain | MySqlTransactionCommand::RollbackAndChain => {
                *self.read_only_transaction.lock().unwrap()
            }
            _ => false,
        };
        *self.read_only_transaction.lock().unwrap() = read_only;
        let statement = match command {
            MySqlTransactionCommand::Begin
            | MySqlTransactionCommand::BeginReadOnly
            | MySqlTransactionCommand::BeginReadWrite
            | MySqlTransactionCommand::BeginWithConsistentSnapshot => {
                self.begin_transaction_isolation();
                self.engine_begin()
            }
            MySqlTransactionCommand::Commit | MySqlTransactionCommand::CommitAndChain => {
                Stmt::Commit { name: None }
            }
            MySqlTransactionCommand::Rollback | MySqlTransactionCommand::RollbackAndChain => {
                Stmt::Rollback {
                    tx_name: None,
                    savepoint_name: None,
                }
            }
            MySqlTransactionCommand::Savepoint(_)
            | MySqlTransactionCommand::RollbackToSavepoint(_)
            | MySqlTransactionCommand::ReleaseSavepoint(_) => {
                unreachable!("a savepoint command is answered before this point")
            }
        };
        let chains = matches!(
            command,
            MySqlTransactionCommand::CommitAndChain | MySqlTransactionCommand::RollbackAndChain
        );
        if chains {
            self.run_transaction_statement(statement, sql)?;
            self.run_transaction_statement(self.engine_begin(), sql)?;
            return Ok(MySqlTransactionOutcome::default());
        }
        self.run_transaction_statement(statement, sql)?;
        if command == MySqlTransactionCommand::BeginWithConsistentSnapshot {
            return self.begin_consistent_snapshot();
        }
        Ok(MySqlTransactionOutcome::default())
    }

    /// Runs one of the three savepoint statements.
    ///
    /// Measured on MySQL 8.4.11 with autocommit on and no transaction open:
    /// `SAVEPOINT s1` answers OK and nothing survives it, the statement being
    /// its own transaction, so a `ROLLBACK TO s1` on the next line answers
    /// 1305. The engine instead opens a transaction for a bare `SAVEPOINT` and
    /// leaves it open across statements, so nothing is run there. With
    /// autocommit off the savepoint does survive — measured, it rolls back an
    /// `INSERT` written after it — so the implicit transaction is opened first,
    /// the way a write opens one.
    ///
    /// The read-only flag is left alone on purpose: a savepoint does not settle
    /// what the next transaction is, and a `START TRANSACTION READ ONLY` is
    /// still in force after one.
    ///
    /// The engine takes the transaction's read snapshot to open a savepoint.
    /// MySQL takes no read view for one: measured on 8.4.11, a transaction
    /// that begins with `SAVEPOINT` still sees a row another session commits
    /// after it. So a transaction that had not read lets the snapshot go
    /// again, and takes one at its first read. That is safe with the savepoint
    /// open, as it is for every `READ COMMITTED` statement: the savepoint
    /// keeps the pages the transaction changes from their first change after
    /// it and its place in the WAL from the transaction's first write, both
    /// taken on the snapshot the transaction writes from.
    fn execute_savepoint_command(
        &self,
        command: &MySqlTransactionCommand,
        sql: &str,
    ) -> std::result::Result<(), MySqlQueryError> {
        self.begin_implicit_transaction_for_write()?;
        let outside_a_transaction = self.inner.get_auto_commit();
        let statement = match command {
            MySqlTransactionCommand::Savepoint(name) => {
                if outside_a_transaction {
                    return Ok(());
                }
                Stmt::Savepoint {
                    name: turso_parser::ast::Name::exact(name.clone()),
                }
            }
            MySqlTransactionCommand::RollbackToSavepoint(name) => {
                if outside_a_transaction {
                    return Err(MySqlQueryError::NoSuchSavepoint);
                }
                Stmt::Rollback {
                    tx_name: None,
                    savepoint_name: Some(turso_parser::ast::Name::exact(name.clone())),
                }
            }
            MySqlTransactionCommand::ReleaseSavepoint(name) => {
                if outside_a_transaction {
                    return Err(MySqlQueryError::NoSuchSavepoint);
                }
                Stmt::Release {
                    name: turso_parser::ast::Name::exact(name.clone()),
                }
            }
            _ => unreachable!("only a savepoint command reaches this"),
        };
        let unread = !self.inner.has_read_snapshot() && !self.inner.mvcc_enabled();
        let result = self
            .run_transaction_statement(statement, sql)
            .map_err(no_such_savepoint_error);
        if unread {
            self.inner
                .release_read_snapshot()
                .map_err(MySqlQueryError::Engine)?;
        }
        result
    }

    /// The engine statement a MySQL `BEGIN` runs.
    ///
    /// In MVCC mode a plain `BEGIN` writes under the database's one exclusive
    /// write slot, so writers would still run one at a time; `BEGIN
    /// CONCURRENT` lets them run side by side, and row locks keep each
    /// isolation level.
    fn engine_begin(&self) -> Stmt {
        Stmt::Begin {
            typ: self
                .begins_concurrently()
                .then_some(turso_parser::ast::TransactionType::Concurrent),
            name: None,
        }
    }

    fn engine_begin_sql(&self) -> &'static str {
        if self.begins_concurrently() {
            "BEGIN CONCURRENT"
        } else {
            "BEGIN"
        }
    }

    fn begins_concurrently(&self) -> bool {
        self.inner.mvcc_enabled()
    }

    /// Runs a write that autocommit makes a transaction of its own inside
    /// `BEGIN CONCURRENT` in MVCC mode, where the engine would otherwise run
    /// it under the one exclusive write slot.
    fn in_a_concurrent_statement_transaction<T, E>(
        &self,
        run: impl FnOnce() -> std::result::Result<T, E>,
        engine_error: impl Fn(LimboError) -> E,
    ) -> std::result::Result<T, E> {
        if !self.inner.mvcc_enabled() || !self.inner.get_auto_commit() || !self.session_autocommit()
        {
            return run();
        }
        self.begin_transaction_isolation();
        if !self.begins_concurrently() {
            return run();
        }
        // The statement is a transaction of its own, so a read-only flag an
        // earlier `START TRANSACTION READ ONLY` left does not hold for it.
        *self.read_only_transaction.lock().unwrap() = self.session_read_only();
        self.run_engine_transaction_statement(self.engine_begin(), "BEGIN CONCURRENT")
            .map_err(&engine_error)?;
        let result = run();
        let ended = if result.is_ok() {
            self.run_engine_transaction_statement(Stmt::Commit { name: None }, "COMMIT")
        } else {
            Ok(())
        };
        if !self.inner.get_auto_commit() {
            let _ = self.inner.execute("ROLLBACK");
        }
        ended.map_err(engine_error)?;
        result
    }

    fn run_engine_transaction_statement(&self, statement: Stmt, sql: &str) -> Result<()> {
        match self.run_transaction_statement(statement, sql) {
            Ok(()) => Ok(()),
            Err(MySqlQueryError::Engine(error)) => Err(error),
            Err(error) => unreachable!("a transaction statement fails only in the engine: {error}"),
        }
    }

    fn run_transaction_statement(
        &self,
        statement: Stmt,
        sql: &str,
    ) -> std::result::Result<(), MySqlQueryError> {
        const KEPT_TRANSACTION_STATEMENTS: usize = 8;
        let keepable = !self.inner.is_closed()
            && matches!(
                statement,
                Stmt::Begin { .. }
                    | Stmt::Commit { .. }
                    | Stmt::Rollback {
                        savepoint_name: None,
                        ..
                    }
            );
        if !keepable {
            return self
                .inner
                .prepare_translated_stmt(statement, sql)
                .and_then(|mut statement| statement.run_ignore_rows())
                .map_err(MySqlQueryError::Engine);
        }
        let kept = {
            let mut kept = self
                .prepared_transaction_statements
                .lock()
                .expect("MySQL transaction statements mutex poisoned");
            kept.iter()
                .position(|(kept, kept_sql, _)| *kept == statement && kept_sql == sql)
                .map(|at| kept.remove(at).2)
        };
        let mut engine_statement = match kept {
            Some(engine_statement) => {
                self.inner.maybe_update_schema();
                engine_statement
            }
            None => self
                .inner
                .prepare_translated_stmt_with_options(
                    statement.clone(),
                    sql,
                    &PrepareOptions::default().with_reprepare_parser(Arc::new(
                        FrozenTransactionStatementParser {
                            statement: statement.clone(),
                        },
                    )),
                )
                .map_err(MySqlQueryError::Engine)?,
        };
        engine_statement
            .run_ignore_rows()
            .map_err(MySqlQueryError::Engine)?;
        if engine_statement.reset().is_ok() {
            let mut kept = self
                .prepared_transaction_statements
                .lock()
                .expect("MySQL transaction statements mutex poisoned");
            if kept.len() == KEPT_TRANSACTION_STATEMENTS {
                kept.remove(0);
            }
            kept.push((statement, sql.to_owned(), engine_statement));
        }
        Ok(())
    }

    /// Whether a statement runs read-only: inside a transaction, by that
    /// transaction's access mode, and outside one by the session's, which the
    /// transaction a statement begins takes.
    pub fn runs_read_only(&self, session_read_only: bool) -> bool {
        if self.inner.get_auto_commit() {
            session_read_only
        } else {
            *self.read_only_transaction.lock().unwrap()
        }
    }

    fn session_read_only(&self) -> bool {
        *self.session_read_only.lock().unwrap()
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
        self.set_time_zone_offset_seconds(0);
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
        if !self.inner.get_auto_commit() {
            return Ok(());
        }
        // With autocommit on the statement is a transaction of its own, which
        // uses up a level set for the next transaction just as a `BEGIN`
        // does. Measured on MySQL 8.4.11: a statement reading a table uses it
        // up, while `SELECT 1` and a statement that fails first do not.
        self.begin_transaction_isolation();
        if self.session_autocommit() {
            return Ok(());
        }
        // Measured on MySQL 8.4.11: with autocommit off, the transaction the
        // first statement reading a table begins keeps the session's access
        // mode even when the session changes it before the transaction ends.
        *self.read_only_transaction.lock().unwrap() = self.session_read_only();
        self.inner
            .prepare(self.engine_begin_sql())
            .and_then(|mut statement| statement.run_ignore_rows())
            .map_err(MySqlQueryError::Engine)
    }

    fn begin_implicit_transaction_for_table_read(
        &self,
    ) -> std::result::Result<(), MySqlQueryError> {
        self.begin_implicit_transaction_for_write()
    }

    /// The lock a plain `SELECT` of a table takes inside a `SERIALIZABLE`
    /// transaction in MVCC mode: `FOR SHARE` on every table it reads, as
    /// InnoDB takes. Measured on MySQL 8.4.11, the same `SELECT` with
    /// autocommit on is a consistent read that locks nothing.
    fn serializable_read_of_a_table(&self, reads_table: bool) -> Option<MySqlLockingRead> {
        let locks = reads_table
            && self.inner.mvcc_enabled()
            && !self.inner.get_auto_commit()
            && self.transaction_isolation() == MySqlIsolationLevel::Serializable;
        locks.then_some(MySqlLockingRead {
            shared: true,
            wait: MySqlRowLockWait::Wait,
        })
    }

    /// Takes the write lock a `SELECT ... FOR UPDATE` asked for.
    ///
    /// The engine holds one write lock over the whole database and takes it
    /// when a statement writes, so a statement that writes no row takes the
    /// lock without changing anything. `BEGIN IMMEDIATE` would take it too,
    /// but only where no transaction is open yet, and the statement asking for
    /// the lock is usually inside one already.
    ///
    /// Outside a transaction the lock would end with the statement that took
    /// it, which is what MySQL's does too, so none is taken there.
    fn take_the_write_lock(
        &self,
        sources: &[MySqlSelectSource],
    ) -> std::result::Result<(), MySqlQueryError> {
        if self.inner.get_auto_commit() {
            return Ok(());
        }
        let schema = self.inner.current_schema();
        let locked = sources.iter().find_map(|source| {
            let table = schema.get_table(source.table().as_str())?;
            let column = table.columns().first()?.name.clone()?;
            Some((source.table().as_str().to_owned(), column))
        });
        let Some((table, column)) = locked else {
            return Err(MySqlQueryError::Unsupported(
                "SELECT ... FOR UPDATE requires a table whose rows can be locked".to_string(),
            ));
        };
        let table = quoted_engine_name(&table);
        let column = quoted_engine_name(&column);
        self.inner
            .prepare(format!("UPDATE {table} SET {column} = {column} WHERE 0"))
            .and_then(|mut statement| statement.run_ignore_rows())
            .map_err(MySqlQueryError::Engine)
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
        self.prepare_with_index_origin(sql, false)
    }

    fn prepare_with_index_origin(&self, sql: &str, implicit_index: bool) -> Result<Statement> {
        self.prepare_schema_with_creator(sql, implicit_index, None)
    }

    fn prepare_schema_with_creator(
        &self,
        sql: &str,
        implicit_index: bool,
        creator: Option<SchemaSqlCreator>,
    ) -> Result<Statement> {
        let mode = self.parser_mode();
        if let Ok(checked) = parse_checked_primary_key_create_table(sql, mode) {
            return self.prepare_checked_primary_key_create_table(checked);
        }
        let mut stmt = match parse_schema_ddl_ast(sql, mode) {
            Ok(stmt) => stmt,
            Err(MySqlParseError::Unsupported {
                feature: "schema statement",
            }) => return self.prepare_non_schema(sql),
            Err(error) => {
                // A join's column written without its table names the table
                // that declares it, which only the tables' columns tell; the
                // text MySQL prints names it.
                if let Ok(Some(written)) =
                    turso_mysql_parser::view_written_as_mysql_prints_it(sql, mode, &|table| {
                        self.declared_column_names(table)
                    })
                {
                    if written != sql {
                        return self.prepare_schema_with_creator(&written, implicit_index, creator);
                    }
                }
                match parse_auto_increment_create_table(sql, mode) {
                    Ok(checked) => return self.prepare_auto_increment_create_table(checked),
                    Err(_) => return Err(LimboError::ParseError(error.to_string())),
                }
            }
        };
        if matches!(stmt, Stmt::CreateView { .. }) {
            if let Some(written) =
                turso_mysql_parser::view_written_as_mysql_prints_it(sql, mode, &|table| {
                    self.declared_column_names(table)
                })
                .map_err(|error| LimboError::ParseError(error.to_string()))?
            {
                // The view is made from, and kept as, the text MySQL prints,
                // so what `SHOW CREATE VIEW` prints is what was stored.
                if written != sql {
                    return self.prepare_schema_with_creator(&written, implicit_index, creator);
                }
                self.check_view_select(&written)?;
            }
        }
        if let Stmt::AlterTable(alter) = &stmt {
            self.reject_alter_with_marked_trigger()?;
            self.reject_alter_with_marked_view(&alter.body)?;
            self.reject_alter_over_a_primary_key_column(&alter.name.name, &alter.body)?;
            self.reject_alter_changing_a_decimal(&alter.name.name, &alter.body)?;
        }
        if matches!(stmt, Stmt::CreateTrigger { .. }) {
            let written = turso_mysql_parser::trigger_written_as_mysql_keeps_it(sql, mode)
                .map_err(|error| LimboError::ParseError(error.to_string()))?
                .ok_or_else(|| {
                    LimboError::ParseError("a CREATE TRIGGER that reads as none".to_string())
                })?;
            // The trigger is made from, and kept as, its header written the
            // way MySQL prints it and its body as it was written.
            if written != sql {
                return self.prepare_schema_with_creator(&written, implicit_index, creator);
            }
            self.check_trigger_body(&written)?;
            self.reject_a_second_trigger_for_one_event(&stmt)?;
        }
        if let Stmt::CreateIndex {
            unique,
            idx_name,
            tbl_name,
            columns,
            ..
        } = &mut stmt
        {
            let table = tbl_name.as_str();
            let logical_name = idx_name.name.as_str();
            MySqlTableName::parse(logical_name)
                .map_err(|error| LimboError::ParseError(error.to_string()))?;
            let indexed_columns = columns
                .iter()
                .filter_map(|column| match column.expr.as_ref() {
                    Expr::Id(name) => Some(name.as_str().to_owned()),
                    _ => None,
                })
                .collect::<Vec<_>>();
            if self.index_targets_json(table, &indexed_columns)? {
                return Err(LimboError::ParseError(
                    "JSON column cannot be indexed directly".to_string(),
                ));
            }
            if the_index_named(&self.inner.current_schema(), table, logical_name).is_some() {
                return Err(LimboError::ParseError(format!(
                    "Duplicate key name '{logical_name}'"
                )));
            }
            let primary_key = if *unique {
                Vec::new()
            } else {
                primary_key_a_plain_index_ends_with(&self.inner.current_schema(), table)
            };
            columns.extend(primary_key.iter().map(|column| SortedColumn {
                expr: Box::new(Expr::Id(turso_parser::ast::Name::exact(column.clone()))),
                order: None,
                nulls: None,
            }));
            idx_name.name = turso_parser::ast::Name::exact(physical_mysql_index_name(
                logical_name,
                StoredIndexKind {
                    implicit: implicit_index,
                    ends_with_primary_key: !primary_key.is_empty(),
                },
            )?);
        }
        let input = match &stmt {
            // The engine's table has no collation of its own, so the one the
            // statement declares is written after what the engine keeps.
            Stmt::CreateTable { .. } => render_create_table_mysql_with_mode(&stmt, mode)
                .and_then(|rendered| {
                    let options = turso_mysql_parser::table_options_of(sql, mode)?;
                    Ok(format!("{rendered}{}", options.written()))
                })
                .map_err(|error| LimboError::ParseError(error.to_string()))?,
            Stmt::CreateIndex { .. } => render_create_index_mysql_with_mode(&stmt, mode)
                .map_err(|error| LimboError::ParseError(error.to_string()))?,
            Stmt::CreateView { .. }
                if turso_mysql_parser::translated_view_is_kept_as_mysql_prints_it(&stmt) =>
            {
                sql.to_string()
            }
            Stmt::CreateView { .. } => render_create_view_mysql_with_mode(&stmt, mode)
                .map_err(|error| LimboError::ParseError(error.to_string()))?,
            Stmt::CreateTrigger { .. } | Stmt::AlterTable(_) => sql.to_string(),
            _ => unreachable!("MySQL schema parser returned an unsupported statement"),
        };
        let formatter: Arc<dyn SchemaSqlFormatter> = match creator {
            Some(mut creator)
                if matches!(&stmt, Stmt::CreateView { .. } | Stmt::CreateTrigger { .. }) =>
            {
                // Measured on MySQL 8.4.11: `SHOW TRIGGERS` reports the
                // collation the database had when the trigger was made, and
                // keeps reporting it after an `ALTER DATABASE`. A view
                // reports none.
                if matches!(&stmt, Stmt::CreateTrigger { .. }) {
                    self.database_collation()
                        .name()
                        .clone_into(&mut creator.database_collation);
                }
                Arc::new(CreatorSchemaSqlFormatter {
                    context: self.schema_context,
                    creator,
                })
            }
            _ => Arc::new(self.schema_context),
        };
        let options = PrepareOptions::default()
            .with_reprepare_parser(Arc::new(FrozenSchemaDdlParser { mode }))
            .with_schema_sql_formatter(formatter);
        self.inner
            .prepare_translated_stmt_with_options(stmt, &input, &options)
    }

    /// Holds a view's `SELECT` to every rule the same `SELECT` written on its
    /// own is held to.
    ///
    /// The view is translated without its columns' types, as it is each time
    /// the database is opened, so a `SELECT` that renders differently once
    /// they are known — a written day against a moment, a word against a
    /// `JSON` column — is refused rather than kept in its untyped form.
    fn check_view_select(&self, written: &str) -> Result<()> {
        let select = turso_mysql_parser::translated_view_select(written, self.parser_mode())
            .map_err(|error| LimboError::ParseError(error.to_string()))?;
        let readings = turso_mysql_parser::written_view_columns(written, self.parser_mode())
            .map_err(|error| LimboError::ParseError(error.to_string()))?;
        if readings.grouped() {
            let columns = self
                .list_shared_columns(readings.table())
                .map_err(|error| {
                    LimboError::ParseError(format!("CREATE VIEW source columns: {error:?}"))
                })?;
            Self::refuse_readings_not_measured(&readings, &columns).map_err(|_| {
                LimboError::ParseError(
                    "a view grouping its rows reads only COUNT, and MIN or MAX of a whole number or a VARCHAR"
                        .to_string(),
                )
            })?;
        }
        let (translated, rendered_differently) = self.parse_select_knowing_column_types(&select)?;
        if rendered_differently {
            return Err(LimboError::ParseError(
                "a view whose SELECT reads differently once its columns' types are known"
                    .to_string(),
            ));
        }
        Self::reject_internal_catalog_select(&translated)?;
        self.reject_binary_scalar_collation(&translated)?;
        self.refuse_select_json_readings_of_other_columns(&translated)?;
        // How MySQL writes a view out and reads its columns back when its keys
        // decide a column beside them has not been measured.
        if translated.columns_the_keys_decide().is_some() {
            return Err(LimboError::ParseError(
                "a view projecting a column its grouping keys decide".to_string(),
            ));
        }
        Self::reject_raw_select_comparisons(&translated)?;
        self.validate_select_comparison_columns(
            translated.source_tables(),
            translated.checked_comparisons(),
        )?;
        self.validate_select_subquery_comparison_columns(
            translated.source_table(),
            translated.source_tables(),
            translated.checked_subquery_comparisons(),
        )
    }

    /// Creates a view or trigger while retaining the authenticated creator.
    pub fn execute_schema_object_ddl_with_creator(
        &self,
        sql: &str,
        creator: SchemaSqlCreator,
    ) -> std::result::Result<(), MySqlQueryError> {
        if let Some(dump_ddl) = turso_mysql_parser::parse_optional_mysqldump_ddl(sql)
            .map_err(|error| MySqlQueryError::Unsupported(error.to_string()))?
        {
            if let Some(definer) = dump_ddl.definer() {
                if definer != creator.username.as_str() {
                    return Err(MySqlQueryError::Unsupported(
                        "mysqldump DEFINER must match the authenticated creator".to_owned(),
                    ));
                }
            }
        }
        let mut statement = self
            .prepare_schema_with_creator(sql, false, Some(creator))
            .map_err(MySqlQueryError::Engine)?;
        if !self.inner.get_auto_commit() {
            self.inner
                .prepare("COMMIT")
                .and_then(|mut commit| commit.run_ignore_rows())
                .map_err(MySqlQueryError::Engine)?;
        }
        let result = statement.run_ignore_rows().map_err(MySqlQueryError::Engine);
        drop(statement);
        if !self.inner.get_auto_commit() {
            self.inner
                .prepare("ROLLBACK")
                .and_then(|mut rollback| rollback.run_ignore_rows())
                .map_err(MySqlQueryError::Engine)?;
        }
        result
    }

    /// Writes a view again, as `CREATE OR REPLACE VIEW` and `ALTER VIEW` do.
    ///
    /// Measured on MySQL 8.4.11: either answers 1347 for a table of that name,
    /// `ALTER VIEW` answers 1146 for a name nothing has, and `CREATE OR
    /// REPLACE VIEW` makes the view then. The old view is dropped and the new
    /// one made inside one transaction, so a body the checked `CREATE VIEW`
    /// refuses leaves the old view standing.
    pub fn replace_view(
        &self,
        replacement: &MySqlViewReplacement,
        creator: Option<SchemaSqlCreator>,
    ) -> std::result::Result<(), MySqlReplaceViewError> {
        let engine = |error| MySqlReplaceViewError::Query(MySqlQueryError::Engine(error));
        let mode = self.parser_mode();
        parse_create_view_ast(replacement.create_view(), mode)
            .or_else(|error| {
                // A join's column written without its table is read in the
                // text MySQL prints, which names the table.
                match turso_mysql_parser::view_written_as_mysql_prints_it(
                    replacement.create_view(),
                    mode,
                    &|table| self.declared_column_names(table),
                ) {
                    Ok(Some(written)) => parse_create_view_ast(&written, mode),
                    _ => Err(error),
                }
            })
            .map_err(|error| {
                MySqlReplaceViewError::Query(MySqlQueryError::Unsupported(error.to_string()))
            })?;
        if !self.inner.get_auto_commit() {
            self.inner
                .prepare("COMMIT")
                .and_then(|mut statement| statement.run_ignore_rows())
                .map_err(engine)?;
        }
        let tables = self.list_tables().map_err(engine)?;
        let replaced = match tables
            .iter()
            .find(|table| table.name() == replacement.view().as_str())
        {
            Some(table) if table.kind() != MySqlTableKind::View => {
                return Err(MySqlReplaceViewError::NotView);
            }
            Some(_) => true,
            None if replacement.requires_the_view() => {
                return Err(MySqlReplaceViewError::MissingView);
            }
            None => false,
        };
        self.inner
            .prepare("BEGIN")
            .and_then(|mut statement| statement.run_ignore_rows())
            .map_err(engine)?;
        let written = self.drop_and_create_view(replacement, replaced, creator);
        let finish = if written.is_ok() {
            "COMMIT"
        } else {
            "ROLLBACK"
        };
        self.inner
            .prepare(finish)
            .and_then(|mut statement| statement.run_ignore_rows())
            .map_err(engine)?;
        written.map_err(engine)
    }

    fn drop_and_create_view(
        &self,
        replacement: &MySqlViewReplacement,
        replaced: bool,
        creator: Option<SchemaSqlCreator>,
    ) -> Result<()> {
        if replaced {
            let name = replacement.view().as_str();
            let stmt = Stmt::DropView {
                if_exists: false,
                view_name: turso_parser::ast::QualifiedName::single(
                    turso_parser::ast::Name::exact(name.to_owned()),
                ),
            };
            let sql = format!("DROP VIEW \"{}\"", name.replace('"', "\"\""));
            self.inner
                .prepare_translated_stmt(stmt, &sql)
                .and_then(|mut statement| statement.run_ignore_rows())?;
        }
        self.prepare_schema_with_creator(replacement.create_view(), false, creator)
            .and_then(|mut statement| statement.run_ignore_rows())
    }

    /// Executes one checked schema statement with MySQL implicit-commit semantics.
    pub fn execute_schema_ddl(&self, sql: &str) -> std::result::Result<(), MySqlQueryError> {
        if let Some(written) = self.with_the_database_collation(sql)? {
            return self.execute_schema_ddl(&written);
        }
        match self.column_default_an_alter_changes(sql)? {
            Some(turso_mysql_parser::MySqlColumnDefaultChange::Restated(restated)) => {
                return self.execute_schema_ddl(&restated);
            }
            Some(turso_mysql_parser::MySqlColumnDefaultChange::NoSuchColumn(name)) => {
                return Err(MySqlQueryError::Engine(LimboError::NoSuchColumn { name }));
            }
            None => {}
        }
        if let Some(collated) = self.with_the_table_collation_on_each_text_column(sql)? {
            return self.execute_schema_ddl(&collated);
        }
        if let Some((table, change)) = self.key_an_alter_changes(sql)? {
            return self.change_the_key(&table, change);
        }
        match self.column_an_alter_places(sql)? {
            Some(turso_mysql_parser::MySqlColumnPlacement::TableWrittenAgain(rewrite)) => {
                return self.write_the_table_again_as(
                    turso_mysql_parser::alter_table_target(sql, self.parser_mode())
                        .unwrap_or_default()
                        .as_str(),
                    &rewrite,
                );
            }
            Some(turso_mysql_parser::MySqlColumnPlacement::AlreadyAtTheEnd(written)) => {
                return self.execute_schema_ddl(&written);
            }
            Some(turso_mysql_parser::MySqlColumnPlacement::NoSuchColumn(name)) => {
                return Err(MySqlQueryError::Engine(LimboError::NoSuchColumn { name }));
            }
            Some(turso_mysql_parser::MySqlColumnPlacement::DuplicateColumn(name)) => {
                return Err(MySqlQueryError::DuplicateColumn(name));
            }
            None => {}
        }
        if let Some((table, change)) = self.check_an_alter_changes(sql)? {
            return match change {
                turso_mysql_parser::MySqlCheckChange::TableWrittenAgain(rewrite) => {
                    self.write_the_table_again_as(&table, &rewrite)
                }
                turso_mysql_parser::MySqlCheckChange::NoSuchCheck(name) => {
                    Err(MySqlQueryError::NoSuchCheck(name))
                }
                turso_mysql_parser::MySqlCheckChange::DuplicateName(name) => {
                    Err(MySqlQueryError::DuplicateCheckName(name))
                }
            };
        }
        if let Some(statements) = self.expanded_alter_table(sql)? {
            return self.execute_expanded_alter_table(&statements);
        }
        if self.added_foreign_key_table(sql).is_some() {
            return self.execute_expanded_alter_table(&[sql.to_owned()]);
        }
        if let Ok(Stmt::CreateIndex {
            idx_name,
            tbl_name,
            columns,
            ..
        }) = parse_schema_ddl_ast(sql, self.parser_mode())
        {
            let indexed_columns = columns
                .iter()
                .filter_map(|column| match column.expr.as_ref() {
                    Expr::Id(name) => Some(name.as_str().to_owned()),
                    _ => None,
                })
                .collect::<Vec<_>>();
            if self
                .index_targets_json(tbl_name.as_str(), &indexed_columns)
                .map_err(MySqlQueryError::Engine)?
            {
                return Err(MySqlQueryError::JsonIndex);
            }
            if the_index_named(
                &self.inner.current_schema(),
                tbl_name.as_str(),
                idx_name.name.as_str(),
            )
            .is_some()
            {
                return Err(MySqlQueryError::DuplicateIndex);
            }
            return self.execute_expanded_alter_table(&[sql.to_owned()]);
        }
        let counter_start = self.counter_start_of_a_new_table(sql)?;

        let mut statement = match self.prepare(sql) {
            Ok(statement) => statement,
            Err(error) => {
                if !self.inner.get_auto_commit() {
                    self.inner
                        .prepare("COMMIT")
                        .and_then(|mut statement| statement.run_ignore_rows())
                        .map_err(MySqlQueryError::Engine)?;
                }
                return Err(self.json_schema_prepare_error(sql, error));
            }
        };
        if !self.inner.get_auto_commit() {
            self.inner
                .prepare("COMMIT")
                .and_then(|mut statement| statement.run_ignore_rows())
                .map_err(MySqlQueryError::Engine)?;
        }
        let result = match &counter_start {
            Some((table, start)) => {
                self.create_a_table_counting_from(&mut statement, table, *start)
            }
            None => statement
                .run_ignore_rows()
                .map(|_| ())
                .map_err(MySqlQueryError::Engine),
        };
        drop(statement);
        if !self.inner.get_auto_commit() {
            self.inner
                .prepare("ROLLBACK")
                .and_then(|mut statement| statement.run_ignore_rows())
                .map_err(MySqlQueryError::Engine)?;
        }
        result
    }

    /// Makes a table whose first row is to take `start`, moving its counter
    /// in the transaction that makes it, so no crash leaves the table there
    /// with its counter still at one.
    fn create_a_table_counting_from(
        &self,
        create: &mut Statement,
        table: &str,
        start: u64,
    ) -> std::result::Result<(), MySqlQueryError> {
        self.run_internal("BEGIN")?;
        let made = create
            .run_ignore_rows()
            .map_err(MySqlQueryError::Engine)
            .and_then(|_| self.start_the_counter(table, start));
        if made.is_err() {
            self.run_internal("ROLLBACK")?;
            return made;
        }
        self.run_internal("COMMIT")?;
        crash_point(CrashPoint::SchemaChangeCommitted);
        Ok(())
    }

    fn json_schema_prepare_error(&self, sql: &str, error: LimboError) -> MySqlQueryError {
        match parse_schema_ddl_ast(sql, self.parser_mode()) {
            Err(MySqlParseError::JsonLiteralDefault) => MySqlQueryError::JsonLiteralDefault,
            Err(MySqlParseError::JsonIndex) => MySqlQueryError::JsonIndex,
            _ => MySqlQueryError::Engine(error),
        }
    }

    /// The table a `CREATE TABLE ... AUTO_INCREMENT=<n>` is about to make, and
    /// the number its first row takes, where the statement is one.
    ///
    /// Read before the statement runs, because what says the counter is this
    /// statement's to move is that the table was not there beforehand:
    /// measured on MySQL 8.4.11, a `CREATE TABLE IF NOT EXISTS` naming a start
    /// leaves the counter of the table it finds exactly where it stood.
    fn counter_start_of_a_new_table(
        &self,
        sql: &str,
    ) -> std::result::Result<Option<(String, u64)>, MySqlQueryError> {
        let Ok(checked) = parse_auto_increment_create_table(sql, self.parser_mode()) else {
            return Ok(None);
        };
        let start = checked.starts_the_counter_at;
        let Some(start) = start else {
            return Ok(None);
        };
        let Ok(name) = MySqlTableName::parse(&checked.table_name) else {
            return Ok(None);
        };
        if self.names_a_table(&name).map_err(MySqlQueryError::Engine)? {
            return Ok(None);
        }
        Ok(Some((checked.table_name, start)))
    }

    /// Raises a freshly created table's counter so its first row takes `start`.
    ///
    /// The allocator counts the numbers already handed out, so a table whose
    /// first row is to be `start` has had `start - 1` of them. The parser
    /// reads 0 and 1 as no start at all, which is where the counter already
    /// stands, so the subtraction below always leaves a mark of at least one.
    fn start_the_counter(
        &self,
        table: &str,
        start: u64,
    ) -> std::result::Result<(), MySqlQueryError> {
        let Some(table) = self
            .load_auto_increment_table(table)
            .map_err(MySqlQueryError::Engine)?
        else {
            return Err(MySqlQueryError::Engine(LimboError::InternalError(
                "AUTO_INCREMENT table has no definition after it was created".to_string(),
            )));
        };
        self.advance_auto_increment_past(&table, start - 1, None)
    }

    /// An `ALTER TABLE ... ALTER COLUMN c SET DEFAULT` or `DROP DEFAULT`, read
    /// against the table it changes. Answers `None` for every other statement.
    fn column_default_an_alter_changes(
        &self,
        sql: &str,
    ) -> std::result::Result<Option<turso_mysql_parser::MySqlColumnDefaultChange>, MySqlQueryError>
    {
        if !sql
            .split_whitespace()
            .next()
            .is_some_and(|word| word.eq_ignore_ascii_case("ALTER"))
        {
            return Ok(None);
        }
        let mode = self.parser_mode();
        let Some(target) = turso_mysql_parser::alter_table_target(sql, mode) else {
            return Ok(None);
        };
        let Some(stored) = self
            .stored_table_statement(&target)
            .map_err(MySqlQueryError::Engine)?
        else {
            return Ok(None);
        };
        turso_mysql_parser::alter_column_default_restated(&stored, sql, mode)
            .map_err(mysql_query_parse_error)
    }

    /// A `CREATE TABLE` naming neither a character set nor a collation, with
    /// its database's collation written on as the table's own, where that is
    /// not the default collation. MySQL gives such a table its database's
    /// collation, so the table is then made exactly as MySQL makes it.
    pub fn with_the_database_collation(
        &self,
        sql: &str,
    ) -> std::result::Result<Option<String>, MySqlQueryError> {
        turso_mysql_parser::create_table_with_the_database_collation(
            sql,
            self.parser_mode(),
            self.database_collation(),
        )
        .map_err(mysql_query_parse_error)
    }

    /// A `CREATE TABLE` or `ALTER TABLE` with the collation of its table
    /// written onto each text column that names none, where that is not the
    /// default collation. What is stored then names each column's collation,
    /// which is what the engine keeps.
    fn with_the_table_collation_on_each_text_column(
        &self,
        sql: &str,
    ) -> std::result::Result<Option<String>, MySqlQueryError> {
        let mode = self.parser_mode();
        if let Some(written) =
            turso_mysql_parser::create_table_with_its_collation_on_each_text_column(sql, mode)
                .map_err(mysql_query_parse_error)?
        {
            return Ok(Some(written));
        }
        let Some(target) = turso_mysql_parser::alter_table_target(sql, mode) else {
            return Ok(None);
        };
        let Some(stored) = self
            .stored_table_statement(&target)
            .map_err(MySqlQueryError::Engine)?
        else {
            return Ok(None);
        };
        let collation = turso_mysql_parser::table_collation_of(&stored, mode)
            .map_err(mysql_query_parse_error)?;
        turso_mysql_parser::alter_table_with_its_collation_on_each_text_column(sql, mode, collation)
            .map_err(mysql_query_parse_error)
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
    /// The table an `ALTER TABLE ... ADD COLUMN ... FIRST` or `... AFTER x`
    /// makes of the one it names, where the place asked for is not the end.
    ///
    /// Answers `None` for every other statement, which keeps its own path.
    fn column_an_alter_places(
        &self,
        sql: &str,
    ) -> std::result::Result<Option<turso_mysql_parser::MySqlColumnPlacement>, MySqlQueryError>
    {
        if !sql
            .split_whitespace()
            .next()
            .is_some_and(|word| word.eq_ignore_ascii_case("ALTER"))
        {
            return Ok(None);
        }
        let mode = self.parser_mode();
        let Some(target) = turso_mysql_parser::alter_table_target(sql, mode) else {
            return Ok(None);
        };
        let Some(stored) = self
            .stored_table_statement(&target)
            .map_err(MySqlQueryError::Engine)?
        else {
            return Ok(None);
        };
        turso_mysql_parser::table_with_a_column_placed(&stored, sql, mode)
            .map_err(mysql_query_parse_error)
    }

    /// The table an `ALTER TABLE` that adds or drops a `CHECK` changes, and
    /// what it does to it. Answers `None` for every other statement.
    fn check_an_alter_changes(
        &self,
        sql: &str,
    ) -> std::result::Result<Option<(String, turso_mysql_parser::MySqlCheckChange)>, MySqlQueryError>
    {
        if !sql
            .split_whitespace()
            .next()
            .is_some_and(|word| word.eq_ignore_ascii_case("ALTER"))
        {
            return Ok(None);
        }
        let mode = self.parser_mode();
        let Some(target) = turso_mysql_parser::table_a_check_is_dropped_from(sql, mode)
            .or_else(|| turso_mysql_parser::alter_table_target(sql, mode))
        else {
            return Ok(None);
        };
        let Some(stored) = self
            .stored_table_statement(&target)
            .map_err(MySqlQueryError::Engine)?
        else {
            return Ok(None);
        };
        // A name is the database's rather than the table's, so a new one is
        // held against every other table's too.
        let mut other_names = Vec::new();
        for table in self.list_tables().map_err(MySqlQueryError::Engine)? {
            if table.kind() != MySqlTableKind::BaseTable
                || table.name().eq_ignore_ascii_case(&target)
            {
                continue;
            }
            let Some(other) = self
                .stored_table_statement(table.name())
                .map_err(MySqlQueryError::Engine)?
            else {
                continue;
            };
            other_names.extend(
                turso_mysql_parser::check_constraints_of(&other, mode)
                    .map_err(mysql_query_parse_error)?
                    .into_iter()
                    .map(|check| check.name().to_owned()),
            );
        }
        Ok(
            turso_mysql_parser::table_with_a_check_changed(&stored, sql, &other_names, mode)
                .map_err(mysql_query_parse_error)?
                .map(|change| (target, change)),
        )
    }

    /// The MySQL `CREATE TABLE` one stored table was written as.
    fn stored_table_statement(&self, table: &str) -> Result<Option<String>> {
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
                .eq_ignore_ascii_case(table)
            {
                continue;
            }
            let sql = sql.to_string();
            let Some(decoded) = decode_schema_sql(SchemaSqlKind::Table, &sql)
                .map_err(|error| LimboError::Corrupt(error.to_string()))?
            else {
                return Ok(None);
            };
            return Ok(Some(decoded.normalized_ddl.to_owned()));
        }
        Ok(None)
    }

    /// Moves the rows of the table set aside into the one written again.
    ///
    /// A table that counts its own ids refuses an ordinary `INSERT` that writes
    /// its counted column, which is what this does: the rows carry the numbers
    /// they already have. So the statement is prepared with the same validator
    /// the counted path uses, which knows that column is the table's rowid and
    /// that a written record leaves it empty.
    fn carry_the_rows_across(
        &self,
        copy: &str,
        table: &str,
    ) -> std::result::Result<(), MySqlQueryError> {
        let counted = self
            .load_auto_increment_table(table)
            .map_err(MySqlQueryError::Engine)?;
        let Some(counted) = counted else {
            return self.run_internal(copy);
        };
        let statement = turso_mysql_parser::parse_engine_statement(copy)
            .map_err(|error| MySqlQueryError::Engine(LimboError::ParseError(error.to_string())))?;
        let options = PrepareOptions::default().with_assignment_validator(Arc::new(
            CountedTableAssignmentValidator {
                table_name: counted.name,
                table_sql: counted.stored_sql.to_string(),
                allocator_column_ordinal: counted.definition.allocator_column_ordinal,
            },
        ));
        self.inner
            .prepare_translated_stmt_with_options(statement, copy, &options)
            .and_then(|mut statement| statement.run_ignore_rows())
            .map_err(MySqlQueryError::Engine)
    }

    /// How high one table's counter stands, for a table that counts.
    fn counter_of_a_stored_table(&self, table: &str) -> Result<Option<u64>> {
        let Some(table) = self.load_auto_increment_table(table)? else {
            return Ok(None);
        };
        let capability = self.auto_increment.as_ref().ok_or_else(|| {
            LimboError::Corrupt(
                "AUTO_INCREMENT table without a registry-backed allocator capability".to_string(),
            )
        })?;
        let high_water = self.when_the_counter_is_free(&capability.allocator, || {
            let mut query = capability.allocator.peek_high_water(table.key)?;
            capability.io.block(|| query.step())
        })?;
        Ok((high_water > 0).then_some(high_water))
    }

    /// Runs one `ALTER TABLE t COMMENT = '...'` or `ALTER TABLE t
    /// ROW_FORMAT=...`.
    ///
    /// Both live at the end of the stored MySQL `CREATE TABLE`, which the
    /// engine writes again whenever it changes the table. So the engine is
    /// asked for the one change that alters nothing — a column renamed to its
    /// own name — and the table is written back with the new option.
    pub fn execute_table_option(
        &self,
        table: &MySqlTableName,
        change: crate::schema_sql::TableOptionChange,
    ) -> std::result::Result<(), MySqlQueryError> {
        let schema = self.inner.current_schema();
        let Some(btree) = schema.get_btree_table(table.as_str()) else {
            return Err(MySqlQueryError::MissingTable);
        };
        let first_column = btree
            .columns()
            .first()
            .and_then(|column| column.name.clone())
            .ok_or_else(|| {
                MySqlQueryError::Engine(LimboError::Corrupt(
                    "a stored table has no columns".to_string(),
                ))
            })?;
        let body = AlterTableBody::RenameColumn {
            old: turso_parser::ast::Name::exact(first_column.clone()),
            new: turso_parser::ast::Name::exact(first_column),
        };
        // The rename rewrites what names the column as well as the table, as
        // every other `ALTER TABLE` does, and is held to the same rules.
        self.reject_alter_with_marked_trigger()
            .and_then(|()| self.reject_alter_with_marked_view(&body))
            .map_err(MySqlQueryError::Engine)?;
        let stmt = Stmt::AlterTable(turso_parser::ast::AlterTable {
            name: turso_parser::ast::QualifiedName::single(turso_parser::ast::Name::exact(
                btree.name.clone(),
            )),
            body,
        });
        let options = PrepareOptions::default()
            .with_reprepare_parser(Arc::new(FrozenSchemaDdlParser {
                mode: self.parser_mode(),
            }))
            .with_schema_sql_formatter(Arc::new(
                crate::schema_sql::TableOptionSchemaSqlFormatter {
                    context: self.schema_context,
                    table: btree.name.clone(),
                    change,
                },
            ));
        // DDL commits what came before it, which is what MySQL does.
        if !self.inner.get_auto_commit() {
            self.run_internal("COMMIT")?;
        }
        self.run_internal("BEGIN")?;
        let applied = self
            .inner
            .prepare_translated_stmt_with_options(
                stmt,
                &format!("ALTER TABLE {} option", mysql_quoted(table.as_str())),
                &options,
            )
            .and_then(|mut statement| statement.run_ignore_rows())
            .map_err(MySqlQueryError::Engine);
        if applied.is_err() {
            self.run_internal("ROLLBACK")?;
            return applied;
        }
        self.run_internal("COMMIT")?;
        if !self.inner.get_auto_commit() {
            self.run_internal("ROLLBACK")?;
        }
        Ok(())
    }

    /// Runs one `ALTER TABLE t CONVERT TO CHARACTER SET ...`: the table is
    /// written again with every column of words in the new collation and its
    /// rows carried across, so its keys are built again under it.
    pub fn execute_table_conversion(
        &self,
        table: &MySqlTableName,
        conversion: turso_mysql_parser::MySqlTableConversion,
    ) -> std::result::Result<(), MySqlQueryError> {
        self.a_base_table_named(table)?;
        let collation = match conversion {
            turso_mysql_parser::MySqlTableConversion::To(collation) => collation,
            turso_mysql_parser::MySqlTableConversion::ToTheDatabases => self.database_collation(),
        };
        let stored = self
            .stored_table_statement(table.as_str())
            .map_err(MySqlQueryError::Engine)?
            .ok_or(MySqlQueryError::MissingTable)?;
        let rewrite =
            turso_mysql_parser::table_with_its_words_in(&stored, collation, self.parser_mode())
                .map_err(mysql_query_parse_error)?;
        self.write_the_table_again_as(table.as_str(), &rewrite)
    }

    /// Runs an `ALTER TABLE t AUTO_INCREMENT = n`, which says where the table's
    /// numbering goes on from.
    ///
    /// Measured on MySQL 8.4.11: the next row takes `n`, or one past the
    /// highest id the table holds when `n` is not past it, and that may be
    /// below where the counter stood — after the rows above it are deleted,
    /// `AUTO_INCREMENT = 5` hands out 5 again where 8 was next. The allocator
    /// only moves forward, so a change that would move it back is refused. A
    /// table that counts nothing takes the statement and nothing changes.
    pub fn execute_table_counter_change(
        &self,
        table: &MySqlTableName,
        next: u64,
    ) -> std::result::Result<(), MySqlQueryError> {
        self.a_base_table_named(table)?;
        if !self.inner.get_auto_commit() {
            self.run_internal("COMMIT")?;
        }
        let Some(table) = self
            .load_auto_increment_table(table.as_str())
            .map_err(MySqlQueryError::Engine)?
        else {
            return Ok(());
        };
        let capability = self.auto_increment.as_ref().ok_or_else(|| {
            MySqlQueryError::Unsupported(
                "AUTO_INCREMENT update requires a registry-backed allocator capability".to_string(),
            )
        })?;
        let highest = self.highest_counted_id(&table)?;
        let next = next.max(highest.saturating_add(1));
        // The allocator counts the numbers already handed out, so a table
        // whose next row is to take `next` has had `next - 1` of them.
        let handed_out = next - 1;
        // Measured on MySQL 8.4.11: a number past the column's type is taken,
        // and the next row answers 1467. This refuses the statement instead of
        // storing a mark no row could take, as `CREATE TABLE` does.
        if handed_out > auto_increment_ceiling(&table) {
            return Err(MySqlQueryError::Unsupported(
                "ALTER TABLE AUTO_INCREMENT past the column's type".to_string(),
            ));
        }
        let (mut lease, current) = self
            .when_the_counter_is_free(&capability.allocator, || {
                let mut lease = capability.allocator.lease_high_water(table.key)?;
                let current = capability.io.block(|| lease.read())?;
                Ok((lease, current))
            })
            .map_err(MySqlQueryError::Engine)?;
        if handed_out < current {
            lease.release().map_err(MySqlQueryError::Engine)?;
            return Err(MySqlQueryError::Unsupported(
                "ALTER TABLE AUTO_INCREMENT below where the counter stands".to_string(),
            ));
        }
        if handed_out > current {
            capability
                .io
                .block(|| lease.advance_past(handed_out))
                .map_err(MySqlQueryError::Engine)?;
        }
        lease.release().map_err(MySqlQueryError::Engine)
    }

    /// Refuses a name that is not a base table.
    ///
    /// Measured on MySQL 8.4.11: 1146 for a name that is not there and 1347
    /// for a view; the second is refused here, having no error of its own.
    fn a_base_table_named(
        &self,
        table: &MySqlTableName,
    ) -> std::result::Result<(), MySqlQueryError> {
        let schema = self.inner.current_schema();
        if schema.get_btree_table(table.as_str()).is_some() {
            return Ok(());
        }
        if schema.get_view(table.as_str()).is_some() {
            return Err(MySqlQueryError::Unsupported(
                "ALTER TABLE naming a view".to_string(),
            ));
        }
        Err(MySqlQueryError::MissingTable)
    }

    /// The highest id a counted table holds, or 0 when it holds none.
    fn highest_counted_id(
        &self,
        table: &AutoIncrementTable,
    ) -> std::result::Result<u64, MySqlQueryError> {
        let sql = format!(
            "SELECT MAX({}) FROM {}",
            sqlite_quoted(&table.definition.allocator_column_name),
            sqlite_quoted(&table.name)
        );
        let rows = self
            .inner
            .prepare(sql)
            .and_then(|mut statement| statement.run_collect_rows())
            .map_err(MySqlQueryError::Engine)?;
        let [row] = rows.as_slice() else {
            return Err(MySqlQueryError::Engine(LimboError::InternalError(
                "MAX answered other than one row".to_string(),
            )));
        };
        match row.as_slice() {
            [Value::Null] => Ok(0),
            [Value::Numeric(Numeric::Integer(id))] => Ok(u64::try_from(*id).unwrap_or(0)),
            _ => Err(MySqlQueryError::Unsupported(
                "the highest id of a table counting past a signed integer".to_string(),
            )),
        }
    }

    /// Runs one `RENAME TABLE`, renaming every pair it names or none of them.
    ///
    /// Measured on MySQL 8.4.11: the pairs are taken in the order written, each
    /// against the names the ones before it left, so `RENAME TABLE a TO t, b TO
    /// a, t TO b` swaps two tables. A pair naming a table that is not there by
    /// then is 1146 and one naming a new name that is taken is 1050, a table
    /// renamed onto its own name among them, and either leaves every table
    /// under the name it had.
    pub fn execute_rename_tables(
        &self,
        pairs: &[(MySqlTableName, MySqlTableName)],
    ) -> std::result::Result<(), MySqlRenameTableError> {
        let mut renamed: Vec<(&MySqlTableName, bool)> = Vec::new();
        let names_a_table_by_then =
            |renamed: &[(&MySqlTableName, bool)], table: &MySqlTableName| match renamed
                .iter()
                .rev()
                .find(|(name, _)| name.as_str().eq_ignore_ascii_case(table.as_str()))
            {
                Some((_, there)) => Ok(*there),
                None => self
                    .names_a_table(table)
                    .map_err(|error| MySqlRenameTableError::Query(MySqlQueryError::Engine(error))),
            };
        for (from, to) in pairs {
            if !names_a_table_by_then(&renamed, from)? {
                return Err(MySqlRenameTableError::MissingTable);
            }
            if names_a_table_by_then(&renamed, to)? {
                return Err(MySqlRenameTableError::NameTaken);
            }
            renamed.push((from, false));
            renamed.push((to, true));
        }
        let statements = pairs
            .iter()
            .map(|(from, to)| {
                format!(
                    "ALTER TABLE {} RENAME TO {}",
                    mysql_quoted(from.as_str()),
                    mysql_quoted(to.as_str())
                )
            })
            .collect::<Vec<_>>();
        self.execute_expanded_alter_table(&statements)
            .map_err(MySqlRenameTableError::Query)
    }

    fn execute_expanded_alter_table(
        &self,
        statements: &[String],
    ) -> std::result::Result<(), MySqlQueryError> {
        // DDL commits what came before it, which is what MySQL does.
        if !self.inner.get_auto_commit() {
            self.run_internal("COMMIT")?;
        }
        self.run_internal("BEGIN")?;
        let mut changed_index_table = None;
        let applied = statements
            .iter()
            .try_for_each(|statement| {
                if let Some(placement) = self.column_an_alter_places(statement)? {
                    return self.place_a_column_in_this_transaction(statement, placement);
                }
                if let Some(indexes) = turso_mysql_parser::parse_optional_alter_table_indexes(
                    statement,
                    self.parser_mode(),
                )
                .map_err(mysql_query_parse_error)?
                {
                    self.apply_alter_table_index_operations(&indexes)
                        .map_err(mysql_query_index_error)?;
                    changed_index_table = Some(indexes.table().clone());
                    return Ok(());
                }
                let added_to = self.added_foreign_key_table(statement);
                let keys_before = match &added_to {
                    Some(table) => {
                        self.check_the_foreign_key_columns_an_alter_names(statement, table)?
                    }
                    None => 0,
                };
                let mut prepared = self
                    .prepare(statement)
                    .map_err(|error| self.json_schema_prepare_error(statement, error))?;
                prepared
                    .run_ignore_rows()
                    .map_err(MySqlQueryError::Engine)?;
                if let Some(table) = &added_to {
                    self.drop_the_indexes_an_added_foreign_key_replaces(table, keys_before)?;
                    self.ensure_foreign_key_child_indexes(table, Some(keys_before))?;
                    self.check_the_foreign_keys_of(table, keys_before)?;
                }
                if let Some(table) = self.created_index_table(statement) {
                    self.remove_replaced_implicit_fk_indexes(&table)?;
                }
                Ok(())
            })
            .and_then(|_| {
                if let Some(table) = &changed_index_table {
                    self.finish_alter_table_indexes(table)
                        .map_err(mysql_query_index_error)?;
                }
                Ok(())
            });
        if applied.is_err() {
            // A failed rollback leaves the connection in a state the caller
            // cannot reason about, so it replaces the original error.
            self.run_internal("ROLLBACK")?;
            return applied;
        }
        self.run_internal("COMMIT")?;
        crash_point(CrashPoint::SchemaChangeCommitted);
        if !self.inner.get_auto_commit() {
            self.run_internal("ROLLBACK")?;
        }
        Ok(())
    }

    /// Adds one clause's column where it asks to stand, inside the
    /// transaction the rest of its `ALTER TABLE` runs in.
    fn place_a_column_in_this_transaction(
        &self,
        statement: &str,
        placement: turso_mysql_parser::MySqlColumnPlacement,
    ) -> std::result::Result<(), MySqlQueryError> {
        match placement {
            turso_mysql_parser::MySqlColumnPlacement::TableWrittenAgain(rewrite) => self
                .write_the_table_again_in_the_callers_transaction(
                    turso_mysql_parser::alter_table_target(statement, self.parser_mode())
                        .unwrap_or_default()
                        .as_str(),
                    &rewrite,
                ),
            turso_mysql_parser::MySqlColumnPlacement::AlreadyAtTheEnd(written) => {
                let mut prepared = self
                    .prepare(&written)
                    .map_err(|error| self.json_schema_prepare_error(&written, error))?;
                prepared
                    .run_ignore_rows()
                    .map_err(MySqlQueryError::Engine)?;
                Ok(())
            }
            turso_mysql_parser::MySqlColumnPlacement::NoSuchColumn(name) => {
                Err(MySqlQueryError::Engine(LimboError::NoSuchColumn { name }))
            }
            turso_mysql_parser::MySqlColumnPlacement::DuplicateColumn(name) => {
                Err(MySqlQueryError::DuplicateColumn(name))
            }
        }
    }

    fn check_the_foreign_key_columns_an_alter_names(
        &self,
        sql: &str,
        table: &MySqlTableName,
    ) -> std::result::Result<usize, MySqlQueryError> {
        let schema = self.inner.current_schema();
        let Some(btree) = schema.get_btree_table(table.as_str()) else {
            return Ok(0);
        };
        let Ok(Stmt::AlterTable(alter)) = parse_schema_ddl_ast(sql, self.parser_mode()) else {
            return Ok(btree.foreign_keys.len());
        };
        let AlterTableBody::AddConstraint(named) = &alter.body else {
            return Ok(btree.foreign_keys.len());
        };
        let turso_parser::ast::TableConstraint::ForeignKey {
            columns: child_columns,
            clause,
            ..
        } = &named.constraint
        else {
            return Ok(btree.foreign_keys.len());
        };
        if let Some(name) = &named.name {
            let taken = btree.foreign_keys.iter().any(|foreign_key| {
                crate::show_create_table::foreign_key_name(
                    table.as_str(),
                    foreign_key,
                    &btree.foreign_keys,
                )
                .eq_ignore_ascii_case(name.as_str())
            });
            if taken {
                return Err(self.foreign_key_definition_error(
                    MySqlForeignKeyDefinitionError::DuplicateName {
                        name: name.as_str().to_owned(),
                    },
                ));
            }
        }
        let columns = btree
            .columns()
            .iter()
            .filter_map(|column| column.name.clone())
            .collect::<Vec<_>>();
        self.check_the_written_foreign_key_columns(
            &columns,
            &[WrittenForeignKey {
                name: named.name.as_ref().map(|name| name.as_str().to_owned()),
                child_columns: child_columns
                    .iter()
                    .map(|column| column.col_name.as_str().to_owned())
                    .collect(),
                parent_column_count: clause.columns.len(),
            }],
        )?;
        Ok(btree.foreign_keys.len())
    }

    fn drop_the_indexes_an_added_foreign_key_replaces(
        &self,
        table: &MySqlTableName,
        added_at: usize,
    ) -> std::result::Result<(), MySqlQueryError> {
        let schema = self.inner.current_schema();
        let Some(btree) = schema.get_btree_table(table.as_str()) else {
            return Ok(());
        };
        let Some(added) = btree
            .foreign_keys
            .iter()
            .find(|foreign_key| foreign_key.decl_order == added_at)
        else {
            return Ok(());
        };
        let columns = &added.child_columns;
        let primary_key = &btree.primary_key_columns;
        let indexes = schema.get_indices(table.as_str()).collect::<Vec<_>>();
        let kept_by_another_key = primary_key_covers_columns(primary_key, columns)
            || indexes.iter().any(|index| {
                index_covers_columns(index, primary_key, columns)
                    && (!is_implicit_index(&index.name)
                        || mysql_index_columns(index, primary_key).len() > columns.len())
            });
        if kept_by_another_key {
            return Ok(());
        }
        let replaced = indexes
            .iter()
            .filter(|index| is_implicit_index(&index.name))
            .filter(|index| {
                let shown = mysql_index_columns(index, primary_key);
                shown.len() <= columns.len()
                    && shown
                        .iter()
                        .zip(columns.iter())
                        .all(|(indexed, column)| indexed.name.eq_ignore_ascii_case(column))
            })
            .map(|index| index.name.clone())
            .collect::<Vec<_>>();
        for name in replaced {
            let sql = format!("DROP INDEX {}", mysql_quoted(&name));
            let stmt = Stmt::DropIndex {
                if_exists: false,
                idx_name: turso_parser::ast::QualifiedName::single(turso_parser::ast::Name::exact(
                    name,
                )),
            };
            self.inner
                .prepare_translated_stmt(stmt, &sql)
                .and_then(|mut statement| statement.run_ignore_rows())
                .map_err(MySqlQueryError::Engine)?;
        }
        Ok(())
    }

    fn added_foreign_key_table(&self, sql: &str) -> Option<MySqlTableName> {
        let Stmt::AlterTable(alter) = parse_schema_ddl_ast(sql, self.parser_mode()).ok()? else {
            return None;
        };
        let AlterTableBody::AddConstraint(named) = &alter.body else {
            return None;
        };
        if !matches!(
            named.constraint,
            turso_parser::ast::TableConstraint::ForeignKey { .. }
        ) {
            return None;
        }
        MySqlTableName::parse(alter.name.name.as_str()).ok()
    }

    fn created_index_table(&self, sql: &str) -> Option<MySqlTableName> {
        let Stmt::CreateIndex { tbl_name, .. } =
            parse_schema_ddl_ast(sql, self.parser_mode()).ok()?
        else {
            return None;
        };
        MySqlTableName::parse(tbl_name.as_str()).ok()
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
        let counter_start = self.counter_start_of_a_new_table(checked.table_sql())?;
        // DDL commits what came before it, which is what MySQL does.
        if !self.inner.get_auto_commit() {
            self.run_internal("COMMIT")?;
        }
        self.run_internal("BEGIN")?;
        // The counter moves in the transaction that makes the table, so no
        // crash leaves the table there with its counter still at one.
        let applied =
            self.apply_create_table_with_keys(checked)
                .and_then(|()| match &counter_start {
                    Some((table, start)) => self.start_the_counter(table, *start),
                    None => Ok(()),
                });
        if applied.is_err() {
            // A failed rollback leaves the connection in a state the caller
            // cannot reason about, so it replaces the original error.
            self.run_internal("ROLLBACK")?;
            return applied;
        }
        self.run_internal("COMMIT")?;
        crash_point(CrashPoint::SchemaChangeCommitted);
        if !self.inner.get_auto_commit() {
            self.run_internal("ROLLBACK")?;
        }
        Ok(())
    }

    /// Runs one `CREATE TABLE ... AS SELECT`, and returns the rows it copied.
    ///
    /// MySQL works the new table's columns out from the ones the `SELECT`
    /// answers, so this reads them out of the source table's stored DDL and
    /// writes a `CREATE TABLE` of its own, then fills it with an `INSERT`.
    /// Both run inside one transaction, so a table is never left behind empty.
    pub fn execute_create_table_as_select(
        &self,
        checked: &MySqlCreateTableAsSelect,
    ) -> std::result::Result<u64, MySqlCreateTableAsSelectError> {
        // DDL commits what came before it, which is what MySQL does.
        if !self.inner.get_auto_commit() {
            self.run_internal("COMMIT")
                .map_err(MySqlCreateTableAsSelectError::Query)?;
        }
        let statements = self.create_table_as_select_statements(checked)?;
        self.run_internal("BEGIN")
            .map_err(MySqlCreateTableAsSelectError::Query)?;
        let applied = self.apply_create_table_as_select(&statements);
        if applied.is_err() {
            // A failed rollback leaves the connection in a state the caller
            // cannot reason about, so it replaces the original error.
            self.run_internal("ROLLBACK")
                .map_err(MySqlCreateTableAsSelectError::Query)?;
            return applied;
        }
        self.run_internal("COMMIT")
            .map_err(MySqlCreateTableAsSelectError::Query)?;
        if !self.inner.get_auto_commit() {
            self.run_internal("ROLLBACK")
                .map_err(MySqlCreateTableAsSelectError::Query)?;
        }
        applied
    }

    /// Writes the `CREATE TABLE` and the `INSERT` one `AS SELECT` means.
    fn create_table_as_select_statements(
        &self,
        checked: &MySqlCreateTableAsSelect,
    ) -> std::result::Result<(String, String), MySqlCreateTableAsSelectError> {
        let source = MySqlTableName::parse(checked.source_table())
            .map_err(|_| MySqlCreateTableAsSelectError::MissingTable)?;
        let held = self
            .list_shared_columns(&source)
            .map_err(|_| MySqlCreateTableAsSelectError::MissingTable)?;
        let nullable = |column_name: &str| {
            held.iter()
                .find(|held| held.name().eq_ignore_ascii_case(column_name))
                .map(MySqlColumnMetadata::nullable)
                .ok_or(MySqlCreateTableAsSelectError::MissingColumn)
        };
        let declarations = match checked.columns() {
            None => held
                .iter()
                .map(|column| {
                    copied_column_declaration(column.name(), column)
                        .ok_or(MySqlCreateTableAsSelectError::UnsupportedColumn)
                })
                .collect::<std::result::Result<Vec<_>, _>>()?,
            Some(columns) => columns
                .iter()
                .map(|column| match column.source() {
                    MySqlCreateTableAsSelectSource::Column(source) => {
                        let source = held
                            .iter()
                            .find(|held| held.name().eq_ignore_ascii_case(source))
                            .ok_or(MySqlCreateTableAsSelectError::MissingColumn)?;
                        copied_column_declaration(column.name(), source)
                            .ok_or(MySqlCreateTableAsSelectError::UnsupportedColumn)
                    }
                    // Measured on MySQL 8.4.11: a BIGINT, NOT NULL when every
                    // column the arithmetic names is NOT NULL, and a NOT NULL
                    // one carries a zero default the way a dropped
                    // AUTO_INCREMENT's copy does.
                    MySqlCreateTableAsSelectSource::IntegerArithmetic { columns } => {
                        let mut not_null = true;
                        for column_name in columns {
                            not_null &= !nullable(column_name)?;
                        }
                        Ok(format!(
                            "{} BIGINT{}",
                            mysql_quoted(column.name()),
                            if not_null { " NOT NULL DEFAULT 0" } else { "" }
                        ))
                    }
                })
                .collect::<std::result::Result<Vec<_>, _>>()?,
        };
        if declarations.is_empty() {
            return Err(MySqlCreateTableAsSelectError::MissingColumn);
        }
        let table = mysql_quoted(checked.table().as_str());
        let names = match checked.columns() {
            None => held
                .iter()
                .map(|column| mysql_quoted(column.name()))
                .collect::<Vec<_>>()
                .join(", "),
            Some(columns) => columns
                .iter()
                .map(|column| mysql_quoted(column.name()))
                .collect::<Vec<_>>()
                .join(", "),
        };
        Ok((
            format!("CREATE TABLE {table} ({})", declarations.join(", ")),
            format!("INSERT INTO {table} ({names}) {}", checked.select_sql()),
        ))
    }

    fn apply_create_table_as_select(
        &self,
        statements: &(String, String),
    ) -> std::result::Result<u64, MySqlCreateTableAsSelectError> {
        let (create, insert) = statements;
        self.prepare(create)
            .and_then(|mut statement| statement.run_ignore_rows())
            .map_err(MySqlCreateTableAsSelectError::Engine)?;
        self.execute_checked_write(insert, None)
            .map(|result| result.affected_rows)
            .map_err(MySqlCreateTableAsSelectError::Query)
    }

    /// Runs one `ALTER TABLE` that only adds or drops indexes.
    ///
    /// The engine has no `ALTER TABLE ADD INDEX`, so each operation becomes a
    /// `CREATE INDEX` or a `DROP INDEX` of its own. MySQL applies the whole
    /// statement or none of it, so they run inside one transaction.
    pub fn execute_alter_table_indexes(
        &self,
        checked: &MySqlAlterTableIndexes,
    ) -> std::result::Result<(), MySqlAlterTableIndexError> {
        // DDL commits what came before it, which is what MySQL does.
        if !self.inner.get_auto_commit() {
            self.run_internal("COMMIT")
                .map_err(alter_table_index_query_error)?;
        }
        self.run_internal("BEGIN")
            .map_err(alter_table_index_query_error)?;
        let applied = self.apply_alter_table_indexes(checked);
        if applied.is_err() {
            // A failed rollback leaves the connection in a state the caller
            // cannot reason about, so it replaces the original error.
            self.run_internal("ROLLBACK")
                .map_err(alter_table_index_query_error)?;
            return applied;
        }
        self.run_internal("COMMIT")
            .map_err(alter_table_index_query_error)?;
        if !self.inner.get_auto_commit() {
            self.run_internal("ROLLBACK")
                .map_err(alter_table_index_query_error)?;
        }
        Ok(())
    }

    fn apply_alter_table_indexes(
        &self,
        checked: &MySqlAlterTableIndexes,
    ) -> std::result::Result<(), MySqlAlterTableIndexError> {
        self.apply_alter_table_index_operations(checked)?;
        self.finish_alter_table_indexes(checked.table())
    }

    fn apply_alter_table_index_operations(
        &self,
        checked: &MySqlAlterTableIndexes,
    ) -> std::result::Result<(), MySqlAlterTableIndexError> {
        let table = checked.table().as_str().replace('`', "``");
        // Each operation is checked against the indexes the table carries as
        // the statement walks it, so `DROP INDEX i, ADD INDEX i (c)` reads the
        // way MySQL reads it.
        let mut names = self
            .list_indexes(checked.table())
            .map_err(|_| MySqlAlterTableIndexError::MissingTable)?
            .iter()
            .map(|index| index.key_name().to_owned())
            .collect::<Vec<_>>();
        names.dedup_by(|left, right| left.eq_ignore_ascii_case(right));
        let renames = checked
            .operations()
            .iter()
            .filter_map(|operation| match operation {
                MySqlAlterTableIndexOperation::Rename { from, to } => Some((from, to)),
                _ => None,
            })
            .collect::<Vec<_>>();
        if !renames.is_empty() {
            assert_eq!(
                renames.len(),
                checked.operations().len(),
                "the parser reads a RENAME INDEX only in a statement of renames alone"
            );
            return self.rename_indexes(checked.table(), &renames, names);
        }
        for operation in checked.operations() {
            let (sql, stored_drop_name) = match operation {
                MySqlAlterTableIndexOperation::Add {
                    name,
                    unique,
                    columns,
                } => {
                    if self
                        .index_targets_json(checked.table().as_str(), columns)
                        .map_err(MySqlAlterTableIndexError::Engine)?
                    {
                        return Err(MySqlAlterTableIndexError::JsonIndex);
                    }
                    let name = match name {
                        Some(name) => {
                            if names.iter().any(|held| held.eq_ignore_ascii_case(name)) {
                                return Err(MySqlAlterTableIndexError::DuplicateIndex);
                            }
                            name.clone()
                        }
                        None => unnamed_index_name(&names, columns)
                            .ok_or(MySqlAlterTableIndexError::DuplicateIndex)?,
                    };
                    let name = &name;
                    names.push(name.clone());
                    let columns = columns
                        .iter()
                        .map(|column| format!("`{}`", column.replace('`', "``")))
                        .collect::<Vec<_>>()
                        .join(", ");
                    (
                        format!(
                            "CREATE {}INDEX `{}` ON `{table}` ({columns})",
                            if *unique { "UNIQUE " } else { "" },
                            name.replace('`', "``")
                        ),
                        None,
                    )
                }
                MySqlAlterTableIndexOperation::Drop { name } => {
                    let Some(position) = names
                        .iter()
                        .position(|held| held.eq_ignore_ascii_case(name))
                    else {
                        return Err(MySqlAlterTableIndexError::MissingIndex);
                    };
                    names.remove(position);
                    let stored_name = the_index_named(
                        &self.inner.current_schema(),
                        checked.table().as_str(),
                        name,
                    )
                    .map(|index| index.name.clone())
                    .ok_or(MySqlAlterTableIndexError::MissingIndex)?;
                    (
                        format!("DROP INDEX `{}`", stored_name.replace('`', "``")),
                        Some(stored_name),
                    )
                }
                MySqlAlterTableIndexOperation::Rename { .. } => {
                    unreachable!("renames are applied together above")
                }
            };
            self.prepare_alter_table_index_statement(&sql, operation, stored_drop_name.as_deref())?;
        }
        Ok(())
    }

    /// Gives indexes new names, each written again under its new one.
    ///
    /// Every rename is read against the names the table carried before the
    /// statement, so all the old indexes are dropped before any new one is
    /// made, which is what lets two indexes swap names. An index the engine
    /// made for a foreign key comes back as an ordinary one: measured on MySQL
    /// 8.4.11, a renamed one is kept when a later index covers the same
    /// columns, where one never renamed is dropped.
    fn rename_indexes(
        &self,
        table: &MySqlTableName,
        renames: &[(&String, &String)],
        names: Vec<String>,
    ) -> std::result::Result<(), MySqlAlterTableIndexError> {
        let mut kept = names;
        for (from, _) in renames {
            let Some(position) = kept.iter().position(|held| held.eq_ignore_ascii_case(from))
            else {
                return Err(MySqlAlterTableIndexError::MissingIndexToRename);
            };
            kept.remove(position);
        }
        for (_, to) in renames {
            if kept.iter().any(|held| held.eq_ignore_ascii_case(to)) {
                return Err(MySqlAlterTableIndexError::DuplicateIndex);
            }
            kept.push((*to).clone());
        }
        let stored = self
            .stored_index_statements(table.as_str())
            .map_err(MySqlAlterTableIndexError::Engine)?;
        let mut new_names = Vec::with_capacity(renames.len());
        for (from, to) in renames {
            let stored_name = the_index_named(&self.inner.current_schema(), table.as_str(), from)
                .map(|index| index.name.clone())
                .ok_or(MySqlAlterTableIndexError::MissingIndexToRename)?;
            if !stored
                .iter()
                .any(|statement| statement.stored_name == stored_name)
            {
                return Err(MySqlAlterTableIndexError::RenamingAColumnsOwnKey);
            }
            new_names.push((stored_name, *to));
        }
        // Measured on MySQL 8.4.11: a renamed index keeps its place among the
        // table's keys, where one written again goes last. So every index from
        // the first renamed one on is written again, in the order they stood.
        let first = stored
            .iter()
            .position(|statement| {
                new_names
                    .iter()
                    .any(|(stored_name, _)| *stored_name == statement.stored_name)
            })
            .expect("every renamed index was found among the stored ones");
        let mut written_again = Vec::with_capacity(stored.len() - first);
        for statement in &stored[first..] {
            let renamed_to = new_names
                .iter()
                .find(|(stored_name, _)| *stored_name == statement.stored_name)
                .map(|(_, to)| *to);
            written_again.push(match renamed_to {
                Some(to) => (
                    statement.stored_name.clone(),
                    create_index_under_another_name(&statement.sql, to, self.parser_mode())
                        .map_err(MySqlAlterTableIndexError::Engine)?,
                    false,
                ),
                None => (
                    statement.stored_name.clone(),
                    statement.sql.clone(),
                    statement.implicit,
                ),
            });
        }
        for (stored_name, _, _) in &written_again {
            let stmt = Stmt::DropIndex {
                if_exists: false,
                idx_name: turso_parser::ast::QualifiedName::single(turso_parser::ast::Name::exact(
                    stored_name.clone(),
                )),
            };
            self.inner
                .prepare_translated_stmt(stmt, &format!("DROP INDEX \"{stored_name}\""))
                .and_then(|mut statement| statement.run_ignore_rows())
                .map_err(MySqlAlterTableIndexError::Engine)?;
        }
        for (_, sql, implicit) in &written_again {
            self.prepare_with_index_origin(sql, *implicit)
                .and_then(|mut statement| statement.run_ignore_rows())
                .map_err(MySqlAlterTableIndexError::Engine)?;
        }
        Ok(())
    }

    fn finish_alter_table_indexes(
        &self,
        table: &MySqlTableName,
    ) -> std::result::Result<(), MySqlAlterTableIndexError> {
        let schema = self.inner.current_schema();
        let btree = schema
            .get_btree_table(table.as_str())
            .ok_or(MySqlAlterTableIndexError::MissingTable)?;
        for foreign_key in &btree.foreign_keys {
            let primary_covers =
                primary_key_covers_columns(&btree.primary_key_columns, &foreign_key.child_columns);
            let index_covers = schema.get_indices(table.as_str()).any(|index| {
                index_covers_columns(
                    index,
                    &btree.primary_key_columns,
                    &foreign_key.child_columns,
                )
            });
            if !primary_covers && !index_covers {
                return Err(MySqlAlterTableIndexError::RequiredByForeignKey);
            }
        }
        self.remove_replaced_implicit_fk_indexes(table)
            .map_err(|error| match error {
                MySqlQueryError::Engine(error) => MySqlAlterTableIndexError::Engine(error),
                other => {
                    MySqlAlterTableIndexError::Engine(LimboError::InternalError(other.to_string()))
                }
            })?;
        Ok(())
    }

    fn prepare_alter_table_index_statement(
        &self,
        sql: &str,
        operation: &MySqlAlterTableIndexOperation,
        stored_drop_name: Option<&str>,
    ) -> std::result::Result<(), MySqlAlterTableIndexError> {
        match operation {
            MySqlAlterTableIndexOperation::Add { .. } => self
                .prepare(sql)
                .and_then(|mut statement| statement.run_ignore_rows())
                .map(|_| ())
                .map_err(MySqlAlterTableIndexError::Engine),
            // The engine's `DROP INDEX` names no table, and it leaves no
            // durable DDL behind, so it goes straight to Core rather than
            // through the MySQL schema path.
            MySqlAlterTableIndexOperation::Drop { .. } => {
                let stored_name = stored_drop_name.expect("checked DROP INDEX has a stored name");
                let stmt = Stmt::DropIndex {
                    if_exists: false,
                    idx_name: turso_parser::ast::QualifiedName::single(
                        turso_parser::ast::Name::exact(stored_name.to_owned()),
                    ),
                };
                self.inner
                    .prepare_translated_stmt(stmt, sql)
                    .and_then(|mut statement| statement.run_ignore_rows())
                    .map(|_| ())
                    .map_err(MySqlAlterTableIndexError::Engine)
            }
            MySqlAlterTableIndexOperation::Rename { .. } => {
                unreachable!("renames are applied by rename_indexes")
            }
        }
    }

    fn apply_create_table_with_keys(
        &self,
        checked: &MySqlCreateTableWithKeys,
    ) -> std::result::Result<(), MySqlQueryError> {
        let existed = self
            .names_a_table(checked.table())
            .map_err(MySqlQueryError::Engine)?;
        if !existed {
            self.check_the_foreign_key_columns_a_new_table_names(checked.table_sql())?;
        }
        let collated = turso_mysql_parser::create_table_with_its_collation_on_each_text_column(
            checked.table_sql(),
            self.parser_mode(),
        )
        .map_err(mysql_query_parse_error)?;
        self.prepare(collated.as_deref().unwrap_or_else(|| checked.table_sql()))
            .and_then(|mut statement| statement.run_ignore_rows())
            .map_err(MySqlQueryError::Engine)?;
        if existed {
            return Ok(());
        }
        let mut held = self
            .list_indexes(checked.table())
            .map_err(|_| {
                MySqlQueryError::Engine(LimboError::Corrupt(
                    "created table is absent before its indexes were added".to_string(),
                ))
            })?
            .iter()
            .map(|index| index.key_name().to_owned())
            .collect::<Vec<_>>();
        for index in checked.indexes() {
            if held
                .iter()
                .any(|name| name.eq_ignore_ascii_case(index.name()))
            {
                return Err(MySqlQueryError::DuplicateIndex);
            }
            held.push(index.name().to_owned());
            let columns = index
                .columns()
                .iter()
                .map(|column| format!("`{}`", column.replace('`', "``")))
                .collect::<Vec<_>>()
                .join(", ");
            let unique = if index.is_unique() { "UNIQUE " } else { "" };
            let sql = format!(
                "CREATE {unique}INDEX `{}` ON `{}` ({columns})",
                index.name().replace('`', "``"),
                checked.table().as_str().replace('`', "``")
            );
            self.prepare_with_index_origin(&sql, index.is_for_a_foreign_key())
                .and_then(|mut statement| statement.run_ignore_rows())
                .map_err(MySqlQueryError::Engine)?;
        }
        self.ensure_foreign_key_child_indexes(checked.table(), None)?;
        self.check_the_foreign_keys_of(checked.table(), 0)?;
        Ok(())
    }

    fn check_the_foreign_key_columns_a_new_table_names(
        &self,
        table_sql: &str,
    ) -> std::result::Result<(), MySqlQueryError> {
        let Ok(Stmt::CreateTable {
            body:
                turso_parser::ast::CreateTableBody::ColumnsAndConstraints {
                    columns,
                    constraints,
                    ..
                },
            ..
        }) = parse_schema_ddl_ast(table_sql, self.parser_mode())
        else {
            return Ok(());
        };
        let columns = columns
            .iter()
            .map(|column| column.col_name.as_str().to_owned())
            .collect::<Vec<_>>();
        let foreign_keys = constraints
            .iter()
            .filter_map(|named| match &named.constraint {
                turso_parser::ast::TableConstraint::ForeignKey {
                    columns: child_columns,
                    clause,
                    ..
                } => Some(WrittenForeignKey {
                    name: named.name.as_ref().map(|name| name.as_str().to_owned()),
                    child_columns: child_columns
                        .iter()
                        .map(|column| column.col_name.as_str().to_owned())
                        .collect(),
                    parent_column_count: clause.columns.len(),
                }),
                _ => None,
            })
            .collect::<Vec<_>>();
        self.check_the_written_foreign_key_columns(&columns, &foreign_keys)
    }

    fn check_the_written_foreign_key_columns(
        &self,
        columns: &[String],
        foreign_keys: &[WrittenForeignKey],
    ) -> std::result::Result<(), MySqlQueryError> {
        for column in foreign_keys
            .iter()
            .flat_map(|foreign_key| &foreign_key.child_columns)
        {
            if !columns
                .iter()
                .any(|declared| declared.eq_ignore_ascii_case(column))
            {
                return Err(self.foreign_key_definition_error(
                    MySqlForeignKeyDefinitionError::ChildColumnMissing {
                        column: column.clone(),
                    },
                ));
            }
        }
        for foreign_key in foreign_keys {
            if foreign_key.parent_column_count != 0
                && foreign_key.parent_column_count != foreign_key.child_columns.len()
            {
                return Err(self.foreign_key_definition_error(
                    MySqlForeignKeyDefinitionError::ColumnCountMismatch {
                        constraint: foreign_key.name.clone(),
                    },
                ));
            }
        }
        Ok(())
    }

    fn foreign_key_definition_error(
        &self,
        error: MySqlForeignKeyDefinitionError,
    ) -> MySqlQueryError {
        *self.explained_error.lock().unwrap() = Some(error.message());
        MySqlQueryError::ForeignKeyDefinition(error)
    }

    fn check_the_foreign_keys_of(
        &self,
        table: &MySqlTableName,
        first_new: usize,
    ) -> std::result::Result<(), MySqlQueryError> {
        let schema = self.inner.current_schema();
        let btree = schema.get_btree_table(table.as_str()).ok_or_else(|| {
            MySqlQueryError::Engine(LimboError::Corrupt(
                "a table with foreign keys disappeared while they were checked".to_string(),
            ))
        })?;
        let mut new_keys = btree
            .foreign_keys
            .iter()
            .filter(|foreign_key| foreign_key.decl_order >= first_new)
            .cloned()
            .collect::<Vec<_>>();
        new_keys.sort_by_key(|foreign_key| foreign_key.decl_order);
        let child_columns = self
            .list_columns(table)
            .map_err(|_| MySqlQueryError::MissingTable)?;
        for foreign_key in &new_keys {
            let constraint = crate::show_create_table::foreign_key_name(
                table.as_str(),
                foreign_key,
                &btree.foreign_keys,
            );
            let Some(parent) = schema.get_btree_table(&foreign_key.parent_table) else {
                if self.inner.foreign_keys_enabled() {
                    return Err(self.foreign_key_definition_error(
                        MySqlForeignKeyDefinitionError::ParentTableMissing {
                            table: foreign_key.parent_table.clone(),
                        },
                    ));
                }
                continue;
            };
            let parent_name = MySqlTableName::parse(&parent.name)
                .map_err(|error| MySqlQueryError::Engine(LimboError::Corrupt(error.to_string())))?;
            let parent_columns = self
                .list_columns(&parent_name)
                .map_err(|_| MySqlQueryError::MissingTable)?;
            let mut pairs = Vec::with_capacity(foreign_key.child_columns.len());
            for (child, parent_column) in foreign_key
                .child_columns
                .iter()
                .zip(foreign_key.parent_columns.iter())
            {
                let Some(referenced) = parent_columns
                    .iter()
                    .find(|column| column.name().eq_ignore_ascii_case(parent_column))
                else {
                    return Err(self.foreign_key_definition_error(
                        MySqlForeignKeyDefinitionError::ParentColumnMissing {
                            column: parent_column.clone(),
                            constraint,
                            table: foreign_key.parent_table.clone(),
                        },
                    ));
                };
                let referencing = child_columns
                    .iter()
                    .find(|column| column.name().eq_ignore_ascii_case(child))
                    .ok_or_else(|| {
                        MySqlQueryError::Engine(LimboError::Corrupt(format!(
                            "foreign key column {child} is not in its table"
                        )))
                    })?;
                pairs.push((child, parent_column, referencing, referenced));
            }
            for (child, parent_column, referencing, referenced) in pairs {
                if StoredAs::of(referencing) != StoredAs::of(referenced) {
                    return Err(self.foreign_key_definition_error(
                        MySqlForeignKeyDefinitionError::IncompatibleColumns {
                            child: child.clone(),
                            parent: parent_column.clone(),
                            constraint,
                        },
                    ));
                }
            }
            let names_these_columns = |columns: &mut dyn Iterator<Item = &str>| {
                let columns = columns.collect::<Vec<_>>();
                columns.len() == foreign_key.parent_columns.len()
                    && columns
                        .iter()
                        .zip(foreign_key.parent_columns.iter())
                        .all(|(key, column)| key.eq_ignore_ascii_case(column))
            };
            let primary_key = &parent.primary_key_columns;
            let keyed = names_these_columns(&mut primary_key.iter().map(|(name, _)| name.as_str()))
                || schema
                    .get_indices(&parent.name)
                    .filter(|index| index.unique && index.where_clause.is_none())
                    .any(|index| {
                        names_these_columns(
                            &mut mysql_index_columns(index, primary_key)
                                .iter()
                                .map(|column| column.name.as_str()),
                        )
                    });
            if !keyed {
                return Err(self.foreign_key_definition_error(
                    MySqlForeignKeyDefinitionError::NoUniqueKeyInParent {
                        constraint,
                        table: foreign_key.parent_table.clone(),
                    },
                ));
            }
        }
        let mut taken = Vec::new();
        for (name, other) in schema.tables.iter() {
            let Some(other_btree) = other.btree() else {
                continue;
            };
            for foreign_key in &other_btree.foreign_keys {
                let is_new = name.eq_ignore_ascii_case(table.as_str())
                    && foreign_key.decl_order >= first_new;
                if !is_new {
                    taken.push(crate::show_create_table::foreign_key_name(
                        &other_btree.name,
                        foreign_key,
                        &other_btree.foreign_keys,
                    ));
                }
            }
        }
        for foreign_key in &new_keys {
            let name = crate::show_create_table::foreign_key_name(
                table.as_str(),
                foreign_key,
                &btree.foreign_keys,
            );
            if taken.iter().any(|other| other.eq_ignore_ascii_case(&name)) {
                return Err(self.foreign_key_definition_error(
                    MySqlForeignKeyDefinitionError::DuplicateName { name },
                ));
            }
            taken.push(name);
        }
        Ok(())
    }

    fn ensure_foreign_key_child_indexes(
        &self,
        table: &MySqlTableName,
        added_at: Option<usize>,
    ) -> std::result::Result<(), MySqlQueryError> {
        let mut foreign_keys = self
            .inner
            .current_schema()
            .get_btree_table(table.as_str())
            .ok_or_else(|| {
                MySqlQueryError::Engine(LimboError::Corrupt(
                    "foreign key table disappeared before its indexes were created".to_string(),
                ))
            })?
            .foreign_keys
            .iter()
            .map(|foreign_key| {
                (
                    foreign_key.decl_order,
                    foreign_key.name.clone(),
                    foreign_key.child_columns.to_vec(),
                )
            })
            .collect::<Vec<_>>();
        foreign_keys.sort_by_key(|(decl_order, _, _)| Some(*decl_order) != added_at);
        for (_, name, columns) in foreign_keys {
            let schema = self.inner.current_schema();
            let btree = schema.get_btree_table(table.as_str()).ok_or_else(|| {
                MySqlQueryError::Engine(LimboError::Corrupt(
                    "foreign key table disappeared while its indexes were created".to_string(),
                ))
            })?;
            let primary_covers = primary_key_covers_columns(&btree.primary_key_columns, &columns);
            let index_covers = schema
                .get_indices(table.as_str())
                .any(|index| index_covers_columns(index, &btree.primary_key_columns, &columns));
            if primary_covers || index_covers {
                continue;
            }
            let held = self
                .list_indexes(table)
                .map_err(|_| {
                    MySqlQueryError::Engine(LimboError::Corrupt(
                        "foreign key table indexes disappeared".to_string(),
                    ))
                })?
                .iter()
                .map(|index| index.key_name().to_owned())
                .collect::<Vec<_>>();
            let name = match name {
                Some(name) => {
                    if held.iter().any(|held| held.eq_ignore_ascii_case(&name)) {
                        return Err(MySqlQueryError::DuplicateIndex);
                    }
                    name
                }
                None => unnamed_index_name(&held, &columns).ok_or_else(|| {
                    MySqlQueryError::Engine(LimboError::Corrupt(
                        "foreign key has no child columns".to_string(),
                    ))
                })?,
            };
            let columns = columns
                .iter()
                .map(|column| mysql_quoted(column))
                .collect::<Vec<_>>()
                .join(", ");
            let sql = format!(
                "CREATE INDEX {} ON {} ({columns})",
                mysql_quoted(&name),
                mysql_quoted(table.as_str())
            );
            self.prepare_with_index_origin(&sql, true)
                .and_then(|mut statement| statement.run_ignore_rows())
                .map_err(MySqlQueryError::Engine)?;
        }
        Ok(())
    }

    fn remove_replaced_implicit_fk_indexes(
        &self,
        table: &MySqlTableName,
    ) -> std::result::Result<(), MySqlQueryError> {
        let schema = self.inner.current_schema();
        let btree = schema.get_btree_table(table.as_str()).ok_or_else(|| {
            MySqlQueryError::Engine(LimboError::Corrupt(
                "indexed table disappeared before redundant indexes were removed".to_string(),
            ))
        })?;
        let redundant = schema
            .get_indices(table.as_str())
            .filter(|index| is_implicit_index(&index.name))
            .filter(|candidate| {
                let using_candidate = btree
                    .foreign_keys
                    .iter()
                    .filter(|foreign_key| {
                        index_covers_columns(
                            candidate,
                            &btree.primary_key_columns,
                            &foreign_key.child_columns,
                        )
                    })
                    .collect::<Vec<_>>();
                !using_candidate.is_empty()
                    && using_candidate.iter().all(|foreign_key| {
                        primary_key_covers_columns(
                            &btree.primary_key_columns,
                            &foreign_key.child_columns,
                        ) || schema.get_indices(table.as_str()).any(|other| {
                            other.name != candidate.name
                                && !is_implicit_index(&other.name)
                                && index_covers_columns(
                                    other,
                                    &btree.primary_key_columns,
                                    &foreign_key.child_columns,
                                )
                        })
                    })
            })
            .map(|index| index.name.clone())
            .collect::<Vec<_>>();
        for name in redundant {
            let sql = format!("DROP INDEX {}", mysql_quoted(&name));
            let stmt = Stmt::DropIndex {
                if_exists: false,
                idx_name: turso_parser::ast::QualifiedName::single(turso_parser::ast::Name::exact(
                    name,
                )),
            };
            self.inner
                .prepare_translated_stmt(stmt, &sql)
                .and_then(|mut statement| statement.run_ignore_rows())
                .map_err(MySqlQueryError::Engine)?;
        }
        Ok(())
    }

    fn index_targets_json(&self, table: &str, indexed_columns: &[String]) -> Result<bool> {
        let table = MySqlTableName::parse(table)
            .map_err(|error| LimboError::ParseError(error.to_string()))?;
        let columns = match self.list_shared_columns(&table) {
            Ok(columns) => columns,
            Err(MySqlColumnMetadataError::TableNotFound) => return Ok(false),
            Err(error) => {
                return Err(LimboError::ParseError(format!(
                    "cannot validate indexed column types: {error}"
                )))
            }
        };
        Ok(indexed_columns.iter().any(|indexed| {
            columns.iter().any(|column| {
                column.name().eq_ignore_ascii_case(indexed)
                    && column.type_name().eq_ignore_ascii_case("json")
            })
        }))
    }

    /// Writes an `INSERT` out as the form the rest of this path reads.
    ///
    /// Two spellings say what the column-list form says: a `SELECT` with no
    /// column list, and `SET a = 1`. Writing them out here is what lets one set
    /// of rules answer all three — the `AUTO_INCREMENT` path, which reads only
    /// the column-list form, and the upsert clause, which the `SET` renderer
    /// had no way to carry.
    fn insert_written_out(
        &self,
        sql: &str,
    ) -> std::result::Result<Option<String>, MySqlQueryError> {
        if let Some(statement) = self.insert_select_column_list(sql)? {
            return Ok(Some(statement));
        }
        if let Some(statement) = self.insert_values_column_list(sql)? {
            return Ok(Some(statement));
        }
        turso_mysql_parser::parse_optional_insert_set_as_values(sql, self.parser_mode())
            .map_err(mysql_query_parse_error)
    }

    /// Writes out the column list an `INSERT INTO t VALUES (...)` leaves off.
    ///
    /// Measured on MySQL 8.4.11: the form means every column of the table, in
    /// order. It is how mysqldump writes every data row, so a dumped table's
    /// rows arrive in exactly this shape.
    ///
    /// The list is written into the statement rather than the statement being
    /// rendered again, because a written value's own spelling is the one thing
    /// that must not change on the way through. Answers `None` for every other
    /// statement, which keeps its own path.
    fn insert_values_column_list(
        &self,
        sql: &str,
    ) -> std::result::Result<Option<String>, MySqlQueryError> {
        let Some(checked) = turso_mysql_parser::parse_optional_insert_values_without_columns(
            sql,
            self.parser_mode(),
        )
        .map_err(mysql_query_parse_error)?
        else {
            return Ok(None);
        };
        let columns = self
            .list_shared_columns(checked.table())
            .map_err(|_| MySqlQueryError::Unsupported("INSERT VALUES table metadata".to_owned()))?
            .iter()
            .map(|column| mysql_quoted(column.name()))
            .collect::<Vec<_>>()
            .join(", ");
        if columns.is_empty() {
            return Ok(None);
        }
        let mut written = sql.to_owned();
        written.insert_str(checked.column_list_at(), &format!("({columns}) "));
        Ok(Some(written))
    }

    /// Writes out the column list an `INSERT INTO t <SELECT>` leaves off.
    ///
    /// Measured on MySQL 8.4.11: the form means every column of the table, in
    /// order — `INSERT INTO dst SELECT * FROM src` copies all three columns.
    /// Answers `None` for every other statement, which keeps its own path.
    fn insert_select_column_list(
        &self,
        sql: &str,
    ) -> std::result::Result<Option<String>, MySqlQueryError> {
        let Some(checked) = turso_mysql_parser::parse_optional_insert_select_without_columns(
            sql,
            self.parser_mode(),
        )
        .map_err(mysql_query_parse_error)?
        else {
            return Ok(None);
        };
        let columns = self
            .list_shared_columns(checked.table())
            .map_err(|_| MySqlQueryError::Unsupported("INSERT SELECT table metadata".to_owned()))?
            .iter()
            .map(|column| mysql_quoted(column.name()))
            .collect::<Vec<_>>()
            .join(", ");
        Ok(Some(format!(
            "{} {} ({columns}) {}",
            if checked.replaces() {
                "REPLACE INTO"
            } else if checked.ignores() {
                "INSERT IGNORE INTO"
            } else {
                "INSERT INTO"
            },
            mysql_quoted(checked.table().as_str()),
            checked.select_sql()
        )))
    }

    fn run_internal(&self, sql: &str) -> std::result::Result<(), MySqlQueryError> {
        self.inner
            .prepare(sql)
            .and_then(|mut statement| statement.run_ignore_rows())
            .map(|_| ())
            .map_err(MySqlQueryError::Engine)
    }

    /// Drops the views one `DROP VIEW` names, committing preceding work first.
    ///
    /// Measured on MySQL 8.4.11: a name given twice is 1066 whatever else the
    /// statement names; without `IF EXISTS`, a table among the names is 1347
    /// and otherwise a missing name is 1051, and either drops none of the
    /// others; with it, every view named is dropped and each other name is
    /// noted, in the order the statement named them.
    pub fn drop_views(
        &self,
        command: &MySqlDropViewCommand,
    ) -> std::result::Result<Vec<MySqlSkippedView>, MySqlDropViewError> {
        let mut named: Vec<&MySqlTableName> = Vec::with_capacity(command.views().len());
        for view in command.views() {
            if named.contains(&view) {
                return Err(MySqlDropViewError::NamedTwice);
            }
            named.push(view);
        }
        if !self.inner.get_auto_commit() {
            self.inner
                .prepare("COMMIT")
                .and_then(|mut statement| statement.run_ignore_rows())
                .map_err(MySqlDropViewError::Engine)?;
        }
        let tables = self.list_tables().map_err(MySqlDropViewError::Engine)?;
        let mut present = Vec::with_capacity(named.len());
        let mut skipped = Vec::new();
        for view in named {
            match tables.iter().find(|table| table.name() == view.as_str()) {
                Some(table) if table.kind() == MySqlTableKind::View => present.push(view),
                Some(_) => skipped.push(MySqlSkippedView::NotView(view.as_str().to_owned())),
                None => skipped.push(MySqlSkippedView::Missing(view.as_str().to_owned())),
            }
        }
        if !command.if_exists() {
            if skipped
                .iter()
                .any(|skipped| matches!(skipped, MySqlSkippedView::NotView(_)))
            {
                return Err(MySqlDropViewError::NotView);
            }
            if !skipped.is_empty() {
                return Err(MySqlDropViewError::MissingView);
            }
        }
        let mut result = Ok(());
        for view in present {
            let stmt = Stmt::DropView {
                if_exists: false,
                view_name: turso_parser::ast::QualifiedName::single(
                    turso_parser::ast::Name::exact(view.as_str().to_owned()),
                ),
            };
            let sql = format!("DROP VIEW \"{}\"", view.as_str().replace('"', "\"\""));
            result = self
                .inner
                .prepare_translated_stmt(stmt, &sql)
                .and_then(|mut statement| statement.run_ignore_rows())
                .map_err(MySqlDropViewError::Engine);
            if result.is_err() {
                break;
            }
        }
        if !self.inner.get_auto_commit() {
            self.inner
                .prepare("ROLLBACK")
                .and_then(|mut statement| statement.run_ignore_rows())
                .map_err(MySqlDropViewError::Engine)?;
        }
        result.map(|()| skipped)
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
        let mut present = Vec::with_capacity(command.tables().len());
        let mut missing = Vec::new();
        for named in command.tables() {
            if present.contains(named) {
                return Err(MySqlDropTableError::NamedTwice);
            }
            match tables.iter().find(|table| table.name() == named.as_str()) {
                Some(table) if table.kind() == MySqlTableKind::BaseTable => {
                    present.push(named.clone());
                }
                _ => missing.push(named.as_str().to_owned()),
            }
        }
        // Measured on MySQL 8.4.11: a statement naming a table that is not
        // there drops none of the others, and `IF EXISTS` drops the ones that
        // are and notes each one that is not.
        if !missing.is_empty() && !command.if_exists() {
            return Err(MySqlDropTableError::MissingTable);
        }
        // Measured on MySQL 8.4.11: with foreign key checks on, a table
        // another table's foreign key names answers 3730 and the statement
        // drops nothing, `IF EXISTS` and a trailing `CASCADE` alike, unless
        // the same statement drops that other table too; a table whose key
        // names itself is dropped. With the checks off it goes.
        if self.inner.foreign_keys_enabled()
            && self
                .another_table_names_one_of(&present)
                .map_err(MySqlDropTableError::Engine)?
        {
            return Err(MySqlDropTableError::ReferencedByForeignKey);
        }
        let mut result = Ok(());
        for table in self.children_before_parents(present) {
            let stmt = Stmt::DropTable {
                if_exists: false,
                tbl_name: turso_parser::ast::QualifiedName::single(turso_parser::ast::Name::exact(
                    table.as_str().to_owned(),
                )),
            };
            let sql = format!("DROP TABLE \"{}\"", table.as_str().replace('"', "\"\""));
            result = self
                .inner
                .prepare_translated_stmt(stmt, &sql)
                .and_then(|mut statement| statement.run_ignore_rows())
                .map_err(MySqlDropTableError::Engine);
            if result.is_err() {
                break;
            }
        }
        if !self.inner.get_auto_commit() {
            self.inner
                .prepare("ROLLBACK")
                .and_then(|mut statement| statement.run_ignore_rows())
                .map_err(MySqlDropTableError::Engine)?;
        }
        result.map(|()| MySqlDropTableResult { missing })
    }

    /// Orders tables dropped together so that a table goes before any table
    /// its foreign keys name, since MySQL drops a parent and its child in one
    /// statement whatever order it names them in.
    fn children_before_parents(&self, mut remaining: Vec<MySqlTableName>) -> Vec<MySqlTableName> {
        let schema = self.inner.current_schema();
        let parents_of = |table: &MySqlTableName| -> Vec<String> {
            schema
                .get_btree_table(table.as_str())
                .map(|table| {
                    table
                        .foreign_keys
                        .iter()
                        .map(|key| key.parent_table.to_lowercase())
                        .collect()
                })
                .unwrap_or_default()
        };
        let mut ordered = Vec::with_capacity(remaining.len());
        while !remaining.is_empty() {
            let unreferenced = remaining.iter().position(|candidate| {
                !remaining.iter().any(|other| {
                    other != candidate
                        && parents_of(other).contains(&candidate.as_str().to_lowercase())
                })
            });
            // Tables naming each other in a ring have no first one; the engine
            // then answers for the order they were named in.
            ordered.push(remaining.remove(unreferenced.unwrap_or(0)));
        }
        ordered
    }

    /// Empties one checked table, committing before and after like MySQL's DDL.
    ///
    /// Measured on MySQL 8.4.11: `TRUNCATE TABLE` reports no affected rows, and
    /// a `ROLLBACK` after one leaves the table empty, so the statement commits
    /// what came before it and cannot itself be undone. The engine has no
    /// `TRUNCATE`, so an unfiltered `DELETE` does the emptying between those two
    /// commits.
    pub fn truncate_table(
        &self,
        command: &MySqlTruncateTableCommand,
    ) -> std::result::Result<(), MySqlTruncateTableError> {
        if !self.inner.get_auto_commit() {
            self.inner
                .prepare("COMMIT")
                .and_then(|mut statement| statement.run_ignore_rows())
                .map_err(MySqlTruncateTableError::Engine)?;
        }
        let tables = self
            .list_tables()
            .map_err(MySqlTruncateTableError::Engine)?;
        match tables
            .iter()
            .find(|table| table.name() == command.table().as_str())
        {
            Some(table) if table.kind() == MySqlTableKind::BaseTable => {}
            // Measured on MySQL 8.4.11: a view answers 1146, the same unknown
            // table a name nothing carries answers.
            _ => return Err(MySqlTruncateTableError::MissingTable),
        }
        // Measured on MySQL 8.4.11: with foreign key checks on, a table another
        // table's foreign key names answers 1701 whatever it holds — the rows
        // are not checked away one by one, so there is no child row for the
        // emptying to fail against. With the checks off it goes ahead and
        // leaves the child rows pointing at nothing, which is what a test
        // suite's teardown asks for when it turns them off to empty every
        // table.
        if self.inner.foreign_keys_enabled()
            && self
                .a_foreign_key_names(command.table().as_str())
                .map_err(MySqlTruncateTableError::Engine)?
        {
            return Err(MySqlTruncateTableError::ReferencedByForeignKey);
        }
        let counted = self
            .load_auto_increment_table(command.table().as_str())
            .map_err(MySqlTruncateTableError::Engine)?;
        let result = match counted {
            Some(table) => self.write_the_counted_table_again(&table),
            None if self.empties_by_writing_the_table_again(command.table().as_str()) => {
                self.write_the_table_again_empty(command.table().as_str())
            }
            None => {
                let sql = format!(
                    "DELETE FROM \"{}\"",
                    command.table().as_str().replace('"', "\"\"")
                );
                self.inner
                    .prepare(&sql)
                    .and_then(|mut statement| statement.run_ignore_rows())
                    .map_err(MySqlTruncateTableError::Engine)
                    .map(|_| ())
            }
        };
        if !self.inner.get_auto_commit() {
            self.inner
                .prepare("COMMIT")
                .and_then(|mut statement| statement.run_ignore_rows())
                .map_err(MySqlTruncateTableError::Engine)?;
        }
        result
    }

    /// Whether `TRUNCATE` empties a table that does not count its ids by
    /// dropping it and making it again, as MySQL does, rather than by deleting
    /// its rows.
    ///
    /// Under MVCC the difference shows: measured on MySQL 8.4.11, a
    /// transaction whose read view is older than the `TRUNCATE` answers 1412
    /// when it reads or deletes from the table, as it does after any other
    /// definition change, and that needs the engine to see one. A table with
    /// triggers is emptied by deleting, since the triggers would not come back
    /// with it.
    fn empties_by_writing_the_table_again(&self, table: &str) -> bool {
        self.inner.mvcc_enabled()
            && self
                .inner
                .current_schema()
                .get_triggers_for_table(table)
                .next()
                .is_none()
    }

    /// Empties a table by writing it again from the MySQL `CREATE TABLE` it
    /// was stored as, its indexes beside it.
    fn write_the_table_again_empty(
        &self,
        table: &str,
    ) -> std::result::Result<(), MySqlTruncateTableError> {
        let statement = self
            .stored_table_statement(table)
            .map_err(MySqlTruncateTableError::Engine)?
            .ok_or_else(|| {
                MySqlTruncateTableError::Engine(LimboError::Corrupt(format!(
                    "table {table} has no stored MySQL definition"
                )))
            })?;
        let indexes = self
            .stored_index_statements(table)
            .map_err(MySqlTruncateTableError::Engine)?;
        let dropped = Stmt::DropTable {
            if_exists: false,
            tbl_name: turso_parser::ast::QualifiedName::single(turso_parser::ast::Name::exact(
                table.to_owned(),
            )),
        };
        let sql = format!("DROP TABLE \"{}\"", table.replace('"', "\"\""));
        self.inner
            .prepare_translated_stmt(dropped, &sql)
            .and_then(|mut statement| statement.run_ignore_rows())
            .map_err(MySqlTruncateTableError::Engine)?;
        self.prepare(&statement)
            .and_then(|mut statement| statement.run_ignore_rows())
            .map_err(MySqlTruncateTableError::Engine)?;
        for index in &indexes {
            self.prepare_with_index_origin(&index.sql, index.implicit)
                .and_then(|mut statement| statement.run_ignore_rows())
                .map_err(MySqlTruncateTableError::Engine)?;
        }
        Ok(())
    }

    /// Whether any table's foreign key names this one as its parent.
    /// Whether a table outside `dropped` has a foreign key naming one of them.
    fn another_table_names_one_of(&self, dropped: &[MySqlTableName]) -> Result<bool> {
        let schema = self.inner.current_schema();
        let is_dropped = |name: &str| {
            dropped
                .iter()
                .any(|table| table.as_str().eq_ignore_ascii_case(name))
        };
        Ok(self.list_tables()?.iter().any(|listed| {
            !is_dropped(listed.name())
                && schema
                    .get_table(listed.name())
                    .and_then(|core_table| core_table.btree())
                    .is_some_and(|btree| {
                        btree
                            .foreign_keys
                            .iter()
                            .any(|key| is_dropped(&key.parent_table))
                    })
        }))
    }

    fn a_foreign_key_names(&self, table: &str) -> Result<bool> {
        let schema = self.inner.current_schema();
        Ok(self.list_tables()?.iter().any(|listed| {
            schema
                .get_table(listed.name())
                .and_then(|core_table| core_table.btree())
                .is_some_and(|btree| {
                    btree
                        .foreign_keys
                        .iter()
                        .any(|key| key.parent_table.eq_ignore_ascii_case(table))
                })
        }))
    }

    /// Empties a table that counts its own ids by writing it again.
    ///
    /// MySQL restarts the counter at 1 — measured on 8.4.11, a truncated table
    /// prints no `AUTO_INCREMENT` trailer at all and its next row takes 1 —
    /// and the durable allocator only ever moves its high water forward, so
    /// there is no winding it back. What MySQL's own `TRUNCATE` does is drop
    /// the table and make it again, and that is what happens here: the table is
    /// written again from what it was stored as, taking a fresh allocator
    /// identity, which counts from 1 the way a new table's does. Its indexes
    /// are written again beside it, from what they were stored as.
    fn write_the_counted_table_again(
        &self,
        table: &AutoIncrementTable,
    ) -> std::result::Result<(), MySqlTruncateTableError> {
        // A trigger is not the table's own row and would not come back with
        // it, where MySQL leaves one where it stood.
        self.reject_insert_target_triggers(&table.name)
            .map_err(MySqlTruncateTableError::Engine)?;
        let indexes = self
            .stored_index_statements(&table.name)
            .map_err(MySqlTruncateTableError::Engine)?;
        let dropped = Stmt::DropTable {
            if_exists: false,
            tbl_name: turso_parser::ast::QualifiedName::single(turso_parser::ast::Name::exact(
                table.name.clone(),
            )),
        };
        let sql = format!("DROP TABLE \"{}\"", table.name.replace('"', "\"\""));
        self.inner
            .prepare_translated_stmt(dropped, &sql)
            .and_then(|mut statement| statement.run_ignore_rows())
            .map_err(MySqlTruncateTableError::Engine)?;
        self.prepare(&table.definition.normalized_mysql_ddl)
            .and_then(|mut statement| statement.run_ignore_rows())
            .map_err(MySqlTruncateTableError::Engine)?;
        for index in &indexes {
            self.prepare_with_index_origin(&index.sql, index.implicit)
                .and_then(|mut statement| statement.run_ignore_rows())
                .map_err(MySqlTruncateTableError::Engine)?;
        }
        Ok(())
    }

    /// The MySQL `CREATE INDEX` each of one table's stored indexes was written
    /// as, for the indexes that carry a statement of their own.
    fn stored_index_statements(&self, table: &str) -> Result<Vec<StoredIndexStatement>> {
        let rows = self
            .inner
            .prepare(
                "SELECT tbl_name, sql FROM sqlite_schema \
                 WHERE type = 'index' AND sql IS NOT NULL",
            )?
            .run_collect_rows()?;
        let mut statements = Vec::new();
        for row in rows {
            let [owner, sql] = row.as_slice() else {
                return Err(LimboError::InternalError(
                    "sqlite_schema index row has an invalid shape".to_string(),
                ));
            };
            if !owner
                .to_string()
                .trim_matches('\'')
                .eq_ignore_ascii_case(table)
            {
                continue;
            }
            let sql = sql.to_string();
            let Some(decoded) = decode_schema_sql(SchemaSqlKind::Index, sql.trim_matches('\''))
                .map_err(|error| LimboError::Corrupt(error.to_string()))?
            else {
                continue;
            };
            let mut statement = parse_schema_ddl_ast(decoded.normalized_ddl, self.parser_mode())
                .map_err(|error| LimboError::Corrupt(error.to_string()))?;
            let Stmt::CreateIndex {
                idx_name, columns, ..
            } = &mut statement
            else {
                return Err(LimboError::Corrupt(
                    "marked index SQL did not describe an index".to_string(),
                ));
            };
            let stored_name = idx_name.name.as_str().to_owned();
            columns.truncate(self.columns_mysql_shows_of(table, &stored_name)?);
            let implicit = is_implicit_index(&stored_name);
            if let Some(logical_name) = logical_mysql_index_name(&stored_name) {
                idx_name.name = turso_parser::ast::Name::exact(logical_name);
            }
            statements.push(StoredIndexStatement {
                sql: render_create_index_mysql_with_mode(&statement, self.parser_mode())
                    .map_err(|error| LimboError::Corrupt(error.to_string()))?,
                stored_name,
                implicit,
            });
        }
        Ok(statements)
    }

    fn columns_mysql_shows_of(&self, table: &str, stored_name: &str) -> Result<usize> {
        let schema = self.inner.current_schema();
        let btree = schema.get_btree_table(table).ok_or_else(|| {
            LimboError::Corrupt(format!("index {stored_name} names a missing table {table}"))
        })?;
        let index = schema
            .get_indices(table)
            .find(|index| index.name == stored_name)
            .ok_or_else(|| {
                LimboError::Corrupt(format!("stored index {stored_name} is not in the schema"))
            })?;
        Ok(mysql_index_columns(index, &btree.primary_key_columns).len())
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
        if checked
            .primary_key_integer_type
            .is_some_and(|key| key.fits_the_rowid())
        {
            return self.prepare_rowid_keyed_create_table(checked);
        }
        self.prepare_index_keyed_create_table(checked)
    }

    /// Makes a table the way every table with a primary key was made before
    /// an integer key became the rowid, for the tests that hold such a table
    /// to the same rules a database made then still has.
    #[cfg(test)]
    pub(crate) fn create_table_keyed_by_an_index(&self, sql: &str) -> Result<()> {
        let checked = parse_checked_primary_key_create_table(sql, self.parser_mode())
            .map_err(|error| LimboError::ParseError(error.to_string()))?;
        self.prepare_index_keyed_create_table(checked)?
            .run_ignore_rows()
    }

    /// Makes a table whose primary key is a unique index over a rowid of its
    /// own, which is how a key over a word or a `BIGINT UNSIGNED` is kept.
    fn prepare_index_keyed_create_table(
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

    /// Makes a table whose one integer primary key is the engine's rowid.
    ///
    /// InnoDB keeps a table's rows in the order of its primary key and finds a
    /// row by key in that one tree, which is what a rowid key does here: no
    /// index of its own to keep beside the rows, and a range of keys reads and
    /// locks the rows in key order. MySQL's rules for the key's value — a row
    /// must give it, and NULL is refused — are kept by the engine for a column
    /// that must be written.
    fn prepare_rowid_keyed_create_table(
        &self,
        checked: CheckedPrimaryKeyCreateTable,
    ) -> Result<Statement> {
        let sqlite_statement =
            turso_mysql_parser::with_the_primary_key_as_the_rowid(checked.sqlite_statement)
                .map_err(|error| LimboError::ParseError(error.to_string()))?;
        let formatter = RowidKeySchemaSqlFormatter {
            context: self.schema_context,
            normalized_mysql_ddl: checked.normalized_mysql_ddl.clone(),
            sqlite_statement: sqlite_statement.clone(),
        };
        let options = PrepareOptions::default()
            .with_reprepare_parser(Arc::new(FrozenRowidKeyDdlParser {
                mode: self.parser_mode(),
            }))
            .with_schema_sql_formatter(Arc::new(formatter));
        self.inner.prepare_translated_stmt_with_options(
            sqlite_statement,
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
    ) -> std::result::Result<(Statement, Vec<Option<StaticSelectMetadata>>), MySqlQueryError> {
        let (translated, rendered_differently) = self.parse_select_knowing_column_types(sql)?;
        Self::reject_internal_catalog_select(&translated)?;
        self.reject_binary_scalar_collation(&translated)?;
        self.refuse_select_json_readings_of_other_columns(&translated)?;
        self.hold_the_projection_to_what_the_keys_decide(&translated)?;
        self.hold_bare_names_in_result_subqueries_to_their_tables(&translated)?;
        Self::reject_raw_select_comparisons(&translated)?;
        self.reject_index_hints_naming_no_key(&translated)?;
        self.validate_select_comparison_columns(
            translated.source_tables(),
            translated.checked_comparisons(),
        )
        .map_err(|error| MySqlQueryError::Unsupported(error.to_string()))?;
        self.validate_select_subquery_comparison_columns(
            translated.source_table(),
            translated.source_tables(),
            translated.checked_subquery_comparisons(),
        )
        .map_err(|error| MySqlQueryError::Unsupported(error.to_string()))?;
        if translated.reads_table() {
            self.begin_implicit_transaction_for_table_read()?;
        }
        let serializable_read = (!translated.locks_rows())
            .then(|| self.serializable_read_of_a_table(translated.reads_table()))
            .flatten();
        if translated.reads_table() && !translated.locks_rows() && serializable_read.is_none() {
            self.note_consistent_read();
        }
        let locking_read = translated.locking_read();
        if let Some(locking_read) = locking_read.filter(|_| !self.inner.mvcc_enabled()) {
            if locking_read.wait == MySqlRowLockWait::NoWait {
                return Err(MySqlQueryError::Unsupported(
                    "SELECT locking clause option".to_string(),
                ));
            }
            self.take_the_write_lock(translated.source_tables())?;
        }
        let stmt = translated
            .parse_ast()
            .map_err(|error| MySqlQueryError::Syntax(error.to_string()))?;
        self.validate_session_timestamp_select(&translated, &stmt)?;
        let frozen = self.frozen_select_parser(&translated, rendered_differently, &stmt);
        let options = PrepareOptions::default().with_reprepare_parser(Arc::new(frozen));
        let mut stmt = self
            .inner
            .prepare_translated_stmt_with_options(stmt, sql, &options)
            .map_err(MySqlQueryError::Engine)?;
        if stmt.parameters_count() != translated.parameter_count() {
            return Err(MySqlQueryError::Engine(LimboError::InternalError(
                "checked SELECT parameter count changed during prepare".to_string(),
            )));
        }
        if let Some(locking_read) = locking_read.filter(|_| self.inner.mvcc_enabled()) {
            lock_the_rows_a_select_reads(&mut stmt, locking_read, translated.source_tables())
                .map_err(MySqlQueryError::Engine)?;
        }
        if let Some(read) = serializable_read {
            lock_the_rows_a_serializable_select_reads(&mut stmt, read, translated.source_tables())
                .map_err(MySqlQueryError::Engine)?;
        }
        let static_result_metadata =
            aligned_static_result_metadata(&stmt, translated.static_result_metadata());
        Ok((stmt, static_result_metadata))
    }

    fn validate_session_timestamp_select(
        &self,
        translated: &turso_mysql_parser::TranslatedSelect,
        statement: &Stmt,
    ) -> std::result::Result<(), MySqlQueryError> {
        if self.time_zone_offset_seconds() == 0 {
            return Ok(());
        }
        let reads_timestamp =
            translated
                .source_tables()
                .iter()
                .try_fold(false, |seen, source| {
                    self.list_shared_columns(source.table())
                        .map(|columns| {
                            seen || columns
                                .iter()
                                .any(|column| column.type_name() == "TIMESTAMP")
                        })
                        .map_err(|error| MySqlQueryError::Unsupported(error.to_string()))
                })?;
        if !reads_timestamp {
            return Ok(());
        }
        let Stmt::Select(select) = statement else {
            return Err(MySqlQueryError::Unsupported(
                "TIMESTAMP query shape".to_owned(),
            ));
        };
        let safe = select.with.is_none()
            && select.body.compounds.is_empty()
            && select.order_by.is_empty()
            && translated.source_tables().len() == 1
            && matches!(
                &select.body.select,
                OneSelect::Select {
                    columns,
                    from: Some(from),
                    where_clause: None,
                    group_by: None,
                    window_clause,
                    ..
                } if from.joins.is_empty()
                    && matches!(&*from.select, SelectTable::Table(..))
                    && window_clause.is_empty()
                    && columns.iter().all(|column| match column {
                        ResultColumn::Star | ResultColumn::TableStar(_) => true,
                        ResultColumn::Expr(expr, _) => {
                            matches!(
                                &**expr,
                                Expr::Name(_)
                                    | Expr::Id(_)
                                    | Expr::Qualified(_, _)
                                    | Expr::DoublyQualified(_, _, _)
                            )
                        }
                    })
            );
        if safe {
            Ok(())
        } else {
            Err(MySqlQueryError::Unsupported(
                "TIMESTAMP queries with a non-UTC time zone require direct columns from one table without filtering or expressions".to_owned(),
            ))
        }
    }

    fn frozen_select_parser(
        &self,
        translated: &turso_mysql_parser::TranslatedSelect,
        typed_rendering: bool,
        statement: &Stmt,
    ) -> FrozenSelectParser {
        let mode = self.parser_mode();
        let reads_type_specific_column = translated.source_tables().iter().any(|source| {
            self.list_shared_columns(source.table())
                .is_ok_and(|columns| {
                    columns.iter().any(|column| {
                        column.decimal_size().is_some() || column.type_name() == "JSON"
                    })
                })
        });
        let typed_statement = (typed_rendering
            && (reads_type_specific_column || translated.reads_a_lateral_table()))
        .then(|| statement.clone());
        let mut table_definitions = Vec::new();
        let mut source_columns = Vec::new();
        let mut untracked_source = false;
        if typed_statement.is_some()
            || typed_rendering
            || translated.needs_column_types()
            || !translated.checked_comparisons().is_empty()
            || !translated.json_reading_columns().is_empty()
        {
            for source in translated.source_tables() {
                if let Some(table) = self
                    .inner
                    .current_schema()
                    .get_btree_table(source.table().as_str())
                {
                    if typed_statement.is_some() {
                        table_definitions
                            .push((source.table().as_str().to_owned(), table.to_sql()));
                    }
                    source_columns.push((
                        source.table().as_str().to_owned(),
                        table
                            .columns()
                            .iter()
                            .map(|column| format!("{column:?}"))
                            .collect(),
                    ));
                } else if source.catalog().is_none() {
                    // An `information_schema` table's columns are fixed when
                    // it is registered, so a reprepare — which switching
                    // `foreign_key_checks` asks for too — reads it as before.
                    untracked_source = true;
                }
            }
        }
        let source_is_catalog = translated.source_table().is_some_and(|table| {
            translated.source_tables().iter().any(|source| {
                source.catalog().is_some() && source.table().as_str().eq_ignore_ascii_case(table)
            })
        });
        FrozenSelectParser {
            mode,
            source_table: translated.source_table().map(str::to_owned),
            source_is_catalog,
            checked_comparisons: translated.checked_comparisons().to_vec(),
            typed_statement,
            table_definitions,
            source_columns,
            untracked_source,
        }
    }

    fn prepare_non_schema(&self, sql: &str) -> Result<Statement> {
        let mode = self.parser_mode();
        match self.parse_checked_dml_translation(sql, mode) {
            Ok((translated, column_types, table_definition)) => {
                self.validate_dml_comparison_columns(&translated)?;
                self.reject_non_utc_timestamp_dml_source(&translated)
                    .map_err(|error| LimboError::ParseError(error.to_string()))?;
                let mut stmt = translated
                    .parse_ast()
                    .map_err(|error| LimboError::ParseError(error.to_string()))?;
                let shifted = self
                    .shift_timestamp_insert_literals(&mut stmt)
                    .map_err(|error| LimboError::ParseError(error.to_string()))?;
                let mut frozen =
                    self.frozen_dml_parser(mode, column_types, table_definition, &translated);
                if shifted {
                    frozen.shifted_timestamp_insert = Some(stmt.clone());
                }
                let options = PrepareOptions::default()
                    .with_reprepare_parser(Arc::new(frozen))
                    .with_rows_foreign_keys_refuse_skipped(
                        turso_mysql_parser::deletes_ignoring_errors(sql, mode),
                    );
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

    /// Holds an index hint to the keys the table actually has.
    ///
    /// The hint says which key to plan with and nothing about which rows come
    /// back, so the renderer drops it. What it does say is that the key
    /// exists: measured on MySQL 8.4.11, `FORCE INDEX (by_nothing)` answers
    /// 1176 rather than the rows, so a statement naming a key the table has
    /// not got is turned away here rather than answered.
    fn reject_index_hints_naming_no_key(
        &self,
        translated: &turso_mysql_parser::TranslatedSelect,
    ) -> std::result::Result<(), MySqlQueryError> {
        for source in translated.source_tables() {
            if source.hinted_indexes().is_empty() {
                continue;
            }
            let schema = self.inner.current_schema();
            let table = source.table().as_str();
            let Some(btree) = schema.get_btree_table(table) else {
                return Err(MySqlQueryError::Unsupported(format!(
                    "index hint names a table the session cannot see: {table}"
                )));
            };
            for named in source.hinted_indexes() {
                // MySQL calls a table's primary key `PRIMARY` whatever the
                // stored DDL named it, which is the name `SHOW INDEX` reports.
                let holds = if named.eq_ignore_ascii_case("PRIMARY") {
                    !btree.primary_key_columns.is_empty()
                } else {
                    the_index_named(&schema, table, named).is_some()
                };
                if !holds {
                    return Err(MySqlQueryError::Unsupported(format!(
                        "key '{named}' does not exist in table '{table}'"
                    )));
                }
            }
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
            // One written inside a subquery names its column among that
            // subquery's tables, which only a `SELECT` records.
            if !comparison.inner_sources().is_empty() {
                return Err(LimboError::InvalidArgument(
                    "a membership test inside a subquery of a statement that writes".to_string(),
                ));
            }
            let source_table = source_table.ok_or_else(|| {
                LimboError::InvalidArgument(
                    "SELECT IN requires a table column on its left".to_string(),
                )
            })?;
            if comparison
                .qualifier()
                .is_some_and(|qualifier| qualifier != source_table)
            {
                return Err(LimboError::InvalidArgument(format!(
                    "a column compared with a subquery has to be one of {source_table}"
                )));
            }
            let inner_table = self.membership_inner_table(comparison)?;
            if !comparison.fixed_columns().is_empty() {
                self.hold_a_subquery_to_one_row(&inner_table, comparison.fixed_columns())?;
            }
            let outer = self.column_kind(source_table, comparison.column_name())?;
            let inner = self.column_kind(&inner_table, comparison.inner_column_name())?;
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

    /// The check `validate_subquery_comparison_columns` makes, for a `SELECT`
    /// that may join tables or nest one membership test inside another.
    ///
    /// Over a join the outer column is named through one of the joined
    /// tables, or without one, when it is the column of the joined table that
    /// has it — Gitea's `team.id IN (SELECT team_id FROM team_unit ...)` over
    /// `team INNER JOIN team_repo`. A name two joined tables have is refused
    /// by the engine as ambiguous.
    fn validate_select_subquery_comparison_columns(
        &self,
        source_table: Option<&str>,
        source_tables: &[MySqlSelectSource],
        comparisons: &[CheckedSubqueryComparison],
    ) -> Result<()> {
        // A membership test written inside a subquery names its column among
        // that subquery's tables first — Gitea's `repo_id IN (SELECT id FROM
        // repository WHERE repository.owner_id NOT IN (SELECT ...))`.
        let (nested, outer): (Vec<_>, Vec<_>) = comparisons
            .iter()
            .cloned()
            .partition(|comparison| !comparison.inner_sources().is_empty());
        if source_table.is_some() {
            self.validate_subquery_comparison_columns(source_table, &outer)?;
        } else {
            self.validate_membership_columns_by_name(source_tables, &outer)?;
        }
        self.validate_membership_columns_by_name(source_tables, &nested)
    }

    /// Holds each membership test's two columns to one kind, finding the
    /// outer column by name: through its qualifier, or among the tables of
    /// the subquery it stands in and then the statement's own, the first that
    /// has it.
    fn validate_membership_columns_by_name(
        &self,
        source_tables: &[MySqlSelectSource],
        comparisons: &[CheckedSubqueryComparison],
    ) -> Result<()> {
        if comparisons.is_empty() {
            return Ok(());
        }
        if source_tables
            .iter()
            .any(|source| source.catalog().is_some())
        {
            return Err(LimboError::InvalidArgument(
                "SELECT IN over a join reading an information_schema table".to_string(),
            ));
        }
        for comparison in comparisons {
            let mut outer_table = None;
            for table in column_tables(
                source_tables,
                comparison.qualifier(),
                comparison.inner_sources(),
            )? {
                if self
                    .compared_column_metadata(&table, comparison.column_name())?
                    .is_some()
                {
                    outer_table = Some(table);
                    break;
                }
            }
            let outer_table = outer_table.ok_or(LimboError::SchemaUpdated)?;
            let inner_table = self.membership_inner_table(comparison)?;
            if !comparison.fixed_columns().is_empty() {
                self.hold_a_subquery_to_one_row(&inner_table, comparison.fixed_columns())?;
            }
            let outer = self.column_kind(outer_table.as_str(), comparison.column_name())?;
            let inner = self.column_kind(&inner_table, comparison.inner_column_name())?;
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

    /// Holds a subquery answering a plain column to one row at most: the
    /// columns its `WHERE` fixes to one value each have to cover the table's
    /// primary key or a unique key over columns that are never NULL.
    ///
    /// Measured on MySQL 8.4.11, a subquery standing for a value that finds
    /// two rows answers 1242, where the engine takes the first it finds.
    fn hold_a_subquery_to_one_row(&self, table: &str, fixed: &[String]) -> Result<()> {
        let refused = || {
            LimboError::InvalidArgument(format!(
                "a subquery standing for one value has to pick its row of {table} by a key"
            ))
        };
        let schema = self.inner.current_schema();
        let btree = schema.get_btree_table(table).ok_or_else(refused)?;
        let is_fixed = |name: &str| fixed.iter().any(|column| column.eq_ignore_ascii_case(name));
        if !btree.primary_key_columns.is_empty()
            && btree
                .primary_key_columns
                .iter()
                .all(|(name, _)| is_fixed(name))
        {
            return Ok(());
        }
        let table_name = MySqlTableName::parse(table)
            .map_err(|error| LimboError::ParseError(error.to_string()))?;
        let columns = self
            .list_shared_columns(&table_name)
            .map_err(|_| refused())?;
        let never_null = |name: &str| {
            columns
                .iter()
                .any(|column| column.name().eq_ignore_ascii_case(name) && !column.nullable())
        };
        let a_unique_key_is_fixed = schema.get_indices(table).any(|index| {
            index.unique
                && index.where_clause.is_none()
                && index
                    .columns
                    .iter()
                    .all(|column| never_null(&column.name) && is_fixed(&column.name))
        });
        if a_unique_key_is_fixed {
            Ok(())
        } else {
            Err(refused())
        }
    }

    /// Returns whether one column holds signed integers or text, refusing the
    /// types this has no comparison rule for.
    fn membership_inner_table(&self, comparison: &CheckedSubqueryComparison) -> Result<String> {
        if comparison.inner_candidates().is_empty() {
            return Ok(comparison.inner_table().to_owned());
        }
        let mut holding = Vec::new();
        for candidate in comparison.inner_candidates() {
            let table = MySqlTableName::parse(candidate)
                .map_err(|error| LimboError::ParseError(error.to_string()))?;
            if self
                .compared_column_metadata(&table, comparison.inner_column_name())?
                .is_some()
                && !holding.contains(candidate)
            {
                holding.push(candidate.clone());
            }
        }
        match holding.as_slice() {
            [table] => Ok(table.clone()),
            _ => Err(LimboError::InvalidArgument(format!(
                "SELECT IN over a join projecting {}, which no one of its tables alone holds",
                comparison.inner_column_name()
            ))),
        }
    }

    fn column_kind(&self, table: &str, column_name: &str) -> Result<ColumnKind> {
        // An `information_schema` table declares its columns itself rather
        // than in stored DDL.
        if let Some(catalog) = turso_mysql_parser::MySqlCatalogTable::from_engine_name(table) {
            let type_name = catalog
                .column_type(column_name)
                .ok_or(LimboError::SchemaUpdated)?;
            return if is_integer_type(type_name) {
                Ok(ColumnKind::Integer)
            } else if is_text_type(type_name) {
                Ok(ColumnKind::Text)
            } else {
                Err(LimboError::InvalidArgument(format!(
                    "SELECT IN requires a signed integer or text column, found {type_name}"
                )))
            };
        }
        let table = MySqlTableName::parse(table)
            .map_err(|error| LimboError::ParseError(error.to_string()))?;
        let columns = self
            .list_shared_columns(&table)
            .map_err(|error| match error {
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
    /// An `ORDER BY` over a bare column, a comparison against a `?` and a
    /// comparison against a written day are the places it depends on it, and
    /// only those are parsed a second time — MySQL compares and orders text
    /// without regard to case where the engine will not unless it is asked to,
    /// and it reads a written day against a column holding a moment as that
    /// day's midnight.
    /// Reads a `SELECT`, a second time with its columns' types where they
    /// change how it is written, and answers whether they did: the reprepare
    /// parser keeps the typed statement when they did.
    fn parse_select_knowing_column_types(
        &self,
        sql: &str,
    ) -> std::result::Result<(turso_mysql_parser::TranslatedSelect, bool), MySqlQueryError> {
        if self.time_zone_offset_seconds() != 0 && uses_session_local_clock(sql) {
            return Err(MySqlQueryError::Unsupported(
                "session-local clock functions in a non-UTC time zone are unsupported".to_owned(),
            ));
        }
        let untyped = parse_select(sql, self.parser_mode()).map_err(mysql_query_parse_error)?;
        let untyped_sql = untyped.as_sql().to_owned();
        let typed = self.with_column_types(sql, untyped)?;
        if typed.renders_a_condition_without_column_types() {
            return Err(MySqlQueryError::Unsupported(
                "a CASE, IF, IFNULL or COALESCE over a column, or a GROUP_CONCAT ordered by one, needs its table's column types"
                    .to_owned(),
            ));
        }
        if typed.renders_a_lateral_table_without_column_kinds() {
            return Err(MySqlQueryError::Unsupported(
                "a LATERAL derived table needs the kinds of the columns its documents are built from"
                    .to_owned(),
            ));
        }
        let rendered_differently = typed.as_sql() != untyped_sql;
        Ok((typed, rendered_differently))
    }

    fn with_column_types(
        &self,
        sql: &str,
        translated: turso_mysql_parser::TranslatedSelect,
    ) -> std::result::Result<turso_mysql_parser::TranslatedSelect, MySqlQueryError> {
        let mode = self.parser_mode();
        let has_decimal_source = translated.source_tables().iter().any(|source| {
            self.list_shared_columns(source.table())
                .is_ok_and(|columns| columns.iter().any(|column| column.decimal_size().is_some()))
        });
        let has_json_source = translated.source_tables().iter().any(|source| {
            self.list_shared_columns(source.table())
                .is_ok_and(|columns| columns.iter().any(|column| column.type_name() == "JSON"))
        });
        if translated
            .source_tables()
            .iter()
            .any(|source| source.branch() > 0)
        {
            let origins = turso_mysql_parser::select_projection_origins(sql, mode);
            let projects_only_plain_columns = origins.as_ref().is_ok_and(|branches| {
                branches.iter().flatten().all(|origin| {
                    !matches!(
                        origin,
                        turso_mysql_parser::MySqlSelectProjectionOrigin::Other
                    )
                })
            });
            let projects_only_literals = !translated.needs_column_types()
                && translated.checked_comparisons().is_empty()
                && origins.as_ref().is_ok_and(|branches| {
                    branches.iter().all(|branch| {
                        !branch.is_empty()
                            && branch.iter().all(|origin| {
                                matches!(
                                    origin,
                                    turso_mysql_parser::MySqlSelectProjectionOrigin::NonNullLiteral
                                        | turso_mysql_parser::MySqlSelectProjectionOrigin::Null
                                )
                            })
                    })
                });
            for source in translated.source_tables() {
                if source.catalog().is_some() {
                    continue;
                }
                if source.subquery()
                    || !source.projected_columns().is_empty()
                    || self
                        .inner
                        .current_schema()
                        .get_btree_table(source.table().as_str())
                        .is_none()
                {
                    return Err(MySqlQueryError::Unsupported(
                        "compound SELECT source column types cannot be checked".to_string(),
                    ));
                }
                let columns = self.list_shared_columns(source.table()).map_err(|error| {
                    MySqlQueryError::Unsupported(format!(
                        "cannot resolve compound SELECT column types: {error}"
                    ))
                })?;
                // Only the first branch's table is read for column types, so a
                // `DECIMAL` a later branch names would be read as whatever the
                // engine stores it as. One the statement never names, through
                // a wildcard or otherwise, is only a neighbour of the columns
                // it reads.
                if !projects_only_literals
                    && columns.iter().any(|column| {
                        column.decimal_size().is_some()
                            && (!projects_only_plain_columns
                                || sql_mentions_column(sql, column.name()))
                    })
                {
                    return Err(MySqlQueryError::Unsupported(
                        "compound SELECT over DECIMAL columns is unsupported".to_string(),
                    ));
                }
            }
        }
        if has_decimal_source && translated.source_tables().len() > 1
            && (translated.checks_type_sensitive_expression()
                || translated.static_result_metadata().iter().any(|projection| {
                let StaticSelectProjectionMetadata::Literal(metadata) = projection else {
                    return false;
                };
                let answer = metadata.answer();
                matches!(answer, StaticSelectMetadata::Arithmetic(shape) if shape.names_a_column())
                    || matches!(answer,
                        StaticSelectMetadata::ColumnAggregate { kind: turso_mysql_parser::ColumnAggregateKind::Sum | turso_mysql_parser::ColumnAggregateKind::Avg, .. }
                        | StaticSelectMetadata::QualifiedAggregate { .. }
                        | StaticSelectMetadata::RoundedAggregate { .. }
                        | StaticSelectMetadata::WindowAggregate { kind: turso_mysql_parser::ColumnAggregateKind::Sum | turso_mysql_parser::ColumnAggregateKind::Avg, .. })
                    || matches!(answer,
                        StaticSelectMetadata::ScalarCall {
                            columns,
                            ..
                        } if !columns.is_empty())
            }))
        {
            for source in translated.source_tables() {
                if source.catalog().is_some() {
                    continue;
                }
                if source.subquery()
                    || !source.projected_columns().is_empty()
                    || self.inner.current_schema().get_btree_table(source.table().as_str()).is_none()
                {
                    return Err(MySqlQueryError::Unsupported(
                        "SELECT source column types cannot be checked".to_string(),
                    ));
                }
                let columns = self.list_shared_columns(source.table()).map_err(|error| {
                    MySqlQueryError::Unsupported(format!(
                        "cannot resolve SELECT arithmetic column types: {error}"
                    ))
                })?;
                if columns.iter().any(|column| {
                    column.decimal_size().is_some()
                        && sql_mentions_column(sql, column.name())
                }) {
                    return Err(MySqlQueryError::Unsupported(
                        "DECIMAL numeric projection over multiple source tables is unsupported"
                            .to_string(),
                    ));
                }
            }
        }
        if !translated.needs_column_types() {
            let decimal_source = translated
                .source_tables()
                .iter()
                .all(|source| !source.subquery() && source.projected_columns().is_empty())
                && translated.source_table().is_some_and(|source| {
                    MySqlTableName::parse(source).ok().is_some_and(|table| {
                        self.list_shared_columns(&table).is_ok_and(|columns| {
                            columns.iter().any(|column| column.decimal_size().is_some())
                        })
                    })
                });
            if !decimal_source && (!has_json_source || translated.checked_comparisons().is_empty())
            {
                return Ok(translated);
            }
        }
        if !has_decimal_source
            && !translated.reads_a_lateral_table()
            && translated
                .source_tables()
                .iter()
                .any(|source| source.subquery() || !source.projected_columns().is_empty())
        {
            if self.compares_an_exact_number_column_by_kind(&translated) {
                return Err(MySqlQueryError::Unsupported(
                    "a BIGINT UNSIGNED column compared with a bound value or a list beside a subquery"
                        .to_string(),
                ));
            }
            return Ok(translated);
        }
        // An `information_schema` table has no stored DDL to read a column's
        // type out of. It names its own columns and says which of them hold
        // text, and text is what MySQL orders without regard to case.
        if let Some(catalog) = translated
            .source_tables()
            .first()
            .and_then(MySqlSelectSource::catalog)
        {
            let text_columns = catalog
                .columns()
                .iter()
                .filter(|(_, type_name)| is_text_type(type_name))
                .map(|(name, _)| (*name).to_owned())
                .collect::<Vec<_>>();
            let table_columns = catalog
                .columns()
                .iter()
                .map(|(name, _)| (*name).to_owned())
                .collect::<Vec<_>>();
            return turso_mysql_parser::parse_select_with_column_types(
                sql,
                mode,
                &text_columns,
                &table_columns,
                &[],
            )
            .map_err(|error| MySqlQueryError::Syntax(error.to_string()));
        }
        let Some(source_table) = translated.source_table() else {
            return self.with_the_column_kinds_of_every_table(sql, translated);
        };
        let Ok(table) = MySqlTableName::parse(source_table) else {
            return Ok(translated);
        };
        // A derived table and a CTE say what each of their columns is, so the
        // statement around them can be read knowing the types. A subquery
        // reads a base table of its own, whose columns are read too.
        let mut subquery_columns = Vec::new();
        for source in translated.source_tables() {
            if source.subquery() {
                if source.catalog().is_some()
                    || self
                        .inner
                        .current_schema()
                        .get_btree_table(source.table().as_str())
                        .is_none()
                {
                    return Err(MySqlQueryError::Unsupported(
                        "SELECT expression needs a base table's column types".to_string(),
                    ));
                }
                subquery_columns.extend(self.list_columns(source.table()).map_err(|error| {
                    MySqlQueryError::Unsupported(format!(
                        "cannot read the columns a subquery reads: {error}"
                    ))
                })?);
                continue;
            }
            if !source.projected_columns().is_empty() && source.derived().is_none() {
                return Err(MySqlQueryError::Unsupported(
                    "SELECT expression needs a base table's column types".to_string(),
                ));
            }
            let Some(derived) = source.derived() else {
                continue;
            };
            // A derived table joining tables reads each column out of a table
            // of its own, so each name it gives carries that table's type.
            for (joined, name) in derived.joined().iter().zip(derived.names()) {
                let Some(column) = self
                    .list_columns(joined.table())
                    .map_err(|error| {
                        MySqlQueryError::Unsupported(format!(
                            "cannot read the columns a derived table joins: {error}"
                        ))
                    })?
                    .into_iter()
                    .find(|column| column.name().eq_ignore_ascii_case(joined.column()))
                else {
                    return Err(MySqlQueryError::Unsupported(
                        "a derived table projecting a column its table does not have".to_string(),
                    ));
                };
                let mut renamed = column;
                renamed.name.clone_from(name);
                subquery_columns.push(renamed);
            }
        }
        if self
            .inner
            .current_schema()
            .get_btree_table(table.as_str())
            .is_none()
        {
            let safe_view = self
                .inner
                .current_schema()
                .get_view(table.as_str())
                .is_some_and(|view| {
                    view.columns.iter().all(|column| {
                        !is_decimal_type(&column.ty_str)
                            && !column.ty_str.eq_ignore_ascii_case("mysql_decimal")
                            && !column.ty_str.eq_ignore_ascii_case("mysql_decimal_unsigned")
                    })
                });
            if !safe_view {
                return Err(MySqlQueryError::Unsupported(
                    "SELECT expression needs a base table's column types".to_string(),
                ));
            }
            return Ok(translated);
        }
        let Ok(table_own_columns) = self.list_shared_columns(&table) else {
            return Ok(translated);
        };
        let mut columns =
            columns_under_derived_names(&table_own_columns, translated.source_tables())?;
        // The reading below goes by names alone, so a subquery's column is
        // told apart from the statement's own only when a name shared between
        // them is of one kind in both.
        for column in subquery_columns {
            match columns
                .iter()
                .find(|named| named.name().eq_ignore_ascii_case(column.name()))
            {
                Some(named) if read_alike(named, &column) => {}
                Some(_) => {
                    return Err(MySqlQueryError::Unsupported(
                        "a subquery's column shares its name with a column of another kind"
                            .to_string(),
                    ));
                }
                None => columns.push(column),
            }
        }
        let text_columns = columns
            .iter()
            .filter(|column| is_text_type(column.type_name()))
            .map(|column| column.name().to_owned())
            .collect::<Vec<_>>();
        // A `DATETIME` and a `TIMESTAMP` hold a moment, and MySQL reads a
        // written day against one of them as that day's midnight.
        let moment_columns = columns
            .iter()
            .filter(|column| matches!(column.type_name(), "DATETIME" | "TIMESTAMP"))
            .map(|column| column.name().to_owned())
            .collect::<Vec<_>>();
        let table_columns = match translated
            .source_tables()
            .first()
            .and_then(MySqlSelectSource::derived)
        {
            Some(derived) if !derived.names().is_empty() => derived.names().to_vec(),
            _ => table_own_columns
                .iter()
                .map(|column| column.name().to_owned())
                .collect::<Vec<_>>(),
        };
        let member_columns = columns
            .iter()
            .filter_map(|column| {
                turso_mysql_parser::enum_members(column.type_name())
                    .map(|members| (column.name().to_owned(), members))
            })
            .collect::<Vec<_>>();
        let set_columns = columns
            .iter()
            .filter_map(|column| {
                turso_mysql_parser::set_members(column.type_name())
                    .map(|members| (column.name().to_owned(), members))
            })
            .collect::<Vec<_>>();
        let decimal_columns = columns
            .iter()
            .filter_map(|column| {
                column
                    .decimal_size()
                    .map(|(_, scale)| (column.name().to_owned(), scale))
                    .or_else(|| {
                        (column.type_name() == "BIGINT UNSIGNED")
                            .then(|| (column.name().to_owned(), 0))
                    })
            })
            .collect::<Vec<_>>();
        let integer_columns = columns
            .iter()
            .filter(|column| is_integer_type(column.type_name()))
            .map(|column| column.name().to_owned())
            .collect::<Vec<_>>();
        let real_columns = columns
            .iter()
            .filter(|column| {
                matches!(
                    column.type_name(),
                    "FLOAT" | "FLOAT UNSIGNED" | "DOUBLE" | "DOUBLE UNSIGNED"
                )
            })
            .map(|column| column.name().to_owned())
            .collect::<Vec<_>>();
        let json_columns = columns
            .iter()
            .filter(|column| column.type_name() == "JSON")
            .map(|column| column.name().to_owned())
            .collect::<Vec<_>>();
        let word_collations = collations_other_than_the_default(&columns);
        if text_columns.is_empty()
            && member_columns.is_empty()
            && set_columns.is_empty()
            && decimal_columns.is_empty()
            && integer_columns.is_empty()
            && real_columns.is_empty()
            && json_columns.is_empty()
            && moment_columns.is_empty()
            && !translated.orders_wildcard_ordinal()
        {
            return Ok(translated);
        }
        turso_mysql_parser::parse_select_knowing_json_columns(
            sql,
            mode,
            &text_columns,
            &table_columns,
            &member_columns,
            &set_columns,
            &moment_columns,
            &decimal_columns,
            &integer_columns,
            &real_columns,
            &json_columns,
            &word_collations,
        )
        .map_err(|error| MySqlQueryError::Syntax(error.to_string()))
    }

    /// Renders a statement over several tables knowing which of the columns
    /// it names hold words, and which are `BIGINT UNSIGNED` or `DECIMAL`, when
    /// it compares one of those with a bound value or one of the last with a
    /// list, the way a statement over one table is rendered.
    ///
    /// A number column then goes through the exact-number calls: GORM counts
    /// and reads an association through such a join — `JOIN post_tags ON
    /// post_tags.tag_id = tags.id AND post_tags.post_id = ?` — and it found no
    /// row. A column of words takes a bound word under its own collation, as
    /// GORM's `Joins("JOIN emails ON emails.user_id = users.id AND
    /// emails.email = ?", ...)` asks, which was refused. A column of whole
    /// numbers compared with a word naming one reads the word as that number,
    /// as TypeORM's relation loading writes `ON (t.tag_id = '1' AND ...)`. A
    /// name some table holds as another kind is refused.
    fn with_the_column_kinds_of_every_table(
        &self,
        sql: &str,
        translated: turso_mysql_parser::TranslatedSelect,
    ) -> std::result::Result<turso_mysql_parser::TranslatedSelect, MySqlQueryError> {
        let compares_a_written_number = translated.compares_a_written_number();
        // A `GROUP_CONCAT` over a joined column ordered by it is rendered by
        // the kind of that column, which is the one order a join is read
        // knowing its kinds for.
        let orders_a_joined_column = translated.renders_a_condition_without_column_types();
        let needs_the_kinds = orders_a_joined_column
            || self.compares_an_exact_number_column_by_kind(&translated)
            || self.compares_a_column_of_words_with_a_bound_value(&translated)
            || compares_a_written_number;
        // Words ordered or compared through a call are read under the
        // collation of the columns they come from, which a second reading
        // knowing each column's collation writes out.
        if !needs_the_kinds && translated.collation_sensitive_call_columns().is_empty() {
            return Ok(translated);
        }
        let mut columns = Vec::new();
        for source in translated.source_tables() {
            if source.subquery() || source.catalog().is_some() {
                continue;
            }
            columns.extend(self.list_columns(source.table()).map_err(|error| {
                MySqlQueryError::Unsupported(format!(
                    "cannot read the columns a comparison over several tables names: {error}"
                ))
            })?);
        }
        let exact = columns
            .iter()
            .filter_map(|column| {
                column
                    .decimal_size()
                    .map(|(_, scale)| (column.name().to_owned(), scale))
                    .or_else(|| {
                        (column.type_name() == "BIGINT UNSIGNED")
                            .then(|| (column.name().to_owned(), 0))
                    })
            })
            .collect::<Vec<_>>();
        if columns.iter().any(|column| {
            !is_integer_type(column.type_name())
                && column.decimal_size().is_none()
                && exact
                    .iter()
                    .any(|(name, _)| name.eq_ignore_ascii_case(column.name()))
        }) {
            return Err(MySqlQueryError::Unsupported(
                "an exact-number column shares its name with a column of another kind".to_string(),
            ));
        }
        let words = columns
            .iter()
            .filter(|column| is_text_type(column.type_name()))
            .map(|column| column.name().to_owned())
            .collect::<Vec<_>>();
        if columns.iter().any(|column| {
            !is_text_type(column.type_name())
                && words
                    .iter()
                    .any(|name| name.eq_ignore_ascii_case(column.name()))
        }) {
            return Err(MySqlQueryError::Unsupported(
                "a column of words shares its name with a column of another kind".to_string(),
            ));
        }
        let word_collations = collations_other_than_the_default(&columns);
        if !needs_the_kinds && word_collations.is_empty() {
            return Ok(translated);
        }
        if orders_a_joined_column {
            let whole_numbers = columns
                .iter()
                .filter(|column| is_integer_type(column.type_name()))
                .map(|column| column.name().to_owned())
                .collect::<Vec<_>>();
            if columns.iter().any(|column| {
                !is_integer_type(column.type_name())
                    && whole_numbers
                        .iter()
                        .any(|name| name.eq_ignore_ascii_case(column.name()))
            }) {
                return Err(MySqlQueryError::Unsupported(
                    "a whole-number column shares its name with a column of another kind"
                        .to_string(),
                ));
            }
            return turso_mysql_parser::parse_select_knowing_the_kinds_of_joined_columns(
                sql,
                self.parser_mode(),
                &words,
                &whole_numbers,
                &exact,
                &word_collations,
            )
            .map_err(|error| MySqlQueryError::Syntax(error.to_string()));
        }
        let whole_numbers = if compares_a_written_number {
            columns
                .iter()
                .filter(|column| is_integer_type(column.type_name()))
                .map(|column| column.name().to_owned())
                .collect::<Vec<_>>()
        } else {
            Vec::new()
        };
        if columns.iter().any(|column| {
            !is_integer_type(column.type_name())
                && whole_numbers
                    .iter()
                    .any(|name| name.eq_ignore_ascii_case(column.name()))
        }) {
            return Err(MySqlQueryError::Unsupported(
                "a column of whole numbers shares its name with a column of another kind"
                    .to_string(),
            ));
        }
        turso_mysql_parser::parse_select_knowing_numeric_columns(
            sql,
            self.parser_mode(),
            &words,
            &[],
            &[],
            &[],
            &[],
            &exact,
            &whole_numbers,
            &[],
            &word_collations,
        )
        .map_err(|error| MySqlQueryError::Syntax(error.to_string()))
    }

    /// Whether a statement read without its column types compares a
    /// `BIGINT UNSIGNED` or `DECIMAL` column in a way the engine answers by
    /// kind rather than by value: against a bound value or a list. The engine
    /// keeps such a column in a stored form of its own and compares it with a
    /// written number, and nothing else, through the type's own calls —
    /// measured, `user_id = ?` found no row, `id > ?` every row, and
    /// `balance > ?` every row binding 50 and none binding `'50'`.
    fn compares_an_exact_number_column_by_kind(
        &self,
        translated: &turso_mysql_parser::TranslatedSelect,
    ) -> bool {
        translated.checked_comparisons().iter().any(|comparison| {
            let compared_by_value = matches!(
                comparison.rhs(),
                CheckedSelectComparisonRhs::SignedInteger(_)
                    | CheckedSelectComparisonRhs::Null
                    | CheckedSelectComparisonRhs::Column { .. }
            ) && !matches!(
                comparison.operator(),
                CheckedSelectComparisonOperator::In | CheckedSelectComparisonOperator::NotIn
            );
            if comparison.answers().is_some() || compared_by_value {
                return false;
            }
            translated.source_tables().iter().any(|source| {
                !source.subquery()
                    && self
                        .list_shared_columns(source.table())
                        .is_ok_and(|columns| {
                            columns.iter().any(|column| {
                                (column.type_name() == "BIGINT UNSIGNED"
                                    || column.decimal_size().is_some())
                                    && column.name().eq_ignore_ascii_case(comparison.column_name())
                            })
                        })
            })
        })
    }

    /// Whether a statement compares a column of words one of its tables holds
    /// with a bound value, which is taken only once the statement is read
    /// knowing that the column holds words.
    fn compares_a_column_of_words_with_a_bound_value(
        &self,
        translated: &turso_mysql_parser::TranslatedSelect,
    ) -> bool {
        translated.checked_comparisons().iter().any(|comparison| {
            comparison.answers().is_none()
                && matches!(
                    comparison.rhs(),
                    CheckedSelectComparisonRhs::Placeholder { .. }
                )
                && translated.source_tables().iter().any(|source| {
                    !source.subquery()
                        && self
                            .list_shared_columns(source.table())
                            .is_ok_and(|columns| {
                                columns.iter().any(|column| {
                                    is_text_type(column.type_name())
                                        && column
                                            .name()
                                            .eq_ignore_ascii_case(comparison.column_name())
                                })
                            })
                })
        })
    }

    /// Holds every column a JSON reading in a condition reads to being a
    /// `JSON` column of the one base table the statement reads.
    fn refuse_select_json_readings_of_other_columns(
        &self,
        translated: &turso_mysql_parser::TranslatedSelect,
    ) -> std::result::Result<(), MySqlQueryError> {
        if translated.json_reading_columns().is_empty() {
            return Ok(());
        }
        let [source] = translated.source_tables() else {
            return Err(MySqlQueryError::Unsupported(
                "a JSON reading in a condition requires one base table".to_string(),
            ));
        };
        // A derived table whose body reads one table reads the JSON column of
        // that table, which Entity Framework Core's `SqlQuery` does around a
        // statement of its own: `SELECT s.Value FROM (SELECT COUNT(*) AS Value
        // FROM Users WHERE Profile->>'$.city' = 'Osaka') AS s LIMIT 2`.
        let reads_one_table_through_a_derived_table = source
            .derived()
            .is_some_and(|derived| derived.joined().is_empty());
        if source.subquery()
            || (!source.projected_columns().is_empty() && !reads_one_table_through_a_derived_table)
        {
            return Err(MySqlQueryError::Unsupported(
                "a JSON reading in a condition requires one base table".to_string(),
            ));
        }
        refuse_json_readings_of_other_columns(
            &self.inner.current_schema(),
            source.table().as_str(),
            translated.json_reading_columns(),
        )
        .map_err(|error| MySqlQueryError::Unsupported(error.to_string()))
    }

    /// Holds each name a subquery standing as a result column reads without a
    /// table to being a column of the table the subquery reads.
    ///
    /// MySQL reads such a name as the subquery's own column when its table
    /// has one and as the statement's otherwise, and a subquery naming the
    /// statement's table changes the shape of that table's result columns —
    /// see [`turso_mysql_parser::MySqlSelectSource::read_by_a_result_subquery`].
    /// Which of the two a bare name is has not been worked out for the
    /// second, so it is refused.
    fn hold_bare_names_in_result_subqueries_to_their_tables(
        &self,
        translated: &turso_mysql_parser::TranslatedSelect,
    ) -> std::result::Result<(), MySqlQueryError> {
        for (table, name) in translated.bare_names_in_result_subqueries() {
            let columns = self.list_shared_columns(table).map_err(|error| {
                MySqlQueryError::Unsupported(format!(
                    "cannot read the columns a subquery reads: {error}"
                ))
            })?;
            if !columns
                .iter()
                .any(|column| column.name().eq_ignore_ascii_case(name))
            {
                return Err(MySqlQueryError::Unsupported(format!(
                    "a subquery in the projection reads the statement's column {name} without its table"
                )));
            }
        }
        Ok(())
    }

    /// Holds a grouped statement's projection to the columns its keys decide,
    /// which is what MySQL's `ONLY_FULL_GROUP_BY` lets stand beside them — see
    /// [`turso_mysql_parser::MySqlColumnsTheKeysDecide`] for what was measured.
    ///
    /// The keys decide a table's row once they hold its primary key or a
    /// unique key whose columns are all `NOT NULL`, and a join's `ON` carries a
    /// decided column to the column it matches. Only whole-number columns are
    /// carried across a match, which is what every measured join matched on.
    fn hold_the_projection_to_what_the_keys_decide(
        &self,
        translated: &turso_mysql_parser::TranslatedSelect,
    ) -> std::result::Result<(), MySqlQueryError> {
        let Some(claim) = translated.columns_the_keys_decide() else {
            return Ok(());
        };
        let refused = || {
            MySqlQueryError::Unsupported(
                "GROUP BY leaves a projected column out of the grouping".to_string(),
            )
        };
        let schema = self.inner.current_schema();
        let mut tables = Vec::new();
        for source in translated.source_tables() {
            if source.subquery() {
                continue;
            }
            if source.branch() != 0
                || source.catalog().is_some()
                || source.derived().is_some()
                || !source.projected_columns().is_empty()
            {
                return Err(refused());
            }
            let Some(btree) = schema.get_btree_table(source.table().as_str()) else {
                return Err(refused());
            };
            let columns = self.list_columns(source.table()).map_err(|_| refused())?;
            let not_null = |name: &str| {
                columns
                    .iter()
                    .any(|column| column.name().eq_ignore_ascii_case(name) && !column.nullable())
            };
            let mut keys = Vec::new();
            if !btree.primary_key_columns.is_empty() {
                keys.push(
                    btree
                        .primary_key_columns
                        .iter()
                        .map(|(name, _)| name.clone())
                        .collect::<Vec<_>>(),
                );
            }
            for index in schema.get_indices(source.table().as_str()) {
                if index.unique
                    && index.where_clause.is_none()
                    && index.columns.iter().all(|column| not_null(&column.name))
                {
                    keys.push(
                        index
                            .columns
                            .iter()
                            .map(|column| column.name.clone())
                            .collect(),
                    );
                }
            }
            tables.push(TableTheKeysMayDecide {
                reference: source.reference(),
                columns,
                keys,
            });
        }
        if tables.len() != claim.joins().len() + 1
            || claim
                .joins()
                .iter()
                .zip(&tables[1..])
                .any(|(join, table)| !join.reference().eq_ignore_ascii_case(table.reference))
        {
            return Err(refused());
        }
        let mut decided = std::collections::HashSet::new();
        for key in claim.keys() {
            decided.insert(resolve_named_column(&tables, key).ok_or_else(refused)?);
        }
        loop {
            let before = decided.len();
            for (position, table) in tables.iter().enumerate() {
                if table.keys.iter().any(|key| {
                    key.iter()
                        .all(|column| decided.contains(&(position, column.to_ascii_lowercase())))
                }) {
                    for column in &table.columns {
                        decided.insert((position, column.name().to_ascii_lowercase()));
                    }
                }
            }
            for (position, join) in claim.joins().iter().enumerate() {
                carry_across_the_join(&tables, position + 1, join, &mut decided);
            }
            if decided.len() == before {
                break;
            }
        }
        for column in claim.columns() {
            let resolved = resolve_named_column(&tables, column).ok_or_else(refused)?;
            if !decided.contains(&resolved) {
                return Err(refused());
            }
        }
        Ok(())
    }

    fn reject_binary_scalar_collation(
        &self,
        translated: &turso_mysql_parser::TranslatedSelect,
    ) -> std::result::Result<(), MySqlQueryError> {
        let schema = self.inner.current_schema();
        for (reference, column) in translated.collation_sensitive_joined_columns() {
            let table = translated
                .source_tables()
                .iter()
                .find(|source| {
                    !source.subquery() && source.reference().eq_ignore_ascii_case(reference)
                })
                .and_then(|source| schema.get_table(source.table().as_str()))
                .ok_or_else(|| {
                    MySqlQueryError::Unsupported(
                        "text call needs a base table's column collation".to_string(),
                    )
                })?;
            if let Some(refusal) = call_over_another_collation(&table, std::slice::from_ref(column))
            {
                return Err(MySqlQueryError::Unsupported(refusal));
            }
        }
        if translated.collation_sensitive_call_columns().is_empty() {
            return Ok(());
        }
        for source in translated.source_tables() {
            if source.subquery() {
                continue;
            }
            let Some(table) = schema.get_table(source.table().as_str()) else {
                return Err(MySqlQueryError::Unsupported(
                    "text call needs a base table's column collation".to_string(),
                ));
            };
            if let Some(refusal) =
                call_over_another_collation(&table, translated.collation_sensitive_call_columns())
            {
                return Err(MySqlQueryError::Unsupported(refusal));
            }
        }
        Ok(())
    }

    /// Holds each comparison to the type of the column it names.
    ///
    /// A join names its tables, so a comparison in one carries the qualifier
    /// that says which table its column belongs to; a statement reading one
    /// table needs no qualifier, and there the bare name is that table's.
    /// Holds every comparison to the type of the column it names, and reports
    /// which parameters have to be put into a stored form before they are
    /// bound.
    fn validate_select_comparison_columns(
        &self,
        source_tables: &[MySqlSelectSource],
        comparisons: &[CheckedSelectComparison],
    ) -> Result<Vec<BoundTemporalParameter>> {
        let mut bound = Vec::new();
        for comparison in comparisons {
            if let CheckedSelectComparisonRhs::Column { qualifier, name } = comparison.rhs() {
                if joins_two_catalog_columns(source_tables, comparison, qualifier.as_deref()) {
                    continue;
                }
                let left = self.compared_column(
                    column_tables(
                        source_tables,
                        comparison.qualifier(),
                        comparison.inner_sources(),
                    )?,
                    comparison.column_name(),
                )?;
                let right = self.compared_column(
                    column_tables(
                        source_tables,
                        qualifier.as_deref(),
                        comparison.inner_sources(),
                    )?,
                    name,
                )?;
                refuse_a_column_pair_compared_differently(comparison, name, &left, &right)?;
                continue;
            }
            // A call says what it answers, so the value it meets is held to
            // that rather than to a column this would have to find first. A
            // bound value meets a call answering a word as a word, which the
            // statement's word parameters hold it to.
            if let Some(answers) = comparison.answers() {
                let a_bound_word = answers == CheckedComparisonAnswer::Text
                    && matches!(
                        comparison.rhs(),
                        CheckedSelectComparisonRhs::Placeholder { .. }
                    );
                if !a_bound_word && !checked_comparison_meets_an_answer(comparison.rhs(), answers) {
                    return Err(LimboError::InvalidArgument(format!(
                        "SELECT comparison against a call requires {}",
                        answered_kind_name(answers)
                    )));
                }
                continue;
            }
            let mut found = false;
            // An `information_schema` table's columns are named by the table
            // itself rather than by stored DDL, so the type a comparison has
            // to fit is the one the table declares for the column.
            if let Some(type_name) = catalog_column_type(source_tables, comparison) {
                bound.extend(select_comparison_fits_column(comparison, type_name, 0)?);
                continue;
            }
            for table in comparison_tables(source_tables, comparison)? {
                refuse_like_over_a_view(&self.inner.current_schema(), table.as_str(), comparison)?;
                if let Some((type_name, temporal_precision)) =
                    self.comparison_column_type(&table, comparison)?
                {
                    if type_name == "JSON"
                        && (source_tables.len() != 1
                            || source_tables[0].subquery()
                            || !source_tables[0].projected_columns().is_empty()
                            || self
                                .inner
                                .current_schema()
                                .get_btree_table(table.as_str())
                                .is_none())
                    {
                        return Err(LimboError::InvalidArgument(
                            "JSON comparison requires one base table".to_string(),
                        ));
                    }
                    if source_tables.len() > 1
                        && is_decimal_type(&type_name)
                        && matches!(
                            comparison.operator(),
                            CheckedSelectComparisonOperator::In
                                | CheckedSelectComparisonOperator::NotIn
                        )
                    {
                        return Err(LimboError::InvalidArgument(
                            "DECIMAL IN over multiple source tables is unsupported".to_string(),
                        ));
                    }
                    bound.extend(select_comparison_fits_column(
                        comparison,
                        &type_name,
                        temporal_precision.unwrap_or(0),
                    )?);
                    found = true;
                    break;
                }
            }
            if !found {
                return Err(LimboError::SchemaUpdated);
            }
        }
        Ok(bound)
    }

    fn decimal_comparison_parameters(
        &self,
        source_tables: &[MySqlSelectSource],
        comparisons: &[CheckedSelectComparison],
    ) -> Result<Vec<usize>> {
        let mut bound = Vec::new();
        for comparison in comparisons {
            let CheckedSelectComparisonRhs::Placeholder { ordinal } = comparison.rhs() else {
                continue;
            };
            // What a bound value meets in an `information_schema` table was
            // held to the type that table declares, and none of its columns
            // holds an exact number.
            if comparison.answers().is_some()
                || catalog_column_type(source_tables, comparison).is_some()
            {
                continue;
            }
            for table in comparison_tables(source_tables, comparison)? {
                if let Some((type_name, _)) = self.comparison_column_type(&table, comparison)? {
                    if is_decimal_type(&type_name) || type_name == "BIGINT UNSIGNED" {
                        bound.push(*ordinal);
                    }
                    break;
                }
            }
        }
        Ok(bound)
    }

    /// Finds the parameters that meet a column holding whole numbers.
    ///
    /// Laravel binds every value it reads from a request as a word, and
    /// MySQL reads a bound word naming a whole number as exactly that number
    /// there — measured on 8.4.11, binding `'9007199254740993'` against a
    /// `BIGINT` finds that row and not its neighbour. A `BIGINT UNSIGNED` is
    /// left to the `DECIMAL` path, which reads a bound word exactly already.
    fn whole_number_comparison_parameters(
        &self,
        source_tables: &[MySqlSelectSource],
        comparisons: &[CheckedSelectComparison],
    ) -> Result<Vec<usize>> {
        let mut bound = Vec::new();
        for comparison in comparisons {
            let CheckedSelectComparisonRhs::Placeholder { ordinal } = comparison.rhs() else {
                continue;
            };
            if comparison.answers() == Some(CheckedComparisonAnswer::RowCount) {
                bound.push(*ordinal);
                continue;
            }
            if comparison.answers().is_some()
                || catalog_column_type(source_tables, comparison).is_some()
                || matches!(
                    comparison.operator(),
                    CheckedSelectComparisonOperator::Like
                        | CheckedSelectComparisonOperator::NotLike
                )
            {
                continue;
            }
            for table in comparison_tables(source_tables, comparison)? {
                if let Some((type_name, _)) = self.comparison_column_type(&table, comparison)? {
                    if is_integer_type(&type_name) && type_name != "BIGINT UNSIGNED" {
                        bound.push(*ordinal);
                    }
                    break;
                }
            }
        }
        Ok(bound)
    }

    /// Finds the parameters that meet a column holding words, which bind a
    /// word compared under the column's collation.
    ///
    /// The parser marks such a comparison collated only where it was told the
    /// statement's one table's columns; one inside a subquery — Laravel's
    /// `select exists(select * from posts where title = ?)` — is found here by
    /// the column's own type.
    fn word_comparison_parameters(
        &self,
        source_tables: &[MySqlSelectSource],
        comparisons: &[CheckedSelectComparison],
    ) -> Result<Vec<usize>> {
        let mut words =
            self.string_comparison_parameters(source_tables, comparisons, is_text_type)?;
        // A value bound against a call answering a word — `lower(path) = ?` —
        // is a word too.
        words.extend(comparisons.iter().filter_map(|comparison| {
            match (comparison.answers(), comparison.rhs()) {
                (
                    Some(CheckedComparisonAnswer::Text),
                    CheckedSelectComparisonRhs::Placeholder { ordinal },
                ) => Some(*ordinal),
                _ => None,
            }
        }));
        Ok(words)
    }

    /// Finds the parameters that meet a column of bytes, which bind a word or
    /// bytes and never a number: MySQL compares a binary string with a number
    /// as two numbers, and the engine never finds a number among bytes.
    fn byte_comparison_parameters(
        &self,
        source_tables: &[MySqlSelectSource],
        comparisons: &[CheckedSelectComparison],
    ) -> Result<Vec<usize>> {
        self.string_comparison_parameters(
            source_tables,
            comparisons,
            turso_mysql_parser::holds_bytes,
        )
    }

    fn string_comparison_parameters(
        &self,
        source_tables: &[MySqlSelectSource],
        comparisons: &[CheckedSelectComparison],
        holds_strings: fn(&str) -> bool,
    ) -> Result<Vec<usize>> {
        let mut words = Vec::new();
        for comparison in comparisons {
            let CheckedSelectComparisonRhs::Placeholder { ordinal } = comparison.rhs() else {
                continue;
            };
            if comparison.answers().is_some() {
                continue;
            }
            if let Some(type_name) = source_tables.iter().find_map(|source| {
                source
                    .catalog()
                    .and_then(|catalog| catalog.column_type(comparison.column_name()))
            }) {
                if holds_strings(type_name) {
                    words.push(*ordinal);
                }
                continue;
            }
            for table in comparison_tables(source_tables, comparison)? {
                if let Some((type_name, _)) = self.comparison_column_type(&table, comparison)? {
                    if holds_strings(&type_name) {
                        words.push(*ordinal);
                    }
                    break;
                }
            }
        }
        Ok(words)
    }

    /// Finds the parameters a DML statement compares with a column of words,
    /// each by the type of the column it meets: one of a table the statement
    /// reads, or else one of the table it writes.
    fn dml_word_parameters(&self, translated: &TranslatedDml) -> Result<Vec<usize>> {
        self.dml_string_parameters(translated, is_text_type)
    }

    /// Finds the parameters a DML statement compares with a column of bytes.
    fn dml_byte_parameters(&self, translated: &TranslatedDml) -> Result<Vec<usize>> {
        self.dml_string_parameters(translated, turso_mysql_parser::holds_bytes)
    }

    fn dml_string_parameters(
        &self,
        translated: &TranslatedDml,
        holds_strings: fn(&str) -> bool,
    ) -> Result<Vec<usize>> {
        let read = translated.read_tables();
        let joins = read.iter().any(|source| !source.subquery());
        let (inner, written): (Vec<_>, Vec<_>) = translated
            .checked_comparisons()
            .iter()
            .cloned()
            .partition(|comparison| {
                joins
                    || comparison
                        .qualifier()
                        .or_else(|| comparison.inner_source())
                        .is_some_and(|name| {
                            read.iter()
                                .any(|source| source.reference().eq_ignore_ascii_case(name))
                        })
            });
        let mut words = self.string_comparison_parameters(read, &inner, holds_strings)?;
        if written.is_empty() {
            return Ok(words);
        }
        let table = translated
            .source_table()
            .ok_or(LimboError::SchemaUpdated)
            .and_then(|table| {
                MySqlTableName::parse(table)
                    .map_err(|error| LimboError::ParseError(error.to_string()))
            })?;
        for comparison in &written {
            let CheckedSelectComparisonRhs::Placeholder { ordinal } = comparison.rhs() else {
                continue;
            };
            if comparison.answers().is_some() {
                continue;
            }
            if let Some((type_name, _)) = self.comparison_column_type(&table, comparison)? {
                if holds_strings(&type_name) {
                    words.push(*ordinal);
                }
            }
        }
        Ok(words)
    }

    /// Holds a DML statement's comparisons to the columns they name.
    ///
    /// A statement that reads no table beyond the one it writes says that
    /// table by name; a joined `DELETE` and an `INSERT ... SELECT` read
    /// several, and there the qualifier says which.
    fn validate_dml_comparison_columns(&self, translated: &TranslatedDml) -> Result<()> {
        if let Some(table) = translated
            .source_table()
            .and_then(|table| self.inner.current_schema().get_table(table))
        {
            if let Some(refusal) =
                call_over_another_collation(&table, translated.collation_sensitive_call_columns())
            {
                return Err(LimboError::InvalidArgument(refusal));
            }
        }
        refuse_dml_json_readings_mysql_reads_differently(&self.inner.current_schema(), translated)?;
        refuse_a_null_ignore_writes_into_a_column_refusing_null(
            &self.inner.current_schema(),
            translated,
        )?;
        self.validate_subquery_comparison_columns(
            translated.source_table(),
            translated.checked_subquery_comparisons(),
        )?;
        let read = translated.read_tables();
        let (pairs, comparisons): (Vec<_>, Vec<_>) = translated
            .checked_comparisons()
            .iter()
            .cloned()
            .partition(|comparison| {
                matches!(comparison.rhs(), CheckedSelectComparisonRhs::Column { .. })
            });
        for pair in &pairs {
            self.validate_dml_column_pair(translated, pair)?;
        }
        if read.is_empty() {
            return self
                .validate_one_table_comparison_columns(translated.source_table(), &comparisons);
        }
        // A joined `DELETE` and an `INSERT ... SELECT` read their tables
        // outright, and every comparison belongs to one of them. A subquery is
        // different: the statement still writes one table it does not read, so
        // a comparison naming none of the subqueries belongs to that one.
        if read.iter().any(|source| !source.subquery()) {
            self.reject_dml_json_comparisons(read, &comparisons)?;
            self.validate_select_comparison_columns(read, &comparisons)?;
            return Ok(());
        }
        let names_a_subquery = |comparison: &CheckedSelectComparison| {
            comparison
                .qualifier()
                .or_else(|| comparison.inner_source())
                .is_some_and(|name| {
                    read.iter()
                        .any(|source| source.reference().eq_ignore_ascii_case(name))
                })
        };
        let (inner, written): (Vec<_>, Vec<_>) =
            comparisons.into_iter().partition(names_a_subquery);
        self.reject_dml_json_comparisons(read, &inner)?;
        self.validate_select_comparison_columns(read, &inner)?;
        self.validate_one_table_comparison_columns(translated.source_table(), &written)
    }

    /// Holds a DML statement's column compared with another column to a pair
    /// MySQL and the engine compare alike.
    ///
    /// Either column may be the written table's, which the statement writes
    /// rather than reads — `EXISTS (SELECT 1 FROM child WHERE child.parent_id
    /// = parent.id)` — so a name no table the statement reads claims is that
    /// table's.
    fn validate_dml_column_pair(
        &self,
        translated: &TranslatedDml,
        comparison: &CheckedSelectComparison,
    ) -> Result<()> {
        let CheckedSelectComparisonRhs::Column { qualifier, name } = comparison.rhs() else {
            unreachable!("the caller passes column pairs only");
        };
        let written = translated
            .source_table()
            .map(MySqlTableName::parse)
            .transpose()
            .map_err(|error| LimboError::ParseError(error.to_string()))?;
        let tables = |qualifier: Option<&str>| {
            let read = translated.read_tables();
            let names_a_read_table = qualifier.is_some_and(|qualifier| {
                read.iter()
                    .any(|source| source.reference().eq_ignore_ascii_case(qualifier))
            });
            let mut tables =
                column_tables(read, qualifier, comparison.inner_sources()).unwrap_or_default();
            if !names_a_read_table {
                tables.extend(written.clone());
            }
            tables
        };
        let left =
            self.compared_column(tables(comparison.qualifier()), comparison.column_name())?;
        let right = self.compared_column(tables(qualifier.as_deref()), name)?;
        refuse_a_column_pair_compared_differently(comparison, name, &left, &right)
    }

    fn reject_dml_json_comparisons(
        &self,
        read: &[MySqlSelectSource],
        comparisons: &[CheckedSelectComparison],
    ) -> Result<()> {
        for comparison in comparisons {
            for table in comparison_tables(read, comparison)? {
                if self
                    .comparison_column_type(&table, comparison)?
                    .is_some_and(|(name, _)| name == "JSON")
                {
                    return Err(LimboError::InvalidArgument(
                        "DML comparison against JSON is unsupported".to_string(),
                    ));
                }
            }
        }
        Ok(())
    }

    /// The same check for a statement that reads one table and says so by
    /// name, which is the shape every checked DML statement has.
    fn validate_one_table_comparison_columns(
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
        for comparison in comparisons {
            // A call says what it answers, as it does in a `SELECT`, so the
            // value it meets is held to that.
            if let Some(answers) = comparison.answers() {
                if !checked_comparison_meets_an_answer(comparison.rhs(), answers) {
                    return Err(LimboError::InvalidArgument(format!(
                        "DML comparison against a call requires {}",
                        answered_kind_name(answers)
                    )));
                }
                continue;
            }
            refuse_like_over_a_view(&self.inner.current_schema(), table.as_str(), comparison)?;
            let Some((type_name, temporal_precision)) =
                self.comparison_column_type(&table, comparison)?
            else {
                return Err(LimboError::SchemaUpdated);
            };
            if let Some(precision) = temporal_precision.filter(|precision| *precision > 0) {
                if !matches!(
                    comparison.rhs(),
                    CheckedSelectComparisonRhs::Placeholder { .. }
                ) {
                    select_comparison_fits_column(comparison, &type_name, precision)?;
                    continue;
                }
            }
            if !checked_comparison_fits_column(comparison.rhs(), &type_name, comparison.operator())
            {
                return Err(checked_comparison_column_refusal(
                    comparison.rhs(),
                    comparison.column_name(),
                    &type_name,
                ));
            }
        }
        Ok(())
    }

    /// Answers whether the table carries the column, having held the value to
    /// its type when it does.
    /// The type of the column one comparison names in one table, or nothing
    /// where that table has no column of that name.
    ///
    /// Whether the comparison fits that type is the caller's to say: a
    /// `SELECT` puts a bound day or moment into the column's own form first
    /// and so takes one where a DML statement, which has no such step, does
    /// not.
    fn comparison_column_type(
        &self,
        table: &MySqlTableName,
        comparison: &CheckedSelectComparison,
    ) -> Result<Option<(String, Option<u8>)>> {
        self.read_compared_column(table, comparison.column_name(), |column| {
            (column.type_name().to_owned(), column.temporal_precision())
        })
    }

    /// Reads one column of a pair compared with each other, from the nearest
    /// of the tables it may belong to.
    ///
    /// The collation is read off the engine's own column as well as off the
    /// stored MySQL declaration: the engine compares two columns under the
    /// left one's collation, so the pair is only the one MySQL compares when
    /// the engine holds both under the same one.
    fn compared_column(&self, tables: Vec<MySqlTableName>, name: &str) -> Result<ComparedColumn> {
        for table in tables {
            let schema = self.inner.current_schema();
            let Some(stored) = schema.get_btree_table(table.as_str()) else {
                return Err(LimboError::InvalidArgument(
                    "a comparison of two columns reads base tables only".to_string(),
                ));
            };
            let Some(column) = self.compared_column_metadata(&table, name)? else {
                continue;
            };
            let (_, engine_column) = stored.get_column(name).ok_or(LimboError::SchemaUpdated)?;
            return Ok(ComparedColumn {
                type_name: column.type_name().to_owned(),
                temporal_precision: column.temporal_precision(),
                collation_name: column.collation_name(),
                engine_collation: engine_column.collation(),
            });
        }
        Err(LimboError::SchemaUpdated)
    }

    /// The stored declaration of the column a comparison names in one table,
    /// or nothing where that table has no column of that name.
    fn compared_column_metadata(
        &self,
        table: &MySqlTableName,
        name: &str,
    ) -> Result<Option<MySqlColumnMetadata>> {
        self.read_compared_column(table, name, MySqlColumnMetadata::clone)
    }

    fn read_compared_column<T>(
        &self,
        table: &MySqlTableName,
        name: &str,
        read: impl FnOnce(&MySqlColumnMetadata) -> T,
    ) -> Result<Option<T>> {
        let columns = self
            .list_shared_columns(table)
            .map_err(|error| match error {
                MySqlColumnMetadataError::Engine(error) => error,
                MySqlColumnMetadataError::TableNotFound => LimboError::SchemaUpdated,
                MySqlColumnMetadataError::CorruptDefinition => {
                    LimboError::Corrupt("invalid SELECT table metadata".to_string())
                }
                MySqlColumnMetadataError::UnsupportedDefinition => {
                    LimboError::ParseError("unsupported SELECT table metadata".to_string())
                }
            })?;
        let mut matching = columns
            .iter()
            .filter(|column| column.name().eq_ignore_ascii_case(name));
        let Some(column) = matching.next() else {
            return Ok(None);
        };
        if matching.next().is_some() {
            return Err(LimboError::Corrupt(
                "duplicate SELECT comparison column metadata".to_string(),
            ));
        }
        Ok(Some(read(column)))
    }

    fn validate_dml_ordered_columns(
        &self,
        source_table: Option<&str>,
        ordered_columns: &[String],
    ) -> Result<()> {
        if ordered_columns.is_empty() {
            return Ok(());
        }
        let source_table = source_table.ok_or_else(|| {
            LimboError::InvalidArgument("DML ORDER BY requires a table column".to_string())
        })?;
        let table = MySqlTableName::parse(source_table)
            .map_err(|error| LimboError::ParseError(error.to_string()))?;
        let columns = self
            .list_shared_columns(&table)
            .map_err(|error| match error {
                MySqlColumnMetadataError::Engine(error) => error,
                MySqlColumnMetadataError::TableNotFound => LimboError::SchemaUpdated,
                MySqlColumnMetadataError::CorruptDefinition => {
                    LimboError::Corrupt("invalid DML table metadata".to_string())
                }
                MySqlColumnMetadataError::UnsupportedDefinition => {
                    LimboError::ParseError("unsupported DML table metadata".to_string())
                }
            })?;
        for col_name in ordered_columns {
            let mut matching = columns
                .iter()
                .filter(|column| column.name().eq_ignore_ascii_case(col_name));
            let Some(column) = matching.next() else {
                return Err(LimboError::SchemaUpdated);
            };
            if matching.next().is_some() {
                return Err(LimboError::Corrupt(
                    "duplicate DML column metadata".to_string(),
                ));
            }
            if !is_integer_type(column.type_name()) {
                return Err(LimboError::ParseError(
                    "DML ORDER BY supports only integer columns".to_string(),
                ));
            }
        }
        Ok(())
    }

    /// Holds what a `LIMIT ?` or `OFFSET ?` binds to a row count.
    ///
    /// The engine reads a negative row count as no limit at all, where MySQL
    /// refuses one, so a bound value that is not a whole number at or above
    /// zero is refused rather than answered with every row.
    fn validate_row_count_values(
        row_count_parameters: &[usize],
        values: &[MySqlPreparedValue],
    ) -> Result<()> {
        for ordinal in row_count_parameters {
            let value = values.get(*ordinal).ok_or_else(|| {
                LimboError::InternalError(
                    "SELECT row count placeholder is outside prepared parameters".to_string(),
                )
            })?;
            if !matches!(value, MySqlPreparedValue::Integer(count) if *count >= 0) {
                return Err(LimboError::InvalidArgument(
                    "SELECT LIMIT parameter requires a whole number that is not negative"
                        .to_string(),
                ));
            }
        }
        Ok(())
    }

    fn validate_select_comparison_values(
        comparisons: &[CheckedSelectComparison],
        values: &[MySqlPreparedValue],
        bound_temporal: &[BoundTemporalParameter],
        bound_decimal: &[usize],
        whole_number_parameters: &[usize],
        word_parameters: &[usize],
        byte_parameters: &[usize],
    ) -> Result<()> {
        for comparison in comparisons {
            let CheckedSelectComparisonRhs::Placeholder { ordinal } = comparison.rhs() else {
                continue;
            };
            // What binds against a JSON reading was held to its own kinds.
            if is_a_json_answer(comparison.answers()) {
                continue;
            }
            let value = values.get(*ordinal).ok_or_else(|| {
                LimboError::InternalError(
                    "SELECT comparison placeholder is outside prepared parameters".to_string(),
                )
            })?;
            // A collated comparison names a text column, which is the one
            // that takes a string. A `LIKE` binds a pattern, which is a string
            // regardless of the column's collation.
            let patterns = matches!(
                comparison.operator(),
                CheckedSelectComparisonOperator::Like | CheckedSelectComparisonOperator::NotLike
            );
            // A value meeting a column that holds a day or a moment is put
            // into that column's own form before it binds, so a word is what
            // it takes and a number is not: MySQL reads a bound number as a
            // moment, which this has no reader for.
            let stored_as_a_moment = bound_temporal
                .iter()
                .any(|parameter| parameter.ordinal == *ordinal);
            let meets_words = comparison.collated() || word_parameters.contains(ordinal);
            let meets_bytes = byte_parameters.contains(ordinal);
            // A number meeting a column of words is refused for the reason a
            // DML statement refuses one; see
            // `refuse_a_word_parameter_bound_otherwise`.
            let fits = match value {
                MySqlPreparedValue::Null => true,
                MySqlPreparedValue::Integer(_) => {
                    !patterns && !stored_as_a_moment && !meets_words && !meets_bytes
                }
                MySqlPreparedValue::UnsignedInteger(_) => {
                    !patterns && !stored_as_a_moment && bound_decimal.contains(ordinal)
                }
                MySqlPreparedValue::Text(written) => {
                    if patterns {
                        true
                    } else {
                        meets_words
                            || meets_bytes
                            || stored_as_a_moment
                            || bound_decimal.contains(ordinal)
                            || (whole_number_parameters.contains(ordinal)
                                && bound_whole_number(written).is_some())
                    }
                }
                // Measured on 8.4.11, a bound double meets a count as a
                // number, `COUNT(*) > 1.5` finding the groups of two, which is
                // how the engine compares a whole number with a real. It meets
                // a column of whole numbers the same way, exactly: mysql2
                // binds every number a statement's parameters are not typed
                // for as a double, and `views = 1.5` finds nothing, `views >=
                // 1.5` finds 2, and a `BIGINT` of 9007199254740993 is not the
                // double 9007199254740992.
                MySqlPreparedValue::Real(_) => {
                    comparison.answers() == Some(CheckedComparisonAnswer::RowCount)
                        || (whole_number_parameters.contains(ordinal)
                            && !patterns
                            && !stored_as_a_moment
                            && !meets_words)
                }
                MySqlPreparedValue::Blob(_) => meets_bytes,
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

    fn refuse_untyped_wide_integer_select_parameters(
        values: &[MySqlPreparedValue],
        bound_exact_numeric: &[usize],
    ) -> Result<()> {
        for (ordinal, value) in values.iter().enumerate() {
            if matches!(value, MySqlPreparedValue::UnsignedInteger(_))
                && !bound_exact_numeric.contains(&ordinal)
            {
                return Err(LimboError::IntegerOverflow);
            }
        }
        Ok(())
    }

    fn refuse_untyped_wide_integer_write_parameters(
        &self,
        insert_target: Option<&CheckedInsertTarget>,
        values: &[MySqlPreparedValue],
    ) -> Result<()> {
        let mut exact_insert_parameters = Vec::new();
        if let Some(CheckedInsertTarget::Listed(insert)) = insert_target {
            let schema = self.inner.current_schema();
            let table = schema
                .get_btree_table(insert.table.as_str())
                .ok_or(LimboError::SchemaUpdated)?;
            for (column_index, column_name) in insert.columns.iter().enumerate() {
                let Some((_, column)) = table.get_column(column_name) else {
                    return Err(LimboError::SchemaUpdated);
                };
                if !["mysql_uint64", "mysql_decimal", "mysql_decimal_unsigned"]
                    .iter()
                    .any(|name| column.ty_str.eq_ignore_ascii_case(name))
                {
                    continue;
                }
                for row in &insert.rows {
                    if let Some(InsertedValue::Marker(ordinal)) = row.get(column_index) {
                        exact_insert_parameters.push(*ordinal);
                    }
                }
            }
        }
        for (ordinal, value) in values.iter().enumerate() {
            if matches!(value, MySqlPreparedValue::UnsignedInteger(_))
                && !exact_insert_parameters.contains(&ordinal)
            {
                return Err(LimboError::IntegerOverflow);
            }
        }
        Ok(())
    }

    pub fn execute(&self, sql: &str) -> Result<()> {
        self.refuse_an_upsert_answered_otherwise(sql, self.parser_mode())
            .map_err(|error| LimboError::ParseError(error.to_string()))?;
        self.refuse_literals_their_columns_store_otherwise(sql, self.parser_mode())
            .map_err(|error| LimboError::ParseError(error.to_string()))?;
        self.refuse_written_ids_the_counted_column_cannot_hold(sql)
            .map_err(LimboError::from)?;
        match parse_auto_increment_insert(sql, self.parser_mode()) {
            Ok(insert) if insert.reads_the_clock() && self.time_zone_offset_seconds() != 0 => {
                Err(LimboError::ParseError(
                    "session-local clock functions in a non-UTC time zone are unsupported"
                        .to_string(),
                ))
            }
            Ok(insert) => match self.load_auto_increment_table(insert.table_name().as_str())? {
                Some(table)
                    if insert.written_row_by_row(&table.definition.allocator_column_name) =>
                {
                    self.execute_auto_increment_conflict_rows(
                        sql,
                        insert,
                        table,
                        &[],
                        None,
                        MySqlAffectedRowsMode::Changed,
                    )?;
                    Ok(())
                }
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
        self.in_a_concurrent_statement_transaction(
            || self.execute_checked_write_in_its_transaction(sql, timeout, affected_rows_mode),
            MySqlQueryError::Engine,
        )
    }

    fn execute_checked_write_in_its_transaction(
        &self,
        sql: &str,
        timeout: Option<Duration>,
        affected_rows_mode: MySqlAffectedRowsMode,
    ) -> std::result::Result<MySqlWriteResult, MySqlQueryError> {
        self.reject_write_in_read_only_transaction()?;
        // An `INSERT INTO t <SELECT>` with no column list means every column of
        // the table, in order, so the list is written out here — where the
        // table is known — and the ordinary statement runs.
        let written_out;
        let sql = match self.insert_written_out(sql)? {
            Some(statement) => {
                written_out = statement;
                written_out.as_str()
            }
            None => sql,
        };
        if self.time_zone_offset_seconds() != 0 {
            let (translated, ..) = self
                .parse_checked_dml_translation(sql, self.parser_mode())
                .map_err(mysql_query_parse_error)?;
            self.reject_non_utc_timestamp_dml_source(&translated)?;
            let mut statement = translated
                .parse_ast()
                .map_err(|error| MySqlQueryError::Syntax(error.to_string()))?;
            if self.shift_timestamp_insert_literals(&mut statement)? {
                if let Stmt::Insert { tbl_name, .. } = &statement {
                    if self
                        .load_auto_increment_table(tbl_name.name.as_str())
                        .map_err(MySqlQueryError::Engine)?
                        .is_some()
                    {
                        return Err(MySqlQueryError::Unsupported(
                            "non-UTC TIMESTAMP INSERT with AUTO_INCREMENT is unsupported"
                                .to_owned(),
                        ));
                    }
                }
            }
        }
        self.refuse_an_upsert_answered_otherwise(sql, self.parser_mode())
            .map_err(mysql_query_parse_error)?;
        self.refuse_literals_their_columns_store_otherwise(sql, self.parser_mode())
            .map_err(mysql_query_parse_error)?;
        self.refuse_written_ids_the_counted_column_cannot_hold(sql)?;
        let deadline = self.write_deadline(timeout);
        self.check_write_deadline(deadline)?;
        self.begin_implicit_transaction_for_write()?;
        if let Some(result) =
            self.write_rows_naming_ids_past_the_counter(sql, deadline, affected_rows_mode)?
        {
            return Ok(result);
        }
        // A statement that writes the counted column its own numbers — which is
        // what a fixture does when it wants known ids — runs as an ordinary
        // INSERT, with the counter raised past the highest number it wrote.
        if let Some(written) = self.raise_the_counter_past_written_ids(sql, deadline)? {
            let mut result = self.execute_ordinary_checked_write_into(
                sql,
                deadline,
                affected_rows_mode,
                Some(&written.table),
            )?;
            result.last_insert_id = written.reported_id;
            match written.raised_once_written {
                Some(_) if self.inner.changes() == 0 => result.last_insert_id = 0,
                Some(high_water) if high_water > 0 => {
                    self.advance_auto_increment_past(&written.table, high_water, deadline)?;
                }
                _ => {}
            }
            return Ok(result);
        }
        match parse_auto_increment_insert(sql, self.parser_mode()) {
            Ok(insert) => match self
                .load_auto_increment_table(insert.table_name().as_str())
                .map_err(MySqlQueryError::Engine)?
            {
                Some(table)
                    if insert.written_row_by_row(&table.definition.allocator_column_name) =>
                {
                    self.execute_auto_increment_conflict_rows(
                        sql,
                        insert,
                        table,
                        &[],
                        deadline,
                        affected_rows_mode,
                    )
                    .map_err(MySqlQueryError::Engine)
                }
                Some(table) => {
                    self.check_write_deadline(deadline)?;
                    let bound = insert
                        .clone()
                        .bind_allocator_table_with(&table.definition, self.written_zero())
                        .map_err(|error| {
                            MySqlQueryError::Engine(LimboError::ParseError(error.to_string()))
                        })?;
                    if let Some(result) = self
                        .execute_high_water_mixed_insert(
                            sql,
                            &bound,
                            &table,
                            &[],
                            deadline,
                            affected_rows_mode,
                        )
                        .map_err(MySqlQueryError::Engine)?
                    {
                        return Ok(result);
                    }
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
                    if let Some(table) = self
                        .load_auto_increment_table(&target)
                        .map_err(MySqlQueryError::Engine)?
                    {
                        self.check_write_deadline(deadline)?;
                        if let Some(copy) = turso_mysql_parser::parse_optional_insert_select(
                            sql,
                            self.parser_mode(),
                        )
                        .map_err(mysql_query_parse_error)?
                        {
                            return self.execute_counted_insert_select(
                                sql,
                                &copy,
                                table,
                                &[],
                                deadline,
                                affected_rows_mode,
                            );
                        }
                        return Err(MySqlQueryError::Unsupported(
                            "AUTO_INCREMENT INSERT supports only an explicit column list and direct literal VALUES rows".to_string(),
                        ));
                    }
                }
                self.execute_ordinary_checked_write(sql, deadline, affected_rows_mode)
            }
        }
    }

    /// Refuses an `INSERT` writing a counted column an id its type cannot
    /// hold.
    ///
    /// Measured on MySQL 8.4.11: `-2147483649` or `2147483648` into an
    /// `INT AUTO_INCREMENT` key is 1264, as `-1` into an `INT UNSIGNED` one
    /// and `-9223372036854775809` into a `BIGINT` one are, in one row or
    /// beside others, in an upsert and in a `REPLACE`, and nothing is written
    /// and the counter does not move. `INSERT IGNORE` writes the nearest id
    /// the type holds and warns instead, which is refused here.
    fn refuse_written_ids_the_counted_column_cannot_hold(
        &self,
        sql: &str,
    ) -> std::result::Result<(), MySqlQueryError> {
        let mode = self.parser_mode();
        let Ok(Some(target)) = parse_auto_increment_insert_target(sql, mode) else {
            return Ok(());
        };
        let Some(table) = self
            .load_auto_increment_table(&target)
            .map_err(MySqlQueryError::Engine)?
        else {
            return Ok(());
        };
        let Ok(Some(written)) =
            parse_insert_values_written_into(sql, mode, &table.definition.allocator_column_name)
        else {
            return Ok(());
        };
        let Some(id) = written
            .into_iter()
            .filter_map(|value| match value {
                CheckedInsertValue::SignedInteger(id) => Some(i128::from(id)),
                CheckedInsertValue::UnsignedInteger(id) => Some(i128::from(id)),
                CheckedInsertValue::PastEveryInteger(id) => Some(id),
                CheckedInsertValue::Null
                | CheckedInsertValue::Default
                | CheckedInsertValue::Other => None,
            })
            .find(|id| !counted_column_holds(&table, *id))
        else {
            return Ok(());
        };
        if turso_mysql_parser::insert_ignores_errors(sql, mode) {
            return Err(MySqlQueryError::Unsupported(
                "INSERT IGNORE writing a counted column an id its type cannot hold".to_string(),
            ));
        }
        hold_the_id_to_the_counted_column(&table, id).map_err(MySqlQueryError::Engine)
    }

    fn execute_ordinary_checked_write(
        &self,
        sql: &str,
        deadline: Option<turso_core::MonotonicInstant>,
        affected_rows_mode: MySqlAffectedRowsMode,
    ) -> std::result::Result<MySqlWriteResult, MySqlQueryError> {
        self.execute_ordinary_checked_write_into(sql, deadline, affected_rows_mode, None)
    }

    /// Runs one ordinary checked write, naming the counted table when the
    /// statement writes one its own numbers.
    ///
    /// A counted table's record is checked by a validator of its own, which is
    /// what keeps an unchecked INSERT from walking past the counter.
    fn execute_ordinary_checked_write_into(
        &self,
        sql: &str,
        deadline: Option<turso_core::MonotonicInstant>,
        affected_rows_mode: MySqlAffectedRowsMode,
        counted: Option<&AutoIncrementTable>,
    ) -> std::result::Result<MySqlWriteResult, MySqlQueryError> {
        let mode = self.parser_mode();
        let (translated, column_types, table_definition) = self
            .parse_checked_dml_translation(sql, mode)
            .map_err(mysql_query_parse_error)?;
        // A DML `WHERE` is held to the rule a `SELECT` `WHERE` obeys, so the
        // rows a comparison names cannot depend on the statement asking.
        self.validate_dml_comparison_columns(&translated)
            .map_err(|error| MySqlQueryError::Unsupported(error.to_string()))?;
        self.validate_dml_ordered_columns(translated.source_table(), translated.ordered_columns())
            .map_err(|error| MySqlQueryError::Unsupported(error.to_string()))?;
        self.reject_non_utc_timestamp_dml_source(&translated)?;
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
                    // Measured on MySQL 8.4.11: an id the column cannot hold
                    // is 1264 here too, and the row keeps its own.
                    if let CheckedUpdateAssignmentValue::SignedInteger(value) = assignment.value() {
                        hold_the_id_to_the_counted_column(&table, i128::from(value))
                            .map_err(MySqlQueryError::Engine)?;
                    }
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
        let mut statement = translated
            .parse_ast()
            .map_err(|error| MySqlQueryError::Syntax(error.to_string()))?;
        let shifted_timestamp_insert = self.shift_timestamp_insert_literals(&mut statement)?;
        let is_update = matches!(statement, Stmt::Update(_));
        let insert_target = checked_insert_target(&statement).map_err(MySqlQueryError::Engine)?;
        if let Some(target) = &insert_target {
            self.check_the_triggers_an_insert_sets_off(
                target.table().as_str(),
                &read_table_names(&translated),
            )
            .map_err(MySqlQueryError::Engine)?;
        }
        let mut frozen = self.frozen_dml_parser(mode, column_types, table_definition, &translated);
        if shifted_timestamp_insert {
            frozen.shifted_timestamp_insert = Some(statement.clone());
        }
        if translated.copies_a_select_rendered_knowing_its_types() {
            frozen.typed_copy = Some(statement.clone());
        }
        let mut options = PrepareOptions::default()
            .with_reprepare_parser(Arc::new(frozen))
            .with_rows_foreign_keys_refuse_skipped(turso_mysql_parser::deletes_ignoring_errors(
                sql, mode,
            ));
        if let Some(table) = counted {
            options =
                options.with_assignment_validator(Arc::new(CountedTableAssignmentValidator {
                    table_name: table.name.clone(),
                    table_sql: table.stored_sql.to_string(),
                    allocator_column_ordinal: table.definition.allocator_column_ordinal,
                }));
        }
        let mut statement = self
            .inner
            .prepare_translated_stmt_with_options(statement, sql, &options)
            .map_err(MySqlQueryError::Engine)?;
        if let Some(target) = &insert_target {
            self.check_write_deadline(deadline)?;
            let missing = self
                .missing_required_insert_column(target, &[])
                .map_err(MySqlQueryError::Engine)?;
            self.check_write_deadline(deadline)?;
            if let Some(column) = missing {
                return Err(MySqlQueryError::MissingRequiredDefault(column));
            }
        }
        let timeout = self.remaining_write_timeout(deadline)?;
        run_checked_write_statement(&mut statement, timeout)
            .map_err(|error| {
                self.map_unsigned_decimal_write_error(
                    error,
                    insert_target
                        .as_ref()
                        .map(|target| target.table().as_str())
                        .or_else(|| translated.source_table()),
                )
            })
            .map_err(MySqlQueryError::Engine)?;
        Ok(MySqlWriteResult {
            affected_rows: self.affected_rows(is_update, affected_rows_mode)?,
            last_insert_id: 0,
        })
    }

    /// Copies the rows a `SELECT` answers into a table that counts its own ids.
    ///
    /// Measured on MySQL 8.4.11: the rows take the next numbers in the order
    /// the `SELECT` answers them, the statement reports the first and
    /// `LAST_INSERT_ID()` answers it, and a `SELECT` reading the table being
    /// written sees only the rows that stood before the statement — which is
    /// why every row is read before any is written. A `SELECT` answering no
    /// rows writes none, reports no id and leaves the counter where it stood.
    ///
    /// Rows naming their own ids raise the counter past the highest and report
    /// the last row's, as a `VALUES` statement's do. Rows asking for the next
    /// number beside rows naming their own are refused: measured, MySQL takes a
    /// new batch of numbers whenever a written id passes the batch it holds,
    /// which this does not repeat.
    fn execute_counted_insert_select(
        &self,
        sql: &str,
        copy: &turso_mysql_parser::MySqlInsertSelect,
        table: AutoIncrementTable,
        values: &[Value],
        deadline: Option<turso_core::MonotonicInstant>,
        affected_rows_mode: MySqlAffectedRowsMode,
    ) -> std::result::Result<MySqlWriteResult, MySqlQueryError> {
        if self.time_zone_offset_seconds() != 0 {
            return Err(MySqlQueryError::Unsupported(
                "INSERT SELECT into an AUTO_INCREMENT table in a non-UTC time zone".to_string(),
            ));
        }
        let mode = self.parser_mode();
        let (translated, ..) = self
            .parse_checked_dml_translation(sql, mode)
            .map_err(mysql_query_parse_error)?;
        self.validate_dml_comparison_columns(&translated)
            .map_err(|error| MySqlQueryError::Unsupported(error.to_string()))?;
        self.validate_dml_ordered_columns(translated.source_table(), translated.ordered_columns())
            .map_err(|error| MySqlQueryError::Unsupported(error.to_string()))?;
        self.check_the_triggers_an_insert_sets_off(&table.name, &read_table_names(&translated))
            .map_err(MySqlQueryError::Engine)?;
        let Stmt::Insert {
            with: None,
            or_conflict,
            body: InsertBody::Select(source, None),
            returning,
            ..
        } = translated
            .parse_ast()
            .map_err(|error| MySqlQueryError::Syntax(error.to_string()))?
        else {
            return Err(MySqlQueryError::Unsupported(
                "INSERT SELECT into an AUTO_INCREMENT table".to_string(),
            ));
        };
        let resolves_as_written = match or_conflict {
            None => !copy.ignores(),
            Some(turso_parser::ast::ResolveType::Ignore) => copy.ignores(),
            Some(_) => false,
        };
        if !resolves_as_written
            || !returning.is_empty()
            || matches!(source.body.select, OneSelect::Values(_))
        {
            return Err(MySqlQueryError::Unsupported(
                "INSERT SELECT into an AUTO_INCREMENT table".to_string(),
            ));
        }
        let rows = self.read_rows_to_copy(sql, source, copy.columns().len(), values, deadline)?;
        if rows.is_empty() {
            return Ok(MySqlWriteResult {
                affected_rows: 0,
                last_insert_id: 0,
            });
        }

        let allocator_column = &table.definition.allocator_column_name;
        let named_at = copy
            .columns()
            .iter()
            .position(|column| column.eq_ignore_ascii_case(allocator_column));
        let mut columns = copy
            .columns()
            .iter()
            .map(|column| mysql_quoted(column))
            .collect::<Vec<_>>();
        if named_at.is_none() {
            columns.insert(0, mysql_quoted(allocator_column));
        }
        let one_row = format!(
            "{} {} ({}) VALUES ({})",
            if copy.ignores() {
                "INSERT IGNORE INTO"
            } else {
                "INSERT INTO"
            },
            mysql_quoted(&table.name),
            columns.join(", "),
            vec!["?"; columns.len()].join(", ")
        );
        let statement = parse_prepared_auto_increment_insert(&one_row, mode)
            .and_then(|insert| {
                insert.bind_allocator_table_with(&table.definition, self.written_zero())
            })
            .and_then(|bound| bound.inject_row_ids(&[None]))
            .map_err(|error| MySqlQueryError::Unsupported(error.to_string()))?;
        let target = checked_insert_target(&statement)
            .map_err(MySqlQueryError::Engine)?
            .ok_or_else(|| {
                MySqlQueryError::Engine(LimboError::InternalError(
                    "a counted copy's INSERT has no target".to_string(),
                ))
            })?;
        if let Some(column) = self
            .missing_required_insert_column(&target, &[])
            .map_err(MySqlQueryError::Engine)?
        {
            return Err(MySqlQueryError::MissingRequiredDefault(column));
        }

        let written_ids = match named_at {
            None => vec![None; rows.len()],
            Some(at) => rows
                .iter()
                .map(|row| self.id_a_copied_row_writes(&row[at], &table))
                .collect::<std::result::Result<Vec<_>, _>>()?,
        };
        if copy.ignores() {
            if written_ids.iter().any(Option::is_some) {
                return Err(MySqlQueryError::Unsupported(
                    "INSERT IGNORE SELECT writing its own AUTO_INCREMENT ids".to_string(),
                ));
            }
            return self.copy_rows_ignoring_collisions(
                sql,
                statement,
                &table,
                named_at,
                rows,
                deadline,
                affected_rows_mode,
            );
        }
        let (ids, first_generated, reported_id) = if written_ids.iter().all(Option::is_none) {
            let first = self.reserve_ids_for_copied_rows(&table, rows.len(), deadline)?;
            let ids = (0..rows.len() as u64)
                .map(|offset| counted_id_value(&table, first + offset))
                .collect::<std::result::Result<Vec<_>, _>>()?;
            (ids, Some(first), first)
        } else if written_ids.iter().all(Option::is_some) {
            let written_ids = written_ids.into_iter().flatten().collect::<Vec<_>>();
            let highest = written_ids.iter().copied().max().unwrap_or(0);
            if highest > 0 {
                self.advance_auto_increment_past(&table, highest as u64, deadline)?;
            }
            let at = named_at.expect("a row names its own id only through a listed column");
            let ids = rows.iter().map(|row| row[at].clone()).collect();
            let last = *written_ids.last().expect("a copy has at least one row");
            (ids, None, last as u64)
        } else {
            return Err(MySqlQueryError::Unsupported(
                "INSERT SELECT mixing written AUTO_INCREMENT ids with ones it asks for".to_string(),
            ));
        };

        let options = injected_auto_increment_prepare_options(&table, statement.clone());
        let mut writing = self
            .inner
            .prepare_translated_stmt_with_options(statement, sql, &options)
            .map_err(MySqlQueryError::Engine)?;
        const SAVEPOINT: &str = "\"__turso_auto_increment_values\"";
        self.run_internal(&format!("SAVEPOINT {SAVEPOINT}"))?;
        let written = (|| -> Result<u64> {
            let mut affected_rows = 0_u64;
            for (mut row, id) in rows.into_iter().zip(ids) {
                self.check_write_deadline(deadline)
                    .map_err(Into::<LimboError>::into)?;
                match named_at {
                    Some(at) => row[at] = id,
                    None => row.insert(0, id),
                }
                bind_prepared_values(&mut writing, &row)?;
                let timeout = self
                    .remaining_write_timeout(deadline)
                    .map_err(Into::<LimboError>::into)?;
                let run = run_checked_write_statement(&mut writing, timeout).map_err(|error| {
                    self.map_unsigned_decimal_write_error(error, Some(&table.name))
                });
                writing.reset()?;
                run?;
                affected_rows = affected_rows
                    .checked_add(
                        self.affected_rows(false, affected_rows_mode)
                            .map_err(Into::<LimboError>::into)?,
                    )
                    .ok_or(LimboError::IntegerOverflow)?;
            }
            Ok(affected_rows)
        })();
        match written {
            Ok(affected_rows) => {
                self.run_internal(&format!("RELEASE SAVEPOINT {SAVEPOINT}"))?;
                if let Some(first) = first_generated {
                    self.inner.set_mysql_last_insert_id(first);
                }
                Ok(MySqlWriteResult {
                    affected_rows,
                    last_insert_id: reported_id,
                })
            }
            Err(error) => {
                self.run_internal(&format!("ROLLBACK TO SAVEPOINT {SAVEPOINT}"))?;
                self.run_internal(&format!("RELEASE SAVEPOINT {SAVEPOINT}"))?;
                Err(MySqlQueryError::Engine(error))
            }
        }
    }

    /// Copies the rows an `INSERT IGNORE ... SELECT` reads into a table that
    /// counts its own ids, a row at a time.
    ///
    /// Measured on MySQL 8.4.11 with `innodb_autoinc_lock_mode = 2`: each row
    /// asks the counter for a number, which it takes in batches of 1, 2, 4
    /// and on up as a plain copy does, and a row `IGNORE` skips gives its
    /// number back to the row after it. So copying 1, 2, 3 and 4 where 1 and
    /// 2 are taken, into a table counting at 3, writes 3 and 4 as ids 3 and 4
    /// and leaves `AUTO_INCREMENT=6`; a copy whose every row collides still
    /// spends the batch of one its first row took; and a skipped row after the
    /// last written one can take a batch of its own. The statement reports
    /// the first id it wrote, and 0 with `LAST_INSERT_ID()` left alone when it
    /// wrote none.
    #[allow(clippy::too_many_arguments)]
    fn copy_rows_ignoring_collisions(
        &self,
        sql: &str,
        statement: Stmt,
        table: &AutoIncrementTable,
        named_at: Option<usize>,
        rows: Vec<Vec<Value>>,
        deadline: Option<turso_core::MonotonicInstant>,
        affected_rows_mode: MySqlAffectedRowsMode,
    ) -> std::result::Result<MySqlWriteResult, MySqlQueryError> {
        let options = injected_auto_increment_prepare_options(table, statement.clone());
        let mut writing = self
            .inner
            .prepare_translated_stmt_with_options(statement, sql, &options)
            .map_err(MySqlQueryError::Engine)?;
        const SAVEPOINT: &str = "\"__turso_auto_increment_values\"";
        self.run_internal(&format!("SAVEPOINT {SAVEPOINT}"))?;
        let written = (|| -> std::result::Result<(u64, Option<u64>), MySqlQueryError> {
            let mut numbers = NumbersInBatches::default();
            let mut affected_rows = 0_u64;
            let mut first_written = None;
            for mut row in rows {
                self.check_write_deadline(deadline)?;
                let id = match numbers.next_unused() {
                    Some(id) => id,
                    None => {
                        let first =
                            self.reserve_counted_numbers(table, numbers.next_batch(), deadline)?;
                        numbers.take_batch(first)
                    }
                };
                let value = counted_id_value(table, id)?;
                match named_at {
                    Some(at) => row[at] = value,
                    None => row.insert(0, value),
                }
                bind_prepared_values(&mut writing, &row).map_err(MySqlQueryError::Engine)?;
                let timeout = self.remaining_write_timeout(deadline)?;
                let run = run_checked_write_statement(&mut writing, timeout).map_err(|error| {
                    self.map_unsigned_decimal_write_error(error, Some(&table.name))
                });
                writing.reset().map_err(MySqlQueryError::Engine)?;
                run.map_err(MySqlQueryError::Engine)?;
                let wrote = self.affected_rows(false, affected_rows_mode)?;
                if wrote > 0 {
                    numbers.spend(id);
                    first_written.get_or_insert(id);
                    affected_rows = affected_rows
                        .checked_add(wrote)
                        .ok_or(MySqlQueryError::Engine(LimboError::IntegerOverflow))?;
                }
            }
            Ok((affected_rows, first_written))
        })();
        match written {
            Ok((affected_rows, first_written)) => {
                self.run_internal(&format!("RELEASE SAVEPOINT {SAVEPOINT}"))?;
                if let Some(first) = first_written {
                    self.inner.set_mysql_last_insert_id(first);
                }
                Ok(MySqlWriteResult {
                    affected_rows,
                    last_insert_id: first_written.unwrap_or(0),
                })
            }
            Err(error) => {
                self.run_internal(&format!("ROLLBACK TO SAVEPOINT {SAVEPOINT}"))?;
                self.run_internal(&format!("RELEASE SAVEPOINT {SAVEPOINT}"))?;
                Err(error)
            }
        }
    }

    /// Reads every row an `INSERT ... SELECT` copies before any is written.
    fn read_rows_to_copy(
        &self,
        sql: &str,
        source: turso_parser::ast::Select,
        width: usize,
        values: &[Value],
        deadline: Option<turso_core::MonotonicInstant>,
    ) -> std::result::Result<Vec<Vec<Value>>, MySqlQueryError> {
        let statement = Stmt::Select(source);
        let options = PrepareOptions::default().with_reprepare_parser(Arc::new(
            FrozenInjectedAutoIncrementInsertParser {
                statement: statement.clone(),
            },
        ));
        let mut reading = self
            .inner
            .prepare_translated_stmt_with_options(statement, sql, &options)
            .map_err(MySqlQueryError::Engine)?;
        if reading.num_columns() != width {
            return Err(MySqlQueryError::Unsupported(
                "INSERT SELECT answering a different number of columns than it names".to_string(),
            ));
        }
        if reading.parameters_count() != values.len() {
            return Err(MySqlQueryError::Engine(LimboError::InternalError(
                "a counted copy's SELECT binds a different number of values".to_string(),
            )));
        }
        bind_prepared_values(&mut reading, values).map_err(MySqlQueryError::Engine)?;
        if let Some(timeout) = self.remaining_write_timeout(deadline)? {
            reading.set_query_timeout_override(Some(Some(timeout)));
        }
        reading.run_collect_rows().map_err(MySqlQueryError::Engine)
    }

    /// The id one copied row writes itself, or `None` where it asks the
    /// counter for the next one — a NULL, and a 0 unless the session's
    /// `sql_mode` names `NO_AUTO_VALUE_ON_ZERO`.
    fn id_a_copied_row_writes(
        &self,
        value: &Value,
        table: &AutoIncrementTable,
    ) -> std::result::Result<Option<i128>, MySqlQueryError> {
        let written = match value {
            Value::Null => return Ok(None),
            Value::Text(text)
                if table.definition.allocator_column_type
                    == turso_mysql_parser::MySqlIntegerType::BigIntUnsigned =>
            {
                text.as_str().parse::<u64>().ok().map(i128::from)
            }
            value => value.as_int().map(i128::from),
        };
        match written {
            Some(0) if self.written_zero() == WrittenZero::AsksForTheNextNumber => Ok(None),
            Some(id) => {
                hold_the_id_to_the_counted_column(table, id).map_err(MySqlQueryError::Engine)?;
                Ok(Some(id))
            }
            None => Err(MySqlQueryError::Unsupported(
                "INSERT SELECT writing an AUTO_INCREMENT id that is not a whole number".to_string(),
            )),
        }
    }

    /// Reserves the numbers MySQL spends on `rows` copied rows and answers the
    /// first of them.
    ///
    /// MySQL cannot know how many rows a `SELECT` answers, so it takes numbers
    /// in batches of 1, 2, 4 and on up, and the ones the last batch leaves
    /// unused are spent: measured on 8.4.11, 1 row moves the counter on by 1,
    /// 3 rows by 3, 4 rows by 7, 8 rows by 15, and 9 rows into an empty table
    /// leave it at `AUTO_INCREMENT=16`.
    fn reserve_ids_for_copied_rows(
        &self,
        table: &AutoIncrementTable,
        rows: usize,
        deadline: Option<turso_core::MonotonicInstant>,
    ) -> std::result::Result<u64, MySqlQueryError> {
        self.reserve_counted_numbers(table, numbers_spent_on_copied_rows(rows as u64), deadline)
    }

    /// Reserves `spent` numbers of a table's counter in one batch and answers
    /// the first of them.
    fn reserve_counted_numbers(
        &self,
        table: &AutoIncrementTable,
        spent: u64,
        deadline: Option<turso_core::MonotonicInstant>,
    ) -> std::result::Result<u64, MySqlQueryError> {
        let capability = self.auto_increment.as_ref().ok_or_else(|| {
            MySqlQueryError::Unsupported(
                "AUTO_INCREMENT INSERT requires a registry-backed allocator capability".to_string(),
            )
        })?;
        let ceiling = if table.definition.allocator_column_type
            == turso_mysql_parser::MySqlIntegerType::BigIntUnsigned
        {
            u64::MAX - 2
        } else {
            auto_increment_ceiling(table)
        };
        // Measured, MySQL cuts the last batch short at the column's highest
        // number and still writes the rows that fit, which this does not
        // repeat.
        let high_water = self
            .when_the_counter_is_free(&capability.allocator, || {
                let mut peek = capability.allocator.peek_high_water(table.key)?;
                capability.io.block(|| peek.step())
            })
            .map_err(MySqlQueryError::Engine)?;
        if high_water
            .checked_add(spent)
            .is_none_or(|last| last > ceiling)
        {
            return Err(MySqlQueryError::Unsupported(
                "INSERT SELECT whose batch of AUTO_INCREMENT numbers passes the column's type"
                    .to_string(),
            ));
        }
        self.check_write_deadline(deadline)?;
        let range = self
            .when_the_counter_is_free(&capability.allocator, || {
                let mut reservation = capability.allocator.reserve(table.key, spent)?;
                capability.io.block(|| reservation.step())
            })
            .map_err(MySqlQueryError::Engine)?;
        if range.last() > ceiling {
            return Err(MySqlQueryError::Engine(LimboError::Constraint(
                "AUTO_INCREMENT value is outside the column's type".to_string(),
            )));
        }
        self.check_write_deadline(deadline)?;
        Ok(range.first())
    }

    fn map_unsigned_decimal_write_error(
        &self,
        error: LimboError,
        table_name: Option<&str>,
    ) -> LimboError {
        let type_name = match &error {
            LimboError::Constraint(message) if message == "negative value for unsigned DECIMAL" => {
                "DECIMAL UNSIGNED"
            }
            LimboError::Constraint(message)
                if message == "value out of range for BIGINT UNSIGNED" =>
            {
                "BIGINT UNSIGNED"
            }
            _ => return error,
        };
        let Some(table_name) = table_name else {
            return error;
        };
        let Ok(table) = MySqlTableName::parse(table_name) else {
            return error;
        };
        let Ok(columns) = self.list_shared_columns(&table) else {
            return error;
        };
        let Some(column_index) = columns
            .iter()
            .position(|column| column.type_name().eq_ignore_ascii_case(type_name))
        else {
            return error;
        };
        turso_core::AssignmentError::OutOfRange {
            table: table_name.to_string(),
            column: column_index + 1,
            type_name: type_name.to_string(),
            value: 0,
        }
        .into()
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
        let set_by_a_trigger = self.columns_set_before_insert(table.as_str());
        Ok(columns
            .into_iter()
            .find(|column| {
                !column.nullable
                    && column.default_value.is_none()
                    && !set_by_a_trigger
                        .iter()
                        .any(|set| set.eq_ignore_ascii_case(&column.name))
            })
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
                // Measured on MySQL 8.4.11, `INSERT IGNORE` stores the type's
                // empty value where a NULL meets a NOT NULL column and warns
                // 1048, where the engine's `OR IGNORE` would skip the row.
                if insert.ignores && insert.binds_null_to_a_not_null_column(&rules.not_null, bound)
                {
                    return Err(LimboError::InvalidArgument(format!(
                        "IGNORE writing a bound NULL into a NOT NULL column of {}",
                        insert.table.as_str()
                    )));
                }
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
        let set_by_a_trigger = self.columns_set_before_insert(table.as_str());
        let mut rules = InsertColumnRules::default();
        for column in core_table.columns() {
            // A counted table's rowid alias and a generated column are filled
            // in by the engine, so the statement never has to name either one.
            // A key that is the rowid of a table counting nothing is given by
            // the row, as every other key is.
            let filled_in_by_the_engine =
                column.is_rowid_alias() && !column.rowid_must_be_written();
            if !column.notnull() || filled_in_by_the_engine || column.is_generated() {
                continue;
            }
            let Some(name) = column.name.clone() else {
                continue;
            };
            if column.default.is_none()
                && !set_by_a_trigger
                    .iter()
                    .any(|set| set.eq_ignore_ascii_case(&name))
            {
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
            (false, _) if self.inner.mysql_replaced_rows() > 0 => self
                .inner
                .changes()
                .saturating_add(self.inner.mysql_replaced_rows()),
            (false, MySqlAffectedRowsMode::Matched) if self.inner.mysql_updated_rows() > 0 => self
                .inner
                .changes()
                .saturating_add(self.inner.mysql_changed_rows()),
            // An `INSERT ... ON DUPLICATE KEY UPDATE` counts by what it did to
            // each row rather than by how many it touched. Measured on MySQL
            // 8.4.11: one for a row it wrote, two for a row it changed and
            // zero for a row it left as it stood. The engine says how many
            // rows it wrote over one already there and how many of those
            // changed, which is what tells the three apart.
            (false, _) if self.inner.mysql_updated_rows() > 0 => {
                let touched = self.inner.changes();
                let updated = self.inner.mysql_updated_rows();
                let changed = self.inner.mysql_changed_rows();
                touched.saturating_sub(updated) + changed.saturating_mul(2)
            }
            _ => self.inner.changes(),
        };
        u64::try_from(rows).map_err(|_| {
            MySqlQueryError::Engine(LimboError::InternalError(
                "successful MySQL write produced a negative affected-row count".to_string(),
            ))
        })
    }

    /// Raises a counted table's counter past the ids one INSERT writes itself.
    ///
    /// Measured on MySQL 8.4.11: the counter moves past the highest id the
    /// statement wrote, so a row written out of order still leaves it at one
    /// past the highest; a written id below the counter leaves it where it is;
    /// and a written id changes nothing about `LAST_INSERT_ID()`, which the
    /// ordinary write path also leaves alone. The statement's own reported id
    /// is the last row's written value, which is a different number from the
    /// one the counter moved past when the rows descend.
    ///
    /// A written 0 and a written NULL each ask the counter for the next number
    /// instead of naming one, exactly as leaving the column out does, so a
    /// statement whose every row does that is left to the reserved path. One
    /// that mixes the two is refused: measured, `VALUES (NULL, 6), (50, 7),
    /// (NULL, 8)` writes 6, 50 and 51, the counter moving past each written
    /// number as the rows go by, which one range reserved up front cannot do.
    /// Writes a `VALUES` insert whose every row names its own positive id, one
    /// of them past the counter, under the counter's lease, moving it past
    /// each row's number once that row is written — as the prepared form is.
    /// Measured on MySQL 8.4.11: an id of 20 refused as a duplicate of another
    /// key leaves the next number where it was. Answers `None` for any other
    /// statement, and for rows a trigger numbering a counted table of its own
    /// sets off, which keep the older path raising the counter first.
    fn write_rows_naming_ids_past_the_counter(
        &self,
        sql: &str,
        deadline: Option<turso_core::MonotonicInstant>,
        affected_rows_mode: MySqlAffectedRowsMode,
    ) -> std::result::Result<Option<MySqlWriteResult>, MySqlQueryError> {
        let Ok(insert) = parse_auto_increment_insert(sql, self.parser_mode()) else {
            return Ok(None);
        };
        let Some(table) = self
            .load_auto_increment_table(insert.table_name().as_str())
            .map_err(MySqlQueryError::Engine)?
        else {
            return Ok(None);
        };
        let Ok(bound) = insert.bind_allocator_table_with(&table.definition, self.written_zero())
        else {
            return Ok(None);
        };
        if bound.rowwise_conflicts()
            || !bound
                .row_values()
                .iter()
                .all(|value| matches!(value, AutoIncrementRowValue::Explicit(id) if *id > 0))
            || self
                .check_the_triggers_an_insert_sets_off(&table.name, &[])
                .map_err(MySqlQueryError::Engine)?
        {
            return Ok(None);
        }
        self.execute_high_water_mixed_insert(sql, &bound, &table, &[], deadline, affected_rows_mode)
            .map_err(MySqlQueryError::Engine)
    }

    fn raise_the_counter_past_written_ids(
        &self,
        sql: &str,
        deadline: Option<turso_core::MonotonicInstant>,
    ) -> std::result::Result<Option<WrittenAutoIncrementIds>, MySqlQueryError> {
        // Several rows of an upsert or an `IGNORE` are written one at a time,
        // each row's id reported as MySQL reports it and the counter moved past
        // the rows written alone, which one statement cannot do.
        let insert = parse_auto_increment_insert(sql, self.parser_mode());
        if insert
            .as_ref()
            .is_ok_and(|insert| insert.rowwise_conflicts())
        {
            return Ok(None);
        }
        let ignores = insert.is_ok_and(|insert| insert.ignores());
        let Some(target) = parse_auto_increment_insert_target(sql, self.parser_mode())
            .map_err(mysql_query_parse_error)?
        else {
            return Ok(None);
        };
        let Some(table) = self
            .load_auto_increment_table(&target)
            .map_err(MySqlQueryError::Engine)?
        else {
            return Ok(None);
        };
        let Some(written) = parse_insert_values_written_into(
            sql,
            self.parser_mode(),
            &table.definition.allocator_column_name,
        )
        .map_err(mysql_query_parse_error)?
        else {
            return Ok(None);
        };
        // `DEFAULT` in that column asks for the next number, which is what
        // leaving the column out asks for, and the reserved path answers it by
        // dropping the column.
        if written.iter().all(|value| {
            matches!(
                value,
                CheckedInsertValue::Default
                    | CheckedInsertValue::Null
                    | CheckedInsertValue::SignedInteger(0)
            )
        }) {
            return Ok(None);
        }
        let mut high_water = 0_u64;
        let mut last = 0_u64;
        for value in written {
            match value {
                // A negative id is stored as written and leaves the counter
                // alone, which is what MySQL does with one.
                CheckedInsertValue::SignedInteger(number) if number != 0 => {
                    if number > 0 {
                        high_water = high_water.max(number as u64);
                    }
                    last = number as u64;
                }
                CheckedInsertValue::UnsignedInteger(number)
                    if table.definition.allocator_column_type
                        == turso_mysql_parser::MySqlIntegerType::BigIntUnsigned
                        && number != 0 =>
                {
                    high_water = high_water.max(number);
                    last = number;
                }
                _ => {
                    return Ok(None);
                }
            }
        }
        if high_water > 0 && !ignores {
            self.advance_auto_increment_past(&table, high_water, deadline)?;
        }
        Ok(Some(WrittenAutoIncrementIds {
            table,
            reported_id: last,
            raised_once_written: ignores.then_some(high_water),
        }))
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
        self.when_the_counter_is_free(&capability.allocator, || {
            let mut operation = capability.allocator.advance_past(table.key, high_water)?;
            capability.io.block(|| operation.step())
        })
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
        let bound = insert
            .clone()
            .bind_allocator_table_with(&table.definition, self.written_zero())
            .map_err(|error| LimboError::ParseError(error.to_string()))?;
        if self
            .execute_high_water_mixed_insert(
                sql,
                &bound,
                &table,
                &[],
                None,
                MySqlAffectedRowsMode::Changed,
            )?
            .is_some()
        {
            return Ok(());
        }
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
        self.check_the_triggers_an_insert_sets_off(&table.name, &[])?;
        self.check_write_deadline(deadline)
            .map_err(Into::<LimboError>::into)?;
        let bound = insert
            .bind_allocator_table_with(&table.definition, self.written_zero())
            .map_err(|error| LimboError::ParseError(error.to_string()))?;
        let stamped = self.moments_an_upsert_stamps(sql)?;
        let reserved = self.write_counted_rows(
            sql,
            &bound,
            &table,
            &[],
            deadline,
            |reserved, take_numbers| {
                self.check_write_deadline(deadline)
                    .map_err(Into::<LimboError>::into)?;
                let take_numbers = std::cell::Cell::new(take_numbers);
                let statement = bound
                    .inject_row_ids(&reserved.ids)
                    .map_err(|error| LimboError::ParseError(error.to_string()))?;
                self.write_stamping_the_row_an_upsert_changes(statement, &stamped, |statement| {
                    let options =
                        injected_auto_increment_prepare_options(&table, statement.clone());
                    let mut statement = self
                        .inner
                        .prepare_translated_stmt_with_options(statement, sql, &options)?;
                    if let Some(take_numbers) = take_numbers.take() {
                        statement.run_before_writing(&table.name, take_numbers);
                    }
                    let timeout = self
                        .remaining_write_timeout(deadline)
                        .map_err(Into::<LimboError>::into)?;
                    run_checked_write_statement(&mut statement, timeout).map_err(|error| {
                        self.map_unsigned_decimal_write_error(error, Some(&table.name))
                    })
                })
            },
        )?;
        // Measured on MySQL 8.4.11: an upsert that changed a row reports that
        // row's own id back to the client and leaves `LAST_INSERT_ID()` where
        // it stood, one that left the row as it stood reports no id at all,
        // and one that added a row reports the number it took and sets the
        // function to it. Which row the upsert matched is decided inside the
        // engine, which answers it here.
        let upserted = self.inner.mysql_upserted_rowid();
        if upserted > 0 {
            if self.inner.mysql_changed_rows() == 0 {
                return Ok(0);
            }
            return self.id_of_counted_row(&table, upserted);
        }
        // A row `IGNORE` skipped took a number and wrote nothing. Measured on
        // 8.4.11: the counter moves past it just the same, the statement
        // reports no id at all, and `LAST_INSERT_ID()` is left where it stood.
        if self.inner.changes() == 0 {
            return Ok(0);
        }
        if let Some(id) = reserved.first_generated {
            self.inner.set_mysql_last_insert_id(id);
            return Ok(id);
        }
        Ok(reserved.last_explicit.unwrap_or(0))
    }

    fn execute_auto_increment_conflict_rows(
        &self,
        sql: &str,
        insert: CheckedAutoIncrementInsert,
        table: AutoIncrementTable,
        values: &[Value],
        deadline: Option<turso_core::MonotonicInstant>,
        affected_rows_mode: MySqlAffectedRowsMode,
    ) -> Result<MySqlWriteResult> {
        self.check_the_triggers_an_insert_sets_off(&table.name, &[])?;
        let upserts = insert.upserts();
        let bound = insert
            .bind_allocator_table_with(&table.definition, self.written_zero())
            .map_err(|error| LimboError::ParseError(error.to_string()))?;
        if !bound.rowwise_conflicts() {
            return Err(LimboError::ParseError(
                "a counted insert written row by row has to meet a conflict in some row"
                    .to_string(),
            ));
        }
        let row_values = self.rows_naming_ids_the_counter_can_follow(&bound, &table, values)?;
        let reserved = if row_values.contains(&InsertAutoIncrementValue::Generated) {
            Some(self.reserve_insert_row_ids(&bound, &table, values, deadline)?)
        } else {
            None
        };
        let mut next_id = reserved.and_then(|reserved| reserved.first_generated);
        let stamped = self.moments_an_upsert_stamps(sql)?;
        const SAVEPOINT: &str = "\"__turso_auto_increment_values\"";
        self.inner
            .prepare(format!("SAVEPOINT {SAVEPOINT}"))?
            .run_ignore_rows()?;
        // MySQL reads the clock once for the whole statement, and each row
        // here is written by a statement of its own, so they read one moment
        // between them.
        let result = turso_core::read_the_clock_once(|| -> Result<MySqlWriteResult> {
            let mut affected_rows = 0_u64;
            let mut first_inserted = None;
            let mut last_row = None;
            let mut wrote_a_row = false;
            for (row, row_value) in row_values.iter().enumerate() {
                self.check_write_deadline(deadline)
                    .map_err(Into::<LimboError>::into)?;
                let id = match row_value {
                    InsertAutoIncrementValue::Generated => Some(next_id.ok_or_else(|| {
                        LimboError::InternalError(
                            "rowwise AUTO_INCREMENT INSERT reserved no ID".to_string(),
                        )
                    })?),
                    InsertAutoIncrementValue::Explicit(_) => None,
                };
                let statement = bound
                    .one_row(row, id)
                    .map_err(|error| LimboError::ParseError(error.to_string()))?;
                self.write_stamping_the_row_an_upsert_changes(statement, &stamped, |statement| {
                    let options =
                        injected_auto_increment_prepare_options(&table, statement.clone());
                    let mut statement = self
                        .inner
                        .prepare_translated_stmt_with_options(statement, sql, &options)?;
                    // One row keeps the numbers its `?`s had in the whole
                    // statement, so the values bound up to its highest one
                    // cover it and the upsert clause after every row.
                    let parameter_count = statement.parameters_count();
                    let bound_values = values.get(..parameter_count).ok_or_else(|| {
                        LimboError::InternalError(
                            "rowwise AUTO_INCREMENT INSERT changed its parameter count".to_string(),
                        )
                    })?;
                    bind_prepared_values(&mut statement, bound_values)?;
                    let timeout = self
                        .remaining_write_timeout(deadline)
                        .map_err(Into::<LimboError>::into)?;
                    run_checked_write_statement(&mut statement, timeout).map_err(|error| {
                        self.map_unsigned_decimal_write_error(error, Some(&table.name))
                    })
                })?;
                affected_rows = affected_rows
                    .checked_add(
                        self.affected_rows(false, affected_rows_mode)
                            .map_err(Into::<LimboError>::into)?,
                    )
                    .ok_or(LimboError::IntegerOverflow)?;
                let upserted = self.inner.mysql_upserted_rowid();
                let inserted = self.inner.changes() > 0 && upserted == 0;
                // A colliding row hands the number it asked for on to the
                // next row asking for one, as MySQL does within a statement.
                last_row = match (row_value, inserted) {
                    (_, false) if upserted > 0 => Some(RowAnUpsertMet::Matched(upserted)),
                    (InsertAutoIncrementValue::Explicit(id), false) if !upserts => {
                        Some(RowAnUpsertMet::Skipped(*id))
                    }
                    (_, false) => None,
                    (InsertAutoIncrementValue::Explicit(id), true) => {
                        self.advance_auto_increment_past(&table, *id, deadline)
                            .map_err(Into::<LimboError>::into)?;
                        Some(RowAnUpsertMet::Written(*id))
                    }
                    (InsertAutoIncrementValue::Generated, true) => {
                        let id = id.expect("a row asking for a number was given one");
                        first_inserted.get_or_insert(id);
                        next_id = Some(id.checked_add(1).ok_or(LimboError::IntegerOverflow)?);
                        Some(RowAnUpsertMet::Written(id))
                    }
                };
                wrote_a_row |= inserted || (upserted > 0 && self.inner.mysql_changed_rows() > 0);
            }
            self.inner
                .prepare(format!("RELEASE SAVEPOINT {SAVEPOINT}"))?
                .run_ignore_rows()?;
            if let Some(id) = first_inserted {
                self.inner.set_mysql_last_insert_id(id);
            }
            // Measured on MySQL 8.4.11: a statement that added a row asking
            // for its number reports the first such number. One that added
            // none but wrote some row — one naming its own id, or one it
            // changed — reports the id of the last row it met, written, changed
            // or left as it stood, and one that wrote nothing reports none.
            let last_insert_id = match (first_inserted, last_row) {
                (Some(id), _) => id,
                (None, Some(RowAnUpsertMet::Written(id) | RowAnUpsertMet::Skipped(id)))
                    if wrote_a_row =>
                {
                    id
                }
                (None, Some(RowAnUpsertMet::Matched(rowid))) if wrote_a_row => {
                    self.id_of_counted_row(&table, rowid)?
                }
                _ => 0,
            };
            Ok(MySqlWriteResult {
                affected_rows,
                last_insert_id,
            })
        });
        if result.is_err() {
            self.inner
                .prepare(format!("ROLLBACK TO SAVEPOINT {SAVEPOINT}"))?
                .run_ignore_rows()?;
            self.inner
                .prepare(format!("RELEASE SAVEPOINT {SAVEPOINT}"))?
                .run_ignore_rows()?;
        }
        result
    }

    /// What each row of a counted upsert written row by row asks the counter
    /// for, holding the ids rows name to ones the counter can follow.
    ///
    /// Measured on MySQL 8.4.11 with GORM's association writes: a row naming
    /// an id at or below the counter leaves the counter where it is, whether
    /// it is written or collides, while the rows asking for a number take the
    /// whole statement's batch at the first of them. A row naming an id past
    /// the counter moves it past that id once the row is written, and not
    /// when it collides. Beside a row asking for a number that decides which
    /// number the next row takes, which the numbers reserved here before any
    /// row is written cannot follow, so the pair is refused, and so are a
    /// negative id and a 0 stored as itself.
    fn rows_naming_ids_the_counter_can_follow(
        &self,
        bound: &BoundAutoIncrementInsert,
        table: &AutoIncrementTable,
        values: &[Value],
    ) -> Result<Vec<InsertAutoIncrementValue>> {
        let row_values = self.auto_increment_row_values(bound, table, values)?;
        let highest_named = row_values
            .iter()
            .filter_map(|value| match value {
                InsertAutoIncrementValue::Explicit(id) => Some(*id),
                InsertAutoIncrementValue::Generated => None,
            })
            .max();
        let Some(highest_named) = highest_named else {
            return Ok(row_values);
        };
        // A negative id is read as 0 here, and so is a 0 the session stores
        // as itself.
        if row_values.contains(&InsertAutoIncrementValue::Explicit(0))
            || highest_named > i64::MAX as u64
        {
            return Err(LimboError::ParseError(
                "a counted upsert row naming an id below 1 or past the engine's integers is unsupported"
                    .to_string(),
            ));
        }
        if highest_named > auto_increment_ceiling(table) {
            return Err(LimboError::Constraint(
                "AUTO_INCREMENT value is outside the column's type".to_string(),
            ));
        }
        if !row_values.contains(&InsertAutoIncrementValue::Generated) {
            return Ok(row_values);
        }
        let capability = self.auto_increment.as_ref().ok_or_else(|| {
            LimboError::ParseError(
                "AUTO_INCREMENT INSERT requires a registry-backed allocator capability".to_string(),
            )
        })?;
        let high_water = self.when_the_counter_is_free(&capability.allocator, || {
            let mut peek = capability.allocator.peek_high_water(table.key)?;
            capability.io.block(|| peek.step())
        })?;
        if highest_named > high_water {
            return Err(LimboError::ParseError(
                "a counted upsert naming an id past the counter beside a row asking for one is unsupported"
                    .to_string(),
            ));
        }
        Ok(row_values)
    }

    /// The `ON UPDATE CURRENT_TIMESTAMP` columns an upsert's clause leaves to
    /// MySQL, each with the places of a second it keeps, or none for any
    /// other statement.
    fn moments_an_upsert_stamps(&self, sql: &str) -> Result<Vec<(String, u8)>> {
        let Some(upsert) = turso_mysql_parser::parse_optional_upsert(sql, self.parser_mode())
            .map_err(|error| LimboError::ParseError(error.to_string()))?
        else {
            return Ok(Vec::new());
        };
        let table = MySqlTableName::parse(&upsert.table)
            .map_err(|error| LimboError::ParseError(error.to_string()))?;
        let columns = self
            .list_shared_columns(&table)
            .map_err(|error| LimboError::ParseError(error.to_string()))?;
        Ok(moments_the_clause_leaves(&columns, &upsert.assigned))
    }

    /// Runs one counted upsert's statement and, where it changed the row it
    /// met, runs it again writing the moment into the `ON UPDATE
    /// CURRENT_TIMESTAMP` columns its clause leaves.
    ///
    /// Measured on MySQL 8.4.11: an upsert writes the moment there whenever
    /// it changes the row it meets — a name differing only in case or by a
    /// trailing space included — and leaves it when the row stands as it was,
    /// a `DECIMAL` offered as `'100.0'` over 100.00 among them. Whether the
    /// row changed is what the engine counts it by, comparing the row it
    /// wrote with the one that stood, so the first run asks that, inside a
    /// savepoint the second one is written in place of.
    fn write_stamping_the_row_an_upsert_changes(
        &self,
        statement: Stmt,
        stamped: &[(String, u8)],
        run: impl Fn(Stmt) -> Result<()>,
    ) -> Result<()> {
        if stamped.is_empty() {
            return run(statement);
        }
        const SAVEPOINT: &str = "\"__turso_stamped_upsert\"";
        self.run_internal(&format!("SAVEPOINT {SAVEPOINT}"))?;
        let leave = |kept: bool| -> Result<()> {
            // A value the assignment check refuses ends the whole
            // transaction, savepoint and all.
            if self.inner.get_auto_commit() {
                return Ok(());
            }
            if !kept {
                self.run_internal(&format!("ROLLBACK TO SAVEPOINT {SAVEPOINT}"))?;
            }
            self.run_internal(&format!("RELEASE SAVEPOINT {SAVEPOINT}"))?;
            Ok(())
        };
        if let Err(error) = run(statement.clone()) {
            leave(false)?;
            return Err(error);
        }
        if self.inner.mysql_upserted_rowid() == 0 || self.inner.mysql_changed_rows() == 0 {
            return leave(true);
        }
        self.run_internal(&format!("ROLLBACK TO SAVEPOINT {SAVEPOINT}"))?;
        let stamping = turso_mysql_parser::stamping_the_moments(&statement, stamped)
            .map_err(|error| LimboError::ParseError(error.to_string()))?;
        let written = run(stamping);
        leave(written.is_ok())?;
        written
    }

    /// The id of the counted row the engine numbers `rowid`.
    ///
    /// A signed or `INT UNSIGNED` counted column is the engine's own row
    /// number. A `BIGINT UNSIGNED` one is a column of its own, since the row
    /// number cannot hold its upper range, and is read back from the row.
    fn id_of_counted_row(&self, table: &AutoIncrementTable, rowid: i64) -> Result<u64> {
        if table.definition.allocator_column_type
            != turso_mysql_parser::MySqlIntegerType::BigIntUnsigned
        {
            return u64::try_from(rowid).map_err(|_| {
                LimboError::InternalError("a counted row has a negative row number".to_string())
            });
        }
        let core_table = self
            .inner
            .current_schema()
            .get_btree_table(&table.name)
            .ok_or(LimboError::SchemaUpdated)?;
        let row_number = ["rowid", "_rowid_", "oid"]
            .into_iter()
            .find(|name| {
                !core_table.columns().iter().any(|column| {
                    column
                        .name
                        .as_deref()
                        .is_some_and(|column| column.eq_ignore_ascii_case(name))
                })
            })
            .ok_or_else(|| {
                LimboError::ParseError(
                    "a table whose columns hide every name for its row number".to_string(),
                )
            })?;
        let rows = self
            .inner
            .prepare(format!(
                "SELECT {} FROM {} WHERE {row_number} = {rowid}",
                sqlite_quoted(&table.definition.allocator_column_name),
                sqlite_quoted(&table.name)
            ))?
            .run_collect_rows()?;
        match rows.as_slice() {
            [row] => match row.as_slice() {
                [value] => match value {
                    Value::Text(text) => text.as_str().parse::<u64>().ok(),
                    value => value.as_int().and_then(|id| u64::try_from(id).ok()),
                },
                _ => None,
            },
            _ => None,
        }
        .ok_or_else(|| {
            LimboError::InternalError("an upserted counted row has no readable id".to_string())
        })
    }

    fn execute_high_water_mixed_insert(
        &self,
        sql: &str,
        bound: &BoundAutoIncrementInsert,
        table: &AutoIncrementTable,
        values: &[Value],
        deadline: Option<turso_core::MonotonicInstant>,
        affected_rows_mode: MySqlAffectedRowsMode,
    ) -> Result<Option<MySqlWriteResult>> {
        let row_values = self.auto_increment_row_values(bound, table, values)?;
        // Rows that all name their own ids come here too when one of them is
        // past the counter — Prisma binds each id it writes — since the counter
        // has to move past a row's number only once the row is written:
        // measured on 8.4.11, an id of 20 refused as a duplicate of another
        // key leaves the next number where it was.
        if bound.rowwise_conflicts() {
            return Ok(None);
        }
        let highest_explicit = row_values
            .iter()
            .filter_map(|value| match value {
                InsertAutoIncrementValue::Explicit(id) => Some(*id),
                InsertAutoIncrementValue::Generated => None,
            })
            .max()
            .unwrap_or(0);
        if highest_explicit == 0 {
            return Ok(None);
        }
        let capability = self.auto_increment.as_ref().ok_or_else(|| {
            LimboError::ParseError(
                "AUTO_INCREMENT INSERT requires a registry-backed allocator capability".to_string(),
            )
        })?;
        let high_water = self.when_the_counter_is_free(&capability.allocator, || {
            let mut peek = capability.allocator.peek_high_water(table.key)?;
            capability.io.block(|| peek.step())
        })?;
        if highest_explicit <= high_water {
            return Ok(None);
        }
        if bound
            .row_values()
            .iter()
            .any(|value| matches!(value, AutoIncrementRowValue::Explicit(id) if *id < 0))
            || bound.row_values().iter().any(|value| match value {
                AutoIncrementRowValue::Parameter(ordinal) => values
                    .get(*ordinal)
                    .and_then(Value::as_int)
                    .is_some_and(|id| id < 0),
                _ => false,
            })
        {
            return Err(LimboError::ParseError(
                "mixed AUTO_INCREMENT INSERT with negative explicit ids is unsupported".to_string(),
            ));
        }
        if highest_explicit > auto_increment_ceiling(table) {
            return Err(LimboError::Constraint(
                "AUTO_INCREMENT value is outside the column's type".to_string(),
            ));
        }
        // The lease holds the counter's file for the whole statement, so a
        // trigger numbering a row from it could not take a number.
        if self.check_the_triggers_an_insert_sets_off(&table.name, &[])? {
            return Err(LimboError::ParseError(
                "rows asking for the next id beside one naming its own id past the counter, with a trigger writing a counted table, are unsupported"
                    .to_string(),
            ));
        }
        self.check_write_deadline(deadline)
            .map_err(Into::<LimboError>::into)?;
        let (mut lease, mut current) =
            self.when_the_counter_is_free(&capability.allocator, || {
                let mut lease = capability.allocator.lease_high_water(table.key)?;
                let current = capability.io.block(|| lease.read())?;
                Ok((lease, current))
            })?;
        if highest_explicit <= current {
            lease.release()?;
            return Ok(None);
        }
        let mut reserved_end = current;
        let mut preallocated = false;
        let generated_ceiling = if table.definition.allocator_column_type
            == turso_mysql_parser::MySqlIntegerType::BigIntUnsigned
        {
            u64::MAX - 2
        } else {
            auto_increment_ceiling(table)
        };
        const SAVEPOINT: &str = "\"__turso_auto_increment_values\"";
        self.inner
            .prepare(format!("SAVEPOINT {SAVEPOINT}"))?
            .run_ignore_rows()?;
        // MySQL reads the clock once for the whole statement, and each row
        // below is written by a statement of its own, so they read one moment
        // between them.
        let result =
            turso_core::read_the_clock_once(|| -> Result<(MySqlWriteResult, Option<u64>)> {
                let mut affected_rows = 0_u64;
                let mut first_generated = None;
                let mut last_explicit = None;
                for (row_number, row_value) in row_values.iter().enumerate() {
                    self.check_write_deadline(deadline)
                        .map_err(Into::<LimboError>::into)?;
                    let id = match row_value {
                        InsertAutoIncrementValue::Generated => {
                            if !preallocated {
                                reserved_end = current
                                    .checked_add(row_values.len() as u64)
                                    .ok_or(LimboError::IntegerOverflow)?;
                                if reserved_end > generated_ceiling {
                                    return Err(LimboError::Constraint(
                                        "AUTO_INCREMENT value is outside the column's type"
                                            .to_string(),
                                    ));
                                }
                                capability.io.block(|| lease.advance_past(reserved_end))?;
                                preallocated = true;
                            }
                            current = current.checked_add(1).ok_or(LimboError::IntegerOverflow)?;
                            if current > generated_ceiling {
                                return Err(LimboError::Constraint(
                                    "AUTO_INCREMENT value is outside the column's type".to_string(),
                                ));
                            }
                            if current > reserved_end {
                                capability.io.block(|| lease.advance_past(current))?;
                                reserved_end = current;
                            }
                            current
                        }
                        InsertAutoIncrementValue::Explicit(id) => *id,
                    };
                    let statement = bound
                        .inject_one_row(row_number, id)
                        .map_err(|error| LimboError::ParseError(error.to_string()))?;
                    let options = injected_auto_increment_prepare_options(table, statement.clone());
                    let mut statement = self
                        .inner
                        .prepare_translated_stmt_with_options(statement, sql, &options)?;
                    let parameter_count = statement.parameters_count();
                    let bound_values = values.get(..parameter_count).ok_or_else(|| {
                        LimboError::InternalError(
                            "rowwise AUTO_INCREMENT INSERT changed its parameter count".to_string(),
                        )
                    })?;
                    bind_prepared_values(&mut statement, bound_values)?;
                    let timeout = self
                        .remaining_write_timeout(deadline)
                        .map_err(Into::<LimboError>::into)?;
                    run_checked_write_statement(&mut statement, timeout).map_err(|error| {
                        self.map_unsigned_decimal_write_error(error, Some(&table.name))
                    })?;
                    affected_rows = affected_rows
                        .checked_add(
                            self.affected_rows(false, affected_rows_mode)
                                .map_err(Into::<LimboError>::into)?,
                        )
                        .ok_or(LimboError::IntegerOverflow)?;
                    match row_value {
                        InsertAutoIncrementValue::Generated => {
                            first_generated.get_or_insert(id);
                        }
                        InsertAutoIncrementValue::Explicit(_) if self.inner.changes() > 0 => {
                            if id > current {
                                current = id;
                                if id > reserved_end {
                                    capability.io.block(|| lease.advance_past(id))?;
                                    reserved_end = id;
                                }
                            }
                            last_explicit = Some(id);
                        }
                        InsertAutoIncrementValue::Explicit(_) => {}
                    }
                }
                Ok((
                    MySqlWriteResult {
                        affected_rows,
                        last_insert_id: first_generated.or(last_explicit).unwrap_or(0),
                    },
                    first_generated,
                ))
            });
        let result = result.and_then(|(result, first_generated)| {
            lease.release()?;
            self.inner
                .prepare(format!("RELEASE SAVEPOINT {SAVEPOINT}"))?
                .run_ignore_rows()?;
            // Measured: a row naming its own id is reported by that id and
            // leaves `LAST_INSERT_ID()` where it stood.
            if let Some(id) = first_generated {
                self.inner.set_mysql_last_insert_id(id);
            }
            Ok(result)
        });
        if result.is_err() {
            self.inner
                .prepare(format!("ROLLBACK TO SAVEPOINT {SAVEPOINT}"))?
                .run_ignore_rows()?;
            self.inner
                .prepare(format!("RELEASE SAVEPOINT {SAVEPOINT}"))?
                .run_ignore_rows()?;
        }
        result.map(Some)
    }

    /// Writes a counted `VALUES` insert whose ids are all asked for, taking
    /// its numbers when MySQL takes them.
    ///
    /// Measured on MySQL 8.4.11: a row found wrong while it is filled — a
    /// value too long, out of range or of the wrong kind, a NULL for a
    /// `NOT NULL` column, a broken `CHECK` — fails before the row takes a
    /// number, so a statement whose first row fails that way spends none. One
    /// failing on a later row spends the statement's whole batch, and so does
    /// one failing on a key or a foreign key, which is found once the row is
    /// written.
    ///
    /// The counter hands out a number only once and never takes one back, so
    /// the rows are written first, inside a savepoint, with the numbers it
    /// would hand out next, and the numbers are taken afterwards — or never,
    /// when the first row failed to fill. A statement whose numbers were
    /// taken by another session in between is undone and written again with
    /// the numbers actually taken, so no number is ever written that the
    /// counter did not hand to this statement. A table carrying a trigger is
    /// written the old way, reserving first: a trigger's own failure is found
    /// after the row is written, and a trigger's rows would take numbers of
    /// their own twice if the statement had to be written again.
    ///
    /// The engine ends the whole transaction, savepoint and all, on a value
    /// the assignment check refuses and on a deadlock, where it ends only the
    /// statement on a broken constraint; the savepoint stands exactly while a
    /// transaction is open, so each step checks that first, and a statement
    /// whose transaction ended is never written again outside it.
    fn write_counted_rows(
        &self,
        sql: &str,
        bound: &BoundAutoIncrementInsert,
        table: &AutoIncrementTable,
        values: &[Value],
        deadline: Option<turso_core::MonotonicInstant>,
        write: impl Fn(&ReservedAutoIncrementRows, Option<TakeNumbersBeforeWriting>) -> Result<()>,
    ) -> Result<ReservedAutoIncrementRows> {
        let reserve_first = || -> Result<ReservedAutoIncrementRows> {
            let reserved = self.reserve_insert_row_ids(bound, table, values, deadline)?;
            write(&reserved, None)?;
            Ok(reserved)
        };
        if !self.counter_numbers_can_be_predicted(bound, table) {
            return reserve_first();
        }
        if self.inner.mvcc_enabled() {
            return match self
                .write_counted_rows_taking_numbers_before_writing(bound, table, values, &write)?
            {
                Some(written) => Ok(written),
                None => reserve_first(),
            };
        }
        let begins_a_transaction = self.inner.get_auto_commit();
        if begins_a_transaction {
            self.run_counted_rows_statement(BEGIN_THE_COUNTED_ROWS_TRANSACTION)?;
        }
        let written =
            self.write_predicted_counted_rows(sql, bound, table, values, deadline, &write);
        let ended = if begins_a_transaction && !self.inner.get_auto_commit() {
            self.run_counted_rows_statement(if written.is_ok() {
                COMMIT_THE_COUNTED_ROWS_TRANSACTION
            } else {
                ROLL_BACK_THE_COUNTED_ROWS_TRANSACTION
            })
        } else {
            Ok(())
        };
        let written = written?;
        ended?;
        match written {
            Some(written) => written,
            None => reserve_first(),
        }
    }

    /// Under MVCC an insert waits for a key another open transaction holds,
    /// and measured on MySQL 8.4.11 InnoDB takes the statement's numbers
    /// before that wait: a session inserting meanwhile takes the numbers after
    /// them. So the rows are written with the numbers the counter would hand
    /// out next, and the engine takes the numbers once the first row is
    /// filled, right before the statement first looks up or writes a key of
    /// the table, which is where it can wait. A first row that fails to fill
    /// ends the statement before then and spends nothing.
    ///
    /// When another session took those numbers in between, the statement
    /// ends there, before it wrote anything, and is written again with the
    /// numbers it took. Answers `None` when the batch would pass the column's
    /// highest value, which the reserving path refuses.
    fn write_counted_rows_taking_numbers_before_writing(
        &self,
        bound: &BoundAutoIncrementInsert,
        table: &AutoIncrementTable,
        values: &[Value],
        write: &impl Fn(&ReservedAutoIncrementRows, Option<TakeNumbersBeforeWriting>) -> Result<()>,
    ) -> Result<Option<ReservedAutoIncrementRows>> {
        let capability = self.auto_increment.as_ref().ok_or_else(|| {
            LimboError::ParseError(
                "AUTO_INCREMENT INSERT requires a registry-backed allocator capability".to_string(),
            )
        })?;
        let last_seen = self.when_the_counter_is_free(&capability.allocator, || {
            capability.allocator.last_seen_high_water(table.key)
        })?;
        let high_water = match last_seen {
            Some(high_water) => high_water,
            None => self.when_the_counter_is_free(&capability.allocator, || {
                let mut peek = capability.allocator.peek_high_water(table.key)?;
                capability.io.block(|| peek.step())
            })?,
        };
        if !the_batch_fits_the_column(table, high_water, bound.row_count().get() as u64) {
            return Ok(None);
        }
        let row_values = self.auto_increment_row_values(bound, table, values)?;
        let predicted = self.row_ids_after(bound, table, values, row_values.clone(), high_water)?;
        let taken_after = Arc::new(std::sync::OnceLock::new());
        let take_numbers = {
            let allocator = capability.allocator.clone();
            let io = capability.io.clone();
            let key = table.key;
            let row_values = row_values.clone();
            let busy_timeout = self.inner.get_busy_timeout();
            let generated_ceiling = generated_ceiling(table);
            let taken_after = taken_after.clone();
            Box::new(move || {
                let reserved = when_the_counter_is_free_within(&allocator, busy_timeout, || {
                    let mut reservation =
                        allocator.reserve_insert_values(key, row_values.clone())?;
                    io.block(|| reservation.step())
                })?;
                taken_after
                    .set(reserved.high_water_before)
                    .expect("a statement takes its numbers once");
                if reserved.high_water_after > generated_ceiling {
                    return Err(LimboError::Constraint(
                        "AUTO_INCREMENT value is outside the column's type".to_string(),
                    ));
                }
                if reserved.high_water_before != high_water {
                    return Err(LimboError::RefusedBeforeWriting(
                        "another session took the AUTO_INCREMENT numbers the rows were written with"
                            .to_string(),
                    ));
                }
                Ok(())
            })
        };
        let written = write(&predicted, Some(take_numbers));
        let Some(&taken_after) = taken_after.get() else {
            return match written {
                Ok(()) => Err(LimboError::InternalError(
                    "a counted insert wrote its rows without taking their numbers".to_string(),
                )),
                Err(error) => Err(error),
            };
        };
        if taken_after == high_water {
            return written.map(|()| Some(predicted));
        }
        match written {
            Err(LimboError::RefusedBeforeWriting(_)) => {}
            Err(error) => return Err(error),
            Ok(()) => {
                return Err(LimboError::InternalError(
                    "a counted insert wrote numbers another session took".to_string(),
                ))
            }
        }
        let taken = self.row_ids_after(bound, table, values, row_values, taken_after)?;
        #[cfg(test)]
        self.counted_rows_written_again
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        write(&taken, None)?;
        Ok(Some(taken))
    }

    /// Writes the rows with the numbers the counter would hand out next, and
    /// answers the statement's own outcome once its savepoint is released, or
    /// `None` when the counter cannot tell them after all.
    fn write_predicted_counted_rows(
        &self,
        sql: &str,
        bound: &BoundAutoIncrementInsert,
        table: &AutoIncrementTable,
        values: &[Value],
        deadline: Option<turso_core::MonotonicInstant>,
        write: &impl Fn(&ReservedAutoIncrementRows, Option<TakeNumbersBeforeWriting>) -> Result<()>,
    ) -> Result<Option<Result<ReservedAutoIncrementRows>>> {
        self.hold_the_write_lock_on(table)?;
        self.run_counted_rows_statement(SET_THE_COUNTED_ROWS_SAVEPOINT)?;
        let predicted = match self.numbers_the_counter_would_hand_out(bound, table, values) {
            Ok(Some(predicted)) => predicted,
            Ok(None) => {
                self.leave_the_counted_rows_savepoint()?;
                return Ok(None);
            }
            Err(error) => {
                self.leave_the_counted_rows_savepoint()?;
                return Err(error);
            }
        };
        let rollback = || self.roll_back_to_the_counted_rows_savepoint();
        let written = (|| -> Result<std::result::Result<ReservedAutoIncrementRows, LimboError>> {
            let failure = match write(&predicted, None) {
                Ok(()) => None,
                Err(error) => {
                    rollback()?;
                    if found_before_a_number_is_taken(&error)
                        && (bound.row_count().get() == 1
                            || self
                                .first_row_fails_to_fill(sql, bound, table, values, &predicted)?)
                    {
                        return Ok(Err(error));
                    }
                    Some(error)
                }
            };
            let taken = self.reserve_insert_row_ids(bound, table, values, deadline)?;
            let the_transaction_ended = self.inner.get_auto_commit();
            if taken.ids == predicted.ids || the_transaction_ended {
                return Ok(match failure {
                    None => Ok(taken),
                    Some(error) => Err(error),
                });
            }
            #[cfg(test)]
            self.counted_rows_written_again
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if failure.is_none() {
                rollback()?;
            }
            write(&taken, None)?;
            Ok(Ok(taken))
        })();
        if written.is_err() {
            rollback()?;
        }
        if !self.inner.get_auto_commit() {
            self.run_counted_rows_statement(RELEASE_THE_COUNTED_ROWS_SAVEPOINT)?;
        }
        written.map(Some)
    }

    /// Under contention the numbers the counter would hand out next are the
    /// same for every waiting session, so they are read once this session
    /// holds the write lock: every other session's counted insert is then
    /// either finished or not yet begun. Measured with eight sysbench
    /// sessions inserting, reading them before waiting for the lock made
    /// most inserts fail on another session's key and be written twice
    /// while holding the lock.
    ///
    /// The lock is taken before the savepoint is set, because setting it
    /// takes the transaction's snapshot. A session that waits for the lock
    /// holding a snapshot keeps every checkpoint from copying the WAL past
    /// it: measured with eight sessions inserting, waiting sessions always
    /// held one, and the WAL grew past 150 MB in eight seconds without being
    /// emptied once.
    fn hold_the_write_lock_on(&self, table: &AutoIncrementTable) -> Result<()> {
        let schema = self.inner.current_schema();
        let column = schema
            .get_table(&table.name)
            .and_then(|stored| stored.columns().first()?.name.clone())
            .ok_or_else(|| {
                LimboError::InternalError("a counted table has no columns".to_string())
            })?;
        drop(schema);
        self.run_internal(&format!(
            "UPDATE {table} SET {column} = {column} WHERE 0",
            table = quoted_engine_name(&table.name),
            column = quoted_engine_name(&column),
        ))
        .map_err(Into::into)
    }

    fn leave_the_counted_rows_savepoint(&self) -> Result<()> {
        if !self.inner.get_auto_commit() {
            self.run_counted_rows_statement(ROLL_BACK_TO_THE_COUNTED_ROWS_SAVEPOINT)?;
            self.run_counted_rows_statement(RELEASE_THE_COUNTED_ROWS_SAVEPOINT)?;
        }
        Ok(())
    }

    fn roll_back_to_the_counted_rows_savepoint(&self) -> Result<()> {
        if !self.inner.get_auto_commit() {
            self.run_counted_rows_statement(ROLL_BACK_TO_THE_COUNTED_ROWS_SAVEPOINT)?;
        }
        Ok(())
    }

    fn run_counted_rows_statement(
        &self,
        sql: &'static str,
    ) -> std::result::Result<(), MySqlQueryError> {
        if self.inner.is_closed() {
            return self.run_internal(sql);
        }
        let prepared = self
            .prepared_counted_rows_statements
            .lock()
            .expect("MySQL counted rows statements mutex poisoned")
            .remove(sql);
        let mut statement = match prepared {
            Some(statement) => statement,
            None => self.inner.prepare(sql).map_err(MySqlQueryError::Engine)?,
        };
        statement
            .run_ignore_rows()
            .map_err(MySqlQueryError::Engine)?;
        if statement.reset().is_ok() {
            self.prepared_counted_rows_statements
                .lock()
                .expect("MySQL counted rows statements mutex poisoned")
                .insert(sql, statement);
        }
        Ok(())
    }

    /// The ids the counter would hand a `VALUES` insert next, when every row
    /// leaves its id to the counter and the table carries no trigger, or
    /// `None` when the numbers have to be reserved before the rows are
    /// written.
    fn numbers_the_counter_would_hand_out(
        &self,
        bound: &BoundAutoIncrementInsert,
        table: &AutoIncrementTable,
        values: &[Value],
    ) -> Result<Option<ReservedAutoIncrementRows>> {
        if !self.counter_numbers_can_be_predicted(bound, table) {
            return Ok(None);
        }
        let capability = self.auto_increment.as_ref().ok_or_else(|| {
            LimboError::ParseError(
                "AUTO_INCREMENT INSERT requires a registry-backed allocator capability".to_string(),
            )
        })?;
        let mut peek = capability.allocator.peek_high_water(table.key)?;
        // Another statement reserving at this moment holds the counter's
        // file; the numbers are then reserved first, as they always were.
        let high_water = match capability.io.block(|| peek.step()) {
            Ok(high_water) => high_water,
            Err(LimboError::Busy) => return Ok(None),
            Err(error) => return Err(error),
        };
        // Numbers past the column's highest are answered by the reserving
        // path, which is where that refusal is made.
        if !the_batch_fits_the_column(table, high_water, bound.row_count().get() as u64) {
            return Ok(None);
        }
        let row_values = self.auto_increment_row_values(bound, table, values)?;
        self.row_ids_after(bound, table, values, row_values, high_water)
            .map(Some)
    }

    fn counter_numbers_can_be_predicted(
        &self,
        bound: &BoundAutoIncrementInsert,
        table: &AutoIncrementTable,
    ) -> bool {
        !bound.rowwise_conflicts()
            && bound
                .row_values()
                .iter()
                .all(|value| *value == AutoIncrementRowValue::Generated)
            && self
                .inner
                .current_schema()
                .get_triggers_for_table(&table.name)
                .next()
                .is_none()
    }

    /// Whether the first row of a counted insert that failed while a row was
    /// filled is the row that failed, which is what decides whether MySQL
    /// took the statement's numbers.
    fn first_row_fails_to_fill(
        &self,
        sql: &str,
        bound: &BoundAutoIncrementInsert,
        table: &AutoIncrementTable,
        values: &[Value],
        predicted: &ReservedAutoIncrementRows,
    ) -> Result<bool> {
        // With the transaction gone there is nowhere to try the row alone,
        // and taking the numbers is what MySQL does for any row but the
        // first.
        if self.inner.get_auto_commit() {
            return Ok(false);
        }
        let first = predicted.first_generated.ok_or_else(|| {
            LimboError::InternalError("a counted insert asking for ids predicted none".to_string())
        })?;
        let statement = bound
            .inject_one_row(0, first)
            .map_err(|error| LimboError::ParseError(error.to_string()))?;
        let options = injected_auto_increment_prepare_options(table, statement.clone());
        let mut statement = self
            .inner
            .prepare_translated_stmt_with_options(statement, sql, &options)?;
        let parameter_count = statement.parameters_count();
        let bound_values = values.get(..parameter_count).ok_or_else(|| {
            LimboError::InternalError(
                "a counted insert's first row changed its parameter count".to_string(),
            )
        })?;
        bind_prepared_values(&mut statement, bound_values)?;
        let written = run_checked_write_statement(&mut statement, None)
            .map_err(|error| self.map_unsigned_decimal_write_error(error, Some(&table.name)));
        statement.reset()?;
        self.roll_back_to_the_counted_rows_savepoint()?;
        Ok(written.is_err_and(|error| found_before_a_number_is_taken(&error)))
    }

    fn reserve_insert_row_ids(
        &self,
        bound: &BoundAutoIncrementInsert,
        table: &AutoIncrementTable,
        values: &[Value],
        deadline: Option<turso_core::MonotonicInstant>,
    ) -> Result<ReservedAutoIncrementRows> {
        self.check_write_deadline(deadline)
            .map_err(Into::<LimboError>::into)?;
        let capability = self.auto_increment.as_ref().ok_or_else(|| {
            LimboError::ParseError(
                "AUTO_INCREMENT INSERT requires a registry-backed allocator capability".to_string(),
            )
        })?;
        let row_values = self.auto_increment_row_values(bound, table, values)?;
        if row_values.iter().any(|value| {
            matches!(value, InsertAutoIncrementValue::Explicit(id) if *id > auto_increment_ceiling(table))
        }) {
            return Err(LimboError::Constraint(
                "AUTO_INCREMENT value is outside the column's type".to_string(),
            ));
        }
        let has_generated = row_values.contains(&InsertAutoIncrementValue::Generated);
        let highest_explicit = row_values
            .iter()
            .filter_map(|value| match value {
                InsertAutoIncrementValue::Explicit(id) => Some(*id),
                InsertAutoIncrementValue::Generated => None,
            })
            .max()
            .unwrap_or(0);
        if highest_explicit > 0 {
            let high_water = self.when_the_counter_is_free(&capability.allocator, || {
                let mut peek = capability.allocator.peek_high_water(table.key)?;
                capability.io.block(|| peek.step())
            })?;
            if highest_explicit > high_water {
                return Err(LimboError::ParseError(
                    "AUTO_INCREMENT INSERT with a new explicit high-water mark is not supported"
                        .to_string(),
                ));
            }
        }
        let high_water_before = if has_generated {
            let reserved = self.when_the_counter_is_free(&capability.allocator, || {
                let mut reservation = capability
                    .allocator
                    .reserve_insert_values(table.key, row_values.clone())?;
                capability.io.block(|| reservation.step())
            })?;
            let generated_ceiling = generated_ceiling(table);
            let has_explicit_above_generated_ceiling = row_values.iter().any(|value| {
                matches!(value, InsertAutoIncrementValue::Explicit(id) if *id > generated_ceiling)
            });
            if reserved.high_water_after > generated_ceiling
                && !has_explicit_above_generated_ceiling
            {
                return Err(LimboError::Constraint(
                    "AUTO_INCREMENT value is outside the column's type".to_string(),
                ));
            }
            reserved.high_water_before
        } else {
            0
        };
        self.check_write_deadline(deadline)
            .map_err(Into::<LimboError>::into)?;
        self.row_ids_after(bound, table, values, row_values, high_water_before)
    }

    /// Runs one step of a table's `AUTO_INCREMENT` counter, starting it again
    /// while another session's step holds the counter.
    ///
    /// The counter lets one step in at a time and answers any other at once
    /// with `Busy`, and a step takes a moment, so a session waits for it as
    /// long as it waits for any other lock, the way MySQL's inserts wait their
    /// turn at a table's AUTO-INC lock. Answering 1205 at once failed one of
    /// Prisma's two concurrent `tag.create` calls in the framework harness.
    fn when_the_counter_is_free<T>(
        &self,
        allocator: &DurableRangeAllocator,
        step: impl FnMut() -> Result<T>,
    ) -> Result<T> {
        when_the_counter_is_free_within(allocator, self.inner.get_busy_timeout(), step)
    }

    /// The ids a `VALUES` insert's rows take when the counter stands at
    /// `high_water_before`, and the bound values with each id asked for by a
    /// `?` put in its place.
    fn row_ids_after(
        &self,
        bound: &BoundAutoIncrementInsert,
        table: &AutoIncrementTable,
        values: &[Value],
        row_values: Vec<InsertAutoIncrementValue>,
        high_water_before: u64,
    ) -> Result<ReservedAutoIncrementRows> {
        let mut current = high_water_before;
        let mut ids = Vec::with_capacity(row_values.len());
        let mut first_generated = None;
        let mut last_explicit = None;
        let mut bound_values = values.to_vec();
        for (source, value) in bound.row_values().iter().zip(row_values) {
            match value {
                InsertAutoIncrementValue::Generated => {
                    current = current.checked_add(1).ok_or(LimboError::IntegerOverflow)?;
                    if table.definition.allocator_column_type
                        == turso_mysql_parser::MySqlIntegerType::BigIntUnsigned
                        && current > u64::MAX - 2
                    {
                        return Err(LimboError::Constraint(
                            "AUTO_INCREMENT value is outside the column's type".to_string(),
                        ));
                    }
                    first_generated.get_or_insert(current);
                    ids.push(Some(current));
                    if let AutoIncrementRowValue::Parameter(ordinal) = source {
                        bound_values[*ordinal] = if table.definition.allocator_column_type
                            == turso_mysql_parser::MySqlIntegerType::BigIntUnsigned
                            && current > i64::MAX as u64
                        {
                            Value::from_text(current.to_string())
                        } else {
                            Value::from_i64(i64::try_from(current).map_err(|_| {
                                LimboError::Constraint(
                                    "AUTO_INCREMENT value is outside engine integer range"
                                        .to_string(),
                                )
                            })?)
                        };
                    }
                }
                InsertAutoIncrementValue::Explicit(id) => {
                    current = current.max(id);
                    ids.push(None);
                    last_explicit = Some(id);
                }
            }
        }
        Ok(ReservedAutoIncrementRows {
            ids,
            bound_values,
            first_generated,
            last_explicit,
        })
    }

    fn auto_increment_row_values(
        &self,
        bound: &BoundAutoIncrementInsert,
        table: &AutoIncrementTable,
        values: &[Value],
    ) -> Result<Vec<InsertAutoIncrementValue>> {
        bound
            .row_values()
            .iter()
            .map(|value| match value {
                AutoIncrementRowValue::Generated => Ok(InsertAutoIncrementValue::Generated),
                AutoIncrementRowValue::Explicit(id) => {
                    hold_the_id_to_the_counted_column(table, *id)?;
                    Ok(InsertAutoIncrementValue::Explicit((*id).max(0) as u64))
                }
                AutoIncrementRowValue::Parameter(ordinal) => match values.get(*ordinal) {
                    Some(Value::Null) => Ok(InsertAutoIncrementValue::Generated),
                    Some(value)
                        if value.as_int() == Some(0)
                            && self.written_zero() == WrittenZero::AsksForTheNextNumber =>
                    {
                        Ok(InsertAutoIncrementValue::Generated)
                    }
                    Some(value) if value.as_int().is_some() => {
                        let id = value.as_int().unwrap();
                        hold_the_id_to_the_counted_column(table, i128::from(id))?;
                        Ok(InsertAutoIncrementValue::Explicit(id.max(0) as u64))
                    }
                    Some(Value::Text(value))
                        if table.definition.allocator_column_type
                            == turso_mysql_parser::MySqlIntegerType::BigIntUnsigned =>
                    {
                        let id = value.as_str().parse::<i128>().map_err(|_| {
                            LimboError::InvalidArgument(
                                "AUTO_INCREMENT parameter must be an unsigned integer".to_string(),
                            )
                        })?;
                        hold_the_id_to_the_counted_column(table, id)?;
                        let id = id as u64;
                        Ok(
                            if id == 0 && self.written_zero() == WrittenZero::AsksForTheNextNumber {
                                InsertAutoIncrementValue::Generated
                            } else {
                                InsertAutoIncrementValue::Explicit(id)
                            },
                        )
                    }
                    _ => Err(LimboError::InvalidArgument(
                        "AUTO_INCREMENT parameter must be an integer or NULL".to_string(),
                    )),
                },
            })
            .collect::<Result<Vec<_>>>()
    }

    fn written_zero(&self) -> WrittenZero {
        *self.written_zero.lock().unwrap()
    }

    fn load_auto_increment_table(&self, target: &str) -> Result<Option<AutoIncrementTable>> {
        self.inner.maybe_update_schema();
        let schema = self.inner.current_schema();
        if let Some(table) = self
            .schema_readings
            .lock()
            .expect("MySQL schema readings mutex poisoned")
            .counted_table(&schema, target)
        {
            return Ok(table);
        }
        let table = self.read_auto_increment_table_from_the_catalog(target)?;
        if Arc::ptr_eq(&schema, &self.inner.current_schema()) {
            self.schema_readings
                .lock()
                .expect("MySQL schema readings mutex poisoned")
                .keep_counted_table(schema, target, &table);
        }
        Ok(table)
    }

    fn read_auto_increment_table_from_the_catalog(
        &self,
        target: &str,
    ) -> Result<Option<AutoIncrementTable>> {
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
            let identity = self
                .inner
                .schema_catalog_validation_context()
                .map(|context| *context.database_identity());
            let mut table = counted_table_from_stored_sql(&sql, identity)?;
            if let Some(table) = &mut table {
                if !table.name.eq_ignore_ascii_case(&name)
                    || !table.name.eq_ignore_ascii_case(target)
                {
                    return Err(LimboError::Corrupt(
                        "AUTO_INCREMENT table definition does not match its catalog name"
                            .to_string(),
                    ));
                }
                table.name = name;
            }
            return Ok(table);
        }
        Ok(None)
    }

    /// Follows the triggers an `INSERT` into `target` sets off and answers
    /// whether any of them writes a table that counts its own ids.
    ///
    /// A `BEFORE` trigger only changes the row it runs for. An `AFTER` trigger
    /// writes the tables its `INSERT`, `UPDATE` and `DELETE` name, which sets
    /// off those tables' own triggers for that kind of write in turn; a
    /// table's triggers for other kinds of write do not run. Measured on MySQL
    /// 8.4.11, a trigger writing a table its statement writes or reads — the
    /// target, or the table an `INSERT ... SELECT` copies from — is answered
    /// 1442, so that is refused here. A counted table a trigger inserts into
    /// is numbered by its own counter when the row is written, which needs
    /// the counted column left for the counter to fill and the row number to
    /// be the id.
    fn check_the_triggers_an_insert_sets_off(
        &self,
        target: &str,
        read_tables: &[String],
    ) -> Result<bool> {
        use turso_parser::ast::{TriggerCmd, TriggerEvent, TriggerTime};

        let schema = self.inner.current_schema();
        let mut written = vec![(target.to_owned(), TriggerEvent::Insert)];
        let mut writes_a_counted_table = false;
        let mut next = 0;
        while let Some((table, event)) = written.get(next).cloned() {
            next += 1;
            for trigger in schema.get_triggers_for_table(&table) {
                let fires = matches!(
                    (&trigger.event, &event),
                    (TriggerEvent::Insert, TriggerEvent::Insert)
                        | (TriggerEvent::Delete, TriggerEvent::Delete)
                        | (
                            TriggerEvent::Update | TriggerEvent::UpdateOf(_),
                            TriggerEvent::Update
                        )
                );
                if !fires {
                    continue;
                }
                if !trigger.for_each_row || trigger.when_clause.is_some() {
                    return Err(LimboError::ParseError(
                        "a trigger other than FOR EACH ROW with no WHEN is unsupported".to_string(),
                    ));
                }
                if trigger.time != TriggerTime::After {
                    if trigger.time == TriggerTime::Before
                        && trigger
                            .commands
                            .iter()
                            .all(|command| matches!(command, TriggerCmd::SetNew { .. }))
                    {
                        continue;
                    }
                    return Err(LimboError::ParseError(
                        "a BEFORE trigger doing anything but SET NEW is unsupported".to_string(),
                    ));
                }
                for command in &trigger.commands {
                    let (into, writes) = match command {
                        TriggerCmd::Insert { tbl_name, .. } => (tbl_name, TriggerEvent::Insert),
                        TriggerCmd::Update { tbl_name, .. } => (tbl_name, TriggerEvent::Update),
                        TriggerCmd::Delete { tbl_name, .. } => (tbl_name, TriggerEvent::Delete),
                        _ => {
                            return Err(LimboError::ParseError(
                                "a trigger doing anything but INSERT, UPDATE or DELETE is unsupported"
                                    .to_string(),
                            ));
                        }
                    };
                    let into = into.as_str();
                    if written
                        .iter()
                        .map(|(table, _)| table)
                        .chain(read_tables)
                        .any(|table| table.eq_ignore_ascii_case(into))
                    {
                        return Err(LimboError::ParseError(format!(
                            "a trigger writing {into}, which the statement setting it off already uses, is refused"
                        )));
                    }
                    if let TriggerCmd::Insert { col_names, .. } = command {
                        if let Some(counted) = self.load_auto_increment_table(into)? {
                            if counted.definition.allocator_column_type
                                == turso_mysql_parser::MySqlIntegerType::BigIntUnsigned
                                || col_names.iter().any(|column| {
                                    column.as_str().eq_ignore_ascii_case(
                                        &counted.definition.allocator_column_name,
                                    )
                                })
                            {
                                return Err(LimboError::ParseError(format!(
                                    "a trigger writing {into} is unsupported unless the counter numbers its rows"
                                )));
                            }
                            writes_a_counted_table = true;
                        }
                    }
                    written.push((into.to_owned(), writes));
                }
            }
        }
        Ok(writes_a_counted_table)
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
            AlterTableBody::AddConstraint(_) => "ALTER TABLE ADD CONSTRAINT",
            AlterTableBody::DropConstraint(_) => "ALTER TABLE DROP CONSTRAINT",
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

    /// Refuses a `MODIFY` or a `CHANGE` of the column a primary key is on.
    ///
    /// MySQL keeps the key through one and the engine's `ALTER COLUMN` replaces
    /// the column with what it was given, taking the key with it — which is the
    /// engine's own meaning, and one a `SELECT` would then read differently
    /// from MySQL. So the statement is refused rather than answered.
    fn reject_alter_over_a_primary_key_column(
        &self,
        target: &turso_parser::ast::Name,
        body: &AlterTableBody,
    ) -> Result<()> {
        let AlterTableBody::AlterColumn { old, .. } = body else {
            return Ok(());
        };
        let schema = self.inner.current_schema();
        let Some(table) = schema.get_table(target.as_str()) else {
            return Ok(());
        };
        let Some(btree) = table.btree() else {
            return Ok(());
        };
        if btree
            .primary_key_columns
            .iter()
            .any(|(name, _)| name.eq_ignore_ascii_case(old.as_str()))
        {
            return Err(LimboError::ParseError(
                "a column carrying the PRIMARY KEY cannot be modified".to_string(),
            ));
        }
        Ok(())
    }

    /// Refuses a `MODIFY` or `CHANGE` that gives a `DECIMAL` column another
    /// size or sign, or turns a column into or out of a `DECIMAL`.
    ///
    /// MySQL writes every stored value again in the column's new form —
    /// measured on 8.4.11, a 1.5 in a `DECIMAL(8,2)` reads `1.500` after
    /// `MODIFY d DECIMAL(12,3)` — while the engine keeps a `DECIMAL` as the
    /// text it was written as, so the rows would keep their old form. The
    /// same `DECIMAL` restated with another default or nullability is taken.
    fn reject_alter_changing_a_decimal(
        &self,
        target: &turso_parser::ast::Name,
        body: &AlterTableBody,
    ) -> Result<()> {
        let AlterTableBody::AlterColumn { old, new } = body else {
            return Ok(());
        };
        let Ok(table) = MySqlTableName::parse(target.as_str()) else {
            return Ok(());
        };
        let Ok(columns) = self.list_shared_columns(&table) else {
            return Ok(());
        };
        let Some(before) = columns
            .iter()
            .find(|column| column.name().eq_ignore_ascii_case(old.as_str()))
        else {
            return Ok(());
        };
        let before = before
            .decimal_size()
            .map(|size| (before.type_name() == "DECIMAL UNSIGNED", size));
        let after = match new.col_type.as_ref() {
            Some(data_type) if data_type.name.eq_ignore_ascii_case("mysql_decimal") => Some((
                false,
                turso_mysql_parser::stored_decimal_size(data_type)
                    .map_err(|error| LimboError::ParseError(error.to_string()))?,
            )),
            Some(data_type)
                if data_type
                    .name
                    .eq_ignore_ascii_case("mysql_decimal_unsigned") =>
            {
                Some((
                    true,
                    turso_mysql_parser::stored_decimal_size(data_type)
                        .map_err(|error| LimboError::ParseError(error.to_string()))?,
                ))
            }
            _ => None,
        };
        if before == after {
            return Ok(());
        }
        Err(LimboError::ParseError(
            "changing a DECIMAL column's size, sign or type in place needs its rows written again"
                .to_string(),
        ))
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

    /// Refuses a second trigger for one table, event and timing: MySQL runs
    /// such triggers in the order they were made, which is not kept here.
    fn reject_a_second_trigger_for_one_event(&self, stmt: &Stmt) -> Result<()> {
        let Stmt::CreateTrigger {
            tbl_name,
            time,
            event,
            ..
        } = stmt
        else {
            unreachable!("checked CREATE TRIGGER statement");
        };
        let schema = self.inner.current_schema();
        let taken = schema.triggers.values().flatten().any(|trigger| {
            trigger
                .table_name
                .eq_ignore_ascii_case(tbl_name.name.as_str())
                && Some(trigger.time) == *time
                && trigger.event == *event
        });
        if taken {
            return Err(LimboError::ParseError(
                "a trigger already exists for this table, event and timing; MySQL trigger ordering is not supported"
                    .to_string(),
            ));
        }
        Ok(())
    }
}

/// The bound values with each id bound for a counted column as a double or
/// a word put into the whole number MySQL stores, or `None` where every id
/// was bound as one already.
///
/// Measured on MySQL 8.4.11 through mysql2, which binds a number as a
/// double: `100.5` bound for an `AUTO_INCREMENT` key stores 100, and the word
/// `'200.5'` stores 200 — each read as a double and rounded half to even —
/// and the counter goes on past each.
fn ids_bound_as_whole_numbers(
    bound: &BoundAutoIncrementInsert,
    values: &[Value],
) -> Option<Vec<Value>> {
    let mut rounded: Option<Vec<Value>> = None;
    for value in bound.row_values() {
        let AutoIncrementRowValue::Parameter(ordinal) = value else {
            continue;
        };
        let number = match values.get(*ordinal) {
            Some(Value::Numeric(Numeric::Float(number))) => f64::from(*number),
            Some(Value::Text(word)) => {
                let word = word.as_str().trim_matches(' ');
                if let Ok(whole) = word.parse::<i64>() {
                    rounded.get_or_insert_with(|| values.to_vec())[*ordinal] =
                        Value::from_i64(whole);
                    continue;
                }
                if word.parse::<u64>().is_ok() {
                    continue;
                }
                match word.parse::<f64>() {
                    Ok(number) if number.is_finite() => number,
                    _ => continue,
                }
            }
            _ => continue,
        };
        let whole = number.round_ties_even();
        if whole.abs() >= 9.0e18 {
            continue;
        }
        rounded.get_or_insert_with(|| values.to_vec())[*ordinal] = Value::from_i64(whole as i64);
    }
    rounded
}

/// A value of a key as MySQL reads it: a `DECIMAL` and a `BIGINT UNSIGNED`
/// the engine keeps as bytes of its own are written out as their digits.
fn key_value_as_mysql_reads_it(value: Value, declared_type: Option<&str>) -> Value {
    let Value::Blob(bytes) = &value else {
        return value;
    };
    match declared_type {
        Some("mysql_decimal" | "mysql_decimal_unsigned") => turso_core::numeric_blob_text(bytes)
            .map(Value::from_text)
            .unwrap_or(value),
        Some("mysql_uint64") => turso_core::mysql_uint64_from_blob(bytes)
            .map(|number| Value::from_text(number.to_string()))
            .unwrap_or(value),
        _ => value,
    }
}

fn reject_incompatible_legacy_tables(connection: &Arc<Connection>) -> Result<()> {
    let tables = connection
        .prepare("SELECT name FROM sqlite_schema WHERE type = 'table'")?
        .run_collect_rows()?;
    let schema = connection.current_schema();
    for row in tables {
        let [name] = row.as_slice() else {
            return Err(LimboError::Corrupt(
                "sqlite_schema table name has an invalid shape".to_string(),
            ));
        };
        let name = name.to_string();
        let name = name.trim_matches('\'');
        let Some(table) = schema.get_btree_table(name) else {
            continue;
        };
        if table
            .columns()
            .iter()
            .any(|column| legacy_decimal_type(&column.ty_str))
        {
            return Err(LimboError::InvalidArgument(format!(
                "table '{name}' has legacy DECIMAL values stored through binary64; re-import this table from the original decimal data"
            )));
        }
        if table.columns().iter().any(|column| {
            column.ty_str.eq_ignore_ascii_case("BIGINT UNSIGNED")
                || column.ty_str.eq_ignore_ascii_case("UNSIGNED BIGINT")
        }) {
            return Err(LimboError::InvalidArgument(format!(
                "table '{name}' has legacy BIGINT UNSIGNED values stored as signed integers; re-import this table from the original unsigned data"
            )));
        }
        for foreign_key in &table.foreign_keys {
            let primary_covers =
                primary_key_covers_columns(&table.primary_key_columns, &foreign_key.child_columns);
            let index_covers = schema.get_indices(name).any(|index| {
                index_covers_columns(
                    index,
                    &table.primary_key_columns,
                    &foreign_key.child_columns,
                )
            });
            if !primary_covers && !index_covers {
                return Err(LimboError::InvalidArgument(format!(
                    "table '{name}' has a legacy foreign key without a child index; rebuild or re-import this table with the current MySQL frontend"
                )));
            }
        }
    }
    Ok(())
}

fn legacy_decimal_type(type_name: &str) -> bool {
    type_name.eq_ignore_ascii_case("DECIMAL") || type_name.eq_ignore_ascii_case("UNSIGNED DECIMAL")
}

/// The same stored DDL with every `ON UPDATE CURRENT_TIMESTAMP` taken out.
///
/// The engine's parser has no such attribute, so what is handed to it carries
/// none. The words are this renderer's own, spelled exactly one way, and a
/// stored definition is always what this rendered, so taking them out where
/// they are not inside a string is taking out exactly what was put in.
pub(crate) fn without_on_update_attributes(sql: &str, mode: SessionSqlMode) -> String {
    let mut remaining = sql.to_owned();
    while let Some(start) = find_unquoted_sql_fragment(
        &remaining,
        turso_mysql_parser::ON_UPDATE_MOMENT,
        mode.no_backslash_escapes,
    ) {
        let mut end = start + turso_mysql_parser::ON_UPDATE_MOMENT.len();
        // A column holding fractional seconds carries its digits after the
        // words, `(3)`, and they go with them.
        let digits = remaining.as_bytes()[end..]
            .strip_prefix(b"(")
            .and_then(|rest| {
                let count = rest.iter().take_while(|byte| byte.is_ascii_digit()).count();
                (count > 0 && rest.get(count) == Some(&b')')).then_some(count + 2)
            });
        end += digits.unwrap_or(0);
        remaining.replace_range(start..end, "");
    }
    remaining
}

/// The same stored DDL with every column `COMMENT '<text>'` taken out.
///
/// The engine's parser has no attribute for a comment, so what is handed to it
/// carries none. The words are this renderer's own — `COLUMN_COMMENT_WORDS`
/// followed by the text and its closing quote — and a stored definition is
/// always what this rendered, so what is taken out is exactly what was put in.
pub(crate) fn without_column_comments(sql: &str, mode: SessionSqlMode) -> String {
    let mut remaining = sql.to_owned();
    while let Some(start) = find_unquoted_sql_fragment(
        &remaining,
        turso_mysql_parser::COLUMN_COMMENT_WORDS,
        mode.no_backslash_escapes,
    ) {
        let opening = start + turso_mysql_parser::COLUMN_COMMENT_WORDS.len();
        let Some(end) = end_of_written_text(&remaining, opening, mode.no_backslash_escapes) else {
            return remaining;
        };
        remaining.replace_range(start..end, "");
    }
    remaining
}

/// Where the text a quote opened ends, counting past the closing quote.
///
/// `opening` stands just after the opening quote. A quote inside is written
/// twice and a backslash escapes what follows it, which is how this renderer
/// writes one and how MySQL reads one back.
fn end_of_written_text(sql: &str, opening: usize, no_backslash_escapes: bool) -> Option<usize> {
    let bytes = sql.as_bytes();
    let mut index = opening;
    while index < bytes.len() {
        if bytes[index] == b'\\' && !no_backslash_escapes {
            index += 2;
        } else if bytes[index] == b'\'' {
            if bytes.get(index + 1) == Some(&b'\'') {
                index += 2;
            } else {
                return Some(index + 1);
            }
        } else {
            index += 1;
        }
    }
    None
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
            last_insert_id_result: statement.result_is_function(index, "last_insert_id", 0),
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
    let reprepares = statement.stmt_status(StatementStatusCounter::Reprepare);
    // A reprepare returns a `?` column to its generic type; an ordinary
    // execution leaves it alone.
    if reprepares == prepared.reprepares_at_last_refresh {
        return Ok(());
    }
    let metadata = prepared_statement_metadata(statement_id, statement)?;
    prepared.reprepares_at_last_refresh = reprepares;
    prepared.metadata = metadata;
    prepared.result_column_type_metadata =
        prepared_result_column_type_metadata(statement, &prepared.static_result_projections);
    #[cfg(test)]
    {
        prepared.metadata_rebuilds += 1;
    }
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
    let parameter_count = match execution_plan {
        PreparedExecutionPlan::AutoIncrementInsert(insert) => insert.parameter_count,
        PreparedExecutionPlan::CountedInsertSelect(copy) => copy.parameter_count,
        _ => {
            return Err(MySqlPreparedStatementError::Prepare(
                MySqlQueryError::Engine(LimboError::InternalError(
                    "prepared statement metadata source is missing a core statement".to_string(),
                )),
            ));
        }
    };
    let parameter_count = u16::try_from(parameter_count).map_err(|_| {
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

fn lock_the_rows_a_select_reads(
    statement: &mut Statement,
    locking_read: MySqlLockingRead,
    sources: &[MySqlSelectSource],
) -> Result<()> {
    let tables = sources
        .iter()
        .filter(|source| {
            !source.subquery() && source.derived().is_none() && source.catalog().is_none()
        })
        .map(|source| source.table().as_str().to_owned())
        .collect();
    lock_the_rows_of_tables(statement, locking_read, tables)
}

fn lock_the_rows_a_serializable_select_reads(
    statement: &mut Statement,
    locking_read: MySqlLockingRead,
    sources: &[MySqlSelectSource],
) -> Result<()> {
    let tables = sources
        .iter()
        .filter(|source| source.derived().is_none() && source.catalog().is_none())
        .map(|source| source.table().as_str().to_owned())
        .collect();
    lock_the_rows_of_tables(statement, locking_read, tables)
}

fn lock_the_rows_of_tables(
    statement: &mut Statement,
    locking_read: MySqlLockingRead,
    tables: Vec<String>,
) -> Result<()> {
    let wait = locking_read.wait;
    statement.lock_rows_it_reads(turso_core::LockingRead {
        mode: if locking_read.shared {
            turso_core::RowLockMode::Shared
        } else {
            turso_core::RowLockMode::Exclusive
        },
        policy: match wait {
            MySqlRowLockWait::Wait => turso_core::RowLockWaitPolicy::Wait,
            MySqlRowLockWait::NoWait => turso_core::RowLockWaitPolicy::NoWait,
            MySqlRowLockWait::SkipLocked => turso_core::RowLockWaitPolicy::SkipLocked,
        },
        tables,
    });
    if wait == MySqlRowLockWait::Wait {
        statement.run_to_lock_rows()?;
    }
    Ok(())
}

fn mysql_prepared_value_to_core(value: &MySqlPreparedValue) -> Result<Value> {
    match value {
        MySqlPreparedValue::Null => Ok(Value::Null),
        MySqlPreparedValue::Integer(value) => Ok(Value::from_i64(*value)),
        MySqlPreparedValue::UnsignedInteger(value) => Ok(Value::from_text(value.to_string())),
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
    let mut temporal_precision = None;
    // A VARBINARY carries a declared size the same way, and the same reader
    // recovers it; what differs is that the count is bytes rather than
    // characters, which is decided where the length is used rather than here.
    let sized_text = ["VARCHAR", "CHAR", "VARBINARY", "BINARY"]
        .into_iter()
        .find(|name| data_type.name.eq_ignore_ascii_case(name));
    let type_name = if let Some(sized_text) = sized_text {
        character_length = Some(
            turso_mysql_parser::stored_character_length(data_type)
                .map_err(|_| MySqlColumnMetadataError::UnsupportedDefinition)?,
        );
        sized_text
    } else if data_type.name.eq_ignore_ascii_case("mysql_decimal") {
        decimal_size = Some(
            turso_mysql_parser::stored_decimal_size(data_type)
                .map_err(|_| MySqlColumnMetadataError::UnsupportedDefinition)?,
        );
        "DECIMAL"
    } else if data_type
        .name
        .eq_ignore_ascii_case("mysql_decimal_unsigned")
    {
        // The engine's declared type takes the sign before the arguments and
        // MySQL writes it after them; the MySQL word order goes back on here,
        // so nothing above this reads the inversion.
        decimal_size = Some(
            turso_mysql_parser::stored_decimal_size(data_type)
                .map_err(|_| MySqlColumnMetadataError::UnsupportedDefinition)?,
        );
        "DECIMAL UNSIGNED"
    } else if data_type.name.eq_ignore_ascii_case("mysql_uint64") {
        if data_type.size.is_some() {
            return Err(MySqlColumnMetadataError::UnsupportedDefinition);
        }
        "BIGINT UNSIGNED"
    } else if let Some(name) = ["DATETIME", "TIME", "TIMESTAMP"]
        .into_iter()
        .find(|name| data_type.name.eq_ignore_ascii_case(name))
    {
        temporal_precision = Some(
            turso_mysql_parser::stored_temporal_precision(data_type)
                .map_err(|_| MySqlColumnMetadataError::UnsupportedDefinition)?,
        );
        name
    } else if turso_mysql_parser::enum_members(&data_type.name).is_some()
        || turso_mysql_parser::set_members(&data_type.name).is_some()
    {
        // An ENUM or a SET rides on the declared type whole, quotes and all,
        // so the metadata carries the same text and every reader of it — SHOW
        // CREATE TABLE, SHOW COLUMNS, the wire column — reads the members out
        // of it.
        let (nullable, default) = enum_column_shape(column)?;
        let (default_sql, default_value) = match default {
            Some((sql, value)) => (Some(sql), Some(value)),
            None => (None, None),
        };
        return Ok(MySqlColumnMetadata {
            character_length: None,
            decimal_size: None,
            temporal_precision: None,
            collation_name: Some(stored_text_collation_name(column)),
            name: column.col_name.as_str().to_owned(),
            type_name: data_type.name.clone(),
            nullable,
            key: MySqlColumnKey::None,
            default_sql,
            default_value,
            extra: String::new(),
            comment: String::new(),
        });
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
            "BIGINT UNSIGNED" => "BIGINT UNSIGNED",
            "TEXT" => "TEXT",
            "TINYTEXT" => "TINYTEXT",
            "MEDIUMTEXT" => "MEDIUMTEXT",
            "LONGTEXT" => "LONGTEXT",
            "BLOB" => "BLOB",
            "TINYBLOB" => "TINYBLOB",
            "MEDIUMBLOB" => "MEDIUMBLOB",
            "LONGBLOB" => "LONGBLOB",
            "DOUBLE" => "DOUBLE",
            "FLOAT" => "FLOAT",
            // The sign travels with the type through the stored DDL, as it
            // does on an integer.
            "DOUBLE UNSIGNED" => "DOUBLE UNSIGNED",
            "FLOAT UNSIGNED" => "FLOAT UNSIGNED",
            "BOOLEAN" => "BOOLEAN",
            "DATETIME" => "DATETIME",
            "TIMESTAMP" => "TIMESTAMP",
            "DATE" => "DATE",
            "TIME" => "TIME",
            "YEAR" => "YEAR",
            "BIT" => "BIT",
            "JSON" => "JSON",
            _ => return Err(MySqlColumnMetadataError::UnsupportedDefinition),
        }
    };

    let mut nullable = true;
    let mut key = MySqlColumnKey::None;
    let mut default_sql = None;
    let mut default_value = None;
    let mut generated_default = false;
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
                // Measured on MySQL 8.4.11: a column defaulting to the moment
                // it is written reports `DEFAULT_GENERATED` where every other
                // default reports nothing.
                if matches!(
                    value,
                    MySqlColumnDefault::Moment | MySqlColumnDefault::MomentCall
                ) {
                    generated_default = true;
                }
                default_value = Some(value);
            }
            ColumnConstraint::Check { .. } => {}
            _ if names_the_collation_of_words(constraint) => {}
            _ => return Err(MySqlColumnMetadataError::UnsupportedDefinition),
        }
    }

    Ok(MySqlColumnMetadata {
        character_length,
        decimal_size,
        temporal_precision,
        collation_name: is_text_type(type_name).then(|| stored_text_collation_name(column)),
        name: column.col_name.as_str().to_owned(),
        type_name: type_name.to_owned(),
        nullable,
        key,
        default_sql,
        default_value,
        extra: if generated_default {
            "DEFAULT_GENERATED".to_owned()
        } else {
            String::new()
        },
        comment: String::new(),
    })
}

fn stored_text_collation_name(column: &turso_parser::ast::ColumnDefinition) -> &'static str {
    let named = column
        .constraints
        .iter()
        .find_map(|constraint| match &constraint.constraint {
            ColumnConstraint::Collate { collation_name } => Some(collation_name.as_str()),
            _ => None,
        });
    match named {
        Some(name) if name.eq_ignore_ascii_case("MYSQL_UTF8MB4_BIN") => "utf8mb4_bin",
        Some(name) if name.eq_ignore_ascii_case("MYSQL_UCA400_CI") => "utf8mb4_unicode_ci",
        Some(name) if name.eq_ignore_ascii_case("MYSQL_UTF8MB3_UCA400_CI") => "utf8mb3_unicode_ci",
        Some(name) if name.eq_ignore_ascii_case("MYSQL_UTF8MB4_GENERAL_CI") => "utf8mb4_general_ci",
        _ => "utf8mb4_0900_ai_ci",
    }
}

/// Reads what an `ENUM` column was declared NOT NULL and defaulting to.
///
/// An ENUM takes no key and no default yet, so anything but the one
/// nullability constraint is refused rather than dropped.
fn enum_column_shape(
    column: &turso_parser::ast::ColumnDefinition,
) -> std::result::Result<(bool, Option<(String, MySqlColumnDefault)>), MySqlColumnMetadataError> {
    let mut nullable = true;
    let mut default = None;
    for constraint in &column.constraints {
        match &constraint.constraint {
            ColumnConstraint::NotNull {
                nullable: false,
                conflict_clause: None,
            } if constraint.name.is_none() => nullable = false,
            ColumnConstraint::NotNull {
                nullable: true,
                conflict_clause: None,
            } if constraint.name.is_none() => {}
            // An `ENUM` takes a default like any other column — a status column
            // written with one is what every schema carries — and it is one of
            // the members, so it is read as the word it is.
            ColumnConstraint::Default(expression) if constraint.name.is_none() => {
                if default.replace(mysql_column_default(expression)?).is_some() {
                    return Err(MySqlColumnMetadataError::UnsupportedDefinition);
                }
            }
            _ if names_the_collation_of_words(constraint) => {}
            _ => return Err(MySqlColumnMetadataError::UnsupportedDefinition),
        }
    }
    Ok((nullable, default))
}

/// Recognize the text collations the checked MySQL DDL can persist.
fn names_the_collation_of_words(constraint: &turso_parser::ast::NamedColumnConstraint) -> bool {
    matches!(
        &constraint.constraint,
        ColumnConstraint::Collate { collation_name }
            if constraint.name.is_none()
                && ["MYSQL_UCA9_AI_CI", "MYSQL_UTF8MB4_BIN", "MYSQL_UCA400_CI", "MYSQL_UTF8MB3_UCA400_CI", "MYSQL_UTF8MB4_GENERAL_CI", "NOCASE"].iter().any(|name|
                    collation_name.as_str().eq_ignore_ascii_case(name))
    )
}

/// Writes one name the way the engine reads it back.
fn quoted_engine_name(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

/// Adds the names a derived table or a CTE gives its table's columns, each
/// carrying the column's own type, so a reading that knows the types knows
/// what the statement around the body names as well as what the body names.
///
/// A name that is also another of the table's columns would stand for two
/// types at once, so it is refused.
fn columns_under_derived_names(
    columns: &[MySqlColumnMetadata],
    sources: &[MySqlSelectSource],
) -> std::result::Result<Vec<MySqlColumnMetadata>, MySqlQueryError> {
    let mut named = columns.to_vec();
    for source in sources {
        // A derived table joining tables names columns of several tables,
        // which are read out of each of them instead.
        let Some(derived) = source
            .derived()
            .filter(|derived| derived.joined().is_empty())
        else {
            continue;
        };
        for (ordinal, name) in derived.names().iter().enumerate() {
            if derived.answer(ordinal).is_some() {
                continue;
            }
            let base = &source.projected_columns()[ordinal];
            if name.eq_ignore_ascii_case(base) {
                continue;
            }
            if columns
                .iter()
                .any(|column| column.name().eq_ignore_ascii_case(name))
            {
                return Err(MySqlQueryError::Unsupported(
                    "a derived table naming a column after another of its table's columns"
                        .to_string(),
                ));
            }
            let Some(column) = columns
                .iter()
                .find(|column| column.name().eq_ignore_ascii_case(base))
            else {
                return Err(MySqlQueryError::Unsupported(
                    "a derived table projecting a column its table does not have".to_string(),
                ));
            };
            let mut renamed = column.clone();
            renamed.name.clone_from(name);
            named.push(renamed);
        }
    }
    Ok(named)
}

/// The collation of each column of words declared with one other than
/// `utf8mb4_0900_ai_ci`, by name. A name two of the columns hold under
/// different collations is left out, which leaves an ordering over it to the
/// check that refuses one over another collation.
fn collations_other_than_the_default(columns: &[MySqlColumnMetadata]) -> Vec<(String, String)> {
    let collation_of = |column: &MySqlColumnMetadata| {
        is_text_type(column.type_name())
            .then(|| column.collation_name())
            .flatten()
            .unwrap_or("utf8mb4_0900_ai_ci")
    };
    columns
        .iter()
        .filter(|column| collation_of(column) != "utf8mb4_0900_ai_ci")
        .filter(|column| {
            columns.iter().all(|other| {
                !other.name().eq_ignore_ascii_case(column.name())
                    || collation_of(other) == collation_of(column)
            })
        })
        .map(|column| (column.name().to_owned(), collation_of(column).to_owned()))
        .collect()
}

/// Reports whether two columns land in the same lists a `SELECT` is read
/// knowing, and so are rendered alike wherever the statement names them.
fn read_alike(first: &MySqlColumnMetadata, second: &MySqlColumnMetadata) -> bool {
    let kind = |column: &MySqlColumnMetadata| {
        let type_name = column.type_name();
        (
            is_text_type(type_name),
            matches!(type_name, "DATETIME" | "TIMESTAMP"),
            column
                .decimal_size()
                .map(|(_, scale)| scale)
                .or_else(|| (type_name == "BIGINT UNSIGNED").then_some(0)),
            is_integer_type(type_name),
            matches!(
                type_name,
                "FLOAT" | "FLOAT UNSIGNED" | "DOUBLE" | "DOUBLE UNSIGNED"
            ),
            type_name == "JSON",
            turso_mysql_parser::enum_members(type_name),
            turso_mysql_parser::set_members(type_name),
        )
    };
    kind(first) == kind(second)
}

/// One table a grouped statement reads, with the keys that decide its row.
struct TableTheKeysMayDecide<'a> {
    reference: &'a str,
    columns: Vec<MySqlColumnMetadata>,
    /// The primary key's columns and each `NOT NULL` unique key's.
    keys: Vec<Vec<String>>,
}

/// Finds the table a named column belongs to, by the name or alias it was
/// written with, or by its being the one table holding a column of that name.
fn resolve_named_column(
    tables: &[TableTheKeysMayDecide<'_>],
    named: &turso_mysql_parser::MySqlNamedColumn,
) -> Option<(usize, String)> {
    let holds = |table: &TableTheKeysMayDecide<'_>| {
        table
            .columns
            .iter()
            .any(|column| column.name().eq_ignore_ascii_case(named.column()))
    };
    let position = match named.table() {
        Some(written) => {
            let position = tables
                .iter()
                .position(|table| written.eq_ignore_ascii_case(table.reference))?;
            holds(&tables[position]).then_some(position)?
        }
        None => {
            let mut holding = tables.iter().enumerate().filter(|(_, table)| holds(table));
            let (position, _) = holding.next()?;
            holding.next().is_none().then_some(position)?
        }
    };
    Some((position, named.column().to_ascii_lowercase()))
}

/// Carries decided columns across one join's `ON`.
///
/// An inner join's match holds on every row it answers, so either side
/// decides the other. A `LEFT JOIN` also answers a row of the tables before
/// it with nothing matched, so it carries a column only from them to the
/// table it adds, and only once every column of theirs its `ON` names is
/// decided.
fn carry_across_the_join(
    tables: &[TableTheKeysMayDecide<'_>],
    joined: usize,
    join: &turso_mysql_parser::MySqlJoinedTable,
    decided: &mut std::collections::HashSet<(usize, String)>,
) {
    let resolve = |named| resolve_named_column(tables, named);
    let Some(matched) = join
        .matched_columns()
        .iter()
        .map(|(left, right)| Some((resolve(left)?, resolve(right)?)))
        .collect::<Option<Vec<_>>>()
    else {
        return;
    };
    let whole_numbers = |(table, column): &(usize, String)| {
        tables[*table].columns.iter().any(|declared| {
            declared.name().eq_ignore_ascii_case(column) && is_integer_type(declared.type_name())
        })
    };
    if !join.left_join() {
        for (left, right) in matched {
            if !whole_numbers(&left) || !whole_numbers(&right) {
                continue;
            }
            if decided.contains(&left) {
                decided.insert(right);
            } else if decided.contains(&right) {
                decided.insert(left);
            }
        }
        return;
    }
    let Some(others) = join
        .other_columns()
        .map(|others| others.iter().map(resolve).collect::<Option<Vec<_>>>())
    else {
        return;
    };
    let Some(others) = others else {
        return;
    };
    let named_before = matched
        .iter()
        .flat_map(|(left, right)| [left, right])
        .chain(&others)
        .filter(|(table, _)| *table != joined)
        .collect::<Vec<_>>();
    if named_before
        .iter()
        .any(|named| named.0 > joined || !decided.contains(*named))
    {
        return;
    }
    for (left, right) in matched {
        let (before, added) = match (left.0 == joined, right.0 == joined) {
            (false, true) => (left, right),
            (true, false) => (right, left),
            _ => continue,
        };
        if whole_numbers(&before) && whole_numbers(&added) && decided.contains(&before) {
            decided.insert(added);
        }
    }
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
            | "BIGINT UNSIGNED"
    )
}

/// The column kinds a comparison can be reasoned about.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ColumnKind {
    Integer,
    Text,
}

fn is_text_type(type_name: &str) -> bool {
    matches!(
        type_name,
        "VARCHAR" | "CHAR" | "TEXT" | "TINYTEXT" | "MEDIUMTEXT" | "LONGTEXT"
    )
}

/// Reports whether a column holds a number that is not counted in whole ones.
fn is_real_type(type_name: &str) -> bool {
    matches!(
        type_name,
        "DECIMAL" | "DECIMAL UNSIGNED" | "DOUBLE" | "DOUBLE UNSIGNED" | "FLOAT" | "FLOAT UNSIGNED"
    )
}

fn is_decimal_type(type_name: &str) -> bool {
    matches!(type_name, "DECIMAL" | "DECIMAL UNSIGNED")
}

fn sql_mentions_column(sql: &str, column: &str) -> bool {
    if !column
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
    {
        return true;
    }
    let pattern = format!(
        r"(?i)(^|[^a-z0-9_$]){}([^a-z0-9_$]|$)",
        regex::escape(column)
    );
    regex::Regex::new(&pattern).map_or(true, |pattern| pattern.is_match(sql))
}

/// One column of a pair compared with each other — `WHERE age > score`.
struct ComparedColumn {
    type_name: String,
    temporal_precision: Option<u8>,
    collation_name: Option<&'static str>,
    engine_collation: turso_core::CollationSeq,
}

/// Whether a comparison sets one `information_schema` column equal to
/// another, which is how TypeORM and Doctrine join the catalog's tables.
///
/// Both sides are compared under the collation the catalog gives its
/// columns, as a comparison against a written word over one already is.
fn joins_two_catalog_columns(
    source_tables: &[MySqlSelectSource],
    comparison: &CheckedSelectComparison,
    other_qualifier: Option<&str>,
) -> bool {
    let reads_the_catalog = |qualifier: Option<&str>| {
        let source = match qualifier {
            Some(qualifier) => source_tables
                .iter()
                .find(|source| source.reference().eq_ignore_ascii_case(qualifier)),
            None => match source_tables {
                [source] => Some(source),
                _ => None,
            },
        };
        source.is_some_and(|source| source.catalog().is_some())
    };
    comparison.operator() == CheckedSelectComparisonOperator::Equal
        && reads_the_catalog(comparison.qualifier())
        && reads_the_catalog(other_qualifier)
}

/// Refuses a column compared with another column where MySQL and the engine
/// could answer the comparison differently.
fn refuse_a_column_pair_compared_differently(
    comparison: &CheckedSelectComparison,
    other_name: &str,
    left: &ComparedColumn,
    right: &ComparedColumn,
) -> Result<()> {
    match column_pair_refusal(left, right, comparison.operator()) {
        None => Ok(()),
        Some(reason) => Err(LimboError::InvalidArgument(format!(
            "SELECT comparison of {} ({}) with {other_name} ({}) is refused: {reason}",
            comparison.column_name(),
            left.type_name,
            right.type_name
        ))),
    }
}

/// Says why MySQL and the engine could answer a comparison of two columns
/// differently, or nothing when they answer it alike.
///
/// Measured on MySQL 8.4.11, which converts one side of a mixed pair to the
/// other's kind where the engine compares the two as they are stored: an `INT`
/// equals a `VARCHAR` holding `'2abc'`, a `DATETIME` holding midnight equals a
/// `DATE` of that day, a `DATETIME` equals a `DATETIME(3)` holding the same
/// moment, and a `BIGINT` of 9007199254740993 equals a `DOUBLE` of
/// 9007199254740992, the two being compared as doubles.
fn column_pair_refusal(
    left: &ComparedColumn,
    right: &ComparedColumn,
    operator: CheckedSelectComparisonOperator,
) -> Option<&'static str> {
    let (Some(left_kind), Some(right_kind)) = (compared_kind(left), compared_kind(right)) else {
        return Some("a comparison of two columns of this type has not been measured");
    };
    match (left_kind, right_kind) {
        (ComparedKind::WholeNumber { .. }, ComparedKind::WholeNumber { .. })
        | (ComparedKind::Double, ComparedKind::Double)
        | (ComparedKind::LargeUnsigned, ComparedKind::LargeUnsigned)
        | (ComparedKind::LargeUnsigned, ComparedKind::WholeNumber { .. })
        | (ComparedKind::WholeNumber { .. }, ComparedKind::LargeUnsigned)
        | (ComparedKind::Day, ComparedKind::Day)
        | (ComparedKind::Year, ComparedKind::Year)
        | (ComparedKind::Decimal, ComparedKind::Decimal) => None,
        // MySQL compares a whole number with a double as two doubles, which
        // is exact up to 2^53 and so is only the engine's exact comparison
        // for a column that cannot hold more.
        (
            ComparedKind::WholeNumber {
                exact_as_a_double: true,
            },
            ComparedKind::Double,
        )
        | (
            ComparedKind::Double,
            ComparedKind::WholeNumber {
                exact_as_a_double: true,
            },
        ) => None,
        (ComparedKind::Words, ComparedKind::Words) => {
            // Measured on 8.4.11: `utf8mb4_0900_ai_ci` against
            // `utf8mb4_unicode_ci` is 1267, and either against `utf8mb4_bin`
            // compares under `utf8mb4_bin`. The engine takes the left one.
            (left.collation_name != right.collation_name
                || left.engine_collation != right.engine_collation)
                .then_some("the two columns compare words under different collations")
        }
        (ComparedKind::Moment(left_type), ComparedKind::Moment(right_type)) => {
            (left_type != right_type || left.temporal_precision != right.temporal_precision)
                .then_some("the two columns hold moments of different types or precisions")
        }
        // A span runs past a day and carries a sign, so its stored form reads
        // in order only for sameness, as it does against a written value.
        (ComparedKind::Span, ComparedKind::Span) => {
            if left.temporal_precision != right.temporal_precision {
                Some("the two columns hold spans of different precisions")
            } else if !compares_for_sameness(operator) {
                Some("an ordering comparison of two TIME columns")
            } else {
                None
            }
        }
        _ => Some("MySQL converts one of the two columns to the other's type first"),
    }
}

/// The kinds of column a pair is compared within.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ComparedKind {
    WholeNumber {
        exact_as_a_double: bool,
    },
    /// A `BIGINT UNSIGNED`, which the engine holds as a blob of its own and
    /// compares exactly against another whole number, as MySQL does: measured
    /// on 8.4.11 and here, a `BIGINT` of 9223372036854775807 is less than an
    /// unsigned 9223372036854775808. MySQL compares one with a double by a
    /// rule not measured here.
    LargeUnsigned,
    Double,
    /// A `DECIMAL`, which the engine compares exactly against another one and
    /// as a double against a whole number: measured, a `DECIMAL(30,20)` of
    /// 1.00000000000000000001 equals an `INT` of 1 here and not in MySQL.
    Decimal,
    /// A `VARCHAR` or a `TEXT`. A `CHAR` is not one of them: MySQL takes its
    /// trailing spaces off when it reads one.
    Words,
    Day,
    Moment(&'static str),
    Span,
    Year,
}

fn compared_kind(column: &ComparedColumn) -> Option<ComparedKind> {
    Some(match column.type_name.as_str() {
        "BIGINT" => ComparedKind::WholeNumber {
            exact_as_a_double: false,
        },
        "BIGINT UNSIGNED" => ComparedKind::LargeUnsigned,
        type_name if is_integer_type(type_name) => ComparedKind::WholeNumber {
            exact_as_a_double: true,
        },
        "DOUBLE" | "DOUBLE UNSIGNED" => ComparedKind::Double,
        "DECIMAL" | "DECIMAL UNSIGNED" => ComparedKind::Decimal,
        "VARCHAR" | "TEXT" | "TINYTEXT" | "MEDIUMTEXT" | "LONGTEXT" => ComparedKind::Words,
        "DATE" => ComparedKind::Day,
        "DATETIME" => ComparedKind::Moment("DATETIME"),
        "TIMESTAMP" => ComparedKind::Moment("TIMESTAMP"),
        "TIME" => ComparedKind::Span,
        "YEAR" => ComparedKind::Year,
        _ => return None,
    })
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
    operator: CheckedSelectComparisonOperator,
) -> bool {
    match rhs {
        CheckedSelectComparisonRhs::SignedInteger(_) => {
            is_integer_type(type_name) || comparison_meets_the_stored_form(rhs, type_name, operator)
        }
        // A number written with a fraction meets a column that holds a number,
        // whether the column counts in whole numbers or not: measured on
        // 8.4.11, `n > 1.5` over an `INT` answers the rows above one, which
        // is what comparing them as numbers answers.
        CheckedSelectComparisonRhs::Decimal(_) => {
            is_integer_type(type_name) || is_real_type(type_name)
        }
        // A reading of the moment answers a value in the form one of these
        // columns holds, so it meets that column and no other. A day against a
        // DATETIME is refused for the reason a written day is: MySQL reads it
        // as that day's midnight.
        CheckedSelectComparisonRhs::Now(now) => match now {
            CheckedComparisonNow::Day => type_name == "DATE",
            CheckedComparisonNow::Moment => matches!(type_name, "DATETIME" | "TIMESTAMP"),
            CheckedComparisonNow::TimeOfDay => {
                type_name == "TIME" && compares_for_sameness(operator)
            }
        },
        CheckedSelectComparisonRhs::Text(_) => {
            is_text_type(type_name)
                || meets_bytes(type_name, operator)
                || comparison_meets_the_stored_form(rhs, type_name, operator)
        }
        // A written moment meets a moment column as the word it names does,
        // and no other column: MySQL compares a column of words with it as a
        // moment, not as a word.
        CheckedSelectComparisonRhs::WrittenMoment(written) => {
            matches!(type_name, "DATETIME" | "TIMESTAMP")
                && comparison_meets_the_stored_form(
                    &CheckedSelectComparisonRhs::Text(written.clone()),
                    type_name,
                    operator,
                )
        }
        CheckedSelectComparisonRhs::Null => {
            is_integer_type(type_name)
                || is_text_type(type_name)
                || turso_mysql_parser::holds_bytes(type_name)
                || stores_a_canonical_form(type_name)
        }
        // Measured on MySQL 8.4.11, a string of bytes is a binary string
        // wherever it is compared with one — `b = X'00'` finds the one byte
        // — and a number or a word by rules not measured anywhere else.
        CheckedSelectComparisonRhs::Bytes => meets_bytes(type_name, operator),
        // A parameter carries no type until it is bound. A column of words
        // compares a bound word under the collation it was declared with, and
        // what binds there is held to a word when the statement runs; a column
        // stored in a canonical form is never safe: the bound value is not put
        // into that form.
        //
        // A `LIKE` binds a pattern rather than a value, which meets the text
        // column it names alone.
        CheckedSelectComparisonRhs::Placeholder { .. } => {
            if matches!(
                operator,
                CheckedSelectComparisonOperator::Like | CheckedSelectComparisonOperator::NotLike
            ) {
                return is_text_type(type_name);
            }
            is_integer_type(type_name)
                || is_decimal_type(type_name)
                || is_text_type(type_name)
                || meets_bytes(type_name, operator)
        }
        // Two columns are held to each other by `column_pair_refusal`, which
        // needs both of them.
        CheckedSelectComparisonRhs::Column { .. } => false,
        // A call meets a column holding what it answers, in the form it
        // answers it in.
        CheckedSelectComparisonRhs::Call(answers) => match answers {
            CheckedComparisonAnswer::Text => is_text_type(type_name),
            CheckedComparisonAnswer::WholeNumber | CheckedComparisonAnswer::RowCount => {
                is_integer_type(type_name)
            }
            CheckedComparisonAnswer::Day => type_name == "DATE",
            CheckedComparisonAnswer::Moment => matches!(type_name, "DATETIME" | "TIMESTAMP"),
            CheckedComparisonAnswer::JsonText
            | CheckedComparisonAnswer::JsonCount
            | CheckedComparisonAnswer::JsonDocument
            | CheckedComparisonAnswer::JsonPath
            | CheckedComparisonAnswer::JsonPattern
            | CheckedComparisonAnswer::JsonValue => false,
        },
        CheckedSelectComparisonRhs::Operand(CheckedComparisonOperand::Arithmetic) => matches!(
            type_name,
            "TINYINT" | "SMALLINT" | "MEDIUMINT" | "INT" | "INTEGER" | "BOOLEAN"
        ),
        CheckedSelectComparisonRhs::Operand(CheckedComparisonOperand::PlainWholeNumber) => {
            is_integer_type(type_name) && type_name != "BIGINT UNSIGNED"
        }
        // The engine answers the fallback in the column's own form for these;
        // a DECIMAL, a moment and an unsigned BIGINT are each held in a form
        // of their own that a written fallback is not.
        CheckedSelectComparisonRhs::Operand(CheckedComparisonOperand::Fallback) => {
            (is_integer_type(type_name) && type_name != "BIGINT UNSIGNED")
                || matches!(
                    type_name,
                    "DOUBLE"
                        | "DOUBLE UNSIGNED"
                        | "VARCHAR"
                        | "TEXT"
                        | "TINYTEXT"
                        | "MEDIUMTEXT"
                        | "LONGTEXT"
                )
        }
    }
}

/// Answers whether a word, written or bound, meets a column of bytes the way
/// MySQL compares them.
///
/// The column's affinity makes the word its bytes before the two are
/// compared, and bytes compare byte by byte with no padding — which is how
/// MySQL compares a binary string: measured on 8.4.11, `b = 'ABC'` finds
/// `ABC` and not `abc`, `vb = 'ab'` does not find `ab  `, and `b > 'ab'`
/// reads the bytes in order. A `LIKE` is not: the engine matches it without
/// regard to case, where MySQL matches a binary string's bytes exactly.
fn meets_bytes(type_name: &str, operator: CheckedSelectComparisonOperator) -> bool {
    turso_mysql_parser::holds_bytes(type_name)
        && !matches!(
            operator,
            CheckedSelectComparisonOperator::Like | CheckedSelectComparisonOperator::NotLike
        )
}

/// Answers whether a comparison against a column that is neither an integer
/// nor text is the same comparison MySQL makes.
///
/// These columns hold the canonical form MySQL stores — a `DATE` holds
/// `2024-01-01` however the value was written — so a comparison against a
/// value already written that way is answered by comparing what is stored,
/// which is what MySQL answers. A value written any other way is refused
/// rather than rewritten: measured on 8.4.11, `d = '2024-1-1'` finds the first
/// of January and comparing the stored form to that text would find nothing.
fn comparison_meets_the_stored_form(
    rhs: &CheckedSelectComparisonRhs,
    type_name: &str,
    operator: CheckedSelectComparisonOperator,
) -> bool {
    let ordered = !matches!(
        operator,
        CheckedSelectComparisonOperator::Like | CheckedSelectComparisonOperator::NotLike
    );
    match (type_name, rhs) {
        // A day and a moment are held zero-padded and widest part first, so
        // reading them in order is reading them in time order.
        ("DATE", CheckedSelectComparisonRhs::Text(written)) => {
            ordered && turso_mysql_parser::normalize_date(written).as_deref() == Some(written)
        }
        ("DATETIME" | "TIMESTAMP", CheckedSelectComparisonRhs::Text(written)) => {
            ordered && turso_mysql_parser::normalize_datetime(written).as_deref() == Some(written)
        }
        // A span is not: it runs past a day, so its hours outgrow two digits,
        // and it carries a sign. `-01:00:00` and `100:00:00` both read out of
        // order, so only sameness is answered.
        ("TIME", CheckedSelectComparisonRhs::Text(written)) => {
            compares_for_sameness(operator)
                && turso_mysql_parser::normalize_time(written).as_deref() == Some(written)
        }
        // A year is held as the number it names, so a number naming the same
        // year is the same value. Measured: MySQL reads `24` as 2024, which
        // the stored 2024 would not meet, so a short year is refused.
        ("YEAR", CheckedSelectComparisonRhs::SignedInteger(number)) => {
            ordered
                && turso_mysql_parser::year_from_number(*number)
                    .is_some_and(|year| i64::from(year) == *number)
        }
        // A bit is held as the number 0 or 1, and MySQL compares one against a
        // number as that number: measured on 8.4.11, `c = 1`, `c = TRUE`,
        // `c <> 0` and `c IN (0)` find the rows holding those bits, and
        // `c = 2` finds none.
        ("BIT", CheckedSelectComparisonRhs::SignedInteger(_)) => ordered,
        // A real is held as a number and compared as one, which is what MySQL
        // compares it as.
        (_, CheckedSelectComparisonRhs::SignedInteger(_)) if is_real_type(type_name) => ordered,
        // A member is held under the spelling it was declared with, and MySQL
        // refuses two members that differ only by case, so a word spelled the
        // way one member is spelled is that one member and no other. Order is
        // not answered: MySQL reads an ENUM by the position its members were
        // declared in, which is not the order their words read in.
        (_, CheckedSelectComparisonRhs::Text(written)) if compares_for_sameness(operator) => {
            if let Some(members) = turso_mysql_parser::enum_members(type_name) {
                return members.iter().any(|member| member == written);
            }
            turso_mysql_parser::set_members(type_name)
                .is_some_and(|members| names_a_stored_subset(&members, written))
        }
        _ => false,
    }
}

/// Reports whether a comparison asks whether two values are the same rather
/// than which of them comes first.
const fn compares_for_sameness(operator: CheckedSelectComparisonOperator) -> bool {
    matches!(
        operator,
        CheckedSelectComparisonOperator::Equal
            | CheckedSelectComparisonOperator::NotEqual
            | CheckedSelectComparisonOperator::NullSafeEqual
            | CheckedSelectComparisonOperator::In
            | CheckedSelectComparisonOperator::NotIn
    )
}

/// Reports whether a word is the way a `SET` holds one of its subsets.
///
/// A `SET` holds the members it was given joined by commas in the order they
/// were declared, so `write,read` is not how any subset is held even though
/// MySQL reads it — measured, MySQL finds no row for it either, but a subset
/// written any other way is refused rather than relied on.
fn names_a_stored_subset(members: &[String], written: &str) -> bool {
    if written.is_empty() {
        return true;
    }
    let mut declared = members.iter();
    written
        .split(',')
        .all(|part| declared.any(|member| member == part))
}

/// The stored form a value bound against one column has to be put into.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BoundTemporalForm {
    /// A day, written the way a `DATE` column holds one.
    Day,
    /// A moment, written the way a `DATETIME` or `TIMESTAMP` column holds one.
    Moment { precision: u8 },
}

/// One parameter that meets a column holding a day or a moment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct BoundTemporalParameter {
    ordinal: usize,
    form: BoundTemporalForm,
}

/// Holds each value an `UPDATE` does arithmetic with to what MySQL reads alike;
/// see [`BoundOperandKind`].
fn hold_bound_operands(
    operands: &[(usize, BoundOperandKind)],
    values: &[MySqlPreparedValue],
) -> Result<()> {
    for (ordinal, kind) in operands {
        let value = values.get(*ordinal).ok_or_else(|| {
            LimboError::InternalError(
                "an arithmetic placeholder is outside the prepared parameters".to_string(),
            )
        })?;
        let fits = match (kind, value) {
            (_, MySqlPreparedValue::Null | MySqlPreparedValue::Integer(_)) => true,
            (BoundOperandKind::WholeNumber, MySqlPreparedValue::Text(written)) => {
                bound_whole_number(written).is_some()
            }
            (BoundOperandKind::ExactNumber, MySqlPreparedValue::Text(written)) => {
                turso_mysql_parser::read_written_number(written).is_some()
            }
            _ => false,
        };
        if !fits {
            return Err(LimboError::InvalidArgument(format!(
                "a value bound in arithmetic written into a {} column has to be a whole number{}",
                match kind {
                    BoundOperandKind::WholeNumber => "whole-number",
                    BoundOperandKind::ExactNumber => "DECIMAL",
                },
                match kind {
                    BoundOperandKind::WholeNumber => ", or a word naming one",
                    BoundOperandKind::ExactNumber => " or a word naming a number",
                }
            )));
        }
    }
    Ok(())
}

/// The whole number a bound word names, which is what MySQL reads it as
/// against a column holding whole numbers.
fn bound_whole_number(written: &str) -> Option<i64> {
    match turso_mysql_parser::read_written_number(written)? {
        turso_mysql_parser::WrittenNumber::Whole(value) => Some(value),
        turso_mysql_parser::WrittenNumber::Decimal(_) => None,
    }
}

/// Which form a column holds, for the columns that hold a day or a moment.
///
/// A `TIME` is not one of them: it holds a span rather than a moment, running
/// past a day and carrying a sign, so reading two of them in order is not
/// reading them in time order.
const fn bound_temporal_form(type_name: &str, precision: u8) -> Option<BoundTemporalForm> {
    match type_name.as_bytes() {
        b"DATE" => Some(BoundTemporalForm::Day),
        b"DATETIME" | b"TIMESTAMP" => Some(BoundTemporalForm::Moment { precision }),
        _ => None,
    }
}

/// Puts a bound value into the form the column it meets holds.
///
/// MySQL reads a bound word the way it reads a written one — measured on
/// 8.4.11, `at > ?` bound `'2026-01-01'` over a `DATETIME` finds the rows after
/// that day's midnight and not the row standing at it, and a loose `'2026-1-5'`
/// is read as that day. A word that reads as no moment at all finds no row
/// there, which is what a NULL finds here; MySQL warns 1292 about it as well,
/// and this does not.
fn temporal_value_in_its_stored_form(
    value: &MySqlPreparedValue,
    form: BoundTemporalForm,
) -> Result<Value> {
    match value {
        MySqlPreparedValue::Null => Ok(Value::Null),
        MySqlPreparedValue::Text(written) => {
            let stored = match form {
                BoundTemporalForm::Day => turso_mysql_parser::normalize_date(written),
                BoundTemporalForm::Moment { precision } => {
                    turso_mysql_parser::normalize_datetime_with_precision(written, 6).map(
                        |normalized| match normalized.split_once('.') {
                            Some((whole, fraction)) => {
                                let kept = fraction.trim_end_matches('0');
                                let keep = kept.len().max(usize::from(precision));
                                if keep == 0 {
                                    whole.to_owned()
                                } else {
                                    format!("{whole}.{}", &fraction[..keep])
                                }
                            }
                            None => normalized,
                        },
                    )
                }
            };
            Ok(stored.map_or(Value::Null, Value::from_text))
        }
        // Measured: MySQL reads a bound number as a moment — 20260101000000
        // names the first of January — and what this can read is a word.
        _ => Err(LimboError::InvalidArgument(
            "a value bound against a column holding a day or a moment has to be written as one"
                .to_string(),
        )),
    }
}

/// Holds one `SELECT` comparison to the type of the column it names, and says
/// which parameter has to be put into a stored form before it is bound.
///
/// A parameter meeting a column that holds a day or a moment is taken where
/// the ordinary check refuses it: the `SELECT` path puts the bound value into
/// the column's own form, which is what the refusal was for.
fn select_comparison_fits_column(
    comparison: &CheckedSelectComparison,
    type_name: &str,
    temporal_precision: u8,
) -> Result<Option<BoundTemporalParameter>> {
    if temporal_precision > 0
        && matches!(type_name, "DATETIME" | "TIMESTAMP" | "TIME")
        && !matches!(
            comparison.rhs(),
            CheckedSelectComparisonRhs::Placeholder { .. } | CheckedSelectComparisonRhs::Null
        )
    {
        let canonical = match comparison.rhs() {
            CheckedSelectComparisonRhs::WrittenMoment(written) if type_name != "TIME" => {
                turso_mysql_parser::normalize_datetime_with_precision(written, temporal_precision)
                    .as_deref()
                    == Some(written)
                    && !matches!(
                        comparison.operator(),
                        CheckedSelectComparisonOperator::Like
                            | CheckedSelectComparisonOperator::NotLike
                    )
            }
            CheckedSelectComparisonRhs::Text(written) => {
                let normalized = if type_name == "TIME" {
                    turso_mysql_parser::normalize_time_with_precision(written, temporal_precision)
                } else {
                    turso_mysql_parser::normalize_datetime_with_precision(
                        written,
                        temporal_precision,
                    )
                };
                normalized.as_deref() == Some(written)
                    && !matches!(
                        comparison.operator(),
                        CheckedSelectComparisonOperator::Like
                            | CheckedSelectComparisonOperator::NotLike
                    )
            }
            _ => false,
        };
        if canonical {
            return Ok(None);
        }
        return Err(checked_comparison_column_refusal(
            comparison.rhs(),
            comparison.column_name(),
            type_name,
        ));
    }
    if type_name == "JSON" {
        let supported = matches!(
            comparison.operator(),
            CheckedSelectComparisonOperator::Equal
                | CheckedSelectComparisonOperator::NotEqual
                | CheckedSelectComparisonOperator::LessThan
                | CheckedSelectComparisonOperator::LessThanOrEqual
                | CheckedSelectComparisonOperator::GreaterThan
                | CheckedSelectComparisonOperator::GreaterThanOrEqual
                | CheckedSelectComparisonOperator::NullSafeEqual
                | CheckedSelectComparisonOperator::In
                | CheckedSelectComparisonOperator::NotIn
        ) && matches!(
            comparison.rhs(),
            CheckedSelectComparisonRhs::Text(_)
                | CheckedSelectComparisonRhs::SignedInteger(_)
                | CheckedSelectComparisonRhs::Null
        );
        return if supported {
            Ok(None)
        } else {
            Err(checked_comparison_column_refusal(
                comparison.rhs(),
                comparison.column_name(),
                type_name,
            ))
        };
    }
    let bound = match comparison.rhs() {
        CheckedSelectComparisonRhs::Placeholder { ordinal } => {
            bound_temporal_form(type_name, temporal_precision).map(|form| BoundTemporalParameter {
                ordinal: *ordinal,
                form,
            })
        }
        _ => None,
    };
    if bound.is_none()
        && !checked_comparison_fits_column(comparison.rhs(), type_name, comparison.operator())
    {
        return Err(checked_comparison_column_refusal(
            comparison.rhs(),
            comparison.column_name(),
            type_name,
        ));
    }
    Ok(bound)
}

/// Reports whether a column is held in a canonical form of its own rather than
/// as the integer or the text it was written as.
fn stores_a_canonical_form(type_name: &str) -> bool {
    matches!(
        type_name,
        "DATE" | "DATETIME" | "TIMESTAMP" | "TIME" | "YEAR" | "BIT"
    ) || is_real_type(type_name)
        || turso_mysql_parser::enum_members(type_name).is_some()
        || turso_mysql_parser::set_members(type_name).is_some()
}

/// Reports whether a value can meet what a call answers.
///
/// A word is compared the way MySQL compares one, so a written word meets it.
/// A day and a moment are held to the form the value is stored in, the way a
/// column of that kind is. A parameter carries no type until it binds, and
/// nothing puts it into that form, so it meets none of them.
fn checked_comparison_meets_an_answer(
    rhs: &CheckedSelectComparisonRhs,
    answers: CheckedComparisonAnswer,
) -> bool {
    match (answers, rhs) {
        (_, CheckedSelectComparisonRhs::Null) => true,
        // The renderer takes two calls only when they answer one kind, and
        // not a kind read out of JSON.
        (answers, CheckedSelectComparisonRhs::Call(other)) => {
            answers == *other && !is_a_json_answer(Some(answers))
        }
        (CheckedComparisonAnswer::Text, CheckedSelectComparisonRhs::Text(_)) => true,
        (
            CheckedComparisonAnswer::WholeNumber,
            CheckedSelectComparisonRhs::SignedInteger(_) | CheckedSelectComparisonRhs::Decimal(_),
        ) => true,
        (CheckedComparisonAnswer::Day, CheckedSelectComparisonRhs::Text(written)) => {
            turso_mysql_parser::normalize_date(written).as_deref() == Some(written)
        }
        (CheckedComparisonAnswer::Moment, CheckedSelectComparisonRhs::Text(written)) => {
            turso_mysql_parser::normalize_datetime(written).as_deref() == Some(written)
        }
        (CheckedComparisonAnswer::Day, CheckedSelectComparisonRhs::Now(now)) => {
            *now == CheckedComparisonNow::Day
        }
        (CheckedComparisonAnswer::Moment, CheckedSelectComparisonRhs::Now(now)) => {
            *now == CheckedComparisonNow::Moment
        }
        // What binds against a JSON reading is held to the kinds it was
        // rendered for each time the statement runs.
        (
            CheckedComparisonAnswer::JsonText,
            CheckedSelectComparisonRhs::Text(_)
            | CheckedSelectComparisonRhs::SignedInteger(_)
            | CheckedSelectComparisonRhs::Decimal(_)
            | CheckedSelectComparisonRhs::Placeholder { .. },
        ) => true,
        (
            CheckedComparisonAnswer::JsonCount,
            CheckedSelectComparisonRhs::SignedInteger(_)
            | CheckedSelectComparisonRhs::Decimal(_)
            | CheckedSelectComparisonRhs::Placeholder { .. },
        ) => true,
        (
            CheckedComparisonAnswer::JsonDocument
            | CheckedComparisonAnswer::JsonPath
            | CheckedComparisonAnswer::JsonPattern
            | CheckedComparisonAnswer::JsonValue,
            CheckedSelectComparisonRhs::Placeholder { .. },
        ) => true,
        (CheckedComparisonAnswer::JsonPattern, CheckedSelectComparisonRhs::Text(_)) => true,
        // What binds against a count is held to a whole number when the
        // statement runs.
        (
            CheckedComparisonAnswer::RowCount,
            CheckedSelectComparisonRhs::SignedInteger(_)
            | CheckedSelectComparisonRhs::Placeholder { .. },
        ) => true,
        _ => false,
    }
}

/// Reports whether an execution binds a number where a JSON reading is
/// compared with a word or looked in for a document.
fn binds_a_number_to_a_json_reading(
    comparisons: &[CheckedSelectComparison],
    values: &[MySqlPreparedValue],
) -> bool {
    json_reading_parameters(comparisons, values).any(|(answers, value)| {
        !matches!(
            answers,
            CheckedComparisonAnswer::JsonCount | CheckedComparisonAnswer::JsonPath
        ) && matches!(
            value,
            MySqlPreparedValue::Integer(_)
                | MySqlPreparedValue::UnsignedInteger(_)
                | MySqlPreparedValue::Real(_)
        )
    })
}

/// Holds what binds against a JSON reading to the kinds its comparison was
/// rendered for.
///
/// Measured on MySQL 8.4.11 with a statement prepared once and run with
/// different values: a word bound against unquoted JSON text is compared as a
/// word and a number as a number, until a number has been bound, after which
/// a word is compared as a number too; `JSON_LENGTH(...) = ?` reads a bound
/// word as a number, `'2x'` as 2; and `JSON_CONTAINS(doc, ?)` bound a number
/// finds nothing, and then refuses every word with 3146. NULL changes none of
/// this.
fn hold_json_reading_parameters(
    comparisons: &[CheckedSelectComparison],
    values: &[MySqlPreparedValue],
    bound_a_number_before: bool,
) -> Result<()> {
    for (answers, value) in json_reading_parameters(comparisons, values) {
        let fits = match (answers, value) {
            (_, MySqlPreparedValue::Null) => true,
            (CheckedComparisonAnswer::JsonText, MySqlPreparedValue::Integer(_))
            | (CheckedComparisonAnswer::JsonText, MySqlPreparedValue::Real(_))
            | (CheckedComparisonAnswer::JsonCount, MySqlPreparedValue::Integer(_))
            | (CheckedComparisonAnswer::JsonValue, MySqlPreparedValue::Integer(_)) => true,
            (CheckedComparisonAnswer::JsonPath, MySqlPreparedValue::Text(path)) => {
                turso_mysql_parser::is_a_json_path_this_reads(path)
            }
            // A pattern is matched as a word however the statement bound
            // before, and a number bound there is refused, so what came before
            // does not change how a word reads.
            (CheckedComparisonAnswer::JsonPattern, MySqlPreparedValue::Text(_)) => true,
            (
                CheckedComparisonAnswer::JsonText
                | CheckedComparisonAnswer::JsonDocument
                | CheckedComparisonAnswer::JsonValue,
                MySqlPreparedValue::Text(_),
            ) => !bound_a_number_before,
            _ => false,
        };
        if !fits {
            return Err(LimboError::InvalidArgument(format!(
                "a value bound against a JSON reading has to be {}{}",
                answered_kind_name(answers),
                if bound_a_number_before {
                    ", and a word no longer is once a number has been bound; prepare the statement again"
                } else {
                    ""
                }
            )));
        }
    }
    Ok(())
}

/// Pairs each parameter a JSON reading meets with what the reading answers.
fn json_reading_parameters<'a>(
    comparisons: &'a [CheckedSelectComparison],
    values: &'a [MySqlPreparedValue],
) -> impl Iterator<Item = (CheckedComparisonAnswer, &'a MySqlPreparedValue)> {
    comparisons.iter().filter_map(move |comparison| {
        let answers = comparison
            .answers()
            .filter(|answers| is_a_json_answer(Some(*answers)))?;
        let CheckedSelectComparisonRhs::Placeholder { ordinal } = comparison.rhs() else {
            return None;
        };
        values.get(*ordinal).map(|value| (answers, value))
    })
}

/// Names what a call answers, for a refusal a client can read.
const fn answered_kind_name(answers: CheckedComparisonAnswer) -> &'static str {
    match answers {
        CheckedComparisonAnswer::Text => "a word",
        CheckedComparisonAnswer::WholeNumber => "a number",
        CheckedComparisonAnswer::Day => "a day written the way one is stored",
        CheckedComparisonAnswer::Moment => "a moment written the way one is stored",
        CheckedComparisonAnswer::JsonText => "a word or a number",
        CheckedComparisonAnswer::JsonCount => "a number",
        CheckedComparisonAnswer::JsonDocument => "a JSON document",
        CheckedComparisonAnswer::JsonPath => "a JSON path read the way MySQL reads it",
        CheckedComparisonAnswer::JsonPattern => "a word",
        CheckedComparisonAnswer::JsonValue => "a word or a whole number",
        CheckedComparisonAnswer::RowCount => "a whole number",
    }
}

fn checked_comparison_column_refusal(
    rhs: &CheckedSelectComparisonRhs,
    column_name: &str,
    type_name: &str,
) -> LimboError {
    let wanted = match rhs {
        CheckedSelectComparisonRhs::SignedInteger(_) => "a signed integer column",
        CheckedSelectComparisonRhs::Decimal(_) => "a column that holds a number",
        CheckedSelectComparisonRhs::Now(CheckedComparisonNow::Day) => "a DATE column",
        CheckedSelectComparisonRhs::Now(CheckedComparisonNow::Moment) => {
            "a DATETIME or TIMESTAMP column"
        }
        CheckedSelectComparisonRhs::Now(CheckedComparisonNow::TimeOfDay) => "a TIME column",
        CheckedSelectComparisonRhs::WrittenMoment(_) => {
            "a DATETIME or TIMESTAMP column holding the moment in its own form"
        }
        CheckedSelectComparisonRhs::Text(_) => "a text column",
        CheckedSelectComparisonRhs::Bytes => "a column of bytes",
        CheckedSelectComparisonRhs::Null => "a signed integer or text column",
        CheckedSelectComparisonRhs::Placeholder { .. } => {
            "a signed integer column, because a parameter carries no type"
        }
        CheckedSelectComparisonRhs::Column { .. } => "a column of the same kind",
        CheckedSelectComparisonRhs::Call(answers) => answered_column_kind_name(*answers),
        CheckedSelectComparisonRhs::Operand(CheckedComparisonOperand::Arithmetic) => {
            "a signed whole-number column no wider than an INT"
        }
        CheckedSelectComparisonRhs::Operand(CheckedComparisonOperand::Fallback) => {
            "a whole-number, DOUBLE or text column"
        }
        CheckedSelectComparisonRhs::Operand(CheckedComparisonOperand::PlainWholeNumber) => {
            "a whole-number column other than a BIGINT UNSIGNED"
        }
    };
    LimboError::InvalidArgument(format!(
        "SELECT comparison on {column_name} requires {wanted}, found {type_name}"
    ))
}

/// Names the column a call answering this meets, for a refusal a client can
/// read.
const fn answered_column_kind_name(answers: CheckedComparisonAnswer) -> &'static str {
    match answers {
        CheckedComparisonAnswer::Text => "a text column",
        CheckedComparisonAnswer::WholeNumber | CheckedComparisonAnswer::RowCount => {
            "a whole-number column"
        }
        CheckedComparisonAnswer::Day => "a DATE column",
        CheckedComparisonAnswer::Moment => "a DATETIME or TIMESTAMP column",
        CheckedComparisonAnswer::JsonText
        | CheckedComparisonAnswer::JsonCount
        | CheckedComparisonAnswer::JsonDocument
        | CheckedComparisonAnswer::JsonPath
        | CheckedComparisonAnswer::JsonPattern
        | CheckedComparisonAnswer::JsonValue => "no column",
    }
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
            // A pair of columns was held to each other when the statement was
            // prepared, and both frozen parsers refuse to go on once any table
            // the statement reads is declared differently.
            if comparison.answers().is_some()
                || matches!(comparison.rhs(), CheckedSelectComparisonRhs::Column { .. })
            {
                continue;
            }
            refuse_like_over_a_view(schema, source_table, comparison)?;
            let Some((_, column)) = table.get_column_by_name(comparison.column_name()) else {
                return Err(LimboError::SchemaUpdated);
            };
            if !checked_comparison_fits_column(
                comparison.rhs(),
                &column.ty_str,
                comparison.operator(),
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
        if comparison.answers().is_some() {
            continue;
        }
        if matches!(comparison.rhs(), CheckedSelectComparisonRhs::Column { .. }) {
            return Err(LimboError::InvalidArgument(
                "a comparison of two columns reads base tables only".to_string(),
            ));
        }
        refuse_like_over_a_view(schema, source_table, comparison)?;
        let Some(column) = view.columns.iter().find(|column| {
            column
                .name
                .as_deref()
                .is_some_and(|name| name.eq_ignore_ascii_case(comparison.column_name()))
        }) else {
            return Err(LimboError::SchemaUpdated);
        };
        if !checked_comparison_fits_column(comparison.rhs(), &column.ty_str, comparison.operator())
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

fn refuse_like_over_a_view(
    schema: &turso_core::schema::Schema,
    table_name: &str,
    comparison: &CheckedSelectComparison,
) -> Result<()> {
    if !matches!(
        comparison.operator(),
        CheckedSelectComparisonOperator::Like | CheckedSelectComparisonOperator::NotLike
    ) {
        return Ok(());
    }
    if schema.get_table(table_name).is_none() && schema.get_view(table_name).is_some() {
        return Err(LimboError::InvalidArgument(
            "LIKE over a view needs its source column collation".to_string(),
        ));
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
        Expr::Literal(Literal::CurrentTimestamp) => {
            Ok(("CURRENT_TIMESTAMP".to_string(), MySqlColumnDefault::Moment))
        }
        expression if turso_mysql_parser::reads_the_clock_as_an_expression(expression) => {
            Ok(("(now())".to_string(), MySqlColumnDefault::MomentCall))
        }
        // A column holding fractional seconds reads the moment to as many
        // digits, which is written into the engine's definition as a reading
        // of its clock and printed back the way MySQL prints it.
        expression if turso_mysql_parser::moment_with_fraction_digits(expression).is_some() => {
            let digits = turso_mysql_parser::moment_with_fraction_digits(expression)
                .expect("the guard read the digits");
            Ok((
                format!("CURRENT_TIMESTAMP({digits})"),
                MySqlColumnDefault::Moment,
            ))
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
    let Ok(value) = text.parse::<i64>() else {
        // A number written with a point belongs to a column that holds one,
        // and is kept as it was written. A whole one too wide for the engine
        // to hold is not: it would be stored as some other number.
        if text.contains('.') && text.parse::<f64>().is_ok() {
            return Ok(MySqlColumnDefault::Number(text.to_owned()));
        }
        return Err(MySqlColumnMetadataError::UnsupportedDefinition);
    };
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

struct FrozenRowidKeyDdlParser {
    mode: SessionSqlMode,
}

struct FrozenDmlParser {
    mode: SessionSqlMode,
    column_types: DmlColumnTypes,
    table_definition: Option<(String, String)>,
    read_table_definitions: Vec<(String, String)>,
    untracked_read_source: bool,
    shifted_timestamp_insert: Option<Stmt>,
    /// An `INSERT ... SELECT` whose `SELECT` was rendered knowing its
    /// columns' types, which only a connection can read. It stands for as
    /// long as the tables it names are the tables it was rendered from.
    typed_copy: Option<Stmt>,
}

/// Holds an `UPDATE` or `DELETE` `WHERE` to the same rule a `SELECT` `WHERE`
/// obeys, so the two engines cannot disagree about which rows it names.
fn validate_dml_comparison_columns(
    schema: &turso_core::schema::Schema,
    translated: &turso_mysql_parser::TranslatedDml,
) -> Result<()> {
    if let Some(table) = translated
        .source_table()
        .and_then(|table| schema.get_table(table))
    {
        if let Some(refusal) =
            call_over_another_collation(&table, translated.collation_sensitive_call_columns())
        {
            return Err(LimboError::InvalidArgument(refusal));
        }
    }
    refuse_dml_json_readings_mysql_reads_differently(schema, translated)?;
    refuse_a_null_ignore_writes_into_a_column_refusing_null(schema, translated)?;
    validate_frozen_select_comparison_columns(
        schema,
        translated.source_table(),
        translated.checked_comparisons(),
    )
}

/// Holds the JSON readings in an `UPDATE` or `DELETE` `WHERE` to what the
/// `SELECT` path holds them to: each reads a `JSON` column of the one table
/// the statement writes.
///
/// A bound value is refused outright. A prepared `SELECT` holds what binds
/// against a JSON reading to a word or a number every time it runs, and a
/// prepared DML statement has no such step.
/// Refuses a written NULL that `IGNORE` writes into a column refusing NULL.
///
/// Measured on MySQL 8.4.11, `INSERT IGNORE` and `UPDATE IGNORE` store the
/// type's own empty value there — 0, `''`, `0000-00-00` — and warn 1048,
/// where the engine's `OR IGNORE` skips the row. Into a column that takes
/// NULL both store NULL and warn nothing.
fn refuse_a_null_ignore_writes_into_a_column_refusing_null(
    schema: &turso_core::schema::Schema,
    translated: &TranslatedDml,
) -> Result<()> {
    let Some((table, columns)) = translated.ignored_null_columns() else {
        return Ok(());
    };
    let Some(btree) = schema.get_btree_table(table) else {
        return Err(LimboError::InvalidArgument(format!(
            "IGNORE writing NULL into {table}, whose columns are not known"
        )));
    };
    for column in columns {
        let takes_null = btree
            .get_column(column)
            .is_some_and(|(_, column)| !column.notnull() && !column.is_rowid_alias());
        if !takes_null {
            return Err(LimboError::InvalidArgument(format!(
                "IGNORE writing NULL into {table}.{column}, which refuses NULL"
            )));
        }
    }
    Ok(())
}

fn refuse_dml_json_readings_mysql_reads_differently(
    schema: &turso_core::schema::Schema,
    translated: &TranslatedDml,
) -> Result<()> {
    if let Some((table, columns)) = translated.json_cast_columns() {
        refuse_json_casts_into_other_columns(schema, table, columns)?;
    }
    if translated.checked_comparisons().iter().any(|comparison| {
        is_a_json_answer(comparison.answers())
            && matches!(
                comparison.rhs(),
                CheckedSelectComparisonRhs::Placeholder { .. }
            )
    }) {
        return Err(LimboError::InvalidArgument(
            "DML comparison of a JSON reading with a bound value is unsupported".to_string(),
        ));
    }
    if translated.json_reading_columns().is_empty() {
        return Ok(());
    }
    if !translated.read_tables().is_empty() {
        return Err(LimboError::InvalidArgument(
            "a JSON reading in a DML condition requires the one table the statement writes"
                .to_string(),
        ));
    }
    let table = translated.source_table().ok_or(LimboError::SchemaUpdated)?;
    refuse_json_readings_of_other_columns(schema, table, translated.json_reading_columns())
}

/// Holds each column a `CAST(... AS JSON)` is written into to being a `JSON`
/// column. What MySQL writes into any other kind has not been measured.
fn refuse_json_casts_into_other_columns(
    schema: &turso_core::schema::Schema,
    table: &str,
    columns: &[String],
) -> Result<()> {
    let table = schema
        .get_btree_table(table)
        .ok_or(LimboError::SchemaUpdated)?;
    for name in columns {
        let (_, column) = table.get_column(name).ok_or(LimboError::SchemaUpdated)?;
        if !column.ty_str.eq_ignore_ascii_case("JSON") {
            return Err(LimboError::InvalidArgument(format!(
                "CAST AS JSON written into {name} requires a JSON column"
            )));
        }
    }
    Ok(())
}

/// Holds every column a JSON reading reads to being a `JSON` column.
///
/// The readings are MySQL's over a document. Over a text column MySQL reads
/// the text as a document first, which is not what this measured.
fn refuse_json_readings_of_other_columns(
    schema: &turso_core::schema::Schema,
    table: &str,
    columns: &[String],
) -> Result<()> {
    let table = schema.get_btree_table(table).ok_or_else(|| {
        LimboError::InvalidArgument("a JSON reading requires a base table".to_string())
    })?;
    for name in columns {
        let (_, column) = table.get_column(name).ok_or(LimboError::SchemaUpdated)?;
        if !column.ty_str.eq_ignore_ascii_case("JSON") {
            return Err(LimboError::InvalidArgument(format!(
                "a JSON reading of {name} requires a JSON column"
            )));
        }
    }
    Ok(())
}

fn is_a_json_answer(answers: Option<CheckedComparisonAnswer>) -> bool {
    matches!(
        answers,
        Some(
            CheckedComparisonAnswer::JsonText
                | CheckedComparisonAnswer::JsonCount
                | CheckedComparisonAnswer::JsonDocument
                | CheckedComparisonAnswer::JsonPath
                | CheckedComparisonAnswer::JsonPattern
                | CheckedComparisonAnswer::JsonValue
        )
    )
}

/// Names the first of `columns` declared with a collation other than
/// `utf8mb4_0900_ai_ci`, whose weights the calls reading them compare under.
fn call_over_another_collation(
    table: &turso_core::schema::Table,
    columns: &[String],
) -> Option<String> {
    columns.iter().find_map(|name| {
        let (_, column) = table.get_column_by_name(name)?;
        match column.collation() {
            turso_core::CollationSeq::MySqlUtf8mb4Bin => Some(format!(
                "text call on utf8mb4_bin column {name} requires binary collation semantics"
            )),
            turso_core::CollationSeq::MySqlUca400 => Some(format!(
                "text call on utf8mb4_unicode_ci column {name} requires its collation"
            )),
            turso_core::CollationSeq::MySqlUtf8mb3Uca400 => Some(format!(
                "text call on utf8mb3_unicode_ci column {name} requires its collation"
            )),
            turso_core::CollationSeq::MySqlUtf8mb4GeneralCi => Some(format!(
                "text call on utf8mb4_general_ci column {name} requires its collation"
            )),
            _ => None,
        }
    })
}

fn validate_dml_ordered_columns_with_schema(
    schema: &turso_core::schema::Schema,
    translated: &turso_mysql_parser::TranslatedDml,
) -> Result<()> {
    if translated.ordered_columns().is_empty() {
        return Ok(());
    }
    let source_table = translated.source_table().ok_or(LimboError::SchemaUpdated)?;
    if let Some(table) = schema.get_table(source_table) {
        let stored_sql = schema
            .table_sql(source_table)
            .ok_or(LimboError::SchemaUpdated)?;
        decode_schema_sql(SchemaSqlKind::Table, stored_sql)
            .map_err(|_| LimboError::Corrupt("invalid DML schema provenance".to_string()))?
            .ok_or(LimboError::SchemaUpdated)?;
        for col_name in translated.ordered_columns() {
            let Some((_, column)) = table.get_column_by_name(col_name) else {
                return Err(LimboError::SchemaUpdated);
            };
            if !is_integer_type(&column.ty_str) {
                return Err(LimboError::ParseError(
                    "DML ORDER BY supports only integer columns".to_string(),
                ));
            }
        }
        return Ok(());
    }
    Err(LimboError::SchemaUpdated)
}

struct FrozenSelectParser {
    mode: SessionSqlMode,
    source_table: Option<String>,
    /// The table the comparisons were held to is an `information_schema`
    /// table, whose columns are fixed when it is registered and which has no
    /// stored definition to read them back from.
    source_is_catalog: bool,
    checked_comparisons: Vec<CheckedSelectComparison>,
    typed_statement: Option<Stmt>,
    table_definitions: Vec<(String, String)>,
    source_columns: Vec<(String, Vec<String>)>,
    untracked_source: bool,
}

#[derive(Clone)]
struct AutoIncrementTable {
    name: String,
    definition: Arc<CheckedAutoIncrementCreateTable>,
    key: AutoIncrementKey,
    stored_sql: Arc<str>,
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

/// Whether a counted column's type holds `id`.
fn counted_column_holds(table: &AutoIncrementTable, id: i128) -> bool {
    let (least, most) = table.definition.allocator_column_type.bounds();
    (least..=most).contains(&id)
}

/// Refuses an id written into a counted column that its type cannot hold,
/// which MySQL answers 1264 for.
fn hold_the_id_to_the_counted_column(table: &AutoIncrementTable, id: i128) -> Result<()> {
    if counted_column_holds(table, id) {
        return Ok(());
    }
    Err(LimboError::from(turso_core::AssignmentError::OutOfRange {
        table: table.name.clone(),
        column: table.definition.allocator_column_ordinal + 1,
        type_name: table.definition.allocator_column_written_type.to_string(),
        value: i64::try_from(id).unwrap_or(if id < 0 { i64::MIN } else { i64::MAX }),
    }))
}

fn read_table_names(translated: &TranslatedDml) -> Vec<String> {
    translated
        .read_tables()
        .iter()
        .map(|source| source.table().as_str().to_owned())
        .collect()
}

/// Bytes a statement that writes binds — a `Buffer` mysql2 binds as a
/// `MYSQL_TYPE_BLOB`, long text a driver sends through
/// `COM_STMT_SEND_LONG_DATA` — are read as a word when they are UTF-8.
///
/// Measured on MySQL 8.4.11, bytes bound into a column of words are read as
/// utf8mb4 and stored as that text, and bytes that are not UTF-8 answer 1366.
/// A column of bytes turns the word back into the same bytes, so what lands
/// there is unchanged. A `SELECT` keeps them bytes: `SELECT ?` answers a
/// `BLOB`, and a comparison takes bytes only against a column of bytes.
fn utf8_bytes_as_words(values: &[MySqlPreparedValue]) -> Vec<MySqlPreparedValue> {
    values
        .iter()
        .map(|value| match value {
            MySqlPreparedValue::Blob(bytes) => std::str::from_utf8(bytes)
                .map(|word| MySqlPreparedValue::Text(word.to_owned()))
                .unwrap_or_else(|_| value.clone()),
            value => value.clone(),
        })
        .collect()
}

/// Holds what binds against a column of words to a word or NULL.
///
/// Measured on MySQL 8.4.11: a number compared with a column of words is
/// compared with the number each word begins with — `name = 0` finds `'abc'`
/// and `name = 5` finds `'5x'` — where the engine compares the number as the
/// word it spells, finding neither.
fn refuse_a_word_parameter_bound_otherwise(
    word_parameters: &[usize],
    values: &[MySqlPreparedValue],
) -> Result<()> {
    for ordinal in word_parameters {
        let value = values.get(*ordinal).ok_or_else(|| {
            LimboError::InternalError("a compared parameter is outside the bound values".into())
        })?;
        if !matches!(
            value,
            MySqlPreparedValue::Text(_) | MySqlPreparedValue::Null
        ) {
            return Err(LimboError::InvalidArgument(
                "a value compared with a column of words has to be bound as a word".to_owned(),
            ));
        }
    }
    Ok(())
}

/// Holds what binds against a column of bytes to a word, bytes or NULL, for
/// the reason `byte_comparison_parameters` gives.
fn refuse_a_byte_parameter_bound_otherwise(
    byte_parameters: &[usize],
    values: &[MySqlPreparedValue],
) -> Result<()> {
    for ordinal in byte_parameters {
        let value = values.get(*ordinal).ok_or_else(|| {
            LimboError::InternalError("a compared parameter is outside the bound values".into())
        })?;
        if !matches!(
            value,
            MySqlPreparedValue::Text(_) | MySqlPreparedValue::Blob(_) | MySqlPreparedValue::Null
        ) {
            return Err(LimboError::InvalidArgument(
                "a value compared with a column of bytes has to be bound as a word or bytes"
                    .to_owned(),
            ));
        }
    }
    Ok(())
}

/// The counted table a stored `CREATE TABLE` describes, or `None` for a table
/// that counts nothing.
/// Reads the counted table a stored definition describes, once for each
/// definition and database.
///
/// Every `INSERT` into a counted table reads its definition, and reading it
/// parses the stored DDL; in sysbench's `oltp_insert` that parse alone took
/// about a fifth of the server's time. What it answers depends on the stored
/// text and the database's identity and nothing else, so it is kept by both;
/// a changed definition is a different text.
fn counted_table_from_stored_sql(
    sql: &str,
    database_identity: Option<[u8; 16]>,
) -> Result<Option<AutoIncrementTable>> {
    type ReadDefinitions = HashMap<(String, Option<[u8; 16]>), Option<AutoIncrementTable>>;
    static READ: std::sync::LazyLock<Mutex<ReadDefinitions>> =
        std::sync::LazyLock::new(|| Mutex::new(HashMap::new()));
    /// Enough for every counted table a server is busy with; past it the
    /// definitions are read again as they come.
    const KEPT: usize = 1024;

    let key = (sql.to_owned(), database_identity);
    if let Some(table) = READ
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get(&key)
    {
        return Ok(table.clone());
    }
    let table = read_counted_table_from_stored_sql(sql, database_identity)?;
    let mut read = READ
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if read.len() >= KEPT {
        read.clear();
    }
    read.insert(key, table.clone());
    Ok(table)
}

fn read_counted_table_from_stored_sql(
    sql: &str,
    database_identity: Option<[u8; 16]>,
) -> Result<Option<AutoIncrementTable>> {
    let Some(decoded) = decode_schema_sql(SchemaSqlKind::Table, sql)
        .map_err(|error| LimboError::Corrupt(error.to_string()))?
    else {
        return Ok(None);
    };
    let Some(metadata) = decoded.v2_metadata() else {
        return Ok(None);
    };
    let expected_database_identity = database_identity.ok_or_else(|| {
        LimboError::Corrupt("AUTO_INCREMENT table has no durable database identity".to_string())
    })?;
    if metadata.database_id.into_bytes() != expected_database_identity {
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
        LimboError::Corrupt("AUTO_INCREMENT table has an invalid durable definition".to_string())
    })?;
    let key = AutoIncrementKey::new(metadata.allocator_id.into_bytes()).map_err(|_| {
        LimboError::Corrupt("AUTO_INCREMENT table has an invalid allocator identity".to_string())
    })?;
    Ok(Some(AutoIncrementTable {
        name: definition.table_name.clone(),
        definition: Arc::new(definition),
        key,
        stored_sql: Arc::from(sql),
    }))
}

/// How many numbers MySQL takes for `rows` rows copied by one
/// `INSERT ... SELECT`: batches of 1, 2, 4 and on, each twice the one before,
/// until one reaches 65535, which is where they stay.
fn numbers_spent_on_copied_rows(rows: u64) -> u64 {
    let (mut spent, mut batch) = (0_u64, 1_u64);
    while spent < rows {
        spent = spent.saturating_add(batch);
        batch = next_batch_of_copied_rows(batch);
    }
    spent
}

/// How many numbers a copy takes in the batch after one of `batch`.
fn next_batch_of_copied_rows(batch: u64) -> u64 {
    (batch * 2).min(65535)
}

/// The numbers one copy has taken from a table's counter and not yet spent
/// on a row it wrote.
#[derive(Debug)]
struct NumbersInBatches {
    /// The next number a row would take, and the last of the batch it is in.
    unused: Option<(u64, u64)>,
    /// How many numbers the next batch takes.
    batch: u64,
}

impl Default for NumbersInBatches {
    fn default() -> Self {
        Self {
            unused: None,
            batch: 1,
        }
    }
}

impl NumbersInBatches {
    fn next_unused(&self) -> Option<u64> {
        self.unused
            .filter(|(next, last)| next <= last)
            .map(|(next, _)| next)
    }

    fn next_batch(&self) -> u64 {
        self.batch
    }

    /// Takes the batch the counter handed out from `first` and answers its
    /// first number.
    fn take_batch(&mut self, first: u64) -> u64 {
        self.unused = Some((first, first + self.batch - 1));
        self.batch = next_batch_of_copied_rows(self.batch);
        first
    }

    /// Spends `id` on a row written, the next row taking the number after it.
    fn spend(&mut self, id: u64) {
        let (next, last) = self
            .unused
            .expect("a number is spent only out of a batch taken");
        assert_eq!(next, id, "a copy spends its numbers in order");
        self.unused = Some((next + 1, last));
    }
}

/// One number the counter handed out, as the value bound into its column.
///
/// The engine's integers stop at `i64::MAX`, so a `BIGINT UNSIGNED` number
/// past it is bound as the text that column reads.
fn counted_id_value(
    table: &AutoIncrementTable,
    id: u64,
) -> std::result::Result<Value, MySqlQueryError> {
    if table.definition.allocator_column_type
        == turso_mysql_parser::MySqlIntegerType::BigIntUnsigned
        && id > i64::MAX as u64
    {
        return Ok(Value::from_text(id.to_string()));
    }
    i64::try_from(id).map(Value::from_i64).map_err(|_| {
        MySqlQueryError::Engine(LimboError::Constraint(
            "AUTO_INCREMENT value is outside engine integer range".to_string(),
        ))
    })
}

fn injected_auto_increment_prepare_options(
    table: &AutoIncrementTable,
    statement: Stmt,
) -> PrepareOptions {
    PrepareOptions::default()
        .with_reprepare_parser(Arc::new(FrozenInjectedAutoIncrementInsertParser {
            statement,
        }))
        .with_assignment_validator(Arc::new(CountedTableAssignmentValidator {
            table_name: table.name.clone(),
            table_sql: table.stored_sql.to_string(),
            allocator_column_ordinal: table.definition.allocator_column_ordinal,
        }))
}

const BEGIN_THE_COUNTED_ROWS_TRANSACTION: &str = "BEGIN";
const COMMIT_THE_COUNTED_ROWS_TRANSACTION: &str = "COMMIT";
const ROLL_BACK_THE_COUNTED_ROWS_TRANSACTION: &str = "ROLLBACK";
const SET_THE_COUNTED_ROWS_SAVEPOINT: &str = "SAVEPOINT \"__turso_auto_increment_values\"";
const RELEASE_THE_COUNTED_ROWS_SAVEPOINT: &str =
    "RELEASE SAVEPOINT \"__turso_auto_increment_values\"";
const ROLL_BACK_TO_THE_COUNTED_ROWS_SAVEPOINT: &str =
    "ROLLBACK TO SAVEPOINT \"__turso_auto_increment_values\"";

/// Whether MySQL finds `error` while it fills a row, before the row takes a
/// number: a value it cannot hold, a NULL for a `NOT NULL` column, a broken
/// `CHECK`. Anything else — a key, a foreign key — it finds once the row is
/// written, having taken the number.
fn found_before_a_number_is_taken(error: &LimboError) -> bool {
    match error {
        LimboError::NotNullConstraint { .. } | LimboError::Assignment(_) => true,
        LimboError::Constraint(message) => message.starts_with("CHECK constraint failed"),
        _ => false,
    }
}

fn the_batch_fits_the_column(table: &AutoIncrementTable, high_water: u64, rows: u64) -> bool {
    high_water
        .checked_add(rows)
        .is_some_and(|last| last <= auto_increment_ceiling(table) && last <= i64::MAX as u64)
}

fn generated_ceiling(table: &AutoIncrementTable) -> u64 {
    if table.definition.allocator_column_type
        == turso_mysql_parser::MySqlIntegerType::BigIntUnsigned
    {
        u64::MAX - 2
    } else {
        auto_increment_ceiling(table)
    }
}

fn when_the_counter_is_free_within<T>(
    allocator: &DurableRangeAllocator,
    busy_timeout: Duration,
    mut step: impl FnMut() -> Result<T>,
) -> Result<T> {
    const LONGEST_WAIT_BEFORE_TRYING_AGAIN: Duration = Duration::from_millis(1);
    let deadline = std::time::Instant::now().checked_add(busy_timeout);
    loop {
        let finished_before = allocator.operations_finished();
        match step() {
            Err(LimboError::Busy)
                if deadline.is_some_and(|deadline| std::time::Instant::now() < deadline) =>
            {
                if allocator
                    .wait_for_an_operation_to_finish(
                        finished_before,
                        LONGEST_WAIT_BEFORE_TRYING_AGAIN,
                    )
                    .is_none()
                {
                    std::thread::sleep(LONGEST_WAIT_BEFORE_TRYING_AGAIN);
                }
            }
            answer => return answer,
        }
    }
}

type TakeNumbersBeforeWriting = Box<dyn FnOnce() -> Result<()> + Send + Sync>;

/// One counted table and the id an INSERT that wrote its own reports.
/// One name written the way the engine's own parser reads one.
fn sqlite_quoted(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

/// The `ON UPDATE CURRENT_TIMESTAMP` columns an upsert's clause does not
/// assign, each with the places of a second it keeps.
fn moments_the_clause_leaves(
    columns: &[MySqlColumnMetadata],
    assigned: &[String],
) -> Vec<(String, u8)> {
    columns
        .iter()
        .filter(|column| {
            column.extra().contains("on update CURRENT_TIMESTAMP")
                && !assigned
                    .iter()
                    .any(|assigned| assigned.eq_ignore_ascii_case(column.name()))
        })
        .map(|column| {
            (
                column.name().to_owned(),
                column.temporal_precision().unwrap_or(0),
            )
        })
        .collect()
}

struct WrittenAutoIncrementIds {
    table: AutoIncrementTable,
    reported_id: u64,
    raised_once_written: Option<u64>,
}

struct ReservedAutoIncrementRows {
    ids: Vec<Option<u64>>,
    bound_values: Vec<Value>,
    first_generated: Option<u64>,
    last_explicit: Option<u64>,
}

/// What one row of a counted upsert written row by row came to: a row it
/// wrote under the id it took, or the row already there it met, by the
/// engine's number for it.
enum RowAnUpsertMet {
    Written(u64),
    Skipped(u64),
    Matched(i64),
}

struct CountedTableAssignmentValidator {
    table_name: String,
    table_sql: String,
    allocator_column_ordinal: usize,
}

impl AssignmentValidator for CountedTableAssignmentValidator {
    fn check_assignment(
        &self,
        table_name: &str,
        table_sql: Option<&str>,
        operation: AssignmentOperation,
        values: &[Value],
    ) -> Result<Option<Vec<Value>>> {
        // An upsert's update half reaches this as an `Update`, the statement
        // being one `INSERT` either way.
        if !table_name.eq_ignore_ascii_case(&self.table_name)
            || table_sql != Some(self.table_sql.as_str())
        {
            return Err(LimboError::Corrupt(
                "a counted table's insert reached a different table or schema".to_string(),
            ));
        }
        crate::dialect::check_mysql_assignment(
            table_name,
            table_sql,
            operation,
            values,
            Some(self.allocator_column_ordinal),
        )
    }
}

/// Numbers each row a trigger writes into a counted table from that table's
/// own counter, one number at a time.
///
/// Measured on MySQL 8.4.11: a trigger's `INSERT` into a counted table takes
/// the next number when it runs, so a row an upsert changes or `IGNORE` skips
/// spends nothing there, and the number is never the one the statement
/// reports or `LAST_INSERT_ID()` answers.
struct CountedTriggerRowSupplier {
    allocator: DurableRangeAllocator,
    io: Arc<dyn IO>,
    database_identity: Option<[u8; 16]>,
}

impl TriggerRowidSupplier for CountedTriggerRowSupplier {
    fn next_rowid(&self, _table_name: &str, table_sql: Option<&str>) -> Result<Option<i64>> {
        let Some(table_sql) = table_sql else {
            return Ok(None);
        };
        let Some(table) = counted_table_from_stored_sql(table_sql, self.database_identity)? else {
            return Ok(None);
        };
        // The id of a `BIGINT UNSIGNED` counted table is a column of its own
        // rather than the row number, which is all the engine lets this choose.
        if table.definition.allocator_column_type
            == turso_mysql_parser::MySqlIntegerType::BigIntUnsigned
        {
            return Err(LimboError::ParseError(
                "a trigger writing a BIGINT UNSIGNED AUTO_INCREMENT table is unsupported"
                    .to_string(),
            ));
        }
        let mut reservation = self.allocator.reserve(table.key, 1)?;
        let range = self.io.block(|| reservation.step())?;
        if range.first() > auto_increment_ceiling(&table) {
            return Err(LimboError::Constraint(
                "AUTO_INCREMENT value is outside the column's type".to_string(),
            ));
        }
        i64::try_from(range.first())
            .map(Some)
            .map_err(|_| LimboError::IntegerOverflow)
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

/// The index MySQL knows by `name` on `table`, other than the primary key.
///
/// The engine keeps a key over a column that is not its row number in an
/// index of its own, which MySQL calls `PRIMARY` and which carries no other
/// name: a `UNIQUE` over the key's own columns is a second index beside it,
/// named after its first column like any other.
pub(crate) fn the_index_named<'a>(
    schema: &'a turso_core::schema::Schema,
    table: &str,
    name: &str,
) -> Option<&'a Arc<turso_core::schema::Index>> {
    let primary_key = schema
        .get_btree_table(table)
        .map(|btree| btree.primary_key_columns.clone())
        .unwrap_or_default();
    schema.get_indices(table).find(|index| {
        !is_the_primary_keys_own_index(index, &primary_key)
            && mysql_index_name(index).eq_ignore_ascii_case(name)
    })
}

/// Whether `index` is the one the engine made for the table's primary key.
pub(crate) fn is_the_primary_keys_own_index(
    index: &turso_core::schema::Index,
    primary_key: &[(String, turso_parser::ast::SortOrder)],
) -> bool {
    index.name.starts_with("sqlite_autoindex_")
        && index.columns.len() == primary_key.len()
        && index
            .columns
            .iter()
            .zip(primary_key)
            .all(|(column, (key, _))| column.name.eq_ignore_ascii_case(key))
}

/// The name MySQL gives an index.
///
/// An index the engine created for an inline UNIQUE carries a generated
/// `sqlite_autoindex_` name; MySQL names such an index after its first column.
pub(crate) fn mysql_index_name(index: &turso_core::schema::Index) -> String {
    if let Some(name) = logical_mysql_index_name(&index.name) {
        return name;
    }
    if index.name.starts_with("sqlite_autoindex_") {
        if let Some(first) = index.columns.first() {
            return first.name.clone();
        }
    }
    index.name.clone()
}

/// One stored `CREATE INDEX`, as the MySQL statement that makes the same index
/// under the name `to`.
fn create_index_under_another_name(sql: &str, to: &str, mode: SessionSqlMode) -> Result<String> {
    let mut statement =
        parse_schema_ddl_ast(sql, mode).map_err(|error| LimboError::Corrupt(error.to_string()))?;
    let Stmt::CreateIndex { idx_name, .. } = &mut statement else {
        return Err(LimboError::Corrupt(
            "stored index SQL did not describe an index".to_string(),
        ));
    };
    idx_name.name = turso_parser::ast::Name::exact(to.to_owned());
    render_create_index_mysql_with_mode(&statement, mode)
        .map_err(|error| LimboError::Corrupt(error.to_string()))
}

struct WrittenForeignKey {
    name: Option<String>,
    child_columns: Vec<String>,
    parent_column_count: usize,
}

#[derive(Debug, PartialEq, Eq)]
enum StoredAs {
    Integer { bytes: u8, unsigned: bool },
    Date,
    Bytes,
    Float,
    Double,
    Text { collation: Option<&'static str> },
    Other(String),
}

impl StoredAs {
    fn of(column: &MySqlColumnMetadata) -> Self {
        let type_name = column.type_name().to_ascii_uppercase();
        let unsigned = type_name.ends_with(" UNSIGNED");
        let base = type_name.trim_end_matches(" UNSIGNED");
        let integer = |bytes| StoredAs::Integer { bytes, unsigned };
        match base {
            "TINYINT" | "BOOLEAN" | "BOOL" => integer(1),
            "SMALLINT" => integer(2),
            "MEDIUMINT" => integer(3),
            "INT" | "INTEGER" => integer(4),
            "BIGINT" => integer(8),
            "YEAR" => StoredAs::Integer {
                bytes: 1,
                unsigned: true,
            },
            "DATE" => StoredAs::Date,
            "DATETIME" | "TIMESTAMP" | "TIME" | "DECIMAL" | "NUMERIC" | "BINARY" | "VARBINARY" => {
                StoredAs::Bytes
            }
            "FLOAT" => StoredAs::Float,
            "DOUBLE" | "REAL" => StoredAs::Double,
            "CHAR" | "VARCHAR" | "TEXT" | "TINYTEXT" | "MEDIUMTEXT" | "LONGTEXT" => {
                StoredAs::Text {
                    collation: column.collation_name(),
                }
            }
            _ if base.starts_with("BIT") => StoredAs::Bytes,
            _ => {
                if let Some(members) = turso_mysql_parser::set_members(column.type_name()) {
                    return StoredAs::Integer {
                        bytes: match members.len() {
                            0..=8 => 1,
                            9..=16 => 2,
                            17..=24 => 3,
                            25..=32 => 4,
                            _ => 8,
                        },
                        unsigned: true,
                    };
                }
                if let Some(members) = turso_mysql_parser::enum_members(column.type_name()) {
                    return StoredAs::Integer {
                        bytes: if members.len() <= 255 { 1 } else { 2 },
                        unsigned: true,
                    };
                }
                StoredAs::Other(base.to_owned())
            }
        }
    }
}

fn primary_key_covers_columns(
    primary: &[(String, turso_parser::ast::SortOrder)],
    columns: &[String],
) -> bool {
    primary.len() >= columns.len()
        && primary
            .iter()
            .zip(columns)
            .all(|((name, _), column)| name.eq_ignore_ascii_case(column))
}

fn index_covers_columns(
    index: &turso_core::schema::Index,
    primary_key: &[(String, turso_parser::ast::SortOrder)],
    columns: &[String],
) -> bool {
    let shown = mysql_index_columns(index, primary_key);
    shown.len() >= columns.len()
        && shown
            .iter()
            .zip(columns)
            .all(|(indexed, column)| indexed.name.eq_ignore_ascii_case(column))
}

pub(crate) fn mysql_index_columns<'a>(
    index: &'a turso_core::schema::Index,
    primary_key: &[(String, turso_parser::ast::SortOrder)],
) -> &'a [turso_core::schema::IndexColumn] {
    if !stored_index_kind(&index.name).is_some_and(|kind| kind.ends_with_primary_key) {
        return &index.columns;
    }
    let shown = index
        .columns
        .len()
        .checked_sub(primary_key.len())
        .filter(|shown| *shown > 0 && !primary_key.is_empty())
        .unwrap_or_else(|| {
            panic!(
                "index {} ends with the primary key but has {} columns for a key of {}",
                index.name,
                index.columns.len(),
                primary_key.len()
            )
        });
    assert!(
        index.columns[shown..]
            .iter()
            .zip(primary_key)
            .all(|(column, (key, _))| column.name.eq_ignore_ascii_case(key)),
        "index {} does not end with its table's primary key",
        index.name
    );
    &index.columns[..shown]
}

fn primary_key_a_plain_index_ends_with(
    schema: &turso_core::schema::Schema,
    table: &str,
) -> Vec<String> {
    let Some(btree) = schema.get_btree_table(table) else {
        return Vec::new();
    };
    if btree.get_rowid_alias_column().is_some() {
        return Vec::new();
    }
    btree
        .primary_key_columns
        .iter()
        .map(|(name, order)| {
            assert_eq!(
                *order,
                turso_parser::ast::SortOrder::Asc,
                "a MySQL primary key column is kept in ascending order"
            );
            let (_, column) = btree
                .get_column(name)
                .unwrap_or_else(|| panic!("primary key column {name} is not in its table"));
            column
                .name
                .clone()
                .unwrap_or_else(|| panic!("primary key column {name} has no name"))
        })
        .collect()
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct StoredIndexKind {
    implicit: bool,
    ends_with_primary_key: bool,
}

const MYSQL_INDEX_STORAGE_PREFIXES: [(&str, StoredIndexKind); 4] = [
    (
        "__turso_mysql_index_namespace_v1__",
        StoredIndexKind {
            implicit: false,
            ends_with_primary_key: false,
        },
    ),
    (
        "__turso_mysql_implicit_index_namespace_v1__",
        StoredIndexKind {
            implicit: true,
            ends_with_primary_key: false,
        },
    ),
    (
        "__turso_mysql_index_ending_with_primary_key_v1__",
        StoredIndexKind {
            implicit: false,
            ends_with_primary_key: true,
        },
    ),
    (
        "__turso_mysql_implicit_index_ending_with_primary_key_v1__",
        StoredIndexKind {
            implicit: true,
            ends_with_primary_key: true,
        },
    ),
];

fn physical_mysql_index_name(logical_name: &str, kind: StoredIndexKind) -> Result<String> {
    let identity = new_allocator_identity()?;
    let (prefix, _) = MYSQL_INDEX_STORAGE_PREFIXES
        .iter()
        .find(|(_, prefix_kind)| *prefix_kind == kind)
        .expect("every kind of stored index has a prefix");
    let mut name =
        String::with_capacity(prefix.len() + (identity.len() + logical_name.len()) * 2 + 1);
    name.push_str(prefix);
    for byte in identity {
        push_hex_byte(&mut name, byte);
    }
    name.push('_');
    for byte in logical_name.bytes() {
        push_hex_byte(&mut name, byte);
    }
    Ok(name)
}

fn is_implicit_index(stored_name: &str) -> bool {
    stored_index_kind(stored_name).is_some_and(|kind| kind.implicit)
}

fn stored_index_kind(stored_name: &str) -> Option<StoredIndexKind> {
    MYSQL_INDEX_STORAGE_PREFIXES
        .iter()
        .find(|(prefix, _)| stored_name.starts_with(prefix))
        .map(|(_, kind)| *kind)
}

fn logical_mysql_index_name(stored_name: &str) -> Option<String> {
    let suffix = MYSQL_INDEX_STORAGE_PREFIXES
        .iter()
        .find_map(|(prefix, _)| stored_name.strip_prefix(prefix))?;
    let (identity, logical) = suffix.split_at_checked(32)?;
    if !identity.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return None;
    }
    let logical = logical.strip_prefix('_')?;
    if logical.len() % 2 != 0 {
        return None;
    }
    let bytes = logical
        .as_bytes()
        .chunks_exact(2)
        .map(|pair| {
            let high = (pair[0] as char).to_digit(16)? as u8;
            let low = (pair[1] as char).to_digit(16)? as u8;
            Some((high << 4) | low)
        })
        .collect::<Option<Vec<_>>>()?;
    String::from_utf8(bytes).ok()
}

fn push_hex_byte(out: &mut String, byte: u8) {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    out.push(char::from(HEX[usize::from(byte >> 4)]));
    out.push(char::from(HEX[usize::from(byte & 15)]));
}

fn checked_insert_target(statement: &Stmt) -> Result<Option<CheckedInsertTarget>> {
    let Stmt::Insert {
        or_conflict,
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
    let ignores = matches!(or_conflict, Some(turso_parser::ast::ResolveType::Ignore));
    match body {
        InsertBody::DefaultValues => Ok(Some(CheckedInsertTarget::DefaultValues(table))),
        // The upsert clause changes what happens to a row that collides, not
        // what the row being offered is, so the required-column check reads the
        // same VALUES either way.
        InsertBody::Select(select, _) if !columns.is_empty() => {
            // An `INSERT ... SELECT` has no rows to look at here. The column
            // list is still checked against the table's required columns; what
            // is skipped is the per-row NULL rule, which needs values MySQL
            // only learns when the SELECT runs.
            let OneSelect::Values(values) = &select.body.select else {
                return Ok(Some(CheckedInsertTarget::Listed(ListedInsert {
                    table,
                    ignores,
                    columns: columns
                        .iter()
                        .map(|name| name.as_str().to_owned())
                        .collect(),
                    rows: Vec::new(),
                })));
            };
            Ok(Some(CheckedInsertTarget::Listed(ListedInsert {
                table,
                ignores,
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

const UPSERT_REFUSED: MySqlParseError = MySqlParseError::Unsupported {
    feature: "an upsert comparing a column of the offered row this does not compare as MySQL does",
};

fn uses_session_local_clock(sql: &str) -> bool {
    sql.split(|character: char| !character.is_ascii_alphanumeric() && character != '_')
        .any(|word| {
            [
                "NOW",
                "CURDATE",
                "CURTIME",
                "CURRENT_DATE",
                "CURRENT_TIME",
                "CURRENT_TIMESTAMP",
                "LOCALTIME",
                "LOCALTIMESTAMP",
                "SYSDATE",
                "UTC_TIMESTAMP",
                "UTC_DATE",
                "UTC_TIME",
                "FROM_UNIXTIME",
                "UNIX_TIMESTAMP",
            ]
            .iter()
            .any(|function| word.eq_ignore_ascii_case(function))
        })
}

/// Writes one column of the table a `CREATE TABLE ... AS SELECT` makes.
///
/// Measured on MySQL 8.4.11: the copy keeps the type, the `NOT NULL` and the
/// `DEFAULT`, and loses the keys and the `AUTO_INCREMENT`. What replaces a
/// dropped `AUTO_INCREMENT` is a zero default — `id int NOT NULL AUTO_INCREMENT
/// PRIMARY KEY` copies as `id int NOT NULL DEFAULT '0'`, and a plain `a int NOT
/// NULL` copies with no default at all.
///
/// Answers `None` for a string `DEFAULT`, whose escaping this does not decide.
fn copied_column_declaration(name: &str, column: &MySqlColumnMetadata) -> Option<String> {
    let mut rendered = format!("{} {}", mysql_quoted(name), copied_column_type(column));
    if let Some(
        collation @ ("utf8mb4_bin" | "utf8mb4_unicode_ci" | "utf8mb3_unicode_ci"
        | "utf8mb4_general_ci"),
    ) = column.collation_name()
    {
        rendered.push_str(" COLLATE ");
        rendered.push_str(collation);
    }
    if !column.nullable() {
        rendered.push_str(" NOT NULL");
    }
    if column.extra() == "AUTO_INCREMENT" {
        rendered.push_str(" DEFAULT 0");
        return Some(rendered);
    }
    match column.default_value() {
        None => {}
        Some(MySqlColumnDefault::Null) => rendered.push_str(" DEFAULT NULL"),
        Some(MySqlColumnDefault::Integer { text, .. }) => {
            rendered.push_str(" DEFAULT ");
            rendered.push_str(text);
        }
        Some(MySqlColumnDefault::Number(text)) => {
            rendered.push_str(" DEFAULT ");
            rendered.push_str(text);
        }
        Some(MySqlColumnDefault::Boolean(value)) => {
            rendered.push_str(if *value {
                " DEFAULT TRUE"
            } else {
                " DEFAULT FALSE"
            });
        }
        // MySQL prints this one without quotes, it naming a moment rather
        // than holding a value.
        Some(MySqlColumnDefault::Moment) => {
            rendered.push_str(" DEFAULT ");
            rendered.push_str(&crate::show_create_table::the_moment(
                column.temporal_precision(),
            ));
        }
        // What a copy of an expression default writes has not been measured.
        Some(MySqlColumnDefault::Text(_) | MySqlColumnDefault::MomentCall) => return None,
    }
    Some(rendered)
}

/// Writes a copied column's type, which is the stored MySQL name and whatever
/// count or precision that name carries.
fn copied_column_type(column: &MySqlColumnMetadata) -> String {
    if let Some(precision) = column.temporal_precision() {
        return if precision == 0 {
            column.type_name().to_owned()
        } else {
            format!("{}({precision})", column.type_name())
        };
    }
    if let Some((precision, scale)) = column.decimal_size() {
        return format!("{}({precision},{scale})", column.type_name());
    }
    match column.character_length() {
        Some(length) => format!("{}({length})", column.type_name()),
        None => column.type_name().to_owned(),
    }
}

fn mysql_quoted(identifier: &str) -> String {
    format!("`{}`", identifier.replace('`', "``"))
}

/// Names an index the statement left unnamed.
///
/// Measured on MySQL 8.4.11: the name is the first column's, and where that is
/// taken it gains `_2`, `_3` and so on until one is free. The names it counts
/// as taken are every one the table already carries plus every one this
/// statement has named so far, in the order they were written — measured,
/// `KEY (a), KEY a_2 (b), KEY (a)` names the three `a`, `a_2` and `a_3`, and a
/// later explicit name that collides answers 1061 rather than moving aside.
fn unnamed_index_name(held: &[String], columns: &[String]) -> Option<String> {
    let first = columns.first()?;
    let taken = |candidate: &str| held.iter().any(|name| name.eq_ignore_ascii_case(candidate));
    if !taken(first) {
        return Some(first.clone());
    }
    (2..=u32::MAX)
        .map(|suffix| format!("{first}_{suffix}"))
        .find(|candidate| !taken(candidate))
}

/// Carries a transaction-control failure into the index-`ALTER` error type.
fn alter_table_index_query_error(error: MySqlQueryError) -> MySqlAlterTableIndexError {
    match error {
        MySqlQueryError::Engine(error) => MySqlAlterTableIndexError::Engine(error),
        other => MySqlAlterTableIndexError::Engine(LimboError::InternalError(other.to_string())),
    }
}

/// Reads the engine's missing-savepoint failure as the one MySQL answers.
///
/// The engine reports it as a transaction error whose message names the
/// savepoint (`core/vdbe/execute.rs`), and nothing else in the error carries
/// that fact, so the message is what tells this failure from another.
fn no_such_savepoint_error(error: MySqlQueryError) -> MySqlQueryError {
    match &error {
        MySqlQueryError::Engine(LimboError::TxError(message))
            if message.starts_with("no such savepoint") =>
        {
            MySqlQueryError::NoSuchSavepoint
        }
        _ => error,
    }
}

fn mysql_query_parse_error(error: MySqlParseError) -> MySqlQueryError {
    match error {
        MySqlParseError::JsonLiteralDefault => MySqlQueryError::JsonLiteralDefault,
        MySqlParseError::JsonIndex => MySqlQueryError::JsonIndex,
        MySqlParseError::Unsupported { .. } => MySqlQueryError::Unsupported(error.to_string()),
        _ => MySqlQueryError::Syntax(error.to_string()),
    }
}

fn mysql_query_index_error(error: MySqlAlterTableIndexError) -> MySqlQueryError {
    match error {
        MySqlAlterTableIndexError::MissingTable => MySqlQueryError::MissingTable,
        MySqlAlterTableIndexError::MissingIndex
        | MySqlAlterTableIndexError::MissingIndexToRename => MySqlQueryError::MissingIndex,
        MySqlAlterTableIndexError::DuplicateIndex => MySqlQueryError::DuplicateIndex,
        error @ MySqlAlterTableIndexError::RenamingAColumnsOwnKey => {
            MySqlQueryError::Unsupported(error.to_string())
        }
        MySqlAlterTableIndexError::JsonIndex => MySqlQueryError::JsonIndex,
        MySqlAlterTableIndexError::RequiredByForeignKey => MySqlQueryError::RequiredByForeignKey,
        MySqlAlterTableIndexError::Engine(error) => MySqlQueryError::Engine(error),
    }
}

impl ReprepareParser for FrozenInjectedAutoIncrementInsertParser {
    fn parse(&self, sql: &str, _context: &ReprepareContext<'_>) -> Result<(Option<Cmd>, usize)> {
        Ok((Some(Cmd::Stmt(self.statement.clone())), sql.len()))
    }
}

struct FrozenTransactionStatementParser {
    statement: Stmt,
}

impl ReprepareParser for FrozenTransactionStatementParser {
    fn parse(&self, sql: &str, _context: &ReprepareContext<'_>) -> Result<(Option<Cmd>, usize)> {
        Ok((Some(Cmd::Stmt(self.statement.clone())), sql.len()))
    }
}

impl ReprepareParser for FrozenDmlParser {
    fn parse(&self, sql: &str, context: &ReprepareContext<'_>) -> Result<(Option<Cmd>, usize)> {
        if let Some((name, definition)) = &self.table_definition {
            let current = context
                .schema
                .get_btree_table(name)
                .ok_or(LimboError::SchemaUpdated)?;
            if &current.to_sql() != definition {
                return Err(LimboError::TableDefinitionChanged(name.clone()));
            }
        }
        if self.untracked_read_source {
            return Err(LimboError::TableDefinitionChanged(String::new()));
        }
        for (name, definition) in &self.read_table_definitions {
            let current = context
                .schema
                .get_btree_table(name)
                .ok_or(LimboError::SchemaUpdated)?;
            if &current.to_sql() != definition {
                return Err(LimboError::TableDefinitionChanged(name.clone()));
            }
        }
        if let Some(statement) = &self.typed_copy {
            return Ok((Some(Cmd::Stmt(statement.clone())), sql.len()));
        }
        let translated = turso_mysql_parser::parse_dml_knowing_column_types(
            sql,
            self.mode,
            &self.column_types.rewritten_on_update,
            &self.column_types.decimal,
            &self.column_types.integer,
            &self.column_types.text,
        )
        .map_err(|error| LimboError::ParseError(error.to_string()))?;
        validate_dml_comparison_columns(context.schema, &translated)?;
        validate_dml_ordered_columns_with_schema(context.schema, &translated)?;
        if let Some(statement) = &self.shifted_timestamp_insert {
            return Ok((Some(Cmd::Stmt(statement.clone())), sql.len()));
        }
        let stmt = translated
            .parse_ast()
            .map_err(|error| LimboError::ParseError(error.to_string()))?;
        Ok((Some(Cmd::Stmt(stmt)), sql.len()))
    }
}

impl ReprepareParser for FrozenSelectParser {
    fn parse(&self, sql: &str, context: &ReprepareContext<'_>) -> Result<(Option<Cmd>, usize)> {
        if !self.source_is_catalog {
            validate_frozen_select_comparison_columns(
                context.schema,
                self.source_table.as_deref(),
                &self.checked_comparisons,
            )?;
        }
        if self.untracked_source {
            return Err(LimboError::TableDefinitionChanged(String::new()));
        }
        for (name, columns) in &self.source_columns {
            let current = context
                .schema
                .get_btree_table(name)
                .ok_or(LimboError::SchemaUpdated)?;
            let current_columns = current
                .columns()
                .iter()
                .map(|column| format!("{column:?}"))
                .collect::<Vec<_>>();
            if !current_columns.starts_with(columns) {
                return Err(LimboError::TableDefinitionChanged(name.clone()));
            }
        }
        if let Some(statement) = &self.typed_statement {
            for (name, definition) in &self.table_definitions {
                let current = context
                    .schema
                    .get_btree_table(name)
                    .ok_or(LimboError::SchemaUpdated)?;
                if &current.to_sql() != definition {
                    return Err(LimboError::TableDefinitionChanged(name.clone()));
                }
            }
            return Ok((Some(Cmd::Stmt(statement.clone())), sql.len()));
        }
        let translated = parse_select(sql, self.mode)
            .map_err(|error| LimboError::ParseError(error.to_string()))?;
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

impl ReprepareParser for FrozenRowidKeyDdlParser {
    fn parse(&self, sql: &str, _context: &ReprepareContext<'_>) -> Result<(Option<Cmd>, usize)> {
        let statement = parse_checked_primary_key_create_table(sql, self.mode)
            .and_then(|checked| {
                turso_mysql_parser::with_the_primary_key_as_the_rowid(checked.sqlite_statement)
            })
            .map_err(|error| LimboError::ParseError(error.to_string()))?;
        Ok((Some(Cmd::Stmt(statement)), sql.len()))
    }
}

/// Stores a new table keyed by its rowid under the v5 envelope, which is
/// what tells the table apart from one whose key is an index.
struct RowidKeySchemaSqlFormatter {
    context: SchemaSqlSessionContext,
    normalized_mysql_ddl: String,
    sqlite_statement: Stmt,
}

impl SchemaSqlFormatter for RowidKeySchemaSqlFormatter {
    fn format_schema_sql(&self, kind: SchemaSqlKind, input: &str, stmt: &Stmt) -> Result<String> {
        if kind != SchemaSqlKind::Table {
            return self.context.format_schema_sql(kind, input, stmt);
        }
        if input != self.normalized_mysql_ddl || stmt != &self.sqlite_statement {
            return Err(LimboError::InternalError(
                "rowid key schema formatter received a different statement".to_string(),
            ));
        }
        crate::schema_sql::encode_schema_sql_v5(
            self.context.for_kind(SchemaSqlKind::Table),
            &self.normalized_mysql_ddl,
        )
        .map_err(|error| LimboError::InternalError(error.to_string()))
    }

    fn format_rewritten_schema_sql(
        &self,
        kind: SchemaSqlKind,
        previous_sql: &str,
        stmt: &Stmt,
    ) -> Result<String> {
        self.context
            .format_rewritten_schema_sql(kind, previous_sql, stmt)
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
        encode_schema_sql_v3(
            self.context.for_kind(SchemaSqlKind::Table),
            Some(self.metadata),
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

/// A moment between two durable steps of a statement, where a test looks at
/// what a crash right then would leave on disk.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CrashPoint {
    /// A schema change has just committed, and the statement has not yet
    /// returned.
    SchemaChangeCommitted,
}

#[cfg(test)]
type CrashPointWatcher = Box<dyn FnMut(CrashPoint)>;

#[cfg(test)]
thread_local! {
    static CRASH_POINT_WATCHER: std::cell::RefCell<Option<CrashPointWatcher>> =
        const { std::cell::RefCell::new(None) };
}

/// Lets `watcher` see every crash point this thread reaches until the
/// answer is dropped.
#[cfg(test)]
pub(crate) fn watch_crash_points(watcher: impl FnMut(CrashPoint) + 'static) -> CrashPointWatch {
    CRASH_POINT_WATCHER.with(|slot| *slot.borrow_mut() = Some(Box::new(watcher)));
    CrashPointWatch
}

#[cfg(test)]
pub(crate) struct CrashPointWatch;

#[cfg(test)]
impl Drop for CrashPointWatch {
    fn drop(&mut self) {
        CRASH_POINT_WATCHER.with(|slot| slot.borrow_mut().take());
    }
}

fn crash_point(point: CrashPoint) {
    #[cfg(test)]
    CRASH_POINT_WATCHER.with(|slot| {
        if let Some(watcher) = slot.borrow_mut().as_mut() {
            watcher(point);
        }
    });
    #[cfg(not(test))]
    let _ = point;
}

#[cfg(test)]
mod tests;
