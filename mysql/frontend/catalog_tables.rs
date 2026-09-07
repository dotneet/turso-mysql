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

use crate::session::mysql_index_name;
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

/// The name the engine knows `information_schema.STATISTICS` by.
pub(crate) const INFORMATION_SCHEMA_STATISTICS: &str = "mysql_information_schema_statistics";

/// The name the engine knows `information_schema.KEY_COLUMN_USAGE` by.
pub(crate) const INFORMATION_SCHEMA_KEY_COLUMN_USAGE: &str =
    "mysql_information_schema_key_column_usage";

/// Registers every `information_schema` table on one logical database.
///
/// A database is opened once and acquired many times, and registering mutates
/// the schema every connection shares — a second registration on a database
/// that already has live connections changes it underneath them, which they
/// read as a schema they cannot use. So one that is already there is left
/// alone.
pub(crate) fn register_catalog_tables(database: &Database, name: &str) -> Result<()> {
    if !database.has_table(INFORMATION_SCHEMA_TABLES) {
        database.register_internal_vtab(InformationSchemaTables {
            database: name.to_owned(),
        })?;
    }
    if !database.has_table(INFORMATION_SCHEMA_STATISTICS) {
        database.register_internal_vtab(InformationSchemaStatistics {
            database: name.to_owned(),
        })?;
    }
    if !database.has_table(INFORMATION_SCHEMA_KEY_COLUMN_USAGE) {
        database.register_internal_vtab(InformationSchemaKeyColumnUsage {
            database: name.to_owned(),
        })?;
    }
    Ok(())
}

/// Reports that a scan takes no constraint of its own.
///
/// Each of these tables builds every row when its cursor opens, so there is
/// nothing to gain by narrowing the scan here: the engine applies the `WHERE`
/// and the `ORDER BY` itself.
fn catalog_best_index(
    constraints: &[turso_ext::ConstraintInfo],
) -> std::result::Result<turso_ext::IndexInfo, turso_ext::ResultCode> {
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
                // A session sees the tables it is allowed to select from and
                // no others, which is what the catalog it replaces did.
                || !connection.mysql_table_is_visible(name)
            {
                continue;
            }
            rows.push((name.clone(), "BASE TABLE"));
        }
        for name in schema.views.keys() {
            if is_system_table(name)
                || is_internal_table(name)
                || !connection.mysql_table_is_visible(name)
            {
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
        catalog_best_index(constraints)
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

/// `information_schema.STATISTICS`, one row per column of every index.
///
/// The engine keeps a rowid-alias primary key without an index of its own, so
/// the primary key is read off the table and everything else off the indexes,
/// which is what `SHOW INDEX` does.
#[derive(Debug)]
struct InformationSchemaStatistics {
    database: String,
}

impl InternalVirtualTable for InformationSchemaStatistics {
    fn name(&self) -> String {
        INFORMATION_SCHEMA_STATISTICS.to_owned()
    }

    fn sql(&self) -> String {
        format!(
            "CREATE TABLE {INFORMATION_SCHEMA_STATISTICS} \
             (TABLE_CATALOG TEXT, TABLE_SCHEMA TEXT, TABLE_NAME TEXT, NON_UNIQUE INTEGER, \
             INDEX_SCHEMA TEXT, INDEX_NAME TEXT, SEQ_IN_INDEX INTEGER, COLUMN_NAME TEXT, \
             COLLATION TEXT, SUB_PART INTEGER, PACKED TEXT, NULLABLE TEXT, \
             INDEX_TYPE TEXT, COMMENT TEXT, INDEX_COMMENT TEXT, IS_VISIBLE TEXT, \
             EXPRESSION TEXT)"
        )
    }

    fn open(
        &self,
        connection: Arc<Connection>,
    ) -> Result<Arc<RwLock<dyn InternalVirtualTableCursor>>> {
        let schema = connection.current_schema();
        let mut rows = Vec::new();
        for (name, table) in &schema.tables {
            let Some(btree) = table.btree() else {
                continue;
            };
            if is_system_table(name)
                || is_internal_table(name)
                || !connection.mysql_table_is_visible(name)
            {
                continue;
            }
            // Measured on MySQL 8.4.11: a nullable indexed column reports
            // `YES`, and one declared NOT NULL reports the empty string.
            let nullable = |column_name: &str| match btree.columns().iter().find(|column| {
                column
                    .name
                    .as_deref()
                    .is_some_and(|column| column.eq_ignore_ascii_case(column_name))
            }) {
                Some(column) if column.notnull() => "",
                _ => "YES",
            };

            for (position, (column_name, _)) in btree.primary_key_columns.iter().enumerate() {
                rows.push(StatisticsRow {
                    table: name.clone(),
                    non_unique: 0,
                    index_name: "PRIMARY".to_owned(),
                    sequence: position as i64 + 1,
                    column_name: column_name.clone(),
                    nullable: nullable(column_name),
                });
            }
            for index in schema.get_indices(name) {
                // The engine's own index behind a primary key is already
                // reported, under the name MySQL gives it.
                if index
                    .columns
                    .iter()
                    .map(|column| column.name.as_str())
                    .eq(btree
                        .primary_key_columns
                        .iter()
                        .map(|(column, _)| column.as_str()))
                {
                    continue;
                }
                let index_name = mysql_index_name(index);
                for (position, column) in index.columns.iter().enumerate() {
                    rows.push(StatisticsRow {
                        table: name.clone(),
                        non_unique: i64::from(!index.unique),
                        index_name: index_name.clone(),
                        sequence: position as i64 + 1,
                        column_name: column.name.clone(),
                        nullable: nullable(&column.name),
                    });
                }
            }
        }
        // A scan with nothing to order it by answers in the order the ORDER BY
        // a client writes over this table almost always asks for.
        rows.sort_by(|left, right| {
            (&left.table, &left.index_name, left.sequence).cmp(&(
                &right.table,
                &right.index_name,
                right.sequence,
            ))
        });
        Ok(Arc::new(RwLock::new(InformationSchemaStatisticsCursor {
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
        catalog_best_index(constraints)
    }
}

/// One column of one index, which is one row of `information_schema.STATISTICS`.
struct StatisticsRow {
    table: String,
    non_unique: i64,
    index_name: String,
    sequence: i64,
    column_name: String,
    nullable: &'static str,
}

struct InformationSchemaStatisticsCursor {
    database: String,
    rows: Vec<StatisticsRow>,
    position: i64,
}

impl InternalVirtualTableCursor for InformationSchemaStatisticsCursor {
    fn next(&mut self) -> std::result::Result<bool, LimboError> {
        self.position += 1;
        Ok((self.position as usize) < self.rows.len())
    }

    fn rowid(&self) -> i64 {
        self.position
    }

    fn column(&self, column: usize) -> std::result::Result<Value, LimboError> {
        let row = &self.rows[self.position as usize];
        Ok(match column {
            // Measured on MySQL 8.4.11: every catalog is `def`, an index is
            // always a `BTREE` sorted ascending and always visible, and
            // neither it nor its columns carry a comment. A prefix length, a
            // packing and an expression belong to index kinds this does not
            // create, so all three are NULL.
            0 => Value::build_text("def"),
            1 => Value::build_text(self.database.clone()),
            2 => Value::build_text(row.table.clone()),
            3 => Value::from_i64(row.non_unique),
            4 => Value::build_text(self.database.clone()),
            5 => Value::build_text(row.index_name.clone()),
            6 => Value::from_i64(row.sequence),
            7 => Value::build_text(row.column_name.clone()),
            8 => Value::build_text("A"),
            9 => Value::Null,
            10 => Value::Null,
            11 => Value::build_text(row.nullable),
            12 => Value::build_text("BTREE"),
            13 => Value::build_text(""),
            14 => Value::build_text(""),
            15 => Value::build_text("YES"),
            16 => Value::Null,
            _ => {
                return Err(LimboError::InternalError(format!(
                    "information_schema.STATISTICS has no column {column}"
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

/// `information_schema.KEY_COLUMN_USAGE`, one row per column of every key that
/// constrains a value: the primary key, the unique keys and the foreign keys.
///
/// Measured on MySQL 8.4.11: a plain index is not a constraint and has no row
/// here, which is what separates this table from `STATISTICS`.
#[derive(Debug)]
struct InformationSchemaKeyColumnUsage {
    database: String,
}

impl InternalVirtualTable for InformationSchemaKeyColumnUsage {
    fn name(&self) -> String {
        INFORMATION_SCHEMA_KEY_COLUMN_USAGE.to_owned()
    }

    fn sql(&self) -> String {
        format!(
            "CREATE TABLE {INFORMATION_SCHEMA_KEY_COLUMN_USAGE} \
             (CONSTRAINT_CATALOG TEXT, CONSTRAINT_SCHEMA TEXT, CONSTRAINT_NAME TEXT, \
             TABLE_CATALOG TEXT, TABLE_SCHEMA TEXT, TABLE_NAME TEXT, COLUMN_NAME TEXT, \
             ORDINAL_POSITION INTEGER, POSITION_IN_UNIQUE_CONSTRAINT INTEGER, \
             REFERENCED_TABLE_SCHEMA TEXT, REFERENCED_TABLE_NAME TEXT, \
             REFERENCED_COLUMN_NAME TEXT)"
        )
    }

    fn open(
        &self,
        connection: Arc<Connection>,
    ) -> Result<Arc<RwLock<dyn InternalVirtualTableCursor>>> {
        let schema = connection.current_schema();
        let mut rows = Vec::new();
        for (name, table) in &schema.tables {
            let Some(btree) = table.btree() else {
                continue;
            };
            if is_system_table(name)
                || is_internal_table(name)
                || !connection.mysql_table_is_visible(name)
            {
                continue;
            }

            for (position, (column_name, _)) in btree.primary_key_columns.iter().enumerate() {
                rows.push(KeyColumnUsageRow {
                    constraint: "PRIMARY".to_owned(),
                    table: name.clone(),
                    column_name: column_name.clone(),
                    ordinal: position as i64 + 1,
                    referenced: None,
                });
            }
            for index in schema.get_indices(name) {
                // A plain index constrains nothing, and the engine's own index
                // behind a primary key is already reported.
                if !index.unique
                    || index
                        .columns
                        .iter()
                        .map(|column| column.name.as_str())
                        .eq(btree
                            .primary_key_columns
                            .iter()
                            .map(|(column, _)| column.as_str()))
                {
                    continue;
                }
                let constraint = mysql_index_name(index);
                for (position, column) in index.columns.iter().enumerate() {
                    rows.push(KeyColumnUsageRow {
                        constraint: constraint.clone(),
                        table: name.clone(),
                        column_name: column.name.clone(),
                        ordinal: position as i64 + 1,
                        referenced: None,
                    });
                }
            }
            for key in &btree.foreign_keys {
                // A key written without a `CONSTRAINT` name is reported under
                // the name MySQL generates for it, which is the same name
                // `SHOW CREATE TABLE` prints.
                let constraint = match &key.name {
                    Some(name) => name.clone(),
                    None => format!("{name}_ibfk_{}", key.decl_order + 1),
                };
                // MySQL writes the parent columns in the constraint, so there
                // is one for each child column; a key stored without them came
                // from no MySQL statement and reports the columns it has.
                let columns = key.child_columns.iter().zip(key.parent_columns.iter());
                for (position, (child, parent)) in columns.enumerate() {
                    rows.push(KeyColumnUsageRow {
                        constraint: constraint.clone(),
                        table: name.clone(),
                        column_name: child.clone(),
                        ordinal: position as i64 + 1,
                        referenced: Some((key.parent_table.clone(), parent.clone())),
                    });
                }
            }
        }
        rows.sort_by(|left, right| {
            (&left.table, &left.constraint, left.ordinal).cmp(&(
                &right.table,
                &right.constraint,
                right.ordinal,
            ))
        });
        Ok(Arc::new(RwLock::new(
            InformationSchemaKeyColumnUsageCursor {
                database: self.database.clone(),
                rows,
                position: -1,
            },
        )))
    }

    fn best_index(
        &self,
        constraints: &[turso_ext::ConstraintInfo],
        _order_by: &[turso_ext::OrderByInfo],
    ) -> std::result::Result<turso_ext::IndexInfo, turso_ext::ResultCode> {
        catalog_best_index(constraints)
    }
}

/// One column of one key, which is one row of
/// `information_schema.KEY_COLUMN_USAGE`.
struct KeyColumnUsageRow {
    constraint: String,
    table: String,
    column_name: String,
    ordinal: i64,
    /// The parent table and column this column points at, for a foreign key.
    /// A primary or unique key points at nothing and leaves MySQL's four
    /// referenced columns NULL.
    referenced: Option<(String, String)>,
}

struct InformationSchemaKeyColumnUsageCursor {
    database: String,
    rows: Vec<KeyColumnUsageRow>,
    position: i64,
}

impl InternalVirtualTableCursor for InformationSchemaKeyColumnUsageCursor {
    fn next(&mut self) -> std::result::Result<bool, LimboError> {
        self.position += 1;
        Ok((self.position as usize) < self.rows.len())
    }

    fn rowid(&self) -> i64 {
        self.position
    }

    fn column(&self, column: usize) -> std::result::Result<Value, LimboError> {
        let row = &self.rows[self.position as usize];
        let referenced_schema = || match &row.referenced {
            Some(_) => Value::build_text(self.database.clone()),
            None => Value::Null,
        };
        Ok(match column {
            0 => Value::build_text("def"),
            1 => Value::build_text(self.database.clone()),
            2 => Value::build_text(row.constraint.clone()),
            3 => Value::build_text("def"),
            4 => Value::build_text(self.database.clone()),
            5 => Value::build_text(row.table.clone()),
            6 => Value::build_text(row.column_name.clone()),
            7 => Value::from_i64(row.ordinal),
            // Measured on MySQL 8.4.11: a foreign key's column points at the
            // column in the same position of the key it references, and a
            // primary or unique key leaves this NULL.
            8 => match &row.referenced {
                Some(_) => Value::from_i64(row.ordinal),
                None => Value::Null,
            },
            9 => referenced_schema(),
            10 => match &row.referenced {
                Some((table, _)) => Value::build_text(table.clone()),
                None => Value::Null,
            },
            11 => match &row.referenced {
                Some((_, column)) => Value::build_text(column.clone()),
                None => Value::Null,
            },
            _ => {
                return Err(LimboError::InternalError(format!(
                    "information_schema.KEY_COLUMN_USAGE has no column {column}"
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
        // A session that has left no list of its own sees every table, which
        // is what an unrestricted one does.
        assert!(connection.mysql_table_is_visible("alpha"));
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

        // A session that may see only some of them sees only those.
        connection.set_mysql_visible_tables(Some(vec!["alpha".to_owned()]));
        assert_eq!(
            read(&format!(
                "SELECT TABLE_NAME FROM {INFORMATION_SCHEMA_TABLES}"
            )),
            vec![vec!["alpha".to_owned()]]
        );
        connection.set_mysql_visible_tables(None);

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

    /// The rows MySQL 8.4.11 answers for the same schema, measured on the
    /// pinned oracle: the primary key first under the name `PRIMARY`, each
    /// index once per column it holds, a unique index reporting `NON_UNIQUE`
    /// zero, and a nullable indexed column reporting `YES`.
    #[test]
    fn the_information_schema_statistics_report_one_row_per_index_column() {
        let io: Arc<dyn turso_core::IO> = Arc::new(MemoryIO::new());
        let database = Database::open_file_with_flags(
            io,
            ":memory:",
            OpenFlags::Create,
            DatabaseOpts::new(),
            None,
            Arc::new(turso_core::SqliteDialect),
        )
        .unwrap();
        register_catalog_tables(&database, "reports").unwrap();

        let connection = database.connect().unwrap();
        for sql in [
            "CREATE TABLE child (cid INTEGER NOT NULL, seq INTEGER NOT NULL, \
             parent_id INTEGER NOT NULL, label TEXT, PRIMARY KEY (cid, seq))",
            "CREATE INDEX idx_label ON child (label)",
            "CREATE UNIQUE INDEX uk_pair ON child (parent_id, seq)",
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
                            Value::Null => "NULL".to_owned(),
                            other => format!("{other}"),
                        })
                        .collect()
                })
                .collect()
        };

        assert_eq!(
            read(&format!(
                "SELECT TABLE_SCHEMA, TABLE_NAME, NON_UNIQUE, INDEX_NAME, SEQ_IN_INDEX, \
                 COLUMN_NAME, NULLABLE, INDEX_TYPE, SUB_PART \
                 FROM {INFORMATION_SCHEMA_STATISTICS}"
            )),
            vec![
                row(&["reports", "child", "0", "PRIMARY", "1", "cid", "", "BTREE", "NULL"]),
                row(&["reports", "child", "0", "PRIMARY", "2", "seq", "", "BTREE", "NULL"]),
                row(&[
                    "reports",
                    "child",
                    "1",
                    "idx_label",
                    "1",
                    "label",
                    "YES",
                    "BTREE",
                    "NULL",
                ]),
                row(&[
                    "reports",
                    "child",
                    "0",
                    "uk_pair",
                    "1",
                    "parent_id",
                    "",
                    "BTREE",
                    "NULL",
                ]),
                row(&["reports", "child", "0", "uk_pair", "2", "seq", "", "BTREE", "NULL"]),
            ]
        );

        // A filter over a numeric column, which the ordinary `SELECT` path
        // answers and no recognizer of written shapes ever could.
        assert_eq!(
            read(&format!(
                "SELECT INDEX_NAME, COLUMN_NAME FROM {INFORMATION_SCHEMA_STATISTICS} \
                 WHERE NON_UNIQUE = 0 AND SEQ_IN_INDEX = 2 ORDER BY INDEX_NAME"
            )),
            vec![row(&["PRIMARY", "seq"]), row(&["uk_pair", "seq"])]
        );

        // A session that may see only some tables reads the indexes of those.
        connection.set_mysql_visible_tables(Some(Vec::new()));
        assert_eq!(
            read(&format!(
                "SELECT TABLE_NAME FROM {INFORMATION_SCHEMA_STATISTICS}"
            )),
            Vec::<Vec<String>>::new()
        );
    }

    /// The rows MySQL 8.4.11 answers for the same schema, measured on the
    /// pinned oracle: the primary key and the unique keys with the four
    /// referenced columns NULL, a foreign key naming the column it points at,
    /// and no row at all for a plain index, which constrains nothing.
    #[test]
    fn the_information_schema_key_column_usage_reports_only_the_keys_that_constrain() {
        let io: Arc<dyn turso_core::IO> = Arc::new(MemoryIO::new());
        let database = Database::open_file_with_flags(
            io,
            ":memory:",
            OpenFlags::Create,
            DatabaseOpts::new(),
            None,
            Arc::new(turso_core::SqliteDialect),
        )
        .unwrap();
        register_catalog_tables(&database, "reports").unwrap();

        let connection = database.connect().unwrap();
        for sql in [
            "CREATE TABLE parent (id INTEGER NOT NULL, code TEXT NOT NULL, PRIMARY KEY (id))",
            "CREATE UNIQUE INDEX uk_code ON parent (code)",
            "CREATE TABLE child (cid INTEGER NOT NULL PRIMARY KEY, parent_id INTEGER NOT NULL, \
             label TEXT, CONSTRAINT fk_child_parent FOREIGN KEY (parent_id) \
             REFERENCES parent (id))",
            "CREATE INDEX idx_label ON child (label)",
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
                            Value::Null => "NULL".to_owned(),
                            other => format!("{other}"),
                        })
                        .collect()
                })
                .collect()
        };

        assert_eq!(
            read(&format!(
                "SELECT TABLE_NAME, CONSTRAINT_NAME, COLUMN_NAME, ORDINAL_POSITION, \
                 POSITION_IN_UNIQUE_CONSTRAINT, REFERENCED_TABLE_SCHEMA, \
                 REFERENCED_TABLE_NAME, REFERENCED_COLUMN_NAME \
                 FROM {INFORMATION_SCHEMA_KEY_COLUMN_USAGE}"
            )),
            vec![
                row(&["child", "PRIMARY", "cid", "1", "NULL", "NULL", "NULL", "NULL"]),
                row(&[
                    "child",
                    "fk_child_parent",
                    "parent_id",
                    "1",
                    "1",
                    "reports",
                    "parent",
                    "id",
                ]),
                row(&["parent", "PRIMARY", "id", "1", "NULL", "NULL", "NULL", "NULL"]),
                row(&["parent", "uk_code", "code", "1", "NULL", "NULL", "NULL", "NULL"]),
            ]
        );

        // A foreign key written without a `CONSTRAINT` name is reported under
        // the name MySQL generates for it.
        connection
            .prepare(
                "CREATE TABLE grandchild (id INTEGER NOT NULL PRIMARY KEY, \
                 child_id INTEGER NOT NULL, FOREIGN KEY (child_id) REFERENCES child (cid))",
            )
            .unwrap()
            .run_ignore_rows()
            .unwrap();
        assert_eq!(
            read(&format!(
                "SELECT CONSTRAINT_NAME, REFERENCED_TABLE_NAME FROM \
                 {INFORMATION_SCHEMA_KEY_COLUMN_USAGE} \
                 WHERE TABLE_NAME = 'grandchild' AND REFERENCED_TABLE_NAME IS NOT NULL"
            )),
            vec![row(&["grandchild_ibfk_1", "child"])]
        );
    }

    fn row(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_owned()).collect()
    }
}
