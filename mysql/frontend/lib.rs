//! MySQL frontend for Turso.

#[allow(dead_code)]
mod database_open;
#[cfg_attr(not(test), allow(dead_code))]
mod database_registry;
mod dialect;
pub mod schema_sql;
mod session;

pub use dialect::MySqlDialect;
pub use session::{MySqlConnection, MySqlQueryError};
