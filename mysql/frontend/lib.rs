//! MySQL frontend for Turso.

mod alter_table_indexes;
mod call_state;
mod catalog_tables;
mod create_table_as_select;
#[cfg(unix)]
#[cfg_attr(not(test), allow(dead_code))]
mod database_catalog;
#[allow(dead_code)]
mod database_open;
#[cfg_attr(not(test), allow(dead_code))]
mod database_registry;
#[cfg_attr(not(unix), allow(dead_code))]
mod database_users;
mod dialect;
mod drop_table;
mod found_rows;
mod group_concat;
pub mod named_locks;
pub mod schema_sql;
mod session;
pub mod session_registry;
pub mod show_create_table;
mod temporal_zone;
mod truncate_table;
mod wal_keeper;

pub use alter_table_indexes::MySqlAlterTableIndexError;
pub use create_table_as_select::MySqlCreateTableAsSelectError;
#[cfg(unix)]
pub use database_catalog::{
    canonicalize_database_name, MySqlAdminCommandError, MySqlAdminCommandResult,
    MySqlDatabaseCatalog, MySqlDatabaseError, MySqlDatabaseSession,
};
pub use database_users::{
    MySqlDatabaseDropped, MySqlStatementNotStarted, DEFAULT_METADATA_LOCK_WAIT,
};
pub use dialect::MySqlDialect;
pub use drop_table::{MySqlDropTableError, MySqlDropTableResult};
pub use group_concat::{DEFAULT_GROUP_CONCAT_MAX_LEN, GROUP_CONCAT_CUT_ERROR};
pub use session::{
    MySqlAffectedRowsMode, MySqlColumnDefault, MySqlColumnKey, MySqlColumnMetadata,
    MySqlColumnMetadataError, MySqlConnection, MySqlDropViewError, MySqlIndexEntry,
    MySqlIsolationLevel, MySqlMarkerType, MySqlPreparedExecutionResult, MySqlPreparedResultColumn,
    MySqlPreparedResultColumnTypeMetadata, MySqlPreparedResultRow, MySqlPreparedResultRows,
    MySqlPreparedStatementAuthority, MySqlPreparedStatementAuthorityError,
    MySqlPreparedStatementError, MySqlPreparedStatementMetadata, MySqlPreparedStatementPlace,
    MySqlPreparedValue, MySqlQueryError, MySqlRenameTableError, MySqlReplaceViewError,
    MySqlShowCreateTableError, MySqlShowCreateTableResult, MySqlSkippedView, MySqlTable,
    MySqlTableKind, MySqlTransactionOutcome, MySqlTriggerMetadata, MySqlViewMetadata,
    MySqlWriteResult, ParameterMarker, DEFAULT_MAX_PREPARED_STMT_COUNT, MAX_PREPARED_STMT_COUNT,
};
pub use temporal_zone::shift_timestamp;
pub use truncate_table::MySqlTruncateTableError;
#[cfg(unix)]
pub use turso_mysql_parser::MySqlAdminCommand;
