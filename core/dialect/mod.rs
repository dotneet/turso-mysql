//! SQL dialects.
//!
//! The [`Dialect`] trait is the boundary between the engine and the SQL
//! dialect a frontend speaks. The engine owns the mechanics — pages,
//! B-trees, the `sqlite_schema` table itself, bytecode — and consults the
//! dialect wherever the meaning of SQL text is dialect-specific: parsing
//! statements into the engine AST and interpreting persisted schema text.
//! The [`sqlite`] module owns [`SqliteDialect`], the SQLite
//! implementation, and the catalog tables that ship with every Turso
//! build (`pragma_*`, `json_each`/`json_tree`, `sqlite_dbpage`,
//! `btree_dump`, `sqlite_turso_types`).

pub mod sqlite;

pub use sqlite::SqliteDialect;

/// The kind of object whose defining SQL is stored in `sqlite_schema`.
///
/// Frontend dialects use this discriminator when their durable schema text
/// needs parsing context that is not present in SQLite SQL alone.
#[non_exhaustive]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SchemaSqlKind {
    Table,
    Index,
    View,
    Trigger,
}

/// One owned row read from `sqlite_schema` during a schema rebuild.
///
/// A dialect receives the complete catalog before the core turns any row into
/// in-memory schema state. The fields mirror SQLite's durable schema table,
/// but do not assign frontend-specific meaning to its SQL text.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SchemaCatalogRow {
    pub object_type: String,
    pub name: String,
    pub table_name: String,
    pub root_page: i64,
    pub sql: Option<String>,
}

/// Trusted durable identity for catalog-wide schema validation.
///
/// The frontend that owns a database verifies this identity before opening the
/// database, then supplies it through [`crate::OpenOptions`]. Core retains it
/// unchanged for the database lifetime and only passes it to the dialect while
/// validating durable `sqlite_schema` rows.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SchemaCatalogValidationContext {
    database_identity: [u8; 16],
}

impl SchemaCatalogValidationContext {
    /// Creates validation context from an identity already verified by the
    /// frontend's durable database registry.
    pub fn new(database_identity: [u8; 16]) -> Self {
        Self { database_identity }
    }

    /// Returns the frontend-verified durable database identity.
    pub fn database_identity(&self) -> &[u8; 16] {
        &self.database_identity
    }
}

/// Immutable frontend state used to encode durable schema SQL.
///
/// Parsing modes and charset defaults belong to a frontend session rather
/// than a database-wide [`Dialect`]. A frontend captures those settings when
/// it prepares a DDL statement and supplies this formatter through
/// [`crate::PrepareOptions`]. Every string returned here must be understood by
/// the database's [`Dialect::parse_schema_sql`] and
/// [`Dialect::schema_sql_for_replay`] implementations. A private format
/// without those matching decode paths would make the database impossible to
/// reopen.
pub trait SchemaSqlFormatter: Send + Sync + 'static {
    fn format_schema_sql(
        &self,
        kind: SchemaSqlKind,
        input: &str,
        stmt: &turso_parser::ast::Stmt,
    ) -> crate::Result<String>;

    fn format_rewritten_schema_sql(
        &self,
        kind: SchemaSqlKind,
        previous_sql: &str,
        stmt: &turso_parser::ast::Stmt,
    ) -> crate::Result<String>;
}

/// Durable ownership of a database file by a SQL frontend.
///
/// SQLite-compatible databases remain unowned and may use `application_id`
/// for their own purposes. Frontend-owned databases reserve that header field
/// for a versioned Turso marker so a fresh process can reject a wrong-dialect
/// open before loading schema SQL or writing the file.
#[non_exhaustive]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum DatabaseFileOwner {
    #[default]
    SqliteCompatible,
    Postgres,
    MySql,
}

impl DatabaseFileOwner {
    pub(crate) const APPLICATION_ID_PREFIX: u32 = 0x5452_0000;
    pub(crate) const APPLICATION_ID_MASK: u32 = 0xffff_0000;
    pub(crate) const OWNER_ONLY_FORMAT_VERSION: u8 = 1;
    pub const MYSQL_FORMAT_VERSION: u8 = 2;
    pub const MYSQL_LOWER_CASE_TABLE_NAMES: u8 = 1;
    const MYSQL_OWNER_NIBBLE: u8 = 2;
    pub(crate) const MYSQL_POLICY_MASK: u8 = 0x0c;
    pub(crate) const MYSQL_RESERVED_MASK: u8 = 0x03;

    pub(crate) const fn application_id(self) -> Option<i32> {
        let kind = match self {
            Self::SqliteCompatible => return None,
            Self::Postgres => 1,
            Self::MySql => {
                return Some(Self::mysql_application_id(
                    Self::MYSQL_LOWER_CASE_TABLE_NAMES,
                ));
            }
        };
        Some(
            (Self::APPLICATION_ID_PREFIX | ((Self::OWNER_ONLY_FORMAT_VERSION as u32) << 8) | kind)
                as i32,
        )
    }

    /// Builds the format-v2 MySQL owner marker for a root name policy.
    ///
    /// The initial MySQL frontend supports only
    /// [`Self::MYSQL_LOWER_CASE_TABLE_NAMES`]. Other values are exposed here
    /// so a future explicit policy implementation can use the same wire
    /// format without reinterpreting existing files.
    pub const fn mysql_application_id(lower_case_table_names: u8) -> i32 {
        assert!(lower_case_table_names <= 3);
        (Self::APPLICATION_ID_PREFIX
            | ((Self::MYSQL_FORMAT_VERSION as u32) << 8)
            | ((Self::MYSQL_OWNER_NIBBLE as u32) << 4)
            | ((lower_case_table_names as u32) << 2)) as i32
    }

    pub(crate) const fn name(self) -> &'static str {
        match self {
            Self::SqliteCompatible => "sqlite-compatible",
            Self::Postgres => "postgres",
            Self::MySql => "mysql",
        }
    }
}

/// SQL dialect layered on top of the engine.
///
/// Every [`crate::Database`] carries a dialect, supplied explicitly by
/// every open path and fixed for the lifetime of the database;
/// SQLite-compatible callers pass [`SqliteDialect`]. Initial statement
/// preparation, re-preparation without a per-statement parser, and every
/// schema load go through this interface.
pub trait Dialect: Send + Sync + 'static {
    /// Stable identifier for this dialect (e.g. "sqlite", "postgres").
    ///
    /// A database file must always be opened with the same dialect it was
    /// created with; the process-wide database registry uses this name to
    /// reject an open whose dialect differs from the already-open instance.
    fn name(&self) -> &'static str;

    /// Durable frontend ownership required for files opened by this dialect.
    ///
    /// Custom dialects default to SQLite-compatible, which preserves existing
    /// files and leaves `PRAGMA application_id` under application control.
    fn database_file_owner(&self) -> DatabaseFileOwner {
        DatabaseFileOwner::SqliteCompatible
    }

    /// Exact durable marker required by this dialect.
    ///
    /// This defaults to the owner's currently supported marker. Dialects that
    /// carry policy in their marker override it so registry hits and file
    /// opens reject a policy mismatch before WAL recovery or schema loading.
    fn database_file_application_id(&self) -> Option<i32> {
        self.database_file_owner().application_id()
    }

    /// Optionally validates evaluated table records before storage.
    ///
    /// A dialect must return `None` unless it can apply this rule to every
    /// generic core prepare as well as frontend-translated statements.
    fn assignment_validator(&self) -> Option<std::sync::Arc<dyn crate::AssignmentValidator>> {
        None
    }

    /// Validate the complete durable schema catalog before the core loads it.
    ///
    /// This is the boundary for checks that require seeing more than one
    /// `sqlite_schema` row. The default preserves SQLite-compatible loading.
    fn validate_schema_catalog(
        &self,
        _rows: &[SchemaCatalogRow],
        _context: Option<&SchemaCatalogValidationContext>,
    ) -> crate::Result<()> {
        Ok(())
    }

    /// Parse the first statement in `sql` into the engine AST.
    ///
    /// Returns the parsed command, if any, and the number of input bytes
    /// consumed. The engine uses the same method for initial preparation and
    /// re-preparation unless translated statements supply a per-statement
    /// [`crate::ReprepareParser`]. Implementations must accept canonical SQLite
    /// text because engine-generated statements use that representation.
    fn parse(&self, sql: &str) -> crate::Result<(Option<turso_parser::ast::Cmd>, usize)>;

    /// Parse a `sqlite_schema` `type='table'` row's SQL into a table
    /// definition.
    ///
    /// Rows written by internal engine paths (sequence backing tables,
    /// `sqlite_sequence`) are plain SQLite text and carry no frontend
    /// marker, so every implementation must fall back to SQLite parsing
    /// for text it does not recognize as its own.
    fn parse_table_sql(
        &self,
        sql: &str,
        root_page: i64,
    ) -> crate::Result<crate::schema::BTreeTable>;

    /// Decode a storage-backed table's persisted SQL into its `CREATE TABLE`
    /// AST.
    ///
    /// Unlike [`Dialect::parse`], this method receives SQL read from
    /// `sqlite_schema` and must recognize the representation produced by
    /// [`Dialect::format_table_sql`] and
    /// [`Dialect::format_rewritten_table_sql`]. Internal engine tables use
    /// plain SQLite text, so implementations must retain the same SQLite
    /// fallback required by [`Dialect::parse_table_sql`].
    fn parse_table_sql_ast(&self, sql: &str) -> crate::Result<turso_parser::ast::Stmt>;

    /// Recover SQL that can be prepared to recreate a persisted table.
    ///
    /// Dialects that wrap original frontend DDL in their stored representation
    /// must unwrap it here so replay preserves that DDL. The returned statement
    /// must create the table in the connection's main schema, even when the
    /// persisted statement originally qualified the source database. Unmarked
    /// internal engine tables must retain the SQLite fallback used by the
    /// schema parsing methods.
    fn table_sql_for_replay(&self, sql: &str) -> crate::Result<String>;

    /// Produce the SQL text to store in `sqlite_schema` for a
    /// `CREATE TABLE`.
    ///
    /// `input` is the original statement text as the user wrote it, in the
    /// frontend's dialect; `tbl_name` and `body` are the translated AST.
    /// The SQLite dialect formats canonical SQLite text from the AST; a
    /// frontend dialect typically stores `input` with a marker it can
    /// recognize in [`Dialect::parse_table_sql`].
    fn format_table_sql(
        &self,
        input: &str,
        tbl_name: &turso_parser::ast::QualifiedName,
        body: &turso_parser::ast::CreateTableBody,
    ) -> crate::Result<String>;

    /// Produce stored SQL after the engine rewrites a `CREATE TABLE` AST.
    ///
    /// Schema rewrites cannot reuse the original frontend text because it no
    /// longer describes the rewritten table. Dialects that need syntax beyond
    /// a marker around canonical SQL can override this to render their native
    /// table definition from the rewritten AST.
    fn format_rewritten_table_sql(&self, stmt: &turso_parser::ast::Stmt) -> crate::Result<String> {
        let turso_parser::ast::Stmt::CreateTable { tbl_name, body, .. } = stmt else {
            return Err(crate::LimboError::InternalError(
                "format_rewritten_table_sql requires CREATE TABLE".to_string(),
            ));
        };
        self.format_table_sql(&stmt.to_string(), tbl_name, body)
    }

    /// Parse SQL read from `sqlite_schema` and verify its object kind.
    ///
    /// The table-only methods above remain the compatibility boundary for
    /// existing dialect implementations. Frontends that persist dialect SQL
    /// for indexes, views, or triggers override this method and decode that
    /// representation before translating it to the engine AST.
    fn parse_schema_sql(
        &self,
        kind: SchemaSqlKind,
        sql: &str,
    ) -> crate::Result<turso_parser::ast::Stmt> {
        let stmt = match kind {
            SchemaSqlKind::Table => self.parse_table_sql_ast(sql)?,
            SchemaSqlKind::Index | SchemaSqlKind::View | SchemaSqlKind::Trigger => {
                let (cmd, _) = self.parse(sql)?;
                let Some(turso_parser::ast::Cmd::Stmt(stmt)) = cmd else {
                    return Err(crate::LimboError::ParseError(format!(
                        "persisted {kind:?} SQL is not a statement"
                    )));
                };
                stmt
            }
        };
        ensure_schema_sql_kind(kind, &stmt)?;
        Ok(stmt)
    }

    /// Produce durable SQL for a newly created schema object.
    ///
    /// `input` is the frontend statement text selected by the translator for
    /// storage. The default keeps existing non-table storage byte-for-byte;
    /// frontend dialects can instead normalize and envelope it with session
    /// parsing context.
    fn format_schema_sql(
        &self,
        kind: SchemaSqlKind,
        input: &str,
        stmt: &turso_parser::ast::Stmt,
    ) -> crate::Result<String> {
        ensure_schema_sql_kind_for_format(kind, stmt)?;
        match stmt {
            turso_parser::ast::Stmt::CreateTable { tbl_name, body, .. }
                if kind == SchemaSqlKind::Table =>
            {
                self.format_table_sql(input, tbl_name, body)
            }
            _ => Ok(input.to_string()),
        }
    }

    /// Produce durable SQL after an engine schema rewrite.
    ///
    /// `previous_sql` is the exact text currently stored in `sqlite_schema`.
    /// A formatter can use it to retain frontend syntax and session metadata
    /// that the engine AST does not represent.
    fn format_rewritten_schema_sql(
        &self,
        kind: SchemaSqlKind,
        _previous_sql: &str,
        stmt: &turso_parser::ast::Stmt,
    ) -> crate::Result<String> {
        ensure_schema_sql_kind_for_format(kind, stmt)?;
        if kind == SchemaSqlKind::Table {
            return self.format_rewritten_table_sql(stmt);
        }
        Ok(stmt.to_string())
    }

    /// Recover SQL that can be prepared to recreate a persisted schema object.
    ///
    /// Existing dialects only persisted frontend-specific table text, so the
    /// default preserves non-table SQL. A dialect that envelopes other object
    /// kinds must override this method and remove its envelope here.
    fn schema_sql_for_replay(&self, kind: SchemaSqlKind, sql: &str) -> crate::Result<String> {
        if kind == SchemaSqlKind::Table {
            return self.table_sql_for_replay(sql);
        }
        Ok(sql.to_string())
    }

    /// Install the dialect's catalog tables into a freshly constructed
    /// schema.
    ///
    /// Called by [`crate::schema::Schema::with_options`] on every schema
    /// construction and rebuild, so catalog tables survive rebuilds
    /// structurally instead of being re-registered by hand. The SQLite
    /// dialect registers the standard built-in catalog here; other
    /// dialects typically compose with it via
    /// [`sqlite::register_builtin_catalog`] and then add their own tables
    /// (constructed with [`crate::VirtualTable::new_internal`], which
    /// requires no connection).
    fn register_catalog(
        &self,
        schema: &mut crate::schema::Schema,
        enable_custom_types: bool,
    ) -> crate::Result<()>;

    /// Resolve a function name in user SQL to the engine's function IR.
    ///
    /// The dialect owns its scalar function surface: the SQLite dialect
    /// resolves the built-in set, another dialect resolves its own —
    /// mapping names onto engine primitives where it wants them (usually
    /// by composing with [`sqlite::resolve_builtin_function`]) and onto
    /// [`crate::Func::Dialect`] for functions it executes
    /// itself via [`Dialect::exec_scalar_function`]. Consulted
    /// before extension functions; engine-generated helper statements
    /// always resolve with SQLite semantics instead.
    fn resolve_function(&self, name: &str, arg_count: usize) -> crate::Result<Option<crate::Func>>;

    /// Execute a dialect scalar function at runtime.
    ///
    /// Receives the connection — unlike extension functions — because
    /// catalog functions (e.g. `pg_get_tabledef`) need to inspect the
    /// schema. Only reached through [`crate::Func::Dialect`],
    /// so a dialect that never resolves to that variant can keep the
    /// default "no such function" error.
    fn exec_scalar_function(
        &self,
        _conn: &crate::Connection,
        name: &str,
        _args: &[crate::Value],
    ) -> crate::Result<crate::Value> {
        Err(crate::LimboError::ParseError(format!(
            "no such function: {name}"
        )))
    }

    /// Whether this dialect needs the custom-type machinery (DECODE/ENCODE,
    /// affinity metadata) regardless of the experimental database flag.
    /// A dialect whose type system leans on custom types (e.g. PostgreSQL)
    /// returns true so its databases never open with the machinery off.
    fn requires_custom_types(&self) -> bool {
        false
    }
}

fn ensure_schema_sql_kind(
    expected: SchemaSqlKind,
    stmt: &turso_parser::ast::Stmt,
) -> crate::Result<()> {
    use turso_parser::ast::Stmt;

    let matches = match expected {
        SchemaSqlKind::Table => matches!(stmt, Stmt::CreateTable { .. }),
        SchemaSqlKind::Index => matches!(stmt, Stmt::CreateIndex { .. }),
        SchemaSqlKind::View => matches!(
            stmt,
            Stmt::CreateView { .. } | Stmt::CreateMaterializedView { .. }
        ),
        SchemaSqlKind::Trigger => matches!(stmt, Stmt::CreateTrigger { .. }),
    };
    if matches {
        return Ok(());
    }
    Err(crate::LimboError::Corrupt(format!(
        "persisted schema SQL kind mismatch: expected {expected:?}, parsed {stmt:?}"
    )))
}

pub(crate) fn ensure_schema_sql_kind_for_format(
    kind: SchemaSqlKind,
    stmt: &turso_parser::ast::Stmt,
) -> crate::Result<()> {
    ensure_schema_sql_kind(kind, stmt).map_err(|err| {
        crate::LimboError::InternalError(format!(
            "cannot format {kind:?} schema SQL from the wrong statement kind: {err}"
        ))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::BTreeTable;
    use crate::storage::database::DatabaseFile;
    use crate::sync::atomic::{AtomicUsize, Ordering};
    #[cfg(feature = "fs")]
    use crate::PlatformIO;
    use crate::{
        Database, DatabaseOpts, MemoryIO, OpenFlags, PrepareOptions, ReprepareContext,
        ReprepareParser, IO,
    };
    use std::sync::{Arc, Mutex};

    /// A dialect that counts schema-row parses and strips a `/* test */ `
    /// marker before delegating to SQLite parsing, mirroring how a frontend
    /// dialect recognizes its own stored text and falls back to SQLite for
    /// unmarked rows.
    #[derive(Default)]
    struct TestDialect {
        parse_calls: AtomicUsize,
        statement_parse_calls: AtomicUsize,
        catalog_validation_calls: AtomicUsize,
        catalog_validation_contexts: Mutex<Vec<Option<[u8; 16]>>>,
        reject_catalog: bool,
        reject_catalog_after_first_validation: bool,
        owner: DatabaseFileOwner,
        application_id: Option<Option<i32>>,
    }

    struct FrozenParser {
        translated_sql: &'static str,
        parse_calls: AtomicUsize,
    }

    impl FrozenParser {
        fn new(translated_sql: &'static str) -> Self {
            Self {
                translated_sql,
                parse_calls: AtomicUsize::new(0),
            }
        }
    }

    impl ReprepareParser for FrozenParser {
        fn parse(
            &self,
            sql: &str,
            context: &ReprepareContext<'_>,
        ) -> crate::Result<(Option<turso_parser::ast::Cmd>, usize)> {
            self.parse_calls.fetch_add(1, Ordering::SeqCst);
            assert_eq!(
                sql,
                r#"SELECT "value", id FROM reprepare_test WHERE id = ?"#
            );
            assert!(context.schema.get_btree_table("reprepare_test").is_some());
            let (cmd, _) = sqlite::parse(self.translated_sql)?;
            Ok((cmd, sql.len()))
        }
    }

    impl Dialect for TestDialect {
        fn name(&self) -> &'static str {
            "test"
        }

        fn database_file_owner(&self) -> DatabaseFileOwner {
            self.owner
        }

        fn database_file_application_id(&self) -> Option<i32> {
            self.application_id
                .unwrap_or_else(|| self.owner.application_id())
        }

        fn validate_schema_catalog(
            &self,
            _rows: &[SchemaCatalogRow],
            context: Option<&SchemaCatalogValidationContext>,
        ) -> crate::Result<()> {
            let previous_calls = self.catalog_validation_calls.fetch_add(1, Ordering::SeqCst);
            self.catalog_validation_contexts
                .lock()
                .unwrap()
                .push(context.map(|context| *context.database_identity()));
            if self.reject_catalog
                || (self.reject_catalog_after_first_validation && previous_calls > 0)
            {
                return Err(crate::LimboError::Corrupt(
                    "test schema catalog rejection".to_string(),
                ));
            }
            Ok(())
        }

        fn parse(&self, sql: &str) -> crate::Result<(Option<turso_parser::ast::Cmd>, usize)> {
            self.statement_parse_calls.fetch_add(1, Ordering::SeqCst);
            if let Some(sql) = sql.strip_prefix("test: ") {
                let (cmd, offset) = sqlite::parse(sql)?;
                Ok((cmd, "test: ".len() + offset))
            } else {
                sqlite::parse(sql)
            }
        }

        fn parse_table_sql(&self, sql: &str, root_page: i64) -> crate::Result<BTreeTable> {
            self.parse_calls.fetch_add(1, Ordering::SeqCst);
            let sql = sql.strip_prefix("/* test */ ").unwrap_or(sql);
            BTreeTable::from_sql(sql, root_page)
        }

        fn parse_table_sql_ast(&self, sql: &str) -> crate::Result<turso_parser::ast::Stmt> {
            let sql = sql.strip_prefix("/* test */ ").unwrap_or(sql);
            sqlite::parse_table_sql_ast(sql)
        }

        fn table_sql_for_replay(&self, sql: &str) -> crate::Result<String> {
            let sql = sql.strip_prefix("/* test */ ").unwrap_or(sql);
            sqlite::table_sql_for_replay(sql)
        }

        fn format_table_sql(
            &self,
            input: &str,
            _tbl_name: &turso_parser::ast::QualifiedName,
            _body: &turso_parser::ast::CreateTableBody,
        ) -> crate::Result<String> {
            Ok(format!("/* test */ {input}"))
        }

        fn resolve_function(
            &self,
            name: &str,
            arg_count: usize,
        ) -> crate::Result<Option<crate::function::Func>> {
            if name.eq_ignore_ascii_case("nvl") {
                return sqlite::resolve_builtin_function("coalesce", arg_count);
            }
            if name.eq_ignore_ascii_case("test_add_one") && arg_count == 1 {
                return Ok(Some(crate::function::Func::Dialect(
                    "test_add_one".to_string(),
                )));
            }
            sqlite::resolve_builtin_function(name, arg_count)
        }

        fn exec_scalar_function(
            &self,
            _conn: &crate::Connection,
            name: &str,
            args: &[crate::Value],
        ) -> crate::Result<crate::Value> {
            assert_eq!(name, "test_add_one");
            let crate::Value::Numeric(crate::numeric::Numeric::Integer(v)) = args[0] else {
                return Err(crate::LimboError::InvalidArgument(
                    "test_add_one expects an integer".to_string(),
                ));
            };
            Ok(crate::Value::Numeric(crate::numeric::Numeric::Integer(
                v + 1,
            )))
        }

        fn register_catalog(
            &self,
            schema: &mut crate::schema::Schema,
            enable_custom_types: bool,
        ) -> crate::Result<()> {
            sqlite::register_builtin_catalog(schema, enable_custom_types)?;
            let vtab = crate::VirtualTable::new_internal(
                "test_catalog".to_string(),
                "CREATE TABLE test_catalog (value INTEGER)".to_string(),
                turso_ext::VTabKind::VirtualTable,
                Arc::new(crate::sync::RwLock::new(TestCatalogTable)),
            )?;
            schema.add_virtual_table(Arc::new(vtab))
        }
    }

    /// Stores table definitions in syntax that SQLite cannot parse and always
    /// adds its marker, so replay tests detect repeated storage formatting.
    struct StrictTestDialect;

    impl StrictTestDialect {
        const PREFIX: &'static str = "strict: ";
    }

    impl Dialect for StrictTestDialect {
        fn name(&self) -> &'static str {
            "strict-test"
        }

        fn parse(&self, sql: &str) -> crate::Result<(Option<turso_parser::ast::Cmd>, usize)> {
            sqlite::parse(sql)
        }

        fn parse_table_sql(&self, sql: &str, root_page: i64) -> crate::Result<BTreeTable> {
            let sql = sql.strip_prefix(Self::PREFIX).unwrap_or(sql);
            BTreeTable::from_sql(sql, root_page)
        }

        fn parse_table_sql_ast(&self, sql: &str) -> crate::Result<turso_parser::ast::Stmt> {
            let sql = sql.strip_prefix(Self::PREFIX).unwrap_or(sql);
            sqlite::parse_table_sql_ast(sql)
        }

        fn table_sql_for_replay(&self, sql: &str) -> crate::Result<String> {
            let sql = sql.strip_prefix(Self::PREFIX).unwrap_or(sql);
            sqlite::table_sql_for_replay(sql)
        }

        fn parse_schema_sql(
            &self,
            kind: SchemaSqlKind,
            sql: &str,
        ) -> crate::Result<turso_parser::ast::Stmt> {
            let sql = sql.strip_prefix(Self::PREFIX).unwrap_or(sql);
            Dialect::parse_schema_sql(&SqliteDialect, kind, sql)
        }

        fn schema_sql_for_replay(&self, kind: SchemaSqlKind, sql: &str) -> crate::Result<String> {
            let sql = sql.strip_prefix(Self::PREFIX).unwrap_or(sql);
            Dialect::schema_sql_for_replay(&SqliteDialect, kind, sql)
        }

        fn format_table_sql(
            &self,
            input: &str,
            _tbl_name: &turso_parser::ast::QualifiedName,
            _body: &turso_parser::ast::CreateTableBody,
        ) -> crate::Result<String> {
            Ok(format!("{}{input}", Self::PREFIX))
        }

        fn format_schema_sql(
            &self,
            kind: SchemaSqlKind,
            input: &str,
            stmt: &turso_parser::ast::Stmt,
        ) -> crate::Result<String> {
            if kind == SchemaSqlKind::Table {
                let turso_parser::ast::Stmt::CreateTable { tbl_name, body, .. } = stmt else {
                    return Err(crate::LimboError::InternalError(
                        "table schema formatter received a non-table statement".to_string(),
                    ));
                };
                return self.format_table_sql(input, tbl_name, body);
            }
            super::ensure_schema_sql_kind_for_format(kind, stmt)?;
            Ok(format!("{}{input}", Self::PREFIX))
        }

        fn format_rewritten_schema_sql(
            &self,
            kind: SchemaSqlKind,
            _previous_sql: &str,
            stmt: &turso_parser::ast::Stmt,
        ) -> crate::Result<String> {
            super::ensure_schema_sql_kind_for_format(kind, stmt)?;
            Ok(format!("{}{stmt}", Self::PREFIX))
        }

        fn register_catalog(
            &self,
            schema: &mut crate::schema::Schema,
            enable_custom_types: bool,
        ) -> crate::Result<()> {
            sqlite::register_builtin_catalog(schema, enable_custom_types)
        }

        fn resolve_function(
            &self,
            name: &str,
            arg_count: usize,
        ) -> crate::Result<Option<crate::function::Func>> {
            sqlite::resolve_builtin_function(name, arg_count)
        }
    }

    struct StrictSchemaSqlFormatter;

    impl SchemaSqlFormatter for StrictSchemaSqlFormatter {
        fn format_schema_sql(
            &self,
            _kind: SchemaSqlKind,
            input: &str,
            _stmt: &turso_parser::ast::Stmt,
        ) -> crate::Result<String> {
            Ok(format!("{}{input}", StrictTestDialect::PREFIX))
        }

        fn format_rewritten_schema_sql(
            &self,
            _kind: SchemaSqlKind,
            _previous_sql: &str,
            stmt: &turso_parser::ast::Stmt,
        ) -> crate::Result<String> {
            Ok(format!("{}{stmt}", StrictTestDialect::PREFIX))
        }
    }

    struct RecordingSchemaSqlFormatter {
        rewritten_previous_sql: Arc<Mutex<Vec<String>>>,
    }

    impl SchemaSqlFormatter for RecordingSchemaSqlFormatter {
        fn format_schema_sql(
            &self,
            _kind: SchemaSqlKind,
            input: &str,
            _stmt: &turso_parser::ast::Stmt,
        ) -> crate::Result<String> {
            Ok(format!("{}{input}", StrictTestDialect::PREFIX))
        }

        fn format_rewritten_schema_sql(
            &self,
            _kind: SchemaSqlKind,
            previous_sql: &str,
            stmt: &turso_parser::ast::Stmt,
        ) -> crate::Result<String> {
            self.rewritten_previous_sql
                .lock()
                .unwrap()
                .push(previous_sql.to_string());
            Ok(format!("{}{stmt}", StrictTestDialect::PREFIX))
        }
    }

    /// A one-row catalog table installed by [`TestDialect`],
    /// standing in for a frontend catalog surface like `pg_class`.
    #[derive(Debug)]
    struct TestCatalogTable;

    impl crate::InternalVirtualTable for TestCatalogTable {
        fn name(&self) -> String {
            "test_catalog".to_string()
        }

        fn sql(&self) -> String {
            "CREATE TABLE test_catalog (value INTEGER)".to_string()
        }

        fn open(
            &self,
            _conn: Arc<crate::Connection>,
        ) -> crate::Result<Arc<crate::sync::RwLock<dyn crate::InternalVirtualTableCursor>>>
        {
            Ok(Arc::new(crate::sync::RwLock::new(TestCatalogCursor {
                row: 0,
            })))
        }

        fn best_index(
            &self,
            constraints: &[turso_ext::ConstraintInfo],
            _order_by: &[turso_ext::OrderByInfo],
        ) -> std::result::Result<turso_ext::IndexInfo, turso_ext::ResultCode> {
            Ok(turso_ext::IndexInfo {
                idx_num: 0,
                idx_str: None,
                order_by_consumed: false,
                estimated_cost: 1.0,
                estimated_rows: 1,
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

    struct TestCatalogCursor {
        row: usize,
    }

    impl crate::InternalVirtualTableCursor for TestCatalogCursor {
        fn filter(
            &mut self,
            _args: &[crate::Value],
            _idx_str: Option<String>,
            _idx_num: i32,
        ) -> crate::Result<bool> {
            self.row = 0;
            Ok(true)
        }

        fn next(&mut self) -> crate::Result<bool> {
            self.row += 1;
            Ok(self.row < 1)
        }

        fn rowid(&self) -> i64 {
            self.row as i64
        }

        fn column(&self, column: usize) -> crate::Result<crate::Value> {
            match column {
                0 => Ok(crate::Value::Numeric(crate::numeric::Numeric::Integer(42))),
                _ => Ok(crate::Value::Null),
            }
        }
    }

    fn open_db(
        io: &Arc<dyn IO>,
        path: &str,
        dialect: Arc<dyn Dialect>,
    ) -> crate::Result<Arc<Database>> {
        open_db_with_options(io, path, dialect, DatabaseOpts::new())
    }

    fn open_db_with_options(
        io: &Arc<dyn IO>,
        path: &str,
        dialect: Arc<dyn Dialect>,
        options: DatabaseOpts,
    ) -> crate::Result<Arc<Database>> {
        let file = io.open_file(path, OpenFlags::Create, true)?;
        let db_file = Arc::new(DatabaseFile::new(file));
        Database::open(
            io.clone(),
            path,
            crate::OpenOptions::new(dialect)
                .storage(db_file)
                .db_opts(options),
        )
    }

    fn execute_with_schema_formatter(
        conn: &Arc<crate::Connection>,
        sql: &str,
        formatter: Arc<dyn SchemaSqlFormatter>,
    ) {
        let (cmd, _) = sqlite::parse(sql).unwrap();
        let options = PrepareOptions::default().with_schema_sql_formatter(formatter);
        conn.prepare_translated_cmd_with_options(cmd.unwrap(), sql, &options)
            .unwrap()
            .run_ignore_rows()
            .unwrap();
    }

    #[test]
    fn schema_sql_contract_rejects_an_object_kind_mismatch() {
        let table_sql = "CREATE TABLE t (x INTEGER)";
        let table_stmt = sqlite::parse_table_sql_ast(table_sql).unwrap();

        assert!(matches!(
            SqliteDialect.parse_schema_sql(SchemaSqlKind::Index, table_sql),
            Err(crate::LimboError::Corrupt(_))
        ));
        assert!(matches!(
            SqliteDialect.format_schema_sql(SchemaSqlKind::Index, table_sql, &table_stmt),
            Err(crate::LimboError::InternalError(_))
        ));
    }

    fn owned_test_dialect(owner: DatabaseFileOwner) -> Arc<TestDialect> {
        Arc::new(TestDialect {
            owner,
            ..TestDialect::default()
        })
    }

    fn owned_test_dialect_with_marker(
        owner: DatabaseFileOwner,
        application_id: i32,
    ) -> Arc<TestDialect> {
        Arc::new(TestDialect {
            owner,
            application_id: Some(Some(application_id)),
            ..TestDialect::default()
        })
    }

    fn owned_test_dialect_without_marker(owner: DatabaseFileOwner) -> Arc<TestDialect> {
        Arc::new(TestDialect {
            owner,
            application_id: Some(None),
            ..TestDialect::default()
        })
    }

    #[test]
    fn durable_file_owner_rejects_wrong_dialects_before_schema_parse() {
        for (path, populate) in [("owned-empty.db", false), ("owned-populated.db", true)] {
            let io: Arc<dyn IO> = Arc::new(MemoryIO::new());
            let owner = owned_test_dialect(DatabaseFileOwner::MySql);
            let db = open_db(&io, path, owner).unwrap();
            assert!(db.db_file.size().unwrap() > 0);
            let conn = db.connect().unwrap();
            if populate {
                conn.execute("CREATE TABLE owned(value)").unwrap();
            }
            let rows = conn
                .prepare("PRAGMA application_id")
                .unwrap()
                .run_collect_rows()
                .unwrap();
            assert_eq!(
                rows,
                vec![vec![crate::Value::from_i64(
                    DatabaseFileOwner::MySql.application_id().unwrap() as i64
                )]]
            );
            conn.close().unwrap();
            drop(conn);
            drop(db);

            let error = open_db(&io, path, Arc::new(SqliteDialect)).unwrap_err();
            assert!(matches!(
                error,
                crate::LimboError::WrongDatabaseDialect {
                    requested: "sqlite-compatible",
                    actual: "mysql"
                }
            ));

            let wrong = owned_test_dialect(DatabaseFileOwner::Postgres);
            let error = open_db(&io, path, wrong.clone()).unwrap_err();
            assert!(matches!(
                error,
                crate::LimboError::WrongDatabaseDialect {
                    requested: "postgres",
                    actual: "mysql"
                }
            ));
            assert_eq!(wrong.parse_calls.load(Ordering::SeqCst), 0);

            let reopened =
                open_db(&io, path, owned_test_dialect(DatabaseFileOwner::MySql)).unwrap();
            reopened.connect().unwrap().close().unwrap();
        }
    }

    #[test]
    fn owned_database_rejects_unmarked_files_and_application_id_writes() {
        let io: Arc<dyn IO> = Arc::new(MemoryIO::new());
        let path = "owned-unmarked.db";
        {
            let db = open_db(&io, path, Arc::new(SqliteDialect)).unwrap();
            let conn = db.connect().unwrap();
            conn.execute("CREATE TABLE legacy(value)").unwrap();
            conn.close().unwrap();
        }

        let error = open_db(&io, path, owned_test_dialect(DatabaseFileOwner::MySql)).unwrap_err();
        assert!(matches!(
            error,
            crate::LimboError::MissingDatabaseDialectMarker { requested: "mysql" }
        ));

        let owned_path = "owned-pragma-guard.db";
        let db = open_db(
            &io,
            owned_path,
            owned_test_dialect(DatabaseFileOwner::MySql),
        )
        .unwrap();
        let conn = db.connect().unwrap();
        let error = conn.execute("PRAGMA application_id = 0").unwrap_err();
        assert!(error.to_string().contains("reserved"));
        let rows = conn
            .prepare("PRAGMA application_id")
            .unwrap()
            .run_collect_rows()
            .unwrap();
        assert_eq!(
            rows,
            vec![vec![crate::Value::from_i64(
                DatabaseFileOwner::MySql.application_id().unwrap() as i64
            )]]
        );
        conn.close().unwrap();
    }

    #[cfg(feature = "fs")]
    #[test]
    fn registry_rejects_an_already_open_mysql_database_with_a_different_policy_marker() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mysql-policy-registry.db");
        let path = path.to_str().unwrap();
        let io: Arc<dyn IO> = Arc::new(PlatformIO::new().unwrap());
        let expected_marker = DatabaseFileOwner::mysql_application_id(
            DatabaseFileOwner::MYSQL_LOWER_CASE_TABLE_NAMES,
        );
        let db = open_db(
            &io,
            path,
            owned_test_dialect_with_marker(DatabaseFileOwner::MySql, expected_marker),
        )
        .unwrap();

        let error = open_db(
            &io,
            path,
            owned_test_dialect_with_marker(
                DatabaseFileOwner::MySql,
                DatabaseFileOwner::mysql_application_id(0),
            ),
        )
        .unwrap_err();
        assert!(matches!(
            error,
            crate::LimboError::InvalidArgument(message)
                if message.contains("marker")
        ));
        drop(db);
    }

    #[cfg(feature = "fs")]
    #[test]
    fn fresh_owned_file_rejects_an_invalid_dialect_marker_without_writing() {
        fn files(path: &std::path::Path) -> Vec<Option<Vec<u8>>> {
            [
                path.to_path_buf(),
                std::path::PathBuf::from(format!("{}-wal", path.display())),
                std::path::PathBuf::from(format!("{}-shm", path.display())),
                std::path::PathBuf::from(format!("{}-journal", path.display())),
            ]
            .into_iter()
            .map(|path| std::fs::read(path).ok())
            .collect()
        }

        let dir = tempfile::tempdir().unwrap();
        let io: Arc<dyn IO> = Arc::new(PlatformIO::new().unwrap());
        let cases = [
            (
                "mysql-policy-2.db",
                DatabaseFileOwner::MySql,
                owned_test_dialect_with_marker(
                    DatabaseFileOwner::MySql,
                    DatabaseFileOwner::mysql_application_id(2),
                ),
            ),
            (
                "mysql-no-marker.db",
                DatabaseFileOwner::MySql,
                owned_test_dialect_without_marker(DatabaseFileOwner::MySql),
            ),
            (
                "postgres-mysql-marker.db",
                DatabaseFileOwner::Postgres,
                owned_test_dialect_with_marker(
                    DatabaseFileOwner::Postgres,
                    DatabaseFileOwner::mysql_application_id(
                        DatabaseFileOwner::MYSQL_LOWER_CASE_TABLE_NAMES,
                    ),
                ),
            ),
        ];

        for (name, owner, invalid_dialect) in cases {
            let path = dir.path().join(name);
            std::fs::File::create(&path).unwrap();
            let before = files(&path);

            let error = open_db(&io, path.to_str().unwrap(), invalid_dialect).unwrap_err();
            assert!(matches!(error, crate::LimboError::InvalidArgument(_)));
            assert_eq!(files(&path), before, "{name} changed after rejection");

            let db = open_db(&io, path.to_str().unwrap(), owned_test_dialect(owner)).unwrap();
            let conn = db.connect().unwrap();
            assert_eq!(
                conn.prepare("PRAGMA application_id")
                    .unwrap()
                    .run_collect_rows()
                    .unwrap(),
                vec![vec![crate::Value::from_i64(
                    owner.application_id().unwrap() as i64
                )]]
            );
            conn.close().unwrap();
            drop(conn);
            drop(db);

            let reopened = open_db(&io, path.to_str().unwrap(), owned_test_dialect(owner)).unwrap();
            reopened.connect().unwrap().close().unwrap();
        }
    }

    #[cfg(feature = "fs")]
    #[test]
    fn unknown_turso_file_owner_version_fails_closed() {
        use std::io::{Seek, SeekFrom, Write};

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("unknown-owner-version.db");
        let unsupported = DatabaseFileOwner::APPLICATION_ID_PREFIX | (2 << 8) | 2;
        {
            let io: Arc<dyn IO> = Arc::new(PlatformIO::new().unwrap());
            let db = open_db(&io, path.to_str().unwrap(), Arc::new(SqliteDialect)).unwrap();
            let conn = db.connect().unwrap();
            conn.execute("CREATE TABLE initialized(value)").unwrap();
            conn.execute("PRAGMA wal_checkpoint(TRUNCATE)").unwrap();
            conn.close().unwrap();
        }

        let mut file = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
        file.seek(SeekFrom::Start(68)).unwrap();
        file.write_all(&unsupported.to_be_bytes()).unwrap();
        file.sync_all().unwrap();

        let io: Arc<dyn IO> = Arc::new(PlatformIO::new().unwrap());
        let error = open_db(&io, path.to_str().unwrap(), Arc::new(SqliteDialect)).unwrap_err();
        assert!(matches!(
            error,
            crate::LimboError::UnsupportedDatabaseDialectMarker { marker }
                if marker == unsupported
        ));
    }

    #[test]
    fn sqlite_application_id_cannot_spoof_a_frontend_owner() {
        let io: Arc<dyn IO> = Arc::new(MemoryIO::new());
        let db = open_db(&io, "owner-spoof.db", Arc::new(SqliteDialect)).unwrap();
        let conn = db.connect().unwrap();
        let marker = DatabaseFileOwner::MySql.application_id().unwrap();

        let error = conn
            .execute(format!("PRAGMA application_id = {marker}"))
            .unwrap_err();
        assert!(error.to_string().contains("reserved"));
        assert_eq!(
            conn.prepare("PRAGMA application_id")
                .unwrap()
                .run_collect_rows()
                .unwrap(),
            vec![vec![crate::Value::from_i64(0)]]
        );
        conn.close().unwrap();
    }

    #[cfg(feature = "fs")]
    #[test]
    fn readonly_zero_byte_database_cannot_be_claimed() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("readonly-empty.db");
        std::fs::File::create(&path).unwrap();
        let path = path.to_str().unwrap();
        let io: Arc<dyn IO> = Arc::new(PlatformIO::new().unwrap());
        let file = io.open_file(path, OpenFlags::ReadOnly, true).unwrap();
        let error = Database::open(
            io,
            path,
            crate::OpenOptions::new(owned_test_dialect(DatabaseFileOwner::MySql))
                .storage(Arc::new(DatabaseFile::new(file)))
                .flags(OpenFlags::ReadOnly),
        )
        .unwrap_err();

        assert!(matches!(error, crate::LimboError::ReadOnly));
        assert_eq!(std::fs::metadata(path).unwrap().len(), 0);
    }

    #[cfg(all(feature = "fs", feature = "conn_raw_api"))]
    #[test]
    fn raw_open_enforces_durable_file_owner() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("raw-owner.db");
        let path = path.to_str().unwrap();
        let io: Arc<dyn IO> = Arc::new(PlatformIO::new().unwrap());
        {
            open_db(&io, path, owned_test_dialect(DatabaseFileOwner::MySql)).unwrap();
        }

        let file = io.open_file(path, OpenFlags::default(), true).unwrap();
        let error = Database::do_open(
            io,
            path,
            crate::OpenOptions::new(Arc::new(SqliteDialect))
                .storage(Arc::new(DatabaseFile::new(file))),
        )
        .unwrap_err();
        assert!(matches!(
            error,
            crate::LimboError::WrongDatabaseDialect {
                requested: "sqlite-compatible",
                actual: "mysql"
            }
        ));
    }

    #[test]
    fn durable_file_owner_survives_checkpoint_vacuum_and_reopen() {
        let io: Arc<dyn IO> = Arc::new(MemoryIO::new());
        let path = "owned-vacuum.db";
        {
            let db = open_db_with_options(
                &io,
                path,
                owned_test_dialect(DatabaseFileOwner::MySql),
                DatabaseOpts::new().with_vacuum(true),
            )
            .unwrap();
            let conn = db.connect().unwrap();
            conn.execute("CREATE TABLE owned(value); INSERT INTO owned VALUES (1)")
                .unwrap();
            conn.execute("PRAGMA wal_checkpoint(TRUNCATE)").unwrap();
            conn.execute("VACUUM").unwrap();
            conn.close().unwrap();
        }

        let db = open_db(&io, path, owned_test_dialect(DatabaseFileOwner::MySql)).unwrap();
        let conn = db.connect().unwrap();
        assert_eq!(
            conn.prepare("SELECT value FROM owned")
                .unwrap()
                .run_collect_rows()
                .unwrap(),
            vec![vec![crate::Value::from_i64(1)]]
        );
        conn.close().unwrap();
    }

    #[cfg(feature = "fs")]
    #[test]
    fn durable_file_owner_survives_vacuum_into_and_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let source_path = dir.path().join("owned-vacuum-source.db");
        let output_path = dir.path().join("owned-vacuum-output.db");
        let source_path = source_path.to_str().unwrap();
        let output_path = output_path.to_str().unwrap();
        let io: Arc<dyn IO> = Arc::new(PlatformIO::new().unwrap());

        {
            let db = open_db_with_options(
                &io,
                source_path,
                owned_test_dialect(DatabaseFileOwner::MySql),
                DatabaseOpts::new().with_vacuum(true),
            )
            .unwrap();
            let conn = db.connect().unwrap();
            conn.execute("CREATE TABLE owned(value); INSERT INTO owned VALUES (1)")
                .unwrap();
            let escaped_output_path = output_path.replace('\'', "''");
            conn.execute(format!("VACUUM INTO '{escaped_output_path}'"))
                .unwrap();
            conn.close().unwrap();
        }

        let db = open_db(
            &io,
            output_path,
            owned_test_dialect(DatabaseFileOwner::MySql),
        )
        .unwrap();
        let conn = db.connect().unwrap();
        assert_eq!(
            conn.prepare("SELECT value FROM owned")
                .unwrap()
                .run_collect_rows()
                .unwrap(),
            vec![vec![crate::Value::from_i64(1)]]
        );
        conn.close().unwrap();
        drop(conn);
        drop(db);

        let error = open_db(&io, output_path, Arc::new(SqliteDialect)).unwrap_err();
        assert!(matches!(
            error,
            crate::LimboError::WrongDatabaseDialect {
                requested: "sqlite-compatible",
                actual: "mysql"
            }
        ));
    }

    #[cfg(feature = "fs")]
    #[test]
    fn fresh_owned_attach_persists_owner_for_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let main_path = dir.path().join("owned-attach-main.db");
        let attached_path = dir.path().join("owned-attach-child.db");
        let main_path = main_path.to_str().unwrap();
        let attached_path = attached_path.to_str().unwrap();
        let io: Arc<dyn IO> = Arc::new(PlatformIO::new().unwrap());

        {
            let db = open_db_with_options(
                &io,
                main_path,
                owned_test_dialect(DatabaseFileOwner::MySql),
                DatabaseOpts::new().with_attach(true),
            )
            .unwrap();
            let conn = db.connect().unwrap();
            let escaped_attached_path = attached_path.replace('\'', "''");
            conn.execute(format!("ATTACH DATABASE '{escaped_attached_path}' AS aux"))
                .unwrap();
            conn.execute("CREATE TABLE aux.attached(value); INSERT INTO aux.attached VALUES (2)")
                .unwrap();
            conn.execute("DETACH DATABASE aux").unwrap();
            conn.close().unwrap();
        }

        let db = open_db(
            &io,
            attached_path,
            owned_test_dialect(DatabaseFileOwner::MySql),
        )
        .unwrap();
        let conn = db.connect().unwrap();
        assert_eq!(
            conn.prepare("SELECT value FROM attached")
                .unwrap()
                .run_collect_rows()
                .unwrap(),
            vec![vec![crate::Value::from_i64(2)]]
        );
        conn.close().unwrap();
        drop(conn);
        drop(db);

        let error = open_db(&io, attached_path, Arc::new(SqliteDialect)).unwrap_err();
        assert!(matches!(
            error,
            crate::LimboError::WrongDatabaseDialect {
                requested: "sqlite-compatible",
                actual: "mysql"
            }
        ));
    }

    #[test]
    fn owned_frontend_internal_databases_choose_layout_before_page1_allocation() {
        let io: Arc<dyn IO> = Arc::new(MemoryIO::new());
        let db = open_db_with_options(
            &io,
            "owned-internal-layout.db",
            owned_test_dialect(DatabaseFileOwner::MySql),
            DatabaseOpts::new().with_attach(true),
        )
        .unwrap();
        let conn = db.connect().unwrap();

        conn.execute("CREATE TEMP TABLE scratch(value); INSERT INTO scratch VALUES (1)")
            .unwrap();
        assert_eq!(
            conn.prepare("SELECT value FROM scratch")
                .unwrap()
                .run_collect_rows()
                .unwrap(),
            vec![vec![crate::Value::from_i64(1)]]
        );

        conn.execute("ATTACH DATABASE 'owned-empty-attach.db' AS aux")
            .unwrap();
        conn.execute("DETACH DATABASE aux").unwrap();
        let error = open_db(&io, "owned-empty-attach.db", Arc::new(SqliteDialect)).unwrap_err();
        assert!(matches!(
            error,
            crate::LimboError::WrongDatabaseDialect {
                requested: "sqlite-compatible",
                actual: "mysql"
            }
        ));

        conn.close().unwrap();
    }

    #[test]
    fn schema_load_routes_through_dialect() {
        let io: Arc<dyn IO> = Arc::new(MemoryIO::new());
        {
            let db = open_db(&io, "dialect-load.db", Arc::new(SqliteDialect)).unwrap();
            let conn = db.connect().unwrap();
            conn.execute("CREATE TABLE t (x INTEGER)").unwrap();
            conn.close().unwrap();
        }

        // Reopening the database parses the stored schema row for `t`
        // through the dialect.
        let dialect = Arc::new(TestDialect::default());
        let db = open_db(&io, "dialect-load.db", dialect.clone()).unwrap();
        assert!(dialect.parse_calls.load(Ordering::SeqCst) >= 1);
        assert_eq!(dialect.catalog_validation_calls.load(Ordering::SeqCst), 1);

        // DDL reparses the schema via the ParseSchema opcode, again through
        // the dialect.
        let conn = db.connect().unwrap();
        let before = dialect.parse_calls.load(Ordering::SeqCst);
        let validations_before = dialect.catalog_validation_calls.load(Ordering::SeqCst);
        conn.execute("CREATE TABLE u (y INTEGER)").unwrap();
        assert!(dialect.parse_calls.load(Ordering::SeqCst) > before);
        conn.force_reparse_schema_without_publish().unwrap();
        assert!(dialect.catalog_validation_calls.load(Ordering::SeqCst) > validations_before);

        // Both tables are usable under the dialect.
        conn.execute("INSERT INTO t VALUES (1)").unwrap();
        conn.execute("INSERT INTO u VALUES (2)").unwrap();
        conn.close().unwrap();
    }

    #[test]
    fn catalog_validation_receives_database_context_on_every_schema_load_path() {
        let io: Arc<dyn IO> = Arc::new(MemoryIO::new());
        {
            let db = open_db(&io, "dialect-context.db", Arc::new(SqliteDialect)).unwrap();
            let conn = db.connect().unwrap();
            conn.execute("CREATE TABLE t (x INTEGER)").unwrap();
            conn.close().unwrap();
        }

        let identity = [7; 16];
        let dialect = Arc::new(TestDialect::default());
        let file = io
            .open_file("dialect-context.db", OpenFlags::Create, true)
            .unwrap();
        let db_file = Arc::new(DatabaseFile::new(file));
        let db = Database::open(
            io.clone(),
            "dialect-context.db",
            crate::OpenOptions::new(dialect.clone())
                .storage(db_file)
                .schema_catalog_validation_context(SchemaCatalogValidationContext::new(identity)),
        )
        .unwrap();
        let conn = db.connect().unwrap();
        assert_eq!(
            conn.schema_catalog_validation_context()
                .map(SchemaCatalogValidationContext::database_identity),
            Some(&identity)
        );
        conn.force_reparse_schema_without_publish().unwrap();
        conn.reparse_schema_after_extension_load().unwrap();

        assert_eq!(
            *dialect.catalog_validation_contexts.lock().unwrap(),
            vec![Some(identity), Some(identity), Some(identity)]
        );
        conn.close().unwrap();
    }

    #[test]
    fn catalog_validation_has_no_context_by_default() {
        let io: Arc<dyn IO> = Arc::new(MemoryIO::new());
        let dialect = Arc::new(TestDialect::default());
        let db = open_db(&io, "dialect-default-context.db", dialect.clone()).unwrap();

        assert_eq!(
            *dialect.catalog_validation_contexts.lock().unwrap(),
            vec![None]
        );
        db.connect().unwrap().close().unwrap();
    }

    #[test]
    fn schema_catalog_validation_runs_before_schema_rows_are_loaded() {
        let io: Arc<dyn IO> = Arc::new(MemoryIO::new());
        {
            let db = open_db(
                &io,
                "dialect-catalog-validation.db",
                Arc::new(SqliteDialect),
            )
            .unwrap();
            let conn = db.connect().unwrap();
            conn.execute("CREATE TABLE t (x INTEGER)").unwrap();
            conn.close().unwrap();
        }

        let dialect = Arc::new(TestDialect {
            reject_catalog: true,
            ..TestDialect::default()
        });
        assert!(matches!(
            open_db(&io, "dialect-catalog-validation.db", dialect.clone()),
            Err(crate::LimboError::Corrupt(message)) if message == "test schema catalog rejection"
        ));
        assert_eq!(dialect.catalog_validation_calls.load(Ordering::SeqCst), 1);
        assert_eq!(dialect.parse_calls.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn extension_schema_reparse_validates_catalog_before_mutating_schema() {
        let io: Arc<dyn IO> = Arc::new(MemoryIO::new());
        {
            let db = open_db(&io, "dialect-catalog-restore.db", Arc::new(SqliteDialect)).unwrap();
            let conn = db.connect().unwrap();
            conn.execute("CREATE TABLE t (x INTEGER)").unwrap();
            conn.execute("INSERT INTO t VALUES (7)").unwrap();
            conn.close().unwrap();
        }

        let dialect = Arc::new(TestDialect {
            reject_catalog_after_first_validation: true,
            ..TestDialect::default()
        });
        let db = open_db(&io, "dialect-catalog-restore.db", dialect.clone()).unwrap();
        let conn = db.connect().unwrap();
        assert!(conn.schema.read().get_btree_table("t").is_some());

        conn.with_schema_mut(|schema| schema.remove_table("t"))
            .unwrap();
        assert!(conn.schema.read().get_btree_table("t").is_none());

        assert!(matches!(
            conn.reparse_schema_after_extension_load(),
            Err(crate::LimboError::Corrupt(message)) if message == "test schema catalog rejection"
        ));
        assert_eq!(
            dialect.catalog_validation_calls.load(Ordering::SeqCst),
            2,
            "initial load and extension reparse must each validate the catalog"
        );
        assert!(conn.schema.read().get_btree_table("t").is_none());
    }

    #[test]
    fn failed_full_schema_reparse_restores_connection_schema() {
        let io: Arc<dyn IO> = Arc::new(MemoryIO::new());
        {
            let db = open_db(
                &io,
                "dialect-catalog-full-restore.db",
                Arc::new(SqliteDialect),
            )
            .unwrap();
            let conn = db.connect().unwrap();
            conn.execute("CREATE TABLE t (x INTEGER)").unwrap();
            conn.execute("INSERT INTO t VALUES (7)").unwrap();
            conn.close().unwrap();
        }

        let dialect = Arc::new(TestDialect {
            reject_catalog_after_first_validation: true,
            ..TestDialect::default()
        });
        let db = open_db(&io, "dialect-catalog-full-restore.db", dialect).unwrap();
        let conn = db.connect().unwrap();
        assert!(conn.schema.read().get_btree_table("t").is_some());

        assert!(matches!(
            conn.force_reparse_schema_without_publish(),
            Err(crate::LimboError::Corrupt(message)) if message == "test schema catalog rejection"
        ));
        assert!(conn.schema.read().get_btree_table("t").is_some());
        assert_eq!(
            conn.query("SELECT x FROM t")
                .unwrap()
                .unwrap()
                .run_collect_rows()
                .unwrap(),
            vec![vec![crate::Value::from_i64(7)]]
        );
    }

    #[test]
    fn dialect_parser_is_used_for_reprepare() {
        let io: Arc<dyn IO> = Arc::new(MemoryIO::new());
        let dialect = Arc::new(TestDialect::default());
        let db = open_db(&io, "dialect-reprepare.db", dialect.clone()).unwrap();
        let conn = db.connect().unwrap();

        let mut stmt = conn.prepare("test: SELECT 42").unwrap();
        conn.set_full_column_names(true);
        let rows = stmt.run_collect_rows().unwrap();

        assert_eq!(rows, vec![vec![crate::Value::from_i64(42)]]);
        assert_eq!(
            stmt.stmt_status(crate::StatementStatusCounter::Reprepare),
            1
        );
        assert_eq!(dialect.statement_parse_calls.load(Ordering::SeqCst), 2);
        conn.close().unwrap();
    }

    #[test]
    fn translated_statements_keep_session_parsers_during_schema_reprepare() {
        let io: Arc<dyn IO> = Arc::new(MemoryIO::new());
        let dialect = Arc::new(TestDialect::default());
        let db = open_db(&io, "translated-reprepare.db", dialect.clone()).unwrap();
        let string_conn = db.connect().unwrap();
        let identifier_conn = db.connect().unwrap();
        let schema_conn = db.connect().unwrap();

        string_conn
            .execute("CREATE TABLE reprepare_test (id INTEGER PRIMARY KEY, value TEXT)")
            .unwrap();
        string_conn
            .execute("INSERT INTO reprepare_test VALUES (1, 'row value')")
            .unwrap();

        let sql = r#"SELECT "value", id FROM reprepare_test WHERE id = ?"#;
        let string_sql = "SELECT 'value', id FROM reprepare_test WHERE id = ?";
        let identifier_sql = r#"SELECT "value", id FROM reprepare_test WHERE id = ?"#;
        let (string_cmd, _) = sqlite::parse(string_sql).unwrap();
        let (identifier_cmd, _) = sqlite::parse(identifier_sql).unwrap();
        let string_parser = Arc::new(FrozenParser::new(string_sql));
        let identifier_parser = Arc::new(FrozenParser::new(identifier_sql));
        let string_options = PrepareOptions::default().with_reprepare_parser(string_parser.clone());
        let identifier_options =
            PrepareOptions::default().with_reprepare_parser(identifier_parser.clone());
        let mut string_stmt = string_conn
            .prepare_translated_cmd_with_options(string_cmd.unwrap(), sql, &string_options)
            .unwrap();
        let mut identifier_stmt = identifier_conn
            .prepare_translated_cmd_with_options(identifier_cmd.unwrap(), sql, &identifier_options)
            .unwrap();
        assert!(!string_stmt
            .get_program()
            .prepared()
            .is_compatible_with(&string_conn));
        assert!(!identifier_stmt
            .get_program()
            .prepared()
            .is_compatible_with(&identifier_conn));
        string_stmt
            .bind_at(1.try_into().unwrap(), crate::Value::from_i64(1))
            .unwrap();
        identifier_stmt
            .bind_at(1.try_into().unwrap(), crate::Value::from_i64(1))
            .unwrap();

        assert_eq!(
            string_stmt.run_collect_rows().unwrap(),
            vec![vec![
                crate::Value::build_text("value"),
                crate::Value::from_i64(1)
            ]]
        );
        assert_eq!(
            identifier_stmt.run_collect_rows().unwrap(),
            vec![vec![
                crate::Value::build_text("row value"),
                crate::Value::from_i64(1)
            ]]
        );
        assert_eq!(
            string_stmt.stmt_status(crate::StatementStatusCounter::Reprepare),
            0
        );
        assert_eq!(
            identifier_stmt.stmt_status(crate::StatementStatusCounter::Reprepare),
            0
        );
        string_stmt.reset().unwrap();
        identifier_stmt.reset().unwrap();

        schema_conn
            .execute("ALTER TABLE reprepare_test ADD COLUMN marker INTEGER")
            .unwrap();
        let dialect_calls_before_reprepare = dialect.statement_parse_calls.load(Ordering::SeqCst);
        let string_rows = string_stmt.run_collect_rows().unwrap();
        let identifier_rows = identifier_stmt.run_collect_rows().unwrap();

        assert_eq!(
            string_rows,
            vec![vec![
                crate::Value::build_text("value"),
                crate::Value::from_i64(1)
            ]]
        );
        assert_eq!(
            identifier_rows,
            vec![vec![
                crate::Value::build_text("row value"),
                crate::Value::from_i64(1)
            ]]
        );
        assert_eq!(
            string_stmt.stmt_status(crate::StatementStatusCounter::Reprepare),
            1
        );
        assert_eq!(
            identifier_stmt.stmt_status(crate::StatementStatusCounter::Reprepare),
            1
        );
        assert_eq!(string_parser.parse_calls.load(Ordering::SeqCst), 1);
        assert_eq!(identifier_parser.parse_calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            dialect.statement_parse_calls.load(Ordering::SeqCst),
            dialect_calls_before_reprepare
        );

        string_stmt.reset().unwrap();
        identifier_stmt.reset().unwrap();
        assert_eq!(
            string_stmt.run_collect_rows().unwrap(),
            vec![vec![
                crate::Value::build_text("value"),
                crate::Value::from_i64(1)
            ]]
        );
        assert_eq!(
            identifier_stmt.run_collect_rows().unwrap(),
            vec![vec![
                crate::Value::build_text("row value"),
                crate::Value::from_i64(1)
            ]]
        );
        assert_eq!(
            string_stmt.stmt_status(crate::StatementStatusCounter::Reprepare),
            1
        );
        assert_eq!(
            identifier_stmt.stmt_status(crate::StatementStatusCounter::Reprepare),
            1
        );
        assert_eq!(string_parser.parse_calls.load(Ordering::SeqCst), 1);
        assert_eq!(identifier_parser.parse_calls.load(Ordering::SeqCst), 1);

        schema_conn.close().unwrap();
        identifier_conn.close().unwrap();
        string_conn.close().unwrap();
    }

    #[test]
    fn rebound_prepared_program_keeps_its_reprepare_parser() {
        let io: Arc<dyn IO> = Arc::new(MemoryIO::new());
        let db = open_db(
            &io,
            "rebound-translated-reprepare.db",
            Arc::new(TestDialect::default()),
        )
        .unwrap();
        let conn = db.connect().unwrap();
        let schema_conn = db.connect().unwrap();

        conn.execute("CREATE TABLE reprepare_test (id INTEGER PRIMARY KEY, value TEXT)")
            .unwrap();
        conn.execute("INSERT INTO reprepare_test VALUES (1, 'row value')")
            .unwrap();

        let sql = r#"SELECT "value", id FROM reprepare_test WHERE id = ?"#;
        let translated_sql = "SELECT 'value', id FROM reprepare_test WHERE id = ?";
        let (cmd, _) = sqlite::parse(translated_sql).unwrap();
        let parser = Arc::new(FrozenParser::new(translated_sql));
        let options = PrepareOptions::default().with_reprepare_parser(parser.clone());
        let original = conn
            .prepare_translated_cmd_with_options(cmd.unwrap(), sql, &options)
            .unwrap();
        let prepared = original.get_program().prepared().clone();
        let query_mode = original.get_query_mode();
        assert!(!prepared.is_compatible_with(&conn));
        drop(original);

        let program = crate::Program::from_prepared(prepared, conn.clone());
        let mut rebound = crate::Statement::new(program, conn.get_pager(), query_mode, 0);
        rebound
            .bind_at(1.try_into().unwrap(), crate::Value::from_i64(1))
            .unwrap();

        schema_conn
            .execute("ALTER TABLE reprepare_test ADD COLUMN marker INTEGER")
            .unwrap();
        assert_eq!(
            rebound.run_collect_rows().unwrap(),
            vec![vec![
                crate::Value::build_text("value"),
                crate::Value::from_i64(1)
            ]]
        );
        assert_eq!(parser.parse_calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            rebound.stmt_status(crate::StatementStatusCounter::Reprepare),
            1
        );

        schema_conn.close().unwrap();
        conn.close().unwrap();
    }

    #[test]
    fn reprepare_rejects_a_changed_query_mode() {
        let io: Arc<dyn IO> = Arc::new(MemoryIO::new());
        let db = open_db(
            &io,
            "reprepare-query-mode.db",
            Arc::new(TestDialect::default()),
        )
        .unwrap();
        let conn = db.connect().unwrap();
        let schema_conn = db.connect().unwrap();

        conn.execute("CREATE TABLE reprepare_test (id INTEGER PRIMARY KEY, value TEXT)")
            .unwrap();
        let sql = r#"SELECT "value", id FROM reprepare_test WHERE id = ?"#;
        let translated_sql = "SELECT 'value', id FROM reprepare_test WHERE id = ?";
        let (cmd, _) = sqlite::parse(translated_sql).unwrap();
        let parser = Arc::new(FrozenParser::new(
            "EXPLAIN SELECT 'value', id FROM reprepare_test WHERE id = ?",
        ));
        let options = PrepareOptions::default().with_reprepare_parser(parser.clone());
        let mut stmt = conn
            .prepare_translated_cmd_with_options(cmd.unwrap(), sql, &options)
            .unwrap();
        stmt.bind_at(1.try_into().unwrap(), crate::Value::from_i64(1))
            .unwrap();

        schema_conn
            .execute("ALTER TABLE reprepare_test ADD COLUMN marker INTEGER")
            .unwrap();
        let error = stmt.run_collect_rows().unwrap_err();

        assert!(
            error.to_string().contains("changed query mode"),
            "unexpected error: {error}"
        );
        assert_eq!(parser.parse_calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            stmt.stmt_status(crate::StatementStatusCounter::Reprepare),
            0
        );

        schema_conn.close().unwrap();
        conn.close().unwrap();
    }

    #[test]
    fn translated_statement_keeps_search_path_during_reprepare() {
        let io: Arc<dyn IO> = Arc::new(MemoryIO::new());
        let db = open_db_with_options(
            &io,
            "translated-search-path.db",
            Arc::new(SqliteDialect),
            DatabaseOpts::new().with_attach(true),
        )
        .unwrap();
        let conn = db.connect().unwrap();
        let schema_conn = db.connect().unwrap();

        conn.execute("CREATE TABLE target (value INTEGER)").unwrap();
        conn.execute("INSERT INTO target VALUES (1)").unwrap();
        conn.execute("ATTACH DATABASE 'search-path-aux.db' AS aux")
            .unwrap();
        conn.execute("CREATE TABLE aux.target (value INTEGER)")
            .unwrap();
        conn.execute("INSERT INTO aux.target VALUES (2)").unwrap();
        schema_conn
            .execute("ATTACH DATABASE 'search-path-aux.db' AS aux")
            .unwrap();

        let sql = "SELECT value FROM target";
        let (cmd, _) = sqlite::parse(sql).unwrap();
        let options = PrepareOptions::default()
            .with_unqualified_database_search_path(Some(vec!["aux".to_string()]));
        let mut stmt = conn
            .prepare_translated_cmd_with_options(cmd.unwrap(), sql, &options)
            .unwrap();

        assert_eq!(
            stmt.run_collect_rows().unwrap(),
            vec![vec![crate::Value::from_i64(2)]]
        );
        assert_eq!(
            stmt.stmt_status(crate::StatementStatusCounter::Reprepare),
            0
        );
        stmt.reset().unwrap();

        schema_conn
            .execute("CREATE TABLE aux.schema_bump (marker INTEGER)")
            .unwrap();
        let rows = stmt.run_collect_rows().unwrap();

        assert_eq!(rows, vec![vec![crate::Value::from_i64(2)]]);
        assert_eq!(
            stmt.stmt_status(crate::StatementStatusCounter::Reprepare),
            1
        );
        stmt.reset().unwrap();
        assert_eq!(
            stmt.run_collect_rows().unwrap(),
            vec![vec![crate::Value::from_i64(2)]]
        );
        assert_eq!(
            stmt.stmt_status(crate::StatementStatusCounter::Reprepare),
            1
        );
        schema_conn.close().unwrap();
        conn.close().unwrap();
    }

    #[test]
    fn query_runner_reports_invalid_utf8_once() {
        let io: Arc<dyn IO> = Arc::new(MemoryIO::new());
        let db = open_db(&io, "query-runner-invalid-utf8.db", Arc::new(SqliteDialect)).unwrap();
        let conn = db.connect().unwrap();
        let mut runner = conn.query_runner(b"SELECT 1;\xff");

        let Some(Err(crate::LimboError::ParseError(message))) = runner.next() else {
            panic!("invalid UTF-8 must produce a parse error");
        };
        assert!(message.contains("invalid UTF-8"));
        assert!(runner.next().is_none());
        conn.close().unwrap();
    }

    #[test]
    fn query_runner_reports_parse_error_once() {
        let io: Arc<dyn IO> = Arc::new(MemoryIO::new());
        let db = open_db(&io, "query-runner-parse-error.db", Arc::new(SqliteDialect)).unwrap();
        let conn = db.connect().unwrap();
        let mut runner = conn.query_runner(b"SELECT * FROM");

        assert!(runner.next().is_some_and(|result| result.is_err()));
        assert!(runner.next().is_none());
        conn.close().unwrap();
    }

    #[test]
    fn dialect_catalog_available_on_every_schema_and_rebuild() {
        let io: Arc<dyn IO> = Arc::new(MemoryIO::new());
        let db = open_db(&io, "dialect-catalog.db", Arc::new(TestDialect::default())).unwrap();

        let query_catalog = |conn: &Arc<crate::Connection>| -> Vec<Vec<crate::Value>> {
            conn.prepare("SELECT value FROM test_catalog")
                .unwrap()
                .run_collect_rows()
                .unwrap()
        };

        let conn1 = db.connect().unwrap();
        let conn2 = db.connect().unwrap();
        assert_eq!(
            query_catalog(&conn1),
            vec![vec![crate::Value::Numeric(
                crate::numeric::Numeric::Integer(42)
            )]]
        );
        assert_eq!(
            query_catalog(&conn2),
            vec![vec![crate::Value::Numeric(
                crate::numeric::Numeric::Integer(42)
            )]]
        );

        // DDL on another connection forces conn1 to rebuild its schema from
        // sqlite_schema; the catalog table must survive because schema
        // construction re-registers it.
        conn2.execute("CREATE TABLE t (x INTEGER)").unwrap();
        assert_eq!(
            query_catalog(&conn1),
            vec![vec![crate::Value::Numeric(
                crate::numeric::Numeric::Integer(42)
            )]]
        );

        conn1.close().unwrap();
        conn2.close().unwrap();
    }

    #[test]
    fn dialect_catalog_cannot_be_dropped() {
        let io: Arc<dyn IO> = Arc::new(MemoryIO::new());
        let db = open_db(
            &io,
            "dialect-catalog-drop.db",
            Arc::new(TestDialect::default()),
        )
        .unwrap();
        let conn = db.connect().unwrap();

        let error = conn.execute("DROP TABLE test_catalog").unwrap_err();
        assert!(
            error
                .to_string()
                .contains("table test_catalog may not be dropped"),
            "unexpected error: {error}"
        );

        let new_conn = db.connect().unwrap();
        for catalog_conn in [&conn, &new_conn] {
            let rows = catalog_conn
                .prepare("SELECT value FROM test_catalog")
                .unwrap()
                .run_collect_rows()
                .unwrap();
            assert_eq!(rows, vec![vec![crate::Value::from_i64(42)]]);
        }
        conn.close().unwrap();
        new_conn.close().unwrap();
    }

    #[test]
    fn dialect_catalog_survives_mvcc_recovery() {
        let io: Arc<dyn IO> = Arc::new(MemoryIO::new());
        let path = "dialect-catalog-mvcc-recovery.db";

        {
            let db = open_db(&io, path, Arc::new(TestDialect::default())).unwrap();
            let conn = db.connect().unwrap();
            conn.execute("PRAGMA journal_mode = mvcc").unwrap();
            conn.execute("CREATE TABLE t (x INTEGER)").unwrap();
            conn.close().unwrap();
        }

        let db = open_db(&io, path, Arc::new(TestDialect::default())).unwrap();
        let conn = db.connect().unwrap();
        let rows = conn
            .prepare("SELECT value FROM test_catalog")
            .unwrap()
            .run_collect_rows()
            .unwrap();
        assert_eq!(rows, vec![vec![crate::Value::from_i64(42)]]);
        conn.close().unwrap();
    }

    #[test]
    fn dialect_catalog_available_in_initialized_temp_schema() {
        let io: Arc<dyn IO> = Arc::new(MemoryIO::new());
        let db = open_db(
            &io,
            "dialect-catalog-temp.db",
            Arc::new(TestDialect::default()),
        )
        .unwrap();

        for temp_store in ["MEMORY", "FILE"] {
            let conn = db.connect().unwrap();
            conn.execute(format!("PRAGMA temp_store = {temp_store}"))
                .unwrap();
            conn.execute("CREATE TEMP TABLE t (x INTEGER)").unwrap();
            let rows = conn
                .prepare("SELECT value FROM temp.test_catalog")
                .unwrap()
                .run_collect_rows()
                .unwrap();
            assert_eq!(rows, vec![vec![crate::Value::from_i64(42)]]);
            conn.close().unwrap();
        }
    }

    #[test]
    fn create_table_stores_dialect_formatted_sql() {
        let io: Arc<dyn IO> = Arc::new(MemoryIO::new());
        {
            let dialect = Arc::new(TestDialect::default());
            let db = open_db(&io, "dialect-store.db", dialect).unwrap();
            let conn = db.connect().unwrap();

            // A frontend prepares its translated AST while supplying the
            // original statement text.
            let input = "CREATE TABLE t (x INTEGER)";
            let stmt = match turso_parser::parser::Parser::new(input.as_bytes())
                .next_cmd()
                .unwrap()
                .unwrap()
            {
                turso_parser::ast::Cmd::Stmt(stmt) => stmt,
                other => panic!("unexpected command: {other:?}"),
            };
            conn.prepare_translated_stmt(stmt, input)
                .unwrap()
                .run_ignore_rows()
                .unwrap();

            // The stored schema row carries the dialect marker and the
            // original text.
            let rows = conn
                .prepare("SELECT sql FROM sqlite_schema WHERE name = 't'")
                .unwrap()
                .run_collect_rows()
                .unwrap();
            assert_eq!(rows.len(), 1);
            let stored = rows[0][0].to_string();
            assert_eq!(stored.trim_matches('\''), format!("/* test */ {input}"));
            conn.close().unwrap();
        }

        // Round-trip: reopening parses the marked row back through the
        // dialect and the table stays usable.
        let dialect = Arc::new(TestDialect::default());
        let db = open_db(&io, "dialect-store.db", dialect.clone()).unwrap();
        assert!(dialect.parse_calls.load(Ordering::SeqCst) >= 1);
        let conn = db.connect().unwrap();
        conn.execute("INSERT INTO t VALUES (1)").unwrap();
        let rows = conn
            .prepare("SELECT x FROM t")
            .unwrap()
            .run_collect_rows()
            .unwrap();
        assert_eq!(rows.len(), 1);
        conn.close().unwrap();
    }

    #[test]
    fn session_formatter_persists_every_schema_object_kind() {
        let io: Arc<dyn IO> = Arc::new(MemoryIO::new());
        let db = open_db_with_options(
            &io,
            "dialect-all-schema-kinds.db",
            Arc::new(StrictTestDialect),
            DatabaseOpts::new().with_views(true),
        )
        .unwrap();
        let conn = db.connect().unwrap();

        for sql in [
            "CREATE TABLE t (x INTEGER)",
            "CREATE TABLE ctas AS SELECT 7 AS y",
            "CREATE INDEX idx ON t (x)",
            "CREATE VIEW v AS SELECT x FROM t",
            "CREATE TRIGGER tr AFTER INSERT ON t BEGIN SELECT new.x; END",
        ] {
            execute_with_schema_formatter(&conn, sql, Arc::new(StrictSchemaSqlFormatter));
        }

        let rows = conn
            .prepare(
                "SELECT sql FROM sqlite_schema \
                 WHERE name IN ('t', 'ctas', 'idx', 'v', 'tr') ORDER BY name",
            )
            .unwrap()
            .run_collect_rows()
            .unwrap();
        assert_eq!(rows.len(), 5);
        assert!(rows.iter().all(|row| {
            row[0]
                .to_string()
                .trim_matches('\'')
                .starts_with(StrictTestDialect::PREFIX)
        }));

        conn.execute("INSERT INTO t VALUES (42)").unwrap();
        assert_eq!(
            conn.prepare("SELECT x FROM v")
                .unwrap()
                .run_collect_rows()
                .unwrap(),
            vec![vec![crate::Value::from_i64(42)]]
        );
        assert_eq!(
            conn.prepare("SELECT y FROM ctas")
                .unwrap()
                .run_collect_rows()
                .unwrap(),
            vec![vec![crate::Value::from_i64(7)]]
        );
        conn.close().unwrap();

        let reopened = open_db_with_options(
            &io,
            "dialect-all-schema-kinds.db",
            Arc::new(StrictTestDialect),
            DatabaseOpts::new().with_views(true),
        )
        .unwrap();
        let reopened_conn = reopened.connect().unwrap();
        assert!(reopened_conn.schema.read().get_index("t", "idx").is_some());
        assert!(reopened_conn.schema.read().get_view("v").is_some());
        assert!(reopened_conn.schema.read().get_trigger("tr").is_some());
    }

    /// A dialect that resolves no functions at all, proving the dialect —
    /// not the engine — owns the function name surface of user SQL.
    struct NoFunctionsDialect;

    impl Dialect for NoFunctionsDialect {
        fn name(&self) -> &'static str {
            "nofuncs"
        }

        fn parse(&self, sql: &str) -> crate::Result<(Option<turso_parser::ast::Cmd>, usize)> {
            sqlite::parse(sql)
        }

        fn parse_table_sql(&self, sql: &str, root_page: i64) -> crate::Result<BTreeTable> {
            BTreeTable::from_sql(sql, root_page)
        }

        fn parse_table_sql_ast(&self, sql: &str) -> crate::Result<turso_parser::ast::Stmt> {
            sqlite::parse_table_sql_ast(sql)
        }

        fn table_sql_for_replay(&self, sql: &str) -> crate::Result<String> {
            sqlite::table_sql_for_replay(sql)
        }

        fn format_table_sql(
            &self,
            _input: &str,
            tbl_name: &turso_parser::ast::QualifiedName,
            body: &turso_parser::ast::CreateTableBody,
        ) -> crate::Result<String> {
            Ok(format!(
                "CREATE TABLE {} {}",
                tbl_name.name.as_ident(),
                body
            ))
        }

        fn register_catalog(
            &self,
            schema: &mut crate::schema::Schema,
            enable_custom_types: bool,
        ) -> crate::Result<()> {
            sqlite::register_builtin_catalog(schema, enable_custom_types)
        }

        fn resolve_function(
            &self,
            _name: &str,
            _arg_count: usize,
        ) -> crate::Result<Option<crate::function::Func>> {
            Ok(None)
        }
    }

    #[test]
    fn dialect_scalar_function_resolves_and_executes() {
        let io: Arc<dyn IO> = Arc::new(MemoryIO::new());
        let db = open_db(&io, "dialect-funcs.db", Arc::new(TestDialect::default())).unwrap();
        let conn = db.connect().unwrap();

        // Dialect-provided scalar executes through exec_scalar_function.
        let rows = conn
            .prepare("SELECT test_add_one(41)")
            .unwrap()
            .run_collect_rows()
            .unwrap();
        assert_eq!(
            rows,
            vec![vec![crate::Value::Numeric(
                crate::numeric::Numeric::Integer(42)
            )]]
        );

        // Built-ins still resolve because the dialect composes with the
        // shared table.
        let rows = conn
            .prepare("SELECT abs(-7)")
            .unwrap()
            .run_collect_rows()
            .unwrap();
        assert_eq!(
            rows,
            vec![vec![crate::Value::Numeric(
                crate::numeric::Numeric::Integer(7)
            )]]
        );

        // Unknown names still error.
        let err = conn.prepare("SELECT no_such_function(1)").unwrap_err();
        assert!(err.to_string().contains("no such function"));
        conn.close().unwrap();
    }

    #[test]
    fn dialect_function_alias_preserves_outer_join() {
        let io: Arc<dyn IO> = Arc::new(MemoryIO::new());
        let db = open_db(
            &io,
            "dialect-outer-join.db",
            Arc::new(TestDialect::default()),
        )
        .unwrap();
        let conn = db.connect().unwrap();

        conn.execute("CREATE TABLE lhs (id INTEGER)").unwrap();
        conn.execute("CREATE TABLE rhs (id INTEGER, value INTEGER)")
            .unwrap();
        conn.execute("INSERT INTO lhs VALUES (1), (2)").unwrap();
        conn.execute("INSERT INTO rhs VALUES (1, 0)").unwrap();

        let rows = conn
            .prepare(
                "SELECT lhs.id FROM lhs LEFT JOIN rhs ON rhs.id = lhs.id \
                 WHERE nvl(rhs.value, 1) = 1 ORDER BY lhs.id",
            )
            .unwrap()
            .run_collect_rows()
            .unwrap();
        assert_eq!(rows, vec![vec![crate::Value::from_i64(2)]]);
        conn.close().unwrap();
    }

    #[test]
    fn dialect_owns_the_function_surface() {
        let io: Arc<dyn IO> = Arc::new(MemoryIO::new());
        let db = open_db(&io, "dialect-nofuncs.db", Arc::new(NoFunctionsDialect)).unwrap();
        let conn = db.connect().unwrap();

        // Function-free SQL works, including DDL (whose internal helper
        // statements resolve with SQLite semantics regardless of dialect).
        conn.execute("CREATE TABLE t (x INTEGER)").unwrap();
        conn.execute("INSERT INTO t VALUES (-7)").unwrap();

        // A SQLite built-in is not part of this dialect's surface.
        let err = conn.prepare("SELECT abs(x) FROM t").unwrap_err();
        assert!(
            err.to_string().contains("no such function"),
            "unexpected error: {err}"
        );
        conn.close().unwrap();
    }

    #[test]
    fn cdc_generated_functions_bypass_the_dialect() {
        let io: Arc<dyn IO> = Arc::new(MemoryIO::new());
        let db = open_db(&io, "dialect-cdc.db", Arc::new(NoFunctionsDialect)).unwrap();
        let conn = db.connect().unwrap();

        conn.execute("CREATE TABLE t (x INTEGER)").unwrap();
        conn.execute("PRAGMA capture_data_changes_conn('full')")
            .unwrap();
        conn.execute("INSERT INTO t VALUES (7)").unwrap();
        conn.execute("BEGIN").unwrap();
        conn.execute("INSERT INTO t VALUES (8)").unwrap();
        conn.execute("COMMIT").unwrap();

        let rows = conn
            .prepare(
                "SELECT change_type, table_name, id, change_txn_id \
                 FROM turso_cdc ORDER BY change_id",
            )
            .unwrap()
            .run_collect_rows()
            .unwrap();
        assert_eq!(rows.len(), 4);
        assert_eq!(rows[0][0], crate::Value::from_i64(1));
        assert_eq!(rows[0][1], crate::Value::build_text("t"));
        assert_eq!(rows[0][2], crate::Value::from_i64(1));
        assert_eq!(rows[1][0], crate::Value::from_i64(2));
        assert_eq!(rows[1][1], crate::Value::Null);
        assert_eq!(rows[1][2], crate::Value::Null);
        assert_eq!(rows[0][3], rows[1][3]);
        assert_eq!(rows[2][0], crate::Value::from_i64(1));
        assert_eq!(rows[2][1], crate::Value::build_text("t"));
        assert_eq!(rows[2][2], crate::Value::from_i64(2));
        assert_eq!(rows[3][0], crate::Value::from_i64(2));
        assert_eq!(rows[3][1], crate::Value::Null);
        assert_eq!(rows[3][2], crate::Value::Null);
        assert_eq!(rows[2][3], rows[3][3]);
        conn.close().unwrap();
    }

    #[test]
    fn alter_table_rewrites_dialect_formatted_sql() {
        let io: Arc<dyn IO> = Arc::new(MemoryIO::new());
        let db = open_db(&io, "dialect-alter-table.db", Arc::new(StrictTestDialect)).unwrap();
        let conn = db.connect().unwrap();

        conn.execute("CREATE TABLE t (x INTEGER)").unwrap();
        conn.execute("ALTER TABLE t RENAME TO u").unwrap();

        let rows = conn
            .prepare("SELECT sql FROM sqlite_schema WHERE name = 'u'")
            .unwrap()
            .run_collect_rows()
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(
            rows[0][0].to_string().trim_matches('\''),
            "strict: CREATE TABLE u (x INTEGER)"
        );
        conn.execute("INSERT INTO u VALUES (1)").unwrap();
        conn.close().unwrap();
    }

    #[test]
    fn alter_table_rename_column_decodes_dialect_formatted_sql() {
        let io: Arc<dyn IO> = Arc::new(MemoryIO::new());
        let db = open_db(&io, "dialect-alter-column.db", Arc::new(StrictTestDialect)).unwrap();
        let conn = db.connect().unwrap();

        conn.execute("CREATE TABLE t (x INTEGER)").unwrap();
        conn.execute("ALTER TABLE t RENAME COLUMN x TO y").unwrap();

        let rows = conn
            .prepare("SELECT sql FROM sqlite_schema WHERE name = 't'")
            .unwrap()
            .run_collect_rows()
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(
            rows[0][0].to_string().trim_matches('\''),
            "strict: CREATE TABLE t (y INTEGER)"
        );
        conn.execute("INSERT INTO t VALUES (1)").unwrap();
        assert_eq!(
            conn.prepare("SELECT y FROM t")
                .unwrap()
                .run_collect_rows()
                .unwrap(),
            vec![vec![crate::Value::from_i64(1)]]
        );
        conn.close().unwrap();
    }

    #[test]
    fn alter_table_add_and_drop_column_keep_dialect_schema_sql() {
        let io: Arc<dyn IO> = Arc::new(MemoryIO::new());
        let db = open_db(
            &io,
            "dialect-alter-add-drop.db",
            Arc::new(StrictTestDialect),
        )
        .unwrap();
        let conn = db.connect().unwrap();

        conn.execute("CREATE TABLE t (x INTEGER, y INTEGER)")
            .unwrap();
        conn.execute("ALTER TABLE t ADD COLUMN z TEXT").unwrap();
        conn.execute("ALTER TABLE t DROP COLUMN y").unwrap();

        let rows = conn
            .prepare("SELECT sql FROM sqlite_schema WHERE name = 't'")
            .unwrap()
            .run_collect_rows()
            .unwrap();
        assert_eq!(rows.len(), 1);
        let stored = rows[0][0].to_string();
        assert_eq!(
            stored.trim_matches('\''),
            "strict: CREATE TABLE t (x INTEGER, z TEXT)"
        );
        conn.execute("INSERT INTO t VALUES (1, 'ok')").unwrap();
        assert_eq!(
            conn.prepare("SELECT x, z FROM t")
                .unwrap()
                .run_collect_rows()
                .unwrap(),
            vec![vec![
                crate::Value::from_i64(1),
                crate::Value::build_text("ok")
            ]]
        );
    }

    #[test]
    fn schema_rewrites_receive_the_exact_stored_sql() {
        let io: Arc<dyn IO> = Arc::new(MemoryIO::new());
        let db = open_db(
            &io,
            "dialect-rewrite-previous-sql.db",
            Arc::new(StrictTestDialect),
        )
        .unwrap();
        let conn = db.connect().unwrap();
        let rewritten_previous_sql = Arc::new(Mutex::new(Vec::new()));
        let formatter: Arc<dyn SchemaSqlFormatter> = Arc::new(RecordingSchemaSqlFormatter {
            rewritten_previous_sql: rewritten_previous_sql.clone(),
        });

        execute_with_schema_formatter(
            &conn,
            "CREATE TABLE add_column (x INTEGER, y INTEGER)",
            formatter.clone(),
        );
        let before_add = stored_schema_sql(&conn, "add_column");
        execute_with_schema_formatter(
            &conn,
            "ALTER TABLE add_column ADD COLUMN z TEXT",
            formatter.clone(),
        );
        execute_with_schema_formatter(
            &conn,
            "CREATE TABLE drop_column (x INTEGER, y INTEGER)",
            formatter.clone(),
        );
        let before_drop = stored_schema_sql(&conn, "drop_column");
        execute_with_schema_formatter(
            &conn,
            "ALTER TABLE drop_column DROP COLUMN y",
            formatter.clone(),
        );

        execute_with_schema_formatter(
            &conn,
            "CREATE TABLE rename_table (x INTEGER)",
            formatter.clone(),
        );
        let before_table_rename = stored_schema_sql(&conn, "rename_table");
        execute_with_schema_formatter(
            &conn,
            "ALTER TABLE rename_table RENAME TO renamed_table",
            formatter.clone(),
        );
        let before_column_rename = stored_schema_sql(&conn, "renamed_table");
        execute_with_schema_formatter(
            &conn,
            "ALTER TABLE renamed_table RENAME COLUMN x TO y",
            formatter,
        );

        let rewritten_previous_sql = rewritten_previous_sql.lock().unwrap();
        for expected in [
            before_add,
            before_drop,
            before_table_rename,
            before_column_rename,
        ] {
            assert!(
                rewritten_previous_sql.contains(&expected),
                "formatter did not receive stored SQL {expected:?}; received {rewritten_previous_sql:?}"
            );
        }
    }

    fn stored_schema_sql(conn: &Arc<crate::Connection>, name: &str) -> String {
        let rows = conn
            .prepare(format!(
                "SELECT sql FROM sqlite_schema WHERE name = '{name}'"
            ))
            .unwrap()
            .run_collect_rows()
            .unwrap();
        assert_eq!(rows.len(), 1, "expected one sqlite_schema row for {name}");
        rows[0][0].to_string().trim_matches('\'').to_string()
    }

    #[test]
    fn registry_rejects_dialect_mismatch() {
        let io: Arc<dyn IO> = Arc::new(MemoryIO::new());
        let _db = open_db(&io, "dialect-mismatch.db", Arc::new(SqliteDialect)).unwrap();

        let err =
            open_db(&io, "dialect-mismatch.db", Arc::new(TestDialect::default())).unwrap_err();
        assert!(
            err.to_string().contains("already open with dialect"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn registry_rejects_default_open_of_dialect_database() {
        let io: Arc<dyn IO> = Arc::new(MemoryIO::new());
        let _db = open_db(
            &io,
            "dialect-mismatch-reverse.db",
            Arc::new(TestDialect::default()),
        )
        .unwrap();

        let err = open_db(&io, "dialect-mismatch-reverse.db", Arc::new(SqliteDialect)).unwrap_err();
        assert!(
            err.to_string().contains("already open with dialect"),
            "unexpected error: {err}"
        );
    }

    #[cfg(feature = "fs")]
    #[test]
    fn shared_memory_registry_rejects_dialect_mismatch() {
        let name = "dialect-shared-memory-mismatch";
        let _db = Database::open_shared_memory(name, Arc::new(SqliteDialect)).unwrap();

        let err = Database::open_shared_memory(name, Arc::new(TestDialect::default())).unwrap_err();
        assert!(
            err.to_string().contains("already open with dialect"),
            "unexpected error: {err}"
        );
    }

    #[cfg(all(feature = "fs", not(target_family = "wasm")))]
    #[test]
    fn vacuum_into_replays_schema_with_source_dialect() {
        let dir = tempfile::tempdir().unwrap();
        let source_path = dir.path().join("source.db");
        let output_path = dir.path().join("output.db");
        let io: Arc<dyn IO> = Arc::new(crate::io::PlatformIO::new().unwrap());
        let db = Database::open_file_with_flags(
            io.clone(),
            source_path.to_str().unwrap(),
            OpenFlags::Create,
            DatabaseOpts::new().with_views(true),
            None,
            Arc::new(StrictTestDialect),
        )
        .unwrap();
        let conn = db.connect().unwrap();
        conn.execute("CREATE TABLE t(x INTEGER)").unwrap();
        conn.execute("CREATE INDEX idx ON t(x)").unwrap();
        conn.execute("CREATE VIEW v AS SELECT x FROM t").unwrap();
        conn.execute("CREATE TRIGGER tr AFTER INSERT ON t BEGIN SELECT new.x; END")
            .unwrap();
        conn.execute("INSERT INTO t VALUES (42)").unwrap();

        conn.execute(format!("VACUUM INTO '{}'", output_path.display()))
            .unwrap();

        let output_db = open_db_with_options(
            &io,
            output_path.to_str().unwrap(),
            Arc::new(StrictTestDialect),
            DatabaseOpts::new().with_views(true),
        )
        .unwrap();
        let output_conn = output_db.connect().unwrap();
        let schema_rows = output_conn
            .prepare(
                "SELECT sql FROM sqlite_schema \
                 WHERE name IN ('t', 'idx', 'v', 'tr') ORDER BY name",
            )
            .unwrap()
            .run_collect_rows()
            .unwrap();
        assert_eq!(schema_rows.len(), 4);
        assert!(schema_rows.iter().all(|row| {
            row[0]
                .to_string()
                .trim_matches('\'')
                .starts_with(StrictTestDialect::PREFIX)
        }));
        assert_eq!(
            output_conn
                .prepare("SELECT x FROM t")
                .unwrap()
                .run_collect_rows()
                .unwrap(),
            vec![vec![crate::Value::from_i64(42)]]
        );
    }

    #[cfg(all(feature = "fs", not(target_family = "wasm")))]
    #[test]
    fn vacuum_attached_database_strips_source_schema_from_replay() {
        let dir = tempfile::tempdir().unwrap();
        let source_path = dir.path().join("source.db");
        let attached_path = dir.path().join("attached.db");
        let output_path = dir.path().join("output.db");
        let io: Arc<dyn IO> = Arc::new(crate::io::PlatformIO::new().unwrap());
        let db = Database::open_file_with_flags(
            io.clone(),
            source_path.to_str().unwrap(),
            OpenFlags::Create,
            DatabaseOpts::new().with_attach(true),
            None,
            Arc::new(StrictTestDialect),
        )
        .unwrap();
        let conn = db.connect().unwrap();
        conn.execute(format!(
            "ATTACH DATABASE '{}' AS aux",
            attached_path.display()
        ))
        .unwrap();
        conn.execute("CREATE TABLE aux.t (x INTEGER)").unwrap();
        conn.execute("INSERT INTO aux.t VALUES (42)").unwrap();

        conn.execute(format!("VACUUM aux INTO '{}'", output_path.display()))
            .unwrap();

        let output_db = Database::open_file(
            io,
            output_path.to_str().unwrap(),
            Arc::new(StrictTestDialect),
        )
        .unwrap();
        let output_conn = output_db.connect().unwrap();
        assert_eq!(
            output_conn
                .prepare("SELECT x FROM t")
                .unwrap()
                .run_collect_rows()
                .unwrap(),
            vec![vec![crate::Value::from_i64(42)]]
        );
    }

    #[cfg(all(feature = "fs", not(target_family = "wasm")))]
    #[test]
    fn in_place_vacuum_replays_schema_with_source_dialect() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("source.db");
        let io: Arc<dyn IO> = Arc::new(crate::io::PlatformIO::new().unwrap());
        let db = Database::open_file_with_flags(
            io,
            path.to_str().unwrap(),
            OpenFlags::Create,
            DatabaseOpts::new().with_vacuum(true),
            None,
            Arc::new(StrictTestDialect),
        )
        .unwrap();
        let conn = db.connect().unwrap();
        conn.execute("CREATE TABLE t(x INTEGER)").unwrap();
        conn.execute("INSERT INTO t VALUES (42)").unwrap();

        conn.execute("VACUUM").unwrap();

        let schema_rows = conn
            .prepare("SELECT sql FROM sqlite_schema WHERE name = 't'")
            .unwrap()
            .run_collect_rows()
            .unwrap();
        assert_eq!(schema_rows.len(), 1);
        assert_eq!(
            schema_rows[0][0].to_string().trim_matches('\''),
            "strict: CREATE TABLE t(x INTEGER)"
        );
        assert_eq!(
            conn.prepare("SELECT x FROM t")
                .unwrap()
                .run_collect_rows()
                .unwrap(),
            vec![vec![crate::Value::from_i64(42)]]
        );
    }
}
