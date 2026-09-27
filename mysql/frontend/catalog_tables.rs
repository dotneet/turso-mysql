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
pub(crate) const INFORMATION_SCHEMA_VIEWS: &str = "mysql_information_schema_views";

/// The name the engine knows `information_schema.STATISTICS` by.
pub(crate) const INFORMATION_SCHEMA_STATISTICS: &str = "mysql_information_schema_statistics";

/// The name the engine knows `information_schema.KEY_COLUMN_USAGE` by.
pub(crate) const INFORMATION_SCHEMA_KEY_COLUMN_USAGE: &str =
    "mysql_information_schema_key_column_usage";

/// The name the engine knows `information_schema.TABLE_CONSTRAINTS` by.
pub(crate) const INFORMATION_SCHEMA_TABLE_CONSTRAINTS: &str =
    "mysql_information_schema_table_constraints";

/// The name the engine knows `information_schema.REFERENTIAL_CONSTRAINTS` by.
pub(crate) const INFORMATION_SCHEMA_REFERENTIAL_CONSTRAINTS: &str =
    "mysql_information_schema_referential_constraints";

/// The name the engine knows `information_schema.ROUTINES` by.
pub(crate) const INFORMATION_SCHEMA_ROUTINES: &str = "mysql_information_schema_routines";

/// The name the engine knows `information_schema.CHECK_CONSTRAINTS` by.
pub(crate) const INFORMATION_SCHEMA_CHECK_CONSTRAINTS: &str =
    "mysql_information_schema_check_constraints";

/// The name the engine knows `information_schema.COLUMNS` by.
pub(crate) const INFORMATION_SCHEMA_COLUMNS: &str = "mysql_information_schema_columns";

/// The name the engine knows `information_schema.SCHEMATA` by.
pub(crate) const INFORMATION_SCHEMA_SCHEMATA: &str = "mysql_information_schema_schemata";

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
    if !database.has_table(INFORMATION_SCHEMA_VIEWS) {
        database.register_internal_vtab(InformationSchemaViews {
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
    if !database.has_table(INFORMATION_SCHEMA_TABLE_CONSTRAINTS) {
        database.register_internal_vtab(InformationSchemaTableConstraints {
            database: name.to_owned(),
        })?;
    }
    if !database.has_table(INFORMATION_SCHEMA_REFERENTIAL_CONSTRAINTS) {
        database.register_internal_vtab(InformationSchemaReferentialConstraints {
            database: name.to_owned(),
        })?;
    }
    if !database.has_table(INFORMATION_SCHEMA_ROUTINES) {
        database.register_internal_vtab(InformationSchemaRoutines)?;
    }
    if !database.has_table(INFORMATION_SCHEMA_CHECK_CONSTRAINTS) {
        database.register_internal_vtab(InformationSchemaCheckConstraints {
            database: name.to_owned(),
        })?;
    }
    if !database.has_table(INFORMATION_SCHEMA_COLUMNS) {
        database.register_internal_vtab(SessionCatalogTable {
            name: INFORMATION_SCHEMA_COLUMNS,
            mysql_name: "COLUMNS",
            // The database name is compared as it was written, the way MySQL
            // compares it: measured on MySQL 8.4.11, `TABLE_SCHEMA =
            // 'TURSO_ORACLE'` answers nothing where `'turso_oracle'` answers
            // the columns. A table or column name is not, this frontend
            // folding the case of every table name it is given.
            columns: &[
                "TABLE_CATALOG TEXT",
                "TABLE_SCHEMA TEXT",
                "TABLE_NAME TEXT COLLATE MYSQL_UCA9_AI_CI",
                "COLUMN_NAME TEXT COLLATE MYSQL_UCA9_AI_CI",
                "ORDINAL_POSITION INTEGER",
                "COLUMN_DEFAULT TEXT",
                "IS_NULLABLE TEXT COLLATE MYSQL_UCA9_AI_CI",
                "DATA_TYPE TEXT COLLATE MYSQL_UCA9_AI_CI",
                "CHARACTER_MAXIMUM_LENGTH INTEGER",
                "CHARACTER_OCTET_LENGTH INTEGER",
                "NUMERIC_PRECISION INTEGER",
                "NUMERIC_SCALE INTEGER",
                "DATETIME_PRECISION INTEGER",
                "CHARACTER_SET_NAME TEXT COLLATE MYSQL_UCA9_AI_CI",
                "COLLATION_NAME TEXT COLLATE MYSQL_UCA9_AI_CI",
                "COLUMN_TYPE TEXT COLLATE MYSQL_UCA9_AI_CI",
                "COLUMN_KEY TEXT COLLATE MYSQL_UCA9_AI_CI",
                "EXTRA TEXT COLLATE MYSQL_UCA9_AI_CI",
                "PRIVILEGES TEXT COLLATE MYSQL_UCA9_AI_CI",
                "COLUMN_COMMENT TEXT COLLATE MYSQL_UCA9_AI_CI",
                "GENERATION_EXPRESSION TEXT COLLATE MYSQL_UCA9_AI_CI",
                "SRS_ID INTEGER",
            ],
        })?;
    }
    if !database.has_table(INFORMATION_SCHEMA_SCHEMATA) {
        database.register_internal_vtab(SessionCatalogTable {
            name: INFORMATION_SCHEMA_SCHEMATA,
            mysql_name: "SCHEMATA",
            // A database name is compared as it was written, as for COLUMNS.
            columns: &[
                "CATALOG_NAME TEXT",
                "SCHEMA_NAME TEXT",
                "DEFAULT_CHARACTER_SET_NAME TEXT COLLATE MYSQL_UCA9_AI_CI",
                "DEFAULT_COLLATION_NAME TEXT COLLATE MYSQL_UCA9_AI_CI",
                "SQL_PATH TEXT",
                "DEFAULT_ENCRYPTION TEXT COLLATE MYSQL_UCA9_AI_CI",
            ],
        })?;
    }
    Ok(())
}

/// The indexes of one table, leaving out the one the engine keeps behind a
/// primary key.
///
/// That one is already reported under the name `PRIMARY`, read off the table
/// rather than off the indexes — which is the only place a rowid-alias primary
/// key, which has no index at all, can be read from.
pub(crate) fn indexes_beside_the_primary_key<'a>(
    schema: &'a turso_core::schema::Schema,
    table: &str,
    btree: &turso_core::schema::BTreeTable,
) -> Vec<&'a Arc<turso_core::schema::Index>> {
    let primary = btree
        .primary_key_columns
        .iter()
        .map(|(column, _)| column.as_str())
        .collect::<Vec<_>>();
    schema
        .get_indices(table)
        .filter(|index| {
            !same_columns(
                index.columns.iter().map(|column| column.name.as_str()),
                primary.iter().copied(),
            )
        })
        .collect()
}

/// Reports whether two lists name the same columns in the same order.
///
/// MySQL reads a column name without regard to case, so two spellings of one
/// name are one column.
fn same_columns<'a>(
    left: impl Iterator<Item = &'a str>,
    right: impl Iterator<Item = &'a str>,
) -> bool {
    left.map(str::to_lowercase).eq(right.map(str::to_lowercase))
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

/// `information_schema.TABLES`, holding the columns this answers.
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
             (TABLE_SCHEMA TEXT COLLATE MYSQL_UCA9_AI_CI, \
             TABLE_NAME TEXT COLLATE MYSQL_UCA9_AI_CI, \
             TABLE_TYPE TEXT COLLATE MYSQL_UCA9_AI_CI, \
             ENGINE TEXT COLLATE MYSQL_UCA9_AI_CI, \
             DATA_LENGTH INTEGER, \
             INDEX_LENGTH INTEGER, \
             TABLE_COLLATION TEXT COLLATE MYSQL_UCA9_AI_CI, \
             TABLE_COMMENT TEXT COLLATE MYSQL_UCA9_AI_CI)"
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
            let options = match schema.table_sql(name) {
                Some(stored) => crate::schema_sql::stored_table_options(stored)
                    .map_err(|error| LimboError::Corrupt(error.to_string()))?,
                None => Default::default(),
            };
            rows.push((
                name.clone(),
                "BASE TABLE",
                Some(options.collation.name()),
                options.comment.unwrap_or_default(),
            ));
        }
        for name in schema.views.keys() {
            if is_system_table(name)
                || is_internal_table(name)
                || !connection.mysql_table_is_visible(name)
            {
                continue;
            }
            rows.push((name.clone(), "VIEW", None, "VIEW".to_owned()));
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
    /// Each table's name, kind, collation and comment.
    rows: Vec<(String, &'static str, Option<&'static str>, String)>,
    position: i64,
}

/// The view attributes used by schema dump clients.
#[derive(Debug)]
struct InformationSchemaViews {
    database: String,
}

impl InternalVirtualTable for InformationSchemaViews {
    fn name(&self) -> String {
        INFORMATION_SCHEMA_VIEWS.to_owned()
    }

    fn sql(&self) -> String {
        format!(
            "CREATE TABLE {INFORMATION_SCHEMA_VIEWS} (\
             TABLE_CATALOG TEXT COLLATE MYSQL_UCA9_AI_CI, \
             TABLE_SCHEMA TEXT COLLATE MYSQL_UCA9_AI_CI, \
             TABLE_NAME TEXT COLLATE MYSQL_UCA9_AI_CI, \
             VIEW_DEFINITION TEXT COLLATE MYSQL_UCA9_AI_CI, \
             CHECK_OPTION TEXT COLLATE MYSQL_UCA9_AI_CI, \
             IS_UPDATABLE TEXT COLLATE MYSQL_UCA9_AI_CI, \
             DEFINER TEXT COLLATE MYSQL_UCA9_AI_CI, \
             SECURITY_TYPE TEXT COLLATE MYSQL_UCA9_AI_CI, \
             CHARACTER_SET_CLIENT TEXT COLLATE MYSQL_UCA9_AI_CI, \
             COLLATION_CONNECTION TEXT COLLATE MYSQL_UCA9_AI_CI)"
        )
    }

    fn open(
        &self,
        connection: Arc<Connection>,
    ) -> Result<Arc<RwLock<dyn InternalVirtualTableCursor>>> {
        let schema = connection.current_schema();
        let mut rows = Vec::new();
        for (name, view) in &schema.views {
            if is_system_table(name)
                || is_internal_table(name)
                || !connection.mysql_table_is_visible(name)
            {
                continue;
            }
            let decoded = crate::schema_sql::decode_persisted_schema_sql(
                turso_core::SchemaSqlKind::View,
                &view.sql,
            )?
            .ok_or_else(|| LimboError::ParseError("view has no MySQL metadata".into()))?;
            let creator = decoded
                .creator()
                .map_err(|error| LimboError::Corrupt(error.to_string()))?
                .ok_or_else(|| LimboError::ParseError("view has no creator metadata".into()))?;
            let mode = turso_mysql_parser::SessionSqlMode {
                ansi_quotes: decoded.context.sql_mode.ansi_quotes,
                no_backslash_escapes: decoded.context.sql_mode.no_backslash_escapes,
            };
            let definition = turso_mysql_parser::parse_schema_ddl_ast(decoded.normalized_ddl, mode)
                .ok()
                .and_then(|statement| {
                    if turso_mysql_parser::translated_view_is_kept_as_mysql_prints_it(&statement) {
                        turso_mysql_parser::written_view_definition(
                            decoded.normalized_ddl,
                            mode,
                            &self.database,
                        )
                    } else {
                        turso_mysql_parser::render_view_definition_mysql(
                            &statement,
                            &self.database,
                            &|table, column| stored_column_name(&schema, table, column),
                        )
                    }
                    .ok()
                });
            rows.push(ViewRow {
                name: name.clone(),
                creator,
                definition,
            });
        }
        rows.sort_unstable_by(|left, right| left.name.cmp(&right.name));
        Ok(Arc::new(RwLock::new(InformationSchemaViewsCursor {
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

/// How `table` spells `column`, when a table of exactly that name has it.
fn stored_column_name(
    schema: &turso_core::schema::Schema,
    table: &str,
    column: &str,
) -> Option<String> {
    let btree = schema.get_btree_table(table)?;
    if btree.name != table {
        return None;
    }
    btree
        .columns()
        .iter()
        .filter_map(|stored| stored.name.as_deref())
        .find(|stored| stored.eq_ignore_ascii_case(column))
        .map(str::to_owned)
}

/// One view, which is one row of `information_schema.VIEWS`.
struct ViewRow {
    name: String,
    creator: crate::schema_sql::SchemaSqlCreator,
    /// The view's definition as MySQL writes it back, and whether it can be
    /// updated through, where this knows how MySQL writes it.
    definition: Option<(String, bool)>,
}

struct InformationSchemaViewsCursor {
    database: String,
    rows: Vec<ViewRow>,
    position: i64,
}

impl InternalVirtualTableCursor for InformationSchemaViewsCursor {
    fn next(&mut self) -> std::result::Result<bool, LimboError> {
        self.position += 1;
        Ok((self.position as usize) < self.rows.len())
    }

    fn rowid(&self) -> i64 {
        self.position
    }

    fn column(&self, column: usize) -> std::result::Result<Value, LimboError> {
        let row = &self.rows[self.position as usize];
        let definition = || {
            row.definition.as_ref().ok_or_else(|| {
                LimboError::ParseError(format!(
                    "how MySQL writes the view {} back has not been measured",
                    row.name
                ))
            })
        };
        let value = match column {
            0 => "def".to_owned(),
            1 => self.database.clone(),
            2 => row.name.clone(),
            3 => definition()?.0.clone(),
            4 => "NONE".to_owned(),
            5 => if definition()?.1 { "YES" } else { "NO" }.to_owned(),
            6 => format!("{}@%", row.creator.username),
            7 => "DEFINER".to_owned(),
            8 => row.creator.character_set_client.clone(),
            9 => row.creator.collation_connection.clone(),
            _ => {
                return Err(LimboError::InternalError(format!(
                    "information_schema.VIEWS has no column {column}"
                )))
            }
        };
        Ok(Value::build_text(value))
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

impl InternalVirtualTableCursor for InformationSchemaTablesCursor {
    fn next(&mut self) -> std::result::Result<bool, LimboError> {
        self.position += 1;
        Ok((self.position as usize) < self.rows.len())
    }

    fn rowid(&self) -> i64 {
        self.position
    }

    fn column(&self, column: usize) -> std::result::Result<Value, LimboError> {
        let (name, kind, collation, comment) = &self.rows[self.position as usize];
        let base_table = *kind == "BASE TABLE";
        // Measured on MySQL 8.4.11: a view has no engine, collation or
        // storage, and its comment is `VIEW`. The storage figures are ones
        // InnoDB keeps and this server does not, so a table has none either.
        Ok(match column {
            0 => Value::build_text(self.database.clone()),
            1 => Value::build_text(name.clone()),
            2 => Value::build_text((*kind).to_owned()),
            3 if base_table => Value::build_text("InnoDB"),
            6 => collation.map_or(Value::Null, Value::build_text),
            3..=6 => Value::Null,
            7 => Value::build_text(comment.clone()),
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
             (TABLE_CATALOG TEXT COLLATE MYSQL_UCA9_AI_CI, \
             TABLE_SCHEMA TEXT COLLATE MYSQL_UCA9_AI_CI, \
             TABLE_NAME TEXT COLLATE MYSQL_UCA9_AI_CI, NON_UNIQUE INTEGER, \
             INDEX_SCHEMA TEXT COLLATE MYSQL_UCA9_AI_CI, \
             INDEX_NAME TEXT COLLATE MYSQL_UCA9_AI_CI, SEQ_IN_INDEX INTEGER, \
             COLUMN_NAME TEXT COLLATE MYSQL_UCA9_AI_CI, \
             COLLATION TEXT COLLATE MYSQL_UCA9_AI_CI, CARDINALITY INTEGER, \
             SUB_PART INTEGER, \
             PACKED TEXT COLLATE MYSQL_UCA9_AI_CI, \
             NULLABLE TEXT COLLATE MYSQL_UCA9_AI_CI, \
             INDEX_TYPE TEXT COLLATE MYSQL_UCA9_AI_CI, \
             COMMENT TEXT COLLATE MYSQL_UCA9_AI_CI, \
             INDEX_COMMENT TEXT COLLATE MYSQL_UCA9_AI_CI, \
             IS_VISIBLE TEXT COLLATE MYSQL_UCA9_AI_CI, \
             EXPRESSION TEXT COLLATE MYSQL_UCA9_AI_CI)"
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
                    // MySQL holds every key column NOT NULL. The engine does
                    // not mark the column its rowid stands for, which is the
                    // counted key of an `AUTO_INCREMENT` table, so it is not
                    // asked.
                    nullable: "",
                });
            }
            for index in indexes_beside_the_primary_key(&schema, name, &btree) {
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
            // MySQL answers InnoDB's estimate of how many distinct values the
            // index holds, cached for a day by default, so what it answers
            // after a write depends on when it last looked. The engine keeps
            // no estimate, and NULL is what MySQL answers for an index it has
            // none for, which is what `SHOW INDEX` here answers too.
            9 => Value::Null,
            10 => Value::Null,
            11 => Value::Null,
            12 => Value::build_text(row.nullable),
            13 => Value::build_text("BTREE"),
            14 => Value::build_text(""),
            15 => Value::build_text(""),
            16 => Value::build_text("YES"),
            17 => Value::Null,
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
             (CONSTRAINT_CATALOG TEXT COLLATE MYSQL_UCA9_AI_CI, \
             CONSTRAINT_SCHEMA TEXT COLLATE MYSQL_UCA9_AI_CI, \
             CONSTRAINT_NAME TEXT COLLATE MYSQL_UCA9_AI_CI, \
             TABLE_CATALOG TEXT COLLATE MYSQL_UCA9_AI_CI, \
             TABLE_SCHEMA TEXT COLLATE MYSQL_UCA9_AI_CI, \
             TABLE_NAME TEXT COLLATE MYSQL_UCA9_AI_CI, \
             COLUMN_NAME TEXT COLLATE MYSQL_UCA9_AI_CI, \
             ORDINAL_POSITION INTEGER, POSITION_IN_UNIQUE_CONSTRAINT INTEGER, \
             REFERENCED_TABLE_SCHEMA TEXT COLLATE MYSQL_UCA9_AI_CI, \
             REFERENCED_TABLE_NAME TEXT COLLATE MYSQL_UCA9_AI_CI, \
             REFERENCED_COLUMN_NAME TEXT COLLATE MYSQL_UCA9_AI_CI)"
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
            // A plain index constrains nothing, so only the unique ones are
            // keys here.
            for index in indexes_beside_the_primary_key(&schema, name, &btree)
                .into_iter()
                .filter(|index| index.unique)
            {
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
                let constraint = foreign_key_name(name, key);
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

/// `information_schema.TABLE_CONSTRAINTS`, one row per constraint.
///
/// The same three kinds `KEY_COLUMN_USAGE` reports, gathered one row per
/// constraint rather than one per column of one.
#[derive(Debug)]
struct InformationSchemaTableConstraints {
    database: String,
}

impl InternalVirtualTable for InformationSchemaTableConstraints {
    fn name(&self) -> String {
        INFORMATION_SCHEMA_TABLE_CONSTRAINTS.to_owned()
    }

    fn sql(&self) -> String {
        format!(
            "CREATE TABLE {INFORMATION_SCHEMA_TABLE_CONSTRAINTS} \
             (CONSTRAINT_CATALOG TEXT COLLATE MYSQL_UCA9_AI_CI, \
             CONSTRAINT_SCHEMA TEXT COLLATE MYSQL_UCA9_AI_CI, \
             CONSTRAINT_NAME TEXT COLLATE MYSQL_UCA9_AI_CI, \
             TABLE_SCHEMA TEXT COLLATE MYSQL_UCA9_AI_CI, \
             TABLE_NAME TEXT COLLATE MYSQL_UCA9_AI_CI, \
             CONSTRAINT_TYPE TEXT COLLATE MYSQL_UCA9_AI_CI, \
             ENFORCED TEXT COLLATE MYSQL_UCA9_AI_CI)"
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

            if !btree.primary_key_columns.is_empty() {
                rows.push(TableConstraintRow {
                    constraint: "PRIMARY".to_owned(),
                    table: name.clone(),
                    kind: "PRIMARY KEY",
                });
            }
            for index in indexes_beside_the_primary_key(&schema, name, &btree)
                .into_iter()
                .filter(|index| index.unique)
            {
                rows.push(TableConstraintRow {
                    constraint: mysql_index_name(index),
                    table: name.clone(),
                    kind: "UNIQUE",
                });
            }
            for key in &btree.foreign_keys {
                rows.push(TableConstraintRow {
                    constraint: foreign_key_name(name, key),
                    table: name.clone(),
                    kind: "FOREIGN KEY",
                });
            }
            for check in stored_checks(&schema, name)? {
                rows.push(TableConstraintRow {
                    constraint: check.name().to_owned(),
                    table: name.clone(),
                    kind: "CHECK",
                });
            }
        }
        rows.sort_by(|left, right| {
            (&left.table, &left.constraint).cmp(&(&right.table, &right.constraint))
        });
        Ok(Arc::new(RwLock::new(
            InformationSchemaTableConstraintsCursor {
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

/// The `CHECK` constraints one stored table carries.
fn stored_checks(
    schema: &turso_core::schema::Schema,
    table: &str,
) -> Result<Vec<turso_mysql_parser::MySqlCheckConstraint>> {
    match schema.table_sql(table) {
        Some(stored) => crate::schema_sql::stored_table_checks(stored)
            .map_err(|error| LimboError::Corrupt(error.to_string())),
        None => Ok(Vec::new()),
    }
}

/// The name MySQL reports for one foreign key.
///
/// A key written without a `CONSTRAINT` name is named after the table it is on,
/// counted from one in declaration order — the same name `SHOW CREATE TABLE`
/// prints for it.
pub(crate) fn foreign_key_name(table: &str, key: &turso_core::schema::ForeignKey) -> String {
    match &key.name {
        Some(name) => name.clone(),
        None => format!("{table}_ibfk_{}", key.decl_order + 1),
    }
}

/// One constraint, which is one row of `information_schema.TABLE_CONSTRAINTS`.
struct TableConstraintRow {
    constraint: String,
    table: String,
    kind: &'static str,
}

struct InformationSchemaTableConstraintsCursor {
    database: String,
    rows: Vec<TableConstraintRow>,
    position: i64,
}

impl InternalVirtualTableCursor for InformationSchemaTableConstraintsCursor {
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
            0 => Value::build_text("def"),
            1 => Value::build_text(self.database.clone()),
            2 => Value::build_text(row.constraint.clone()),
            3 => Value::build_text(self.database.clone()),
            4 => Value::build_text(row.table.clone()),
            5 => Value::build_text(row.kind),
            // Measured on MySQL 8.4.11: every constraint but an unenforced
            // CHECK reports YES, and one cannot be unenforced here.
            6 => Value::build_text("YES"),
            _ => {
                return Err(LimboError::InternalError(format!(
                    "information_schema.TABLE_CONSTRAINTS has no column {column}"
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

/// `information_schema.REFERENTIAL_CONSTRAINTS`, one row per foreign key.
///
/// This is where the `ON DELETE` and `ON UPDATE` a key was written with are
/// read back, which no other `information_schema` table reports.
#[derive(Debug)]
struct InformationSchemaReferentialConstraints {
    database: String,
}

impl InternalVirtualTable for InformationSchemaReferentialConstraints {
    fn name(&self) -> String {
        INFORMATION_SCHEMA_REFERENTIAL_CONSTRAINTS.to_owned()
    }

    fn sql(&self) -> String {
        format!(
            "CREATE TABLE {INFORMATION_SCHEMA_REFERENTIAL_CONSTRAINTS} \
             (CONSTRAINT_CATALOG TEXT COLLATE MYSQL_UCA9_AI_CI, \
             CONSTRAINT_SCHEMA TEXT COLLATE MYSQL_UCA9_AI_CI, \
             CONSTRAINT_NAME TEXT COLLATE MYSQL_UCA9_AI_CI, \
             UNIQUE_CONSTRAINT_CATALOG TEXT COLLATE MYSQL_UCA9_AI_CI, \
             UNIQUE_CONSTRAINT_SCHEMA TEXT COLLATE MYSQL_UCA9_AI_CI, \
             UNIQUE_CONSTRAINT_NAME TEXT COLLATE MYSQL_UCA9_AI_CI, \
             MATCH_OPTION TEXT COLLATE MYSQL_UCA9_AI_CI, \
             UPDATE_RULE TEXT COLLATE MYSQL_UCA9_AI_CI, \
             DELETE_RULE TEXT COLLATE MYSQL_UCA9_AI_CI, \
             TABLE_NAME TEXT COLLATE MYSQL_UCA9_AI_CI, \
             REFERENCED_TABLE_NAME TEXT COLLATE MYSQL_UCA9_AI_CI)"
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
            for key in &btree.foreign_keys {
                rows.push(ReferentialConstraintRow {
                    constraint: foreign_key_name(name, key),
                    table: name.clone(),
                    parent_table: key.parent_table.clone(),
                    parent_key: referenced_key_name(&schema, key),
                    update_rule: mysql_reference_rule(key.on_update),
                    delete_rule: mysql_reference_rule(key.on_delete),
                });
            }
        }
        rows.sort_by(|left, right| {
            (&left.table, &left.constraint).cmp(&(&right.table, &right.constraint))
        });
        Ok(Arc::new(RwLock::new(
            InformationSchemaReferentialConstraintsCursor {
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

/// The name of the key in the parent table that a foreign key points at.
///
/// Measured on MySQL 8.4.11: `PRIMARY` when the referenced columns are the
/// parent's primary key, and the unique key's own name when they are one of
/// its unique keys. A parent this session cannot see leaves it NULL, which is
/// what MySQL leaves for a key it cannot resolve.
fn referenced_key_name(
    schema: &turso_core::schema::Schema,
    key: &turso_core::schema::ForeignKey,
) -> Option<String> {
    let btree = schema.get_table(&key.parent_table)?.btree()?;
    let parent_columns = || key.parent_columns.iter().map(String::as_str);
    if same_columns(
        btree
            .primary_key_columns
            .iter()
            .map(|(column, _)| column.as_str()),
        parent_columns(),
    ) {
        return Some("PRIMARY".to_owned());
    }
    indexes_beside_the_primary_key(schema, &key.parent_table, &btree)
        .into_iter()
        .find(|index| {
            index.unique
                && same_columns(
                    index.columns.iter().map(|column| column.name.as_str()),
                    parent_columns(),
                )
        })
        .map(|index| mysql_index_name(index))
}

/// The rule MySQL reports for what a foreign key does to a child row.
///
/// Measured on MySQL 8.4.11: a key written with no rule at all reports
/// `NO ACTION`, and `RESTRICT` is reported as written rather than folded into
/// it — even though the two behave the same and neither is printed by
/// `SHOW CREATE TABLE`.
const fn mysql_reference_rule(action: turso_parser::ast::RefAct) -> &'static str {
    match action {
        turso_parser::ast::RefAct::NoAction => "NO ACTION",
        turso_parser::ast::RefAct::Restrict => "RESTRICT",
        turso_parser::ast::RefAct::Cascade => "CASCADE",
        turso_parser::ast::RefAct::SetNull => "SET NULL",
        turso_parser::ast::RefAct::SetDefault => "SET DEFAULT",
    }
}

/// One foreign key, which is one row of
/// `information_schema.REFERENTIAL_CONSTRAINTS`.
struct ReferentialConstraintRow {
    constraint: String,
    table: String,
    parent_table: String,
    parent_key: Option<String>,
    update_rule: &'static str,
    delete_rule: &'static str,
}

struct InformationSchemaReferentialConstraintsCursor {
    database: String,
    rows: Vec<ReferentialConstraintRow>,
    position: i64,
}

impl InternalVirtualTableCursor for InformationSchemaReferentialConstraintsCursor {
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
            0 => Value::build_text("def"),
            1 => Value::build_text(self.database.clone()),
            2 => Value::build_text(row.constraint.clone()),
            3 => Value::build_text("def"),
            4 => Value::build_text(self.database.clone()),
            5 => match &row.parent_key {
                Some(name) => Value::build_text(name.clone()),
                None => Value::Null,
            },
            // MySQL has only one match option and reports it for every key.
            6 => Value::build_text("NONE"),
            7 => Value::build_text(row.update_rule),
            8 => Value::build_text(row.delete_rule),
            9 => Value::build_text(row.table.clone()),
            10 => Value::build_text(row.parent_table.clone()),
            _ => {
                return Err(LimboError::InternalError(format!(
                    "information_schema.REFERENTIAL_CONSTRAINTS has no column {column}"
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

/// `information_schema.CHECK_CONSTRAINTS`, one row per `CHECK` of every table
/// the session may see.
///
/// Measured on MySQL 8.4.11: `CHECK_CLAUSE` is the expression as MySQL writes
/// it back. A clause this does not know how MySQL writes is refused when it is
/// read, while its constraint's name is still answered.
#[derive(Debug)]
struct InformationSchemaCheckConstraints {
    database: String,
}

impl InternalVirtualTable for InformationSchemaCheckConstraints {
    fn name(&self) -> String {
        INFORMATION_SCHEMA_CHECK_CONSTRAINTS.to_owned()
    }

    fn sql(&self) -> String {
        format!(
            "CREATE TABLE {INFORMATION_SCHEMA_CHECK_CONSTRAINTS} \
             (CONSTRAINT_CATALOG TEXT COLLATE MYSQL_UCA9_AI_CI, \
             CONSTRAINT_SCHEMA TEXT COLLATE MYSQL_UCA9_AI_CI, \
             CONSTRAINT_NAME TEXT COLLATE MYSQL_UCA9_AI_CI, \
             CHECK_CLAUSE TEXT COLLATE MYSQL_UCA9_AI_CI)"
        )
    }

    fn open(
        &self,
        connection: Arc<Connection>,
    ) -> Result<Arc<RwLock<dyn InternalVirtualTableCursor>>> {
        let schema = connection.current_schema();
        let mut rows = Vec::new();
        for (name, table) in &schema.tables {
            if table.btree().is_none()
                || is_system_table(name)
                || is_internal_table(name)
                || !connection.mysql_table_is_visible(name)
            {
                continue;
            }
            rows.extend(stored_checks(&schema, name)?);
        }
        rows.sort_by(|left, right| left.name().cmp(right.name()));
        Ok(Arc::new(RwLock::new(
            InformationSchemaCheckConstraintsCursor {
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

struct InformationSchemaCheckConstraintsCursor {
    database: String,
    rows: Vec<turso_mysql_parser::MySqlCheckConstraint>,
    position: i64,
}

impl InternalVirtualTableCursor for InformationSchemaCheckConstraintsCursor {
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
            0 => Value::build_text("def"),
            1 => Value::build_text(self.database.clone()),
            2 => Value::build_text(row.name().to_owned()),
            3 => Value::build_text(
                row.clause()
                    .ok_or_else(|| {
                        LimboError::ParseError(format!(
                            "how MySQL writes the CHECK named {} has not been measured",
                            row.name()
                        ))
                    })?
                    .to_owned(),
            ),
            _ => {
                return Err(LimboError::InternalError(format!(
                    "information_schema.CHECK_CONSTRAINTS has no column {column}"
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

/// `information_schema.ROUTINES`, which is always empty.
///
/// Stored procedures and functions are refused here, so no database holds
/// one, and no rows is the true answer to a client asking which there are.
/// The columns are MySQL's own, so a wildcard answers the width MySQL does.
#[derive(Debug)]
struct InformationSchemaRoutines;

impl InternalVirtualTable for InformationSchemaRoutines {
    fn name(&self) -> String {
        INFORMATION_SCHEMA_ROUTINES.to_owned()
    }

    fn sql(&self) -> String {
        let columns = [
            "SPECIFIC_NAME TEXT",
            "ROUTINE_CATALOG TEXT",
            "ROUTINE_SCHEMA TEXT",
            "ROUTINE_NAME TEXT",
            "ROUTINE_TYPE TEXT",
            "DATA_TYPE TEXT",
            "CHARACTER_MAXIMUM_LENGTH INTEGER",
            "CHARACTER_OCTET_LENGTH INTEGER",
            "NUMERIC_PRECISION INTEGER",
            "NUMERIC_SCALE INTEGER",
            "DATETIME_PRECISION INTEGER",
            "CHARACTER_SET_NAME TEXT",
            "COLLATION_NAME TEXT",
            "DTD_IDENTIFIER TEXT",
            "ROUTINE_BODY TEXT",
            "ROUTINE_DEFINITION TEXT",
            "EXTERNAL_NAME TEXT",
            "EXTERNAL_LANGUAGE TEXT",
            "PARAMETER_STYLE TEXT",
            "IS_DETERMINISTIC TEXT",
            "SQL_DATA_ACCESS TEXT",
            "SQL_PATH TEXT",
            "SECURITY_TYPE TEXT",
            "CREATED TEXT",
            "LAST_ALTERED TEXT",
            "SQL_MODE TEXT",
            "ROUTINE_COMMENT TEXT",
            "DEFINER TEXT",
            "CHARACTER_SET_CLIENT TEXT",
            "COLLATION_CONNECTION TEXT",
            "DATABASE_COLLATION TEXT",
        ];
        format!(
            "CREATE TABLE {INFORMATION_SCHEMA_ROUTINES} ({})",
            columns.join(", ")
        )
    }

    fn open(
        &self,
        _connection: Arc<Connection>,
    ) -> Result<Arc<RwLock<dyn InternalVirtualTableCursor>>> {
        Ok(Arc::new(RwLock::new(NoRoutinesCursor)))
    }

    fn best_index(
        &self,
        constraints: &[turso_ext::ConstraintInfo],
        _order_by: &[turso_ext::OrderByInfo],
    ) -> std::result::Result<turso_ext::IndexInfo, turso_ext::ResultCode> {
        catalog_best_index(constraints)
    }
}

struct NoRoutinesCursor;

impl InternalVirtualTableCursor for NoRoutinesCursor {
    fn next(&mut self) -> std::result::Result<bool, LimboError> {
        Ok(false)
    }

    fn rowid(&self) -> i64 {
        unreachable!("information_schema.ROUTINES has no row to stand on")
    }

    fn column(&self, column: usize) -> std::result::Result<Value, LimboError> {
        Err(LimboError::InternalError(format!(
            "information_schema.ROUTINES has no row to read column {column} from"
        )))
    }

    fn filter(
        &mut self,
        _args: &[Value],
        _idx_str: Option<String>,
        _idx_num: i32,
    ) -> std::result::Result<bool, LimboError> {
        Ok(false)
    }
}

/// An `information_schema` table whose rows the session works out and leaves
/// on the connection before a statement that scans it runs.
///
/// `SCHEMATA` answers the databases the session may see, which live in the
/// server's catalog rather than in this database, and `COLUMNS` answers each
/// column the way MySQL prints it, which is read out of the stored DDL by
/// statements a scan in the middle of a statement cannot run.
///
/// A row shorter than the table holds only the columns the session could work
/// out, and reading one past them fails: a table whose columns cannot be read
/// still answers its name, so a query about another table is not refused for
/// it.
#[derive(Debug)]
struct SessionCatalogTable {
    name: &'static str,
    mysql_name: &'static str,
    columns: &'static [&'static str],
}

impl InternalVirtualTable for SessionCatalogTable {
    fn name(&self) -> String {
        self.name.to_owned()
    }

    fn sql(&self) -> String {
        format!("CREATE TABLE {} ({})", self.name, self.columns.join(", "))
    }

    fn open(
        &self,
        connection: Arc<Connection>,
    ) -> Result<Arc<RwLock<dyn InternalVirtualTableCursor>>> {
        let rows = connection.mysql_catalog_rows(self.name).ok_or_else(|| {
            LimboError::InternalError(format!(
                "information_schema.{} was scanned before its rows were worked out",
                self.mysql_name
            ))
        })?;
        Ok(Arc::new(RwLock::new(SessionCatalogCursor {
            mysql_name: self.mysql_name,
            width: self.columns.len(),
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

struct SessionCatalogCursor {
    mysql_name: &'static str,
    width: usize,
    rows: Arc<Vec<Vec<Value>>>,
    position: i64,
}

impl InternalVirtualTableCursor for SessionCatalogCursor {
    fn next(&mut self) -> std::result::Result<bool, LimboError> {
        self.position += 1;
        Ok((self.position as usize) < self.rows.len())
    }

    fn rowid(&self) -> i64 {
        self.position
    }

    fn column(&self, column: usize) -> std::result::Result<Value, LimboError> {
        if column >= self.width {
            return Err(LimboError::InternalError(format!(
                "information_schema.{} has no column {column}",
                self.mysql_name
            )));
        }
        let row = &self.rows[self.position as usize];
        row.get(column).cloned().ok_or_else(|| {
            LimboError::ParseError(format!(
                "information_schema.{} cannot answer column {column} of this row",
                self.mysql_name
            ))
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

    /// The rows MySQL 8.4.11 answers for the same schema, measured on the
    /// pinned oracle: one row per constraint rather than one per column of
    /// one, a plain index still absent, and the referenced key named — the
    /// primary key as `PRIMARY` and a unique key by its own name.
    #[test]
    fn the_constraint_tables_name_each_key_and_the_rules_it_carries() {
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
            "CREATE TABLE child (cid INTEGER NOT NULL PRIMARY KEY, parent_id INTEGER, \
             parent_code TEXT, label TEXT, \
             CONSTRAINT fk_by_id FOREIGN KEY (parent_id) REFERENCES parent (id) \
             ON DELETE CASCADE ON UPDATE SET NULL, \
             CONSTRAINT fk_by_code FOREIGN KEY (parent_code) REFERENCES parent (code))",
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
                "SELECT TABLE_NAME, CONSTRAINT_NAME, CONSTRAINT_TYPE, ENFORCED \
                 FROM {INFORMATION_SCHEMA_TABLE_CONSTRAINTS}"
            )),
            vec![
                row(&["child", "PRIMARY", "PRIMARY KEY", "YES"]),
                row(&["child", "fk_by_code", "FOREIGN KEY", "YES"]),
                row(&["child", "fk_by_id", "FOREIGN KEY", "YES"]),
                row(&["parent", "PRIMARY", "PRIMARY KEY", "YES"]),
                row(&["parent", "uk_code", "UNIQUE", "YES"]),
            ]
        );

        assert_eq!(
            read(&format!(
                "SELECT TABLE_NAME, CONSTRAINT_NAME, UNIQUE_CONSTRAINT_NAME, MATCH_OPTION, \
                 UPDATE_RULE, DELETE_RULE, REFERENCED_TABLE_NAME \
                 FROM {INFORMATION_SCHEMA_REFERENTIAL_CONSTRAINTS}"
            )),
            vec![
                // Written with no rule at all, which MySQL reads back as
                // NO ACTION on both.
                row(&[
                    "child",
                    "fk_by_code",
                    "uk_code",
                    "NONE",
                    "NO ACTION",
                    "NO ACTION",
                    "parent",
                ]),
                row(&["child", "fk_by_id", "PRIMARY", "NONE", "SET NULL", "CASCADE", "parent"]),
            ]
        );

        // A session that may see only some tables reads only their
        // constraints.
        connection.set_mysql_visible_tables(Some(vec!["parent".to_owned()]));
        assert_eq!(
            read(&format!(
                "SELECT CONSTRAINT_NAME FROM {INFORMATION_SCHEMA_REFERENTIAL_CONSTRAINTS}"
            )),
            Vec::<Vec<String>>::new()
        );
        assert_eq!(
            read(&format!(
                "SELECT TABLE_NAME, CONSTRAINT_NAME FROM {INFORMATION_SCHEMA_TABLE_CONSTRAINTS}"
            )),
            vec![row(&["parent", "PRIMARY"]), row(&["parent", "uk_code"])]
        );
    }

    /// `COLUMNS` answers the rows the session left for it, and a table whose
    /// columns the session could not read answers its name and no more: a
    /// query about another table is answered, and one reading that table's
    /// columns is refused rather than answered with made-up ones.
    #[test]
    fn a_table_the_session_could_not_read_answers_its_name_and_nothing_else() {
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
        let scan = |sql: &str| {
            connection
                .prepare(sql)
                .and_then(|mut statement| statement.run_collect_rows())
        };

        // Nothing has been left yet, and a scan says so rather than
        // answering no columns at all.
        assert!(scan(&format!(
            "SELECT COLUMN_NAME FROM {INFORMATION_SCHEMA_COLUMNS}"
        ))
        .is_err());

        let mut readable = vec![
            Value::build_text("def"),
            Value::build_text("reports"),
            Value::build_text("alpha"),
            Value::build_text("id"),
            Value::from_i64(1),
        ];
        readable.resize(22, Value::Null);
        connection.set_mysql_catalog_rows(
            INFORMATION_SCHEMA_COLUMNS,
            vec![
                readable,
                vec![
                    Value::build_text("def"),
                    Value::build_text("reports"),
                    Value::build_text("broken"),
                ],
            ],
        );
        let texts = |rows: Vec<Vec<Value>>| -> Vec<String> {
            rows.into_iter()
                .map(|row| match &row[0] {
                    Value::Text(text) => text.as_str().to_owned(),
                    other => panic!("{other:?} is not text"),
                })
                .collect()
        };
        assert_eq!(
            texts(
                scan(&format!(
                    "SELECT TABLE_NAME FROM {INFORMATION_SCHEMA_COLUMNS}"
                ))
                .unwrap()
            ),
            ["alpha", "broken"]
        );
        assert_eq!(
            texts(
                scan(&format!(
                    "SELECT COLUMN_NAME FROM {INFORMATION_SCHEMA_COLUMNS} \
                     WHERE TABLE_NAME = 'alpha'"
                ))
                .unwrap()
            ),
            ["id"]
        );
        assert!(scan(&format!(
            "SELECT COLUMN_NAME FROM {INFORMATION_SCHEMA_COLUMNS}"
        ))
        .is_err());
    }

    fn row(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_owned()).collect()
    }
}
