//! `information_schema` as tables the engine can scan.
//!
//! MySQL answers an `information_schema` query with its ordinary query engine,
//! so a client may name any columns it likes, filter on any of them, order by
//! any of them and join them together. Recognizing one written shape per table,
//! which is what this frontend did before, cannot answer that — so each of
//! these is registered as a table the engine scans, and the ordinary `SELECT`
//! path does the rest.
//!
//! One database is one of these, so the logical database name is fixed when the
//! table is registered rather than read out of the engine, which has no notion
//! of one.

use std::sync::Arc;

use parking_lot::RwLock;
use turso_core::{
    schema::is_system_table, Connection, Database, InternalVirtualTable,
    InternalVirtualTableCursor, LimboError, Result, Value,
};

/// The name the engine knows `information_schema.TABLES` by.
///
/// The engine has no schema-qualified names, so the qualifier is spelled into
/// the name and the `SELECT` renderer writes this where a query wrote
/// `information_schema.TABLES`.
pub(crate) const INFORMATION_SCHEMA_TABLES: &str = "mysql_information_schema_tables";

/// Registers every `information_schema` table on one logical database.
pub(crate) fn register_catalog_tables(database: &Database, name: &str) -> Result<()> {
    database.register_internal_vtab(InformationSchemaTables {
        database: name.to_owned(),
    })?;
    Ok(())
}

/// `information_schema.TABLES`, holding the three columns this answers.
#[derive(Debug)]
struct InformationSchemaTables {
    database: String,
}

impl InternalVirtualTable for InformationSchemaTables {
    fn name(&self) -> String {
        INFORMATION_SCHEMA_TABLES.to_owned()
    }

    fn sql(&self) -> String {
        format!(
            "CREATE TABLE {INFORMATION_SCHEMA_TABLES} \
             (TABLE_SCHEMA TEXT, TABLE_NAME TEXT, TABLE_TYPE TEXT)"
        )
    }

    fn open(
        &self,
        connection: Arc<Connection>,
    ) -> Result<Arc<RwLock<dyn InternalVirtualTableCursor>>> {
        let schema = connection.current_schema();
        // The rows are read out of the schema the connection already holds
        // rather than by running a statement of its own: a cursor is opened in
        // the middle of the statement that is scanning it.
        let mut rows = Vec::new();
        for (name, table) in &schema.tables {
            // The schema also holds every table-valued function the engine
            // registers — `pragma_table_info`, `json_each` and the rest — and
            // these tables themselves. None of those is a table MySQL has.
            if !matches!(table.as_ref(), turso_core::schema::Table::BTree(_))
                || is_system_table(name)
                || is_internal_table(name)
            {
                continue;
            }
            rows.push((name.clone(), "BASE TABLE"));
        }
        for name in schema.views.keys() {
            if is_system_table(name) || is_internal_table(name) {
                continue;
            }
            rows.push((name.clone(), "VIEW"));
        }
        // A scan with nothing to order it by answers in name order, which is
        // what a client that leaves the ORDER BY off is most likely reading.
        rows.sort();
        Ok(Arc::new(RwLock::new(InformationSchemaTablesCursor {
            database: self.database.clone(),
            rows,
            position: -1,
        })))
    }

    fn best_index(
        &self,
        constraints: &[turso_ext::ConstraintInfo],
        _order_by: &[turso_ext::OrderByInfo],
    ) -> std::result::Result<turso_ext::IndexInfo, turso_ext::ResultCode> {
        // Every row is built up front, so nothing is gained by taking a
        // constraint here: the engine applies them all itself.
        Ok(turso_ext::IndexInfo {
            idx_num: 0,
            idx_str: None,
            order_by_consumed: false,
            estimated_cost: 1.0,
            estimated_rows: 32,
            constraint_usages: constraints
                .iter()
                .map(|_| turso_ext::ConstraintUsage {
                    argv_index: None,
                    omit: false,
                })
                .collect(),
        })
    }
}

/// Answers whether a name is one of the tables this frontend keeps for itself.
///
/// `is_system_table` covers the engine's own; these are the sidecars the MySQL
/// catalog writes, which a client must not be told about.
fn is_internal_table(name: &str) -> bool {
    name.to_lowercase().starts_with("__turso_internal_")
}

struct InformationSchemaTablesCursor {
    database: String,
    rows: Vec<(String, &'static str)>,
    position: i64,
}

impl InternalVirtualTableCursor for InformationSchemaTablesCursor {
    fn next(&mut self) -> std::result::Result<bool, LimboError> {
        self.position += 1;
        Ok((self.position as usize) < self.rows.len())
    }

    fn rowid(&self) -> i64 {
        self.position
    }

    fn column(&self, column: usize) -> std::result::Result<Value, LimboError> {
        let (name, kind) = &self.rows[self.position as usize];
        Ok(match column {
            0 => Value::build_text(self.database.clone()),
            1 => Value::build_text(name.clone()),
            2 => Value::build_text((*kind).to_owned()),
            _ => {
                return Err(LimboError::InternalError(format!(
                    "information_schema.TABLES has no column {column}"
                )))
            }
        })
    }

    fn filter(
        &mut self,
        _args: &[Value],
        _idx_str: Option<String>,
        _idx_num: i32,
    ) -> std::result::Result<bool, LimboError> {
        self.position = -1;
        self.next()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use turso_core::{Database, DatabaseOpts, MemoryIO, OpenFlags};

    /// The point of registering these as tables the engine scans: the ordinary
    /// query path answers them, so a query may name any columns it likes, in
    /// any order, filter on any of them and order by any of them — none of
    /// which the shape-matching catalog could do.
    #[test]
    fn the_information_schema_tables_are_scanned_by_the_ordinary_query_path() {
        let io: Arc<dyn turso_core::IO> = Arc::new(MemoryIO::new());
        let database = Database::open_file_with_flags(
            io,
            ":memory:",
            OpenFlags::Create,
            DatabaseOpts::new().with_views(true),
            None,
            Arc::new(turso_core::SqliteDialect),
        )
        .unwrap();
        register_catalog_tables(&database, "reports").unwrap();

        let connection = database.connect().unwrap();
        for sql in [
            "CREATE TABLE beta (id INTEGER)",
            "CREATE TABLE alpha (id INTEGER)",
            "CREATE VIEW gamma AS SELECT id FROM alpha",
        ] {
            connection.prepare(sql).unwrap().run_ignore_rows().unwrap();
        }

        let read = |sql: &str| -> Vec<Vec<String>> {
            connection
                .prepare(sql)
                .unwrap()
                .run_collect_rows()
                .unwrap()
                .into_iter()
                .map(|row| {
                    row.iter()
                        .map(|value| match value {
                            Value::Text(text) => text.as_str().to_owned(),
                            other => format!("{other:?}"),
                        })
                        .collect()
                })
                .collect()
        };

        // Every column, in declaration order.
        assert_eq!(
            read(&format!(
                "SELECT TABLE_SCHEMA, TABLE_NAME, TABLE_TYPE FROM {INFORMATION_SCHEMA_TABLES}"
            )),
            vec![
                vec![
                    "reports".to_owned(),
                    "alpha".to_owned(),
                    "BASE TABLE".to_owned()
                ],
                vec![
                    "reports".to_owned(),
                    "beta".to_owned(),
                    "BASE TABLE".to_owned()
                ],
                vec!["reports".to_owned(), "gamma".to_owned(), "VIEW".to_owned()],
            ]
        );

        // Fewer columns, in another order — which the shape-matching catalog
        // had to be taught one shape at a time.
        assert_eq!(
            read(&format!(
                "SELECT TABLE_TYPE, TABLE_NAME FROM {INFORMATION_SCHEMA_TABLES} \
                 ORDER BY TABLE_NAME DESC"
            )),
            vec![
                vec!["VIEW".to_owned(), "gamma".to_owned()],
                vec!["BASE TABLE".to_owned(), "beta".to_owned()],
                vec!["BASE TABLE".to_owned(), "alpha".to_owned()],
            ]
        );

        // A WHERE over any column at all, which is the whole reason for this.
        assert_eq!(
            read(&format!(
                "SELECT TABLE_NAME FROM {INFORMATION_SCHEMA_TABLES} \
                 WHERE TABLE_TYPE = 'BASE TABLE' AND TABLE_NAME LIKE 'a%'"
            )),
            vec![vec!["alpha".to_owned()]]
        );

        // The tables the engine and this frontend keep for themselves are not
        // listed.
        let listed = read(&format!(
            "SELECT TABLE_NAME FROM {INFORMATION_SCHEMA_TABLES}"
        ));
        assert!(
            listed
                .iter()
                .all(|row| !row[0].starts_with("sqlite_") && !row[0].starts_with("__turso")),
            "{listed:?}"
        );
    }
}
