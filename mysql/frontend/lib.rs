//! MySQL frontend for Turso.

mod alter_table_indexes;
mod catalog_tables;
mod create_table_as_select;
#[cfg(unix)]
#[cfg_attr(not(test), allow(dead_code))]
mod database_catalog;
#[allow(dead_code)]
mod database_open;
#[cfg_attr(not(test), allow(dead_code))]
mod database_registry;
mod dialect;
mod drop_table;
pub mod schema_sql;
mod session;
pub mod show_create_table;
mod temporal_zone;
mod truncate_table;

pub use alter_table_indexes::MySqlAlterTableIndexError;
pub use create_table_as_select::MySqlCreateTableAsSelectError;
#[cfg(unix)]
pub use database_catalog::{
    canonicalize_database_name, MySqlAdminCommandError, MySqlAdminCommandResult,
    MySqlDatabaseCatalog, MySqlDatabaseError, MySqlDatabaseSession,
};
pub use dialect::MySqlDialect;
pub use drop_table::{MySqlDropTableError, MySqlDropTableResult};
pub use session::{
    MySqlAffectedRowsMode, MySqlColumnDefault, MySqlColumnKey, MySqlColumnMetadata,
    MySqlColumnMetadataError, MySqlConnection, MySqlDropViewError, MySqlIndexEntry,
    MySqlIsolationLevel, MySqlMarkerType, MySqlPreparedExecutionResult, MySqlPreparedResultColumn,
    MySqlPreparedResultColumnTypeMetadata, MySqlPreparedResultRow, MySqlPreparedResultRows,
    MySqlPreparedStatementAuthority, MySqlPreparedStatementAuthorityError,
    MySqlPreparedStatementError, MySqlPreparedStatementMetadata, MySqlPreparedValue,
    MySqlQueryError, MySqlShowCreateTableError, MySqlShowCreateTableResult, MySqlTable,
    MySqlTableKind, MySqlTransactionOutcome, MySqlTriggerMetadata, MySqlViewMetadata,
    MySqlWriteResult, ParameterMarker, DEFAULT_MAX_PREPARED_STMT_COUNT, MAX_PREPARED_STMT_COUNT,
};
pub use temporal_zone::shift_timestamp;
pub use truncate_table::MySqlTruncateTableError;
#[cfg(unix)]
pub use turso_mysql_parser::MySqlAdminCommand;
