//! MySQL frontend for Turso.

#[cfg(unix)]
#[cfg_attr(not(test), allow(dead_code))]
mod database_catalog;
#[allow(dead_code)]
mod database_open;
#[cfg_attr(not(test), allow(dead_code))]
mod database_registry;
mod alter_table_indexes;
mod dialect;
mod drop_table;
pub mod schema_sql;
mod session;
mod show_create_table;
mod truncate_table;

#[cfg(unix)]
pub use database_catalog::{
    canonicalize_database_name, MySqlAdminCommandError, MySqlAdminCommandResult,
    MySqlDatabaseCatalog, MySqlDatabaseError, MySqlDatabaseSession,
};
pub use alter_table_indexes::MySqlAlterTableIndexError;
pub use dialect::MySqlDialect;
pub use drop_table::{MySqlDropTableError, MySqlDropTableResult};
pub use session::{
    MySqlColumnDefault, MySqlDropViewError, MySqlIndexEntry, MySqlMarkerType,
    MySqlShowCreateTableError,
    MySqlShowCreateTableResult, ParameterMarker,
    MySqlAffectedRowsMode, MySqlColumnKey, MySqlColumnMetadata, MySqlColumnMetadataError,
    MySqlConnection, MySqlPreparedExecutionResult, MySqlPreparedResultColumn,
    MySqlPreparedResultColumnTypeMetadata,
    MySqlPreparedResultRow, MySqlPreparedResultRows, MySqlPreparedStatementError,
    MySqlPreparedStatementAuthority, MySqlPreparedStatementAuthorityError,
    MySqlPreparedStatementMetadata, MySqlPreparedValue, MySqlQueryError, MySqlTable,
    MySqlTableKind, MySqlWriteResult,
    DEFAULT_MAX_PREPARED_STMT_COUNT, MAX_PREPARED_STMT_COUNT,
};
pub use truncate_table::MySqlTruncateTableError;
#[cfg(unix)]
pub use turso_mysql_parser::MySqlAdminCommand;
