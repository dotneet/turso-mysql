//! MySQL frontend for Turso.

#[cfg(unix)]
#[cfg_attr(not(test), allow(dead_code))]
mod database_catalog;
#[allow(dead_code)]
mod database_open;
#[cfg_attr(not(test), allow(dead_code))]
mod database_registry;
mod dialect;
pub mod schema_sql;
mod session;

#[cfg(unix)]
pub use database_catalog::{
    canonicalize_database_name, MySqlAdminCommandError, MySqlAdminCommandResult,
    MySqlDatabaseCatalog, MySqlDatabaseError, MySqlDatabaseSession,
};
pub use dialect::MySqlDialect;
pub use session::{
    MySqlColumnDefault,
    MySqlAffectedRowsMode, MySqlColumnKey, MySqlColumnMetadata, MySqlColumnMetadataError,
    MySqlConnection, MySqlPreparedExecutionResult, MySqlPreparedResultColumn,
    MySqlPreparedResultColumnTypeMetadata,
    MySqlPreparedResultRow, MySqlPreparedResultRows, MySqlPreparedStatementError,
    MySqlPreparedStatementMetadata, MySqlPreparedValue, MySqlQueryError, MySqlTable,
    MySqlTableKind, MySqlWriteResult,
};
#[cfg(unix)]
pub use turso_mysql_parser::MySqlAdminCommand;
