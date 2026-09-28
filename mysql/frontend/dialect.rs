use std::sync::Arc;

use parking_lot::Mutex;

use turso_core::CollationSeq;
use turso_core::{
    dialect::{SchemaCatalogRow, SchemaCatalogValidationContext},
    schema::{is_system_table, BTreeTable, Schema},
    AssignmentError, AssignmentOperation, AssignmentValidator, DatabaseFileOwner, Dialect, Func,
    LimboError, Numeric, Result, SchemaSqlKind, Value,
};
use turso_mysql_parser::{
    parse_auto_increment_create_table, parse_checked_primary_key_create_table,
    parse_create_index_ast, parse_create_table_ast, parse_create_trigger_ast,
    parse_create_view_ast, parse_mysql_numeric_spec, render_create_index_mysql_with_mode,
    render_create_table_mysql_with_mode, table_options_of, SessionSqlMode,
};
use turso_parser::ast::{Cmd, ColumnConstraint, CreateTableBody, Stmt};

use crate::found_rows;
use crate::group_concat;
use crate::schema_sql::{
    decode_persisted_schema_sql, decode_schema_sql_any, reencode_schema_sql,
    validate_schema_sql_catalog, DecodedSchemaSql, SchemaSqlCatalogEntry, SchemaSqlId,
    SchemaSqlMode, SchemaSqlSessionContext,
};

/// The MySQL frontend dialect for persisted schema rows.
///
/// This bounded implementation recognizes only the versioned MySQL table
/// envelopes emitted with [`crate::schema_sql::SchemaSqlSessionContext`].
/// Tables and indexes have native MySQL translators; other schema objects
/// still need one before they can be stored safely.
#[derive(Debug, Default)]
pub struct MySqlDialect;

impl Dialect for MySqlDialect {
    fn name(&self) -> &'static str {
        "mysql"
    }

    fn database_file_owner(&self) -> DatabaseFileOwner {
        DatabaseFileOwner::MySql
    }

    fn database_file_application_id(&self) -> Option<i32> {
        Some(DatabaseFileOwner::mysql_application_id(
            DatabaseFileOwner::MYSQL_LOWER_CASE_TABLE_NAMES,
        ))
    }

    fn assignment_validator(&self) -> Option<Arc<dyn AssignmentValidator>> {
        Some(Arc::new(MySqlIntegerValidator))
    }

    fn validate_schema_catalog(
        &self,
        rows: &[SchemaCatalogRow],
        context: Option<&SchemaCatalogValidationContext>,
    ) -> Result<()> {
        let mut table_rows = Vec::new();
        for row in rows {
            if !row.object_type.eq_ignore_ascii_case("table") {
                continue;
            }
            let sql = row.sql.as_deref().ok_or_else(|| {
                LimboError::Corrupt(format!(
                    "MySQL user table {} is missing sqlite_schema SQL",
                    row.name
                ))
            })?;

            if !row.name.eq_ignore_ascii_case(&row.table_name) {
                return Err(LimboError::Corrupt(format!(
                    "MySQL schema catalog table {} has table_name {}",
                    row.name, row.table_name
                )));
            }
            if row.root_page == 0 {
                return Err(LimboError::Corrupt(format!(
                    "MySQL schema catalog table {} has an invalid root page",
                    row.name
                )));
            }

            let decoded = decode_persisted_schema_sql(SchemaSqlKind::Table, sql)?;
            let sql_table_name = catalog_table_sql_name(sql, decoded)?;
            if !row.name.eq_ignore_ascii_case(&sql_table_name) {
                return Err(LimboError::Corrupt(format!(
                    "MySQL schema catalog table {} SQL defines table {}",
                    row.name, sql_table_name
                )));
            }

            // Internal tables are plain SQLite rows. A reserved name is not
            // enough to skip validation: marked MySQL SQL and rows whose
            // catalog identity does not match must take the user-table path.
            if is_system_table(&row.name) && decoded.is_none() {
                continue;
            }
            if decoded.is_some_and(|decoded| decoded.v2_metadata().is_some()) && context.is_none() {
                return Err(LimboError::Corrupt(
                    "MySQL AUTO_INCREMENT schema metadata requires a durable database identity"
                        .to_string(),
                ));
            }
            table_rows.push(SchemaSqlCatalogEntry::encoded(sql));
        }

        // V1 envelopes have no database identity. They still use a nonzero
        // placeholder because catalog validation verifies their table shape,
        // while every v2 envelope must match the opener-provided identity.
        let expected_database_id = context
            .map(|context| SchemaSqlId::from_bytes(*context.database_identity()))
            .transpose()
            .map_err(|error| LimboError::Corrupt(error.to_string()))?
            .unwrap_or_else(|| {
                SchemaSqlId::from_bytes([1; 16])
                    .expect("nonzero schema catalog placeholder identity is valid")
            });
        validate_schema_sql_catalog(expected_database_id, table_rows)
            .map_err(|error| LimboError::Corrupt(error.to_string()))
    }

    fn parse(&self, sql: &str) -> Result<(Option<Cmd>, usize)> {
        if let Some(decoded) =
            decode_schema_sql_any(sql).map_err(|error| LimboError::Corrupt(error.to_string()))?
        {
            let statement = match decoded.context.kind {
                SchemaSqlKind::Table => parse_marked_table(decoded)?,
                SchemaSqlKind::Index => parse_marked_index(decoded)?,
                SchemaSqlKind::View => parse_marked_view(decoded)?,
                SchemaSqlKind::Trigger => parse_marked_trigger(decoded)?,
                kind => {
                    return Err(LimboError::Corrupt(format!(
                        "persisted MySQL {kind:?} SQL is not supported by the generic parser"
                    )));
                }
            };
            return Ok((Some(Cmd::Stmt(statement)), sql.len()));
        }
        turso_core::dialect::sqlite::parse(sql)
    }

    fn parse_table_sql(&self, sql: &str, root_page: i64) -> Result<BTreeTable> {
        let stmt = self.parse_table_sql_ast(sql)?;
        let Stmt::CreateTable { tbl_name, body, .. } = stmt else {
            unreachable!("parse_table_sql_ast returned a non-CREATE TABLE statement");
        };
        let mut table = BTreeTable::from_create_table_ast(&tbl_name, &body, root_page)?;
        // SQLite's affinity rules read these type names as numbers', so a value
        // that looks like a number would be converted on the way in: the engine
        // stores the document `1e15` as the integer 1000000000000000, a `SET`
        // written as the bits `'3'` has to stay text long enough to be told
        // from a member spelled `3`, a `YEAR` written as `'0'` is 2000 where
        // the number 0 is the zero year, a `VARBINARY` holding `'007'`
        // would otherwise read back as `7`, and a `BIT` written `'1'`, which
        // MySQL refuses, would be taken as the number 1.
        for column in table.columns_mut().iter_mut() {
            let holds_text = matches!(
                column.ty_str.to_ascii_uppercase().as_str(),
                "JSON" | "DATE" | "TIME" | "DATETIME" | "TIMESTAMP" | "YEAR" | "VARBINARY" | "BIT"
            ) || turso_mysql_parser::enum_members(&column.ty_str).is_some()
                || turso_mysql_parser::set_members(&column.ty_str).is_some();
            if holds_text {
                column.store_values_verbatim();
            }
        }
        Ok(table)
    }

    fn parse_table_sql_ast(&self, sql: &str) -> Result<Stmt> {
        let Some(decoded) = decode_persisted_schema_sql(SchemaSqlKind::Table, sql)? else {
            return turso_core::dialect::sqlite::parse_table_sql_ast(sql);
        };
        parse_marked_table(decoded)
    }

    fn table_sql_for_replay(&self, sql: &str) -> Result<String> {
        let Some(decoded) = decode_persisted_schema_sql(SchemaSqlKind::Table, sql)? else {
            return turso_core::dialect::sqlite::table_sql_for_replay(sql);
        };

        let mut stmt = parse_marked_table(decoded)?;
        let Stmt::CreateTable { tbl_name, .. } = &mut stmt else {
            unreachable!("parse_marked_table returned a non-CREATE TABLE statement");
        };
        let is_auto_increment = decoded.v2_metadata().is_some()
            && parse_auto_increment_create_table(
                decoded.normalized_ddl,
                session_sql_mode(decoded.context.sql_mode),
            )
            .is_ok();
        if is_auto_increment {
            if tbl_name.db_name.is_some() {
                return Err(LimboError::Corrupt(
                    "cannot replay a schema-qualified MySQL AUTO_INCREMENT table".to_string(),
                ));
            }
            return reencode_schema_sql(decoded, decoded.normalized_ddl)
                .map_err(|error| LimboError::Corrupt(error.to_string()));
        }
        if decoded.v2_metadata().is_none()
            && parse_checked_primary_key_create_table(
                decoded.normalized_ddl,
                session_sql_mode(decoded.context.sql_mode),
            )
            .is_ok()
        {
            // The checked parser lowers the primary key to a regular INT
            // column so replay cannot turn it into SQLite's rowid alias.
            // Keep the original MySQL DDL because the table option and source
            // integer spelling are part of the durable schema contract.
            return reencode_schema_sql(decoded, decoded.normalized_ddl)
                .map_err(|error| LimboError::Corrupt(error.to_string()));
        }
        tbl_name.db_name = None;
        let mode = session_sql_mode(decoded.context.sql_mode);
        let normalized = render_create_table_mysql_with_mode(&stmt, mode)
            .and_then(|rendered| {
                let options = table_options_of(decoded.normalized_ddl, mode)?;
                Ok(format!("{rendered}{}", options.written()))
            })
            .map_err(|error| {
                LimboError::Corrupt(format!("cannot replay MySQL table SQL: {error}"))
            })?;
        reencode_schema_sql(decoded, &normalized)
            .map_err(|error| LimboError::Corrupt(error.to_string()))
    }

    fn format_table_sql(
        &self,
        input: &str,
        tbl_name: &turso_parser::ast::QualifiedName,
        body: &turso_parser::ast::CreateTableBody,
    ) -> Result<String> {
        if let Some(decoded) = decode_persisted_schema_sql(SchemaSqlKind::Table, input)? {
            let stored = parse_marked_table(decoded)?;
            let Stmt::CreateTable {
                tbl_name: stored_name,
                body: stored_body,
                ..
            } = stored
            else {
                unreachable!("parse_marked_table returned a non-CREATE TABLE statement");
            };
            if &stored_name != tbl_name || &stored_body != body {
                return Err(LimboError::Corrupt(
                    "MySQL replay SQL does not match its translated table definition".to_string(),
                ));
            }
            return Ok(input.to_string());
        }
        Err(LimboError::ParseError(
            "MySQL schema writes require SchemaSqlSessionContext".to_string(),
        ))
    }

    fn format_schema_sql(&self, kind: SchemaSqlKind, input: &str, stmt: &Stmt) -> Result<String> {
        match (kind, stmt) {
            (SchemaSqlKind::Table, Stmt::CreateTable { tbl_name, body, .. }) => {
                self.format_table_sql(input, tbl_name, body)
            }
            (SchemaSqlKind::Index, Stmt::CreateIndex { .. }) => self.format_index_sql(input, stmt),
            (SchemaSqlKind::View, Stmt::CreateView { .. }) => self.format_view_sql(input, stmt),
            (SchemaSqlKind::View, _) => Err(LimboError::ParseError(
                "MySQL schema formatter supports only CREATE VIEW".to_string(),
            )),
            (SchemaSqlKind::Trigger, Stmt::CreateTrigger { .. }) => {
                self.format_trigger_sql(input, stmt)
            }
            (SchemaSqlKind::Trigger, _) => Err(LimboError::ParseError(
                "MySQL schema formatter supports only CREATE TRIGGER".to_string(),
            )),
            _ => Dialect::format_schema_sql(&turso_core::SqliteDialect, kind, input, stmt),
        }
    }

    fn format_rewritten_schema_sql(
        &self,
        kind: SchemaSqlKind,
        previous_sql: &str,
        stmt: &Stmt,
    ) -> Result<String> {
        if kind == SchemaSqlKind::Table {
            if let Some(decoded) = decode_persisted_schema_sql(kind, previous_sql)? {
                if decoded.v2_metadata().is_none() {
                    let mode = session_sql_mode(decoded.context.sql_mode);
                    match parse_checked_primary_key_create_table(decoded.normalized_ddl, mode) {
                        Ok(checked) if checked.normalized_mysql_ddl != decoded.normalized_ddl => {
                            return Err(LimboError::Corrupt(
                                "persisted MySQL PRIMARY KEY table SQL is not canonical"
                                    .to_string(),
                            ));
                        }
                        Ok(_) | Err(_) => {}
                    }
                }
            }
            return Dialect::format_rewritten_table_sql(self, stmt);
        }
        if kind == SchemaSqlKind::Index {
            if decode_schema_sql_any(previous_sql)
                .map_err(|error| LimboError::Corrupt(error.to_string()))?
                .is_some()
            {
                return Err(LimboError::ParseError(
                    "rewriting a marked MySQL index requires SchemaSqlSessionContext".to_string(),
                ));
            }
            return Dialect::format_rewritten_schema_sql(
                &turso_core::SqliteDialect,
                kind,
                previous_sql,
                stmt,
            );
        }
        if kind == SchemaSqlKind::View {
            if decode_schema_sql_any(previous_sql)
                .map_err(|error| LimboError::Corrupt(error.to_string()))?
                .is_some()
            {
                return Err(LimboError::ParseError(
                    "rewriting a marked MySQL view requires SchemaSqlSessionContext".to_string(),
                ));
            }
            return Dialect::format_rewritten_schema_sql(
                &turso_core::SqliteDialect,
                kind,
                previous_sql,
                stmt,
            );
        }
        if kind == SchemaSqlKind::Trigger {
            if decode_schema_sql_any(previous_sql)
                .map_err(|error| LimboError::Corrupt(error.to_string()))?
                .is_some()
            {
                return Err(LimboError::ParseError(
                    "rewriting a marked MySQL trigger is not supported".to_string(),
                ));
            }
            return Dialect::format_rewritten_schema_sql(
                &turso_core::SqliteDialect,
                kind,
                previous_sql,
                stmt,
            );
        }
        Dialect::format_rewritten_schema_sql(&turso_core::SqliteDialect, kind, previous_sql, stmt)
    }

    fn parse_schema_sql(&self, kind: SchemaSqlKind, sql: &str) -> Result<Stmt> {
        match kind {
            SchemaSqlKind::Table => return self.parse_table_sql_ast(sql),
            SchemaSqlKind::Index => {
                let Some(decoded) = decode_persisted_schema_sql(kind, sql)? else {
                    return Dialect::parse_schema_sql(&turso_core::SqliteDialect, kind, sql);
                };
                return parse_marked_index(decoded);
            }
            SchemaSqlKind::View => {
                let Some(decoded) = decode_persisted_schema_sql(kind, sql)? else {
                    return Dialect::parse_schema_sql(&turso_core::SqliteDialect, kind, sql);
                };
                return parse_marked_view(decoded);
            }
            SchemaSqlKind::Trigger => {
                let Some(decoded) = decode_persisted_schema_sql(kind, sql)? else {
                    return Dialect::parse_schema_sql(&turso_core::SqliteDialect, kind, sql);
                };
                return parse_marked_trigger(decoded);
            }
            _ => {}
        }
        Dialect::parse_schema_sql(&turso_core::SqliteDialect, kind, sql)
    }

    fn schema_sql_for_replay(&self, kind: SchemaSqlKind, sql: &str) -> Result<String> {
        match kind {
            SchemaSqlKind::Table => return self.table_sql_for_replay(sql),
            SchemaSqlKind::Index => {
                let Some(decoded) = decode_persisted_schema_sql(kind, sql)? else {
                    return Dialect::schema_sql_for_replay(&turso_core::SqliteDialect, kind, sql);
                };
                let statement = parse_marked_index(decoded)?;
                let normalized = render_create_index_mysql_with_mode(
                    &statement,
                    session_sql_mode(decoded.context.sql_mode),
                )
                .map_err(|error| {
                    LimboError::Corrupt(format!("cannot replay MySQL index SQL: {error}"))
                })?;
                return reencode_schema_sql(decoded, &normalized)
                    .map_err(|error| LimboError::Corrupt(error.to_string()));
            }
            SchemaSqlKind::View => {
                let Some(decoded) = decode_persisted_schema_sql(kind, sql)? else {
                    return Dialect::schema_sql_for_replay(&turso_core::SqliteDialect, kind, sql);
                };
                let statement = parse_marked_view(decoded)?;
                let normalized = turso_mysql_parser::mysql_create_view_ddl(
                    &statement,
                    decoded.normalized_ddl,
                    session_sql_mode(decoded.context.sql_mode),
                )
                .map_err(|error| {
                    LimboError::Corrupt(format!("cannot replay MySQL view SQL: {error}"))
                })?;
                return reencode_schema_sql(decoded, &normalized)
                    .map_err(|error| LimboError::Corrupt(error.to_string()));
            }
            SchemaSqlKind::Trigger => {
                let Some(decoded) = decode_persisted_schema_sql(kind, sql)? else {
                    return Dialect::schema_sql_for_replay(&turso_core::SqliteDialect, kind, sql);
                };
                let statement = parse_marked_trigger(decoded)?;
                let normalized = turso_mysql_parser::mysql_create_trigger_ddl(
                    &statement,
                    decoded.normalized_ddl,
                    session_sql_mode(decoded.context.sql_mode),
                )
                .map_err(|error| {
                    LimboError::Corrupt(format!("cannot replay MySQL trigger SQL: {error}"))
                })?;
                return reencode_schema_sql(decoded, &normalized)
                    .map_err(|error| LimboError::Corrupt(error.to_string()));
            }
            _ => {}
        }
        Dialect::schema_sql_for_replay(&turso_core::SqliteDialect, kind, sql)
    }

    fn register_catalog(&self, schema: &mut Schema, enable_custom_types: bool) -> Result<()> {
        turso_core::dialect::sqlite::register_builtin_catalog(schema, enable_custom_types)
    }

    fn resolve_function(&self, name: &str, arg_count: usize) -> Result<Option<Func>> {
        if name.eq_ignore_ascii_case("last_insert_id") && arg_count == 0 {
            return Ok(Some(Func::Dialect("last_insert_id".to_string())));
        }
        if arg_count == 1
            && MYSQL_JSON_READINGS
                .iter()
                .any(|reading| name.eq_ignore_ascii_case(reading))
        {
            return Ok(Some(Func::Dialect(name.to_ascii_lowercase())));
        }
        if arg_count == 2
            && (name.eq_ignore_ascii_case(MYSQL_DATE_FORMAT)
                || name.eq_ignore_ascii_case(MYSQL_STR_TO_DATE))
        {
            return Ok(Some(Func::Dialect(name.to_ascii_lowercase())));
        }
        if arg_count == 3 && name.eq_ignore_ascii_case(MYSQL_SHIFT_MOMENT) {
            return Ok(Some(Func::Dialect(MYSQL_SHIFT_MOMENT.to_string())));
        }
        if arg_count == 3 && name.eq_ignore_ascii_case(MYSQL_TIMESTAMPDIFF) {
            return Ok(Some(Func::Dialect(MYSQL_TIMESTAMPDIFF.to_string())));
        }
        if arg_count == 2 && name.eq_ignore_ascii_case(MYSQL_DATEDIFF) {
            return Ok(Some(Func::Dialect(MYSQL_DATEDIFF.to_string())));
        }
        if arg_count == 2
            && (name.eq_ignore_ascii_case(MYSQL_WEEK) || name.eq_ignore_ascii_case(MYSQL_YEARWEEK))
        {
            return Ok(Some(Func::Dialect(name.to_ascii_lowercase())));
        }
        if arg_count == 1 && name.eq_ignore_ascii_case(MYSQL_TO_DAYS) {
            return Ok(Some(Func::Dialect(MYSQL_TO_DAYS.to_string())));
        }
        if arg_count == 1
            && MYSQL_BYTE_READINGS
                .iter()
                .any(|reading| name.eq_ignore_ascii_case(reading))
        {
            return Ok(Some(Func::Dialect(name.to_ascii_lowercase())));
        }
        if arg_count == 1
            && (name.eq_ignore_ascii_case(MYSQL_MD5) || name.eq_ignore_ascii_case(MYSQL_SHA1))
        {
            return Ok(Some(Func::Dialect(name.to_ascii_lowercase())));
        }
        if arg_count == 2 && name.eq_ignore_ascii_case(MYSQL_SHA2) {
            return Ok(Some(Func::Dialect(MYSQL_SHA2.to_string())));
        }
        if (arg_count == 2 || arg_count == 3) && name.eq_ignore_ascii_case(MYSQL_SUBSTRING) {
            return Ok(Some(Func::Dialect(MYSQL_SUBSTRING.to_string())));
        }
        if arg_count == 3 && name.eq_ignore_ascii_case(MYSQL_SUBSTRING_INDEX) {
            return Ok(Some(Func::Dialect(MYSQL_SUBSTRING_INDEX.to_string())));
        }
        if arg_count == 2 && name.eq_ignore_ascii_case(MYSQL_ROUND) {
            return Ok(Some(Func::Dialect(MYSQL_ROUND.to_string())));
        }
        if arg_count == 2 && name.eq_ignore_ascii_case(MYSQL_REGEXP) {
            return Ok(Some(Func::Dialect(MYSQL_REGEXP.to_string())));
        }
        if arg_count == 1
            && (name.eq_ignore_ascii_case(MYSQL_LOWER) || name.eq_ignore_ascii_case(MYSQL_UPPER))
        {
            return Ok(Some(Func::Dialect(name.to_ascii_lowercase())));
        }
        if (arg_count == 2 && name.eq_ignore_ascii_case(MYSQL_INSTR))
            || ((arg_count == 2 || arg_count == 3) && name.eq_ignore_ascii_case(MYSQL_LOCATE))
        {
            return Ok(Some(Func::Dialect(name.to_ascii_lowercase())));
        }
        if arg_count == 3 && name.eq_ignore_ascii_case(MYSQL_UCA9_LIKE) {
            return Ok(Some(Func::Dialect(MYSQL_UCA9_LIKE.to_string())));
        }
        if arg_count == 1
            && (name.eq_ignore_ascii_case(MYSQL_BIN) || name.eq_ignore_ascii_case(MYSQL_OCT))
        {
            return Ok(Some(Func::Dialect(name.to_ascii_lowercase())));
        }
        if arg_count >= 2
            && (name.eq_ignore_ascii_case(MYSQL_FIELD) || name.eq_ignore_ascii_case(MYSQL_ELT))
        {
            return Ok(Some(Func::Dialect(name.to_ascii_lowercase())));
        }
        if arg_count >= 2
            && (name.eq_ignore_ascii_case(MYSQL_TEXT_GREATEST)
                || name.eq_ignore_ascii_case(MYSQL_TEXT_LEAST))
        {
            return Ok(Some(Func::Dialect(name.to_ascii_lowercase())));
        }
        if arg_count == 2 && name.eq_ignore_ascii_case(MYSQL_TEXT_NULLIF) {
            return Ok(Some(Func::Dialect(MYSQL_TEXT_NULLIF.to_string())));
        }
        if arg_count == 4 && name.eq_ignore_ascii_case(MYSQL_JSON_SEARCH) {
            return Ok(Some(Func::Dialect(MYSQL_JSON_SEARCH.to_string())));
        }
        if arg_count == 1 && name.eq_ignore_ascii_case(MYSQL_JSON_UNQUOTE) {
            return Ok(Some(Func::Dialect(MYSQL_JSON_UNQUOTE.to_string())));
        }
        if arg_count == 2
            && (name.eq_ignore_ascii_case(MYSQL_JSON_EXTRACT)
                || name.eq_ignore_ascii_case(MYSQL_JSON_TEXT_COMPARE)
                || name.eq_ignore_ascii_case(MYSQL_JSON_HOLDS))
        {
            return Ok(Some(Func::Dialect(name.to_ascii_lowercase())));
        }
        if arg_count == 2
            && (name.eq_ignore_ascii_case(MYSQL_FORMAT)
                || name.eq_ignore_ascii_case(MYSQL_TRUNCATE)
                || name.eq_ignore_ascii_case(MYSQL_JSON_CONTAINS)
                || name.eq_ignore_ascii_case(MYSQL_JSON_EQUALS_INTEGER)
                || name.eq_ignore_ascii_case(MYSQL_JSON_COMPARE_INTEGER)
                || name.eq_ignore_ascii_case(MYSQL_JSON_COMPARE_STRING)
                || name.eq_ignore_ascii_case(MYSQL_JSON_OVERLAPS)
                || name.eq_ignore_ascii_case(MYSQL_JSON_MERGE_PATCH)
                || name.eq_ignore_ascii_case(MYSQL_JSON_MERGE_PRESERVE))
        {
            return Ok(Some(Func::Dialect(name.to_ascii_lowercase())));
        }
        if arg_count == 4
            && (name.eq_ignore_ascii_case(group_concat::MYSQL_GROUP_CONCAT)
                || name.eq_ignore_ascii_case(group_concat::MYSQL_GROUP_CONCAT_COUNT))
        {
            return Ok(Some(Func::Dialect(name.to_ascii_lowercase())));
        }
        if arg_count == 2 && name.eq_ignore_ascii_case(found_rows::MYSQL_NOTE_FOUND_ROWS) {
            return Ok(Some(Func::Dialect(name.to_ascii_lowercase())));
        }
        turso_core::dialect::sqlite::resolve_builtin_function(name, arg_count)
    }

    /// MySQL matches `LIKE` under the collation of the column it reads, and a
    /// `utf8mb4_unicode_ci` column matches under Unicode 4.0.0's weights.
    fn function_for_collation(&self, name: &str, collation: CollationSeq) -> Option<String> {
        (name.eq_ignore_ascii_case(MYSQL_UCA9_LIKE) && collation == CollationSeq::MySqlUca400)
            .then(|| MYSQL_UCA400_LIKE.to_owned())
    }

    fn exec_scalar_function(
        &self,
        connection: &turso_core::Connection,
        name: &str,
        args: &[Value],
    ) -> Result<Value> {
        if name.eq_ignore_ascii_case("last_insert_id") && args.is_empty() {
            let id = connection.mysql_last_insert_id();
            return Ok(match i64::try_from(id) {
                Ok(signed) => Value::from_i64(signed),
                Err(_) => Value::from_text(id.to_string()),
            });
        }
        if name.eq_ignore_ascii_case(group_concat::MYSQL_GROUP_CONCAT)
            || name.eq_ignore_ascii_case(group_concat::MYSQL_GROUP_CONCAT_COUNT)
        {
            return group_concat::call(connection, name, args);
        }
        if name.eq_ignore_ascii_case(found_rows::MYSQL_NOTE_FOUND_ROWS) {
            return found_rows::note(connection, args);
        }
        if name.eq_ignore_ascii_case(MYSQL_BIN) || name.eq_ignore_ascii_case(MYSQL_OCT) {
            let [value] = args else {
                return Err(LimboError::ParseError(format!("{name} takes one argument")));
            };
            let Value::Numeric(turso_core::Numeric::Integer(number)) = value else {
                return Ok(Value::Null);
            };
            let radix = if name.eq_ignore_ascii_case(MYSQL_BIN) {
                2
            } else {
                8
            };
            return Ok(Value::build_text(written_in_radix(*number, radix)));
        }
        if name.eq_ignore_ascii_case(MYSQL_FIELD) {
            let [looked_for, choices @ ..] = args else {
                return Err(LimboError::ParseError(format!(
                    "{name} takes a value and the choices to look for it among"
                )));
            };
            // Measured on MySQL 8.4.11: a value that is not among them answers
            // 0, and so does one that is nothing at all.
            let Value::Text(looked_for) = looked_for else {
                return Ok(Value::from_i64(0));
            };
            let found = choices.iter().position(|choice| {
                matches!(choice, Value::Text(choice)
                    if turso_core::mysql_uca9_compare(choice.as_str(), looked_for.as_str()).is_eq())
            });
            let found = found.map_or(0, |position| position as i64 + 1);
            return Ok(Value::from_i64(found));
        }
        if name.eq_ignore_ascii_case(MYSQL_TEXT_GREATEST)
            || name.eq_ignore_ascii_case(MYSQL_TEXT_LEAST)
        {
            return checked_mysql_text_extreme(
                args,
                name.eq_ignore_ascii_case(MYSQL_TEXT_GREATEST),
            );
        }
        if name.eq_ignore_ascii_case(MYSQL_TEXT_NULLIF) {
            let [first, second] = args else {
                return Err(LimboError::ParseError(format!(
                    "{name} takes two arguments"
                )));
            };
            return checked_mysql_text_nullif(first, second);
        }
        if name.eq_ignore_ascii_case(MYSQL_ELT) {
            let [which, choices @ ..] = args else {
                return Err(LimboError::ParseError(format!(
                    "{name} takes a place and the choices to read one out of"
                )));
            };
            // Measured: a place below one or past the last answers nothing.
            let Value::Numeric(turso_core::Numeric::Integer(which)) = which else {
                return Ok(Value::Null);
            };
            let Ok(which) = usize::try_from(*which) else {
                return Ok(Value::Null);
            };
            let chosen = match which.checked_sub(1) {
                Some(at) => choices.get(at),
                None => None,
            };
            return Ok(match chosen {
                Some(Value::Text(chosen)) => Value::build_text(chosen.as_str().to_owned()),
                _ => Value::Null,
            });
        }
        if name.eq_ignore_ascii_case(MYSQL_REGEXP) {
            let [value, pattern] = args else {
                return Err(LimboError::ParseError(format!(
                    "{name} takes two arguments"
                )));
            };
            // MySQL answers nothing for a comparison against nothing, on
            // either side.
            let (Some(value), Value::Text(pattern)) = (matched_text(value), pattern) else {
                return Ok(Value::Null);
            };
            let matched = checked_regexp_match(&value, pattern.as_str())?;
            return Ok(Value::from_i64(i64::from(matched)));
        }
        if name.eq_ignore_ascii_case(MYSQL_LOWER) || name.eq_ignore_ascii_case(MYSQL_UPPER) {
            let [value] = args else {
                return Err(LimboError::ParseError(format!("{name} takes one argument")));
            };
            return checked_mysql_case(value, name.eq_ignore_ascii_case(MYSQL_UPPER));
        }
        if name.eq_ignore_ascii_case(MYSQL_INSTR) {
            let [haystack, needle] = args else {
                return Err(LimboError::ParseError(format!(
                    "{name} takes two arguments"
                )));
            };
            return checked_mysql_search(haystack, needle, 1);
        }
        if name.eq_ignore_ascii_case(MYSQL_LOCATE) {
            let [needle, haystack, rest @ ..] = args else {
                return Err(LimboError::ParseError(format!(
                    "{name} takes two or three arguments"
                )));
            };
            let start = match rest {
                [] => 1,
                [Value::Numeric(Numeric::Integer(start))] => *start,
                [Value::Null] => return Ok(Value::Null),
                _ => {
                    return Err(LimboError::ParseError(
                        "MySQL LOCATE requires an integer start".to_string(),
                    ))
                }
            };
            return checked_mysql_search(haystack, needle, start);
        }
        let like = if name.eq_ignore_ascii_case(MYSQL_UCA9_LIKE) {
            Some(turso_core::mysql_uca9_like as fn(&str, &str, Option<char>) -> Result<bool>)
        } else if name.eq_ignore_ascii_case(MYSQL_UCA400_LIKE) {
            Some(turso_core::mysql_uca400_like as fn(&str, &str, Option<char>) -> Result<bool>)
        } else {
            None
        };
        if let Some(like) = like {
            let [value, pattern, escape] = args else {
                return Err(LimboError::ParseError(format!(
                    "{name} takes three arguments"
                )));
            };
            if args.iter().any(|arg| matches!(arg, Value::Null)) {
                return Ok(Value::Null);
            }
            let (Value::Text(value), Value::Text(pattern), Value::Text(escape)) =
                (value, pattern, escape)
            else {
                return Err(LimboError::ParseError(
                    "MySQL LIKE requires text values".to_string(),
                ));
            };
            let escape = match escape.as_str().chars().collect::<Vec<_>>()[..] {
                [] => None,
                [character] => Some(character),
                _ => {
                    return Err(LimboError::ParseError(
                        "MySQL LIKE escape must be one character".to_string(),
                    ))
                }
            };
            let matched = like(value.as_str(), pattern.as_str(), escape)?;
            return Ok(Value::from_i64(i64::from(matched)));
        }
        if name.eq_ignore_ascii_case(MYSQL_MD5) {
            let [value] = args else {
                return Err(LimboError::ParseError(format!("{name} takes one argument")));
            };
            let Value::Text(text) = value else {
                return Ok(Value::Null);
            };
            return Ok(Value::build_text(format!(
                "{:x}",
                md5::compute(text.as_str().as_bytes())
            )));
        }
        if name.eq_ignore_ascii_case(MYSQL_SHA1) || name.eq_ignore_ascii_case(MYSQL_SHA2) {
            let (value, bits) = match args {
                [value] => (value, Some(160)),
                [value, Value::Numeric(Numeric::Integer(bits))] => (value, Some(*bits)),
                [value, _] => (value, None),
                _ => {
                    return Err(LimboError::ParseError(format!(
                        "{name} takes one or two arguments"
                    )))
                }
            };
            let (Value::Text(text), Some(bits)) = (value, bits) else {
                return Ok(Value::Null);
            };
            return Ok(match written_digest(text.as_str().as_bytes(), bits) {
                Some(digest) => Value::build_text(digest),
                None => Value::Null,
            });
        }
        if name.eq_ignore_ascii_case(MYSQL_SUBSTRING) {
            let (value, from, count) = match args {
                [value, from] => (value, from, None),
                [value, from, count] => (value, from, Some(count)),
                _ => {
                    return Err(LimboError::ParseError(format!(
                        "{name} takes two or three arguments"
                    )))
                }
            };
            let Value::Text(text) = value else {
                return Ok(Value::Null);
            };
            let Value::Numeric(Numeric::Integer(from)) = from else {
                return Ok(Value::Null);
            };
            let count = match count {
                None => None,
                Some(Value::Numeric(Numeric::Integer(count))) => Some(*count),
                Some(_) => return Ok(Value::Null),
            };
            return Ok(Value::build_text(mysql_substring(
                text.as_str(),
                *from,
                count,
            )));
        }
        if name.eq_ignore_ascii_case(MYSQL_ROUND) {
            let [value, places] = args else {
                return Err(LimboError::ParseError(format!(
                    "{name} takes two arguments"
                )));
            };
            let Value::Numeric(Numeric::Integer(places)) = places else {
                return Ok(Value::Null);
            };
            return match value {
                Value::Null => Ok(Value::Null),
                Value::Numeric(Numeric::Integer(whole)) => mysql_round_whole(*whole, *places)
                    .map(Value::from_i64)
                    .ok_or_else(|| {
                        LimboError::InvalidArgument(
                            "BIGINT value is out of range in ROUND".to_string(),
                        )
                    }),
                Value::Numeric(Numeric::Float(real)) => {
                    Ok(Value::from_f64(mysql_round_real(f64::from(*real), *places)))
                }
                _ => Err(LimboError::InvalidArgument(
                    "MySQL ROUND requires a number".to_string(),
                )),
            };
        }
        if name.eq_ignore_ascii_case(MYSQL_SUBSTRING_INDEX) {
            let [value, delimiter, count] = args else {
                return Err(LimboError::ParseError(format!(
                    "{name} takes three arguments"
                )));
            };
            if args.iter().any(|arg| matches!(arg, Value::Null)) {
                return Ok(Value::Null);
            }
            let (
                Value::Text(text),
                Value::Text(delimiter),
                Value::Numeric(Numeric::Integer(count)),
            ) = (value, delimiter, count)
            else {
                return Err(LimboError::InvalidArgument(
                    "MySQL SUBSTRING_INDEX requires text, a text delimiter and a whole count"
                        .to_string(),
                ));
            };
            return Ok(Value::build_text(mysql_substring_index(
                text.as_str(),
                delimiter.as_str(),
                *count,
            )));
        }
        if name.eq_ignore_ascii_case(MYSQL_JSON_EQUALS_INTEGER) {
            let [document, number] = args else {
                return Err(LimboError::ParseError(format!(
                    "{name} takes two arguments"
                )));
            };
            let (Value::Text(document), Value::Numeric(Numeric::Integer(number))) =
                (document, number)
            else {
                return Ok(Value::Null);
            };
            return Ok(
                turso_mysql_parser::json_equals_integer(document.as_str(), *number)
                    .map_or(Value::Null, |equal| Value::from_i64(i64::from(equal))),
            );
        }
        if name.eq_ignore_ascii_case(MYSQL_JSON_COMPARE_INTEGER)
            || name.eq_ignore_ascii_case(MYSQL_JSON_COMPARE_STRING)
        {
            let [document, operand] = args else {
                return Err(LimboError::ParseError(format!(
                    "{name} takes two arguments"
                )));
            };
            let Value::Text(document) = document else {
                return Ok(Value::Null);
            };
            let order = if name.eq_ignore_ascii_case(MYSQL_JSON_COMPARE_INTEGER) {
                let Value::Numeric(Numeric::Integer(integer)) = operand else {
                    return Ok(Value::Null);
                };
                turso_mysql_parser::json_compare_integer(document.as_str(), *integer)
            } else {
                let Value::Text(written) = operand else {
                    return Ok(Value::Null);
                };
                turso_mysql_parser::json_compare_string(document.as_str(), written.as_str())
            };
            return Ok(order.map_or(Value::Null, |order| {
                Value::from_i64(match order {
                    std::cmp::Ordering::Less => -1,
                    std::cmp::Ordering::Equal => 0,
                    std::cmp::Ordering::Greater => 1,
                })
            }));
        }
        if name.eq_ignore_ascii_case(MYSQL_JSON_EXTRACT) {
            let [document, path] = args else {
                return Err(LimboError::ParseError(format!(
                    "{name} takes two arguments"
                )));
            };
            let (Value::Text(document), Value::Text(path)) = (document, path) else {
                return Ok(Value::Null);
            };
            let found = turso_mysql_parser::json_extract(document.as_str(), path.as_str())
                .ok_or_else(|| {
                    LimboError::InvalidArgument(
                        "JSON reading over a value that is not a document, or a path it cannot read"
                            .to_string(),
                    )
                })?;
            return Ok(found.map_or(Value::Null, Value::build_text));
        }
        if name.eq_ignore_ascii_case(MYSQL_JSON_UNQUOTE) {
            let [document] = args else {
                return Err(LimboError::ParseError(format!("{name} takes one argument")));
            };
            let Value::Text(document) = document else {
                return Ok(Value::Null);
            };
            let unquoted =
                turso_mysql_parser::json_unquote(document.as_str()).ok_or_else(|| {
                    LimboError::InvalidArgument(
                        "JSON_UNQUOTE over text that is not a document".into(),
                    )
                })?;
            return Ok(Value::build_text(unquoted));
        }
        if name.eq_ignore_ascii_case(MYSQL_JSON_TEXT_COMPARE) {
            let [text, operand] = args else {
                return Err(LimboError::ParseError(format!(
                    "{name} takes two arguments"
                )));
            };
            return Ok(match compare_json_text(text, operand)? {
                Some(order) => Value::from_i64(match order {
                    std::cmp::Ordering::Less => -1,
                    std::cmp::Ordering::Equal => 0,
                    std::cmp::Ordering::Greater => 1,
                }),
                None => Value::Null,
            });
        }
        if name.eq_ignore_ascii_case(MYSQL_JSON_HOLDS) {
            let [target, candidate] = args else {
                return Err(LimboError::ParseError(format!(
                    "{name} takes two arguments"
                )));
            };
            let candidate = match candidate {
                Value::Null => return Ok(Value::Null),
                Value::Text(candidate) => candidate.as_str(),
                _ => {
                    return Err(LimboError::InvalidArgument(
                        "JSON_CONTAINS requires a JSON document to look for".to_string(),
                    ))
                }
            };
            // Measured on MySQL 8.4.11: text that is not a document is error
            // 3141 there, whether it was written or bound, rather than no
            // answer at all.
            turso_mysql_parser::normalize_json(candidate).map_err(|_| {
                LimboError::InvalidArgument(
                    "JSON_CONTAINS was given text that is not a JSON document".to_string(),
                )
            })?;
            let Value::Text(target) = target else {
                return Ok(Value::Null);
            };
            return Ok(
                match turso_mysql_parser::json_contains(target.as_str(), candidate) {
                    Some(held) => Value::from_i64(i64::from(held)),
                    None => Value::Null,
                },
            );
        }
        if name.eq_ignore_ascii_case(MYSQL_JSON_SEARCH) {
            let [document, every, pattern, escape] = args else {
                return Err(LimboError::ParseError(format!(
                    "{name} takes four arguments"
                )));
            };
            let (Value::Text(document), Value::Text(every), Value::Text(pattern)) =
                (document, every, pattern)
            else {
                return Ok(Value::Null);
            };
            // An empty escape means the pattern has none at all, which is how
            // `JSON_SEARCH(doc, 'one', p, NULL)` reaches here.
            let escape = match escape {
                Value::Text(text) => text.as_str().chars().next(),
                _ => None,
            };
            let every = every.as_str().eq_ignore_ascii_case("all");
            return Ok(
                match turso_mysql_parser::json_search(
                    document.as_str(),
                    every,
                    pattern.as_str(),
                    escape,
                ) {
                    Some(found) => Value::build_text(found),
                    None => Value::Null,
                },
            );
        }
        if name.eq_ignore_ascii_case(MYSQL_JSON_CONTAINS)
            || name.eq_ignore_ascii_case(MYSQL_JSON_OVERLAPS)
            || name.eq_ignore_ascii_case(MYSQL_JSON_MERGE_PATCH)
            || name.eq_ignore_ascii_case(MYSQL_JSON_MERGE_PRESERVE)
        {
            let [left, right] = args else {
                return Err(LimboError::ParseError(format!(
                    "{name} takes two arguments"
                )));
            };
            let (Value::Text(left), Value::Text(right)) = (left, right) else {
                return Ok(Value::Null);
            };
            let (left, right) = (left.as_str(), right.as_str());
            let answer = if name.eq_ignore_ascii_case(MYSQL_JSON_CONTAINS) {
                turso_mysql_parser::json_contains(left, right)
                    .map(|held| Value::from_i64(i64::from(held)))
            } else if name.eq_ignore_ascii_case(MYSQL_JSON_OVERLAPS) {
                turso_mysql_parser::json_overlaps(left, right)
                    .map(|shared| Value::from_i64(i64::from(shared)))
            } else if name.eq_ignore_ascii_case(MYSQL_JSON_MERGE_PATCH) {
                turso_mysql_parser::json_merge_patch(left, right).map(Value::build_text)
            } else {
                turso_mysql_parser::json_merge_preserve(left, right).map(Value::build_text)
            };
            return Ok(answer.unwrap_or(Value::Null));
        }
        if name.eq_ignore_ascii_case(MYSQL_TRUNCATE) {
            let [value, decimals] = args else {
                return Err(LimboError::ParseError(format!(
                    "{name} takes two arguments"
                )));
            };
            let Value::Numeric(Numeric::Integer(decimals)) = decimals else {
                return Ok(Value::Null);
            };
            // A whole number cut at or past the point is the number itself, and
            // the engine holds it as one, so only a negative count moves it.
            return Ok(match value {
                Value::Numeric(Numeric::Integer(integer)) if *decimals >= 0 => {
                    Value::from_i64(*integer)
                }
                Value::Numeric(Numeric::Integer(integer)) => Value::from_i64(
                    turso_mysql_parser::truncate_number(*integer as f64, *decimals) as i64,
                ),
                Value::Numeric(Numeric::Float(float)) => Value::from_f64(
                    turso_mysql_parser::truncate_number(f64::from(*float), *decimals),
                ),
                _ => Value::Null,
            });
        }
        if name.eq_ignore_ascii_case(MYSQL_FORMAT) {
            let [value, decimals] = args else {
                return Err(LimboError::ParseError(format!(
                    "{name} takes two arguments"
                )));
            };
            // Measured on MySQL 8.4.11: a negative count answers no fraction
            // at all rather than rounding to a whole ten.
            let decimals = match decimals {
                Value::Numeric(Numeric::Integer(integer)) => u32::try_from(*integer).unwrap_or(0),
                _ => return Ok(Value::Null),
            };
            // A DECIMAL reaches here written out in full, so it is rounded as
            // written rather than through a double.
            if let Value::Text(written) = value {
                return Ok(Value::build_text(
                    turso_mysql_parser::format_written_decimal(written.as_str(), decimals),
                ));
            }
            let number = match value {
                Value::Numeric(Numeric::Integer(integer)) => *integer as f64,
                Value::Numeric(Numeric::Float(float)) => f64::from(*float),
                _ => return Ok(Value::Null),
            };
            return Ok(Value::build_text(turso_mysql_parser::format_number(
                number, decimals,
            )));
        }
        if name.eq_ignore_ascii_case(MYSQL_DATE_FORMAT)
            || name.eq_ignore_ascii_case(MYSQL_STR_TO_DATE)
        {
            let [value, format] = args else {
                return Err(LimboError::ParseError(format!(
                    "{name} takes two arguments"
                )));
            };
            let (Value::Text(value), Value::Text(format)) = (value, format) else {
                return Ok(Value::Null);
            };
            let read = if name.eq_ignore_ascii_case(MYSQL_DATE_FORMAT) {
                turso_mysql_parser::format_moment(value.as_str(), format.as_str())
            } else {
                turso_mysql_parser::read_by_format(value.as_str(), format.as_str())
            };
            return Ok(match read {
                Some(written) => Value::build_text(written),
                None => Value::Null,
            });
        }
        if name.eq_ignore_ascii_case(MYSQL_SHIFT_MOMENT) {
            let [moment, count, unit] = args else {
                return Err(LimboError::ParseError(format!(
                    "{name} takes three arguments"
                )));
            };
            let (Value::Text(moment), Value::Text(unit)) = (moment, unit) else {
                return Ok(Value::Null);
            };
            let Value::Numeric(Numeric::Integer(count)) = count else {
                return Ok(Value::Null);
            };
            return Ok(
                match turso_mysql_parser::shifted_moment(moment.as_str(), *count, unit.as_str()) {
                    Some(written) => Value::build_text(written),
                    None => Value::Null,
                },
            );
        }
        if name.eq_ignore_ascii_case(MYSQL_TIMESTAMPDIFF) {
            let [unit, from, to] = args else {
                return Err(LimboError::ParseError(format!(
                    "{name} takes three arguments"
                )));
            };
            let (Value::Text(unit), Value::Text(from), Value::Text(to)) = (unit, from, to) else {
                return Ok(Value::Null);
            };
            return Ok(
                match turso_mysql_parser::units_between(unit.as_str(), from.as_str(), to.as_str()) {
                    Some(counted) => Value::from_i64(counted),
                    None => Value::Null,
                },
            );
        }
        if name.eq_ignore_ascii_case(MYSQL_DATEDIFF) {
            let [later, earlier] = args else {
                return Err(LimboError::ParseError(format!(
                    "{name} takes two arguments"
                )));
            };
            let (Value::Text(later), Value::Text(earlier)) = (later, earlier) else {
                return Ok(Value::Null);
            };
            return Ok(
                match turso_mysql_parser::days_between(later.as_str(), earlier.as_str()) {
                    Some(counted) => Value::from_i64(counted),
                    None => Value::Null,
                },
            );
        }
        if MYSQL_BYTE_READINGS
            .iter()
            .any(|reading| name.eq_ignore_ascii_case(reading))
        {
            let [value] = args else {
                return Err(LimboError::ParseError(format!("{name} takes one argument")));
            };
            return byte_reading(name, value);
        }
        if name.eq_ignore_ascii_case(MYSQL_YEARWEEK) {
            let [moment, mode] = args else {
                return Err(LimboError::ParseError(format!(
                    "{name} takes two arguments"
                )));
            };
            let Value::Numeric(Numeric::Integer(mode @ 0..=7)) = mode else {
                return Err(LimboError::ParseError(format!(
                    "{name} takes a mode from 0 through 7"
                )));
            };
            let Value::Text(moment) = moment else {
                return Ok(Value::Null);
            };
            return Ok(
                match turso_mysql_parser::year_and_week(moment.as_str(), *mode as u32) {
                    Some(year_and_week) => Value::from_i64(i64::from(year_and_week)),
                    None => Value::Null,
                },
            );
        }
        if name.eq_ignore_ascii_case(MYSQL_TO_DAYS) {
            let [moment] = args else {
                return Err(LimboError::ParseError(format!("{name} takes one argument")));
            };
            let Value::Text(moment) = moment else {
                return Ok(Value::Null);
            };
            return Ok(
                match turso_mysql_parser::days_from_the_year_zero(moment.as_str()) {
                    Some(days) => Value::from_i64(days),
                    None => Value::Null,
                },
            );
        }
        if name.eq_ignore_ascii_case(MYSQL_WEEK) {
            let [moment, mode] = args else {
                return Err(LimboError::ParseError(format!(
                    "{name} takes two arguments"
                )));
            };
            let Value::Numeric(Numeric::Integer(mode @ 0..=7)) = mode else {
                return Err(LimboError::ParseError(format!(
                    "{name} takes a mode from 0 through 7"
                )));
            };
            let Value::Text(moment) = moment else {
                return Ok(Value::Null);
            };
            return Ok(
                match turso_mysql_parser::week_number(moment.as_str(), *mode as u32) {
                    Some(week) => Value::from_i64(i64::from(week)),
                    None => Value::Null,
                },
            );
        }
        if MYSQL_JSON_READINGS
            .iter()
            .any(|reading| name.eq_ignore_ascii_case(reading))
        {
            let [value] = args else {
                return Err(LimboError::ParseError(format!("{name} takes one argument")));
            };
            return Ok(json_reading(name, value));
        }
        Err(LimboError::ParseError(format!(
            "no such MySQL function: {name}"
        )))
    }
}

/// Writes a JSON document the way MySQL writes one.
///
/// The engine's own JSON reading answers a document without the space MySQL
/// puts after a comma or a colon, so a value read out of a column has to be
/// written again before a client sees it. Nothing but the rendered SQL names
/// this, and it is not a function a client can call.
pub(crate) const MYSQL_JSON_DOCUMENT: &str = "mysql_json_document";
/// Names a document's kind, counts what it holds at the top, lists an object's
/// keys, and writes text as a JSON string. The engine's own JSON functions
/// answer each of these differently — a different vocabulary, arrays only, no
/// keys at all — so each is read here instead.
pub(crate) const MYSQL_JSON_TYPE: &str = "mysql_json_type";
pub(crate) const MYSQL_JSON_LENGTH: &str = "mysql_json_length";
pub(crate) const MYSQL_JSON_KEYS: &str = "mysql_json_keys";
pub(crate) const MYSQL_JSON_QUOTE: &str = "mysql_json_quote";

/// Writes a moment out the way `DATE_FORMAT` writes one, and reads one back
/// the way `STR_TO_DATE` reads one. The engine's own strftime answers a few of
/// MySQL's specifiers and none of the rest, and has no reader at all.
pub(crate) const MYSQL_DATE_FORMAT: &str = "mysql_date_format";
pub(crate) const MYSQL_STR_TO_DATE: &str = "mysql_str_to_date";
/// Shifting a moment MySQL's way, which the engine's own month arithmetic does
/// not do: measured on 8.4.11, `2026-01-31` a month on is `2026-02-28`, where
/// the engine overflows into March.
pub(crate) const MYSQL_SHIFT_MOMENT: &str = "mysql_shift_moment";
/// Counting the time between two moments MySQL's way: `TIMESTAMPDIFF` counts
/// months by the calendar and seconds to the microsecond, and `DATEDIFF`
/// counts the days alone. The engine has no calendar month, and its
/// `unixepoch` drops the fraction of a second.
pub(crate) const MYSQL_TIMESTAMPDIFF: &str = "mysql_timestampdiff";
pub(crate) const MYSQL_DATEDIFF: &str = "mysql_datediff";
/// Numbering a moment's week the way `WEEK` does, in one of its eight modes.
/// The engine's `strftime` has two week numberings, neither of them MySQL's.
pub(crate) const MYSQL_WEEK: &str = "mysql_week";
/// Counts `YEARWEEK`'s year and week, and `TO_DAYS`'s days, the way MySQL
/// counts them; the engine has neither.
pub(crate) const MYSQL_YEARWEEK: &str = "mysql_yearweek";
pub(crate) const MYSQL_TO_DAYS: &str = "mysql_to_days";
/// Reads the bytes of a value the way `ASCII`, `ORD`, `CRC32`, `QUOTE` and
/// `TO_BASE64` do; the engine has none of them.
pub(crate) const MYSQL_BYTE_READINGS: [&str; 10] = [
    "mysql_ascii",
    "mysql_ord",
    "mysql_crc32",
    "mysql_quote",
    "mysql_to_base64",
    "mysql_inet_aton",
    "mysql_inet_ntoa",
    "mysql_is_ipv4",
    "mysql_time_to_sec",
    "mysql_sec_to_time",
];

/// Answers one of `MYSQL_BYTE_READINGS` over a value, which reaches it as a
/// word, or as a whole number MySQL would write out before reading its bytes.
fn byte_reading(name: &str, value: &Value) -> Result<Value> {
    if name.eq_ignore_ascii_case("mysql_sec_to_time") {
        return Ok(match value {
            Value::Numeric(Numeric::Integer(seconds)) => {
                Value::build_text(turso_mysql_parser::time_of_seconds(*seconds).0)
            }
            _ => Value::Null,
        });
    }
    if name.eq_ignore_ascii_case("mysql_time_to_sec") {
        return Ok(match value {
            Value::Text(stored) => turso_mysql_parser::seconds_in_the_time(stored.as_str())
                .map_or(Value::Null, Value::from_i64),
            _ => Value::Null,
        });
    }
    if name.eq_ignore_ascii_case("mysql_inet_ntoa") {
        return Ok(match value {
            Value::Numeric(Numeric::Integer(number)) => {
                turso_mysql_parser::inet_ntoa(*number).map_or(Value::Null, Value::build_text)
            }
            _ => Value::Null,
        });
    }
    let written = match value {
        Value::Null => {
            return Ok(if name.eq_ignore_ascii_case("mysql_quote") {
                Value::build_text(turso_mysql_parser::quoted_for_sql(None))
            } else {
                Value::Null
            });
        }
        Value::Text(text) => text.as_str().to_owned(),
        Value::Numeric(Numeric::Integer(number)) => number.to_string(),
        _ => {
            return Err(LimboError::ParseError(format!(
                "{name} takes a word or a whole number"
            )))
        }
    };
    Ok(if name.eq_ignore_ascii_case("mysql_ascii") {
        Value::from_i64(turso_mysql_parser::first_byte(written.as_bytes()))
    } else if name.eq_ignore_ascii_case("mysql_ord") {
        Value::from_i64(turso_mysql_parser::first_character_code(&written))
    } else if name.eq_ignore_ascii_case("mysql_crc32") {
        Value::from_i64(i64::from(turso_mysql_parser::crc32(written.as_bytes())))
    } else if name.eq_ignore_ascii_case("mysql_quote") {
        Value::build_text(turso_mysql_parser::quoted_for_sql(Some(&written)))
    } else if name.eq_ignore_ascii_case("mysql_inet_aton") {
        match turso_mysql_parser::inet_aton(&written) {
            Some(number) => Value::from_i64(
                i64::try_from(number).expect("four bytes of an address fit a signed number"),
            ),
            None => Value::Null,
        }
    } else if name.eq_ignore_ascii_case("mysql_is_ipv4") {
        Value::from_i64(i64::from(turso_mysql_parser::is_ipv4(&written)))
    } else {
        Value::build_text(turso_mysql_parser::to_base64(written.as_bytes()))
    })
}
/// Writes the thirty-two hexadecimal characters `MD5` answers. The engine
/// keeps its digests in an extension this frontend does not register.
pub(crate) const MYSQL_MD5: &str = "mysql_md5";
/// Write the forty hexadecimal characters `SHA1` answers, and the ones `SHA2`
/// answers for each size it takes. Neither is in the engine.
pub(crate) const MYSQL_SHA1: &str = "mysql_sha1";
pub(crate) const MYSQL_SHA2: &str = "mysql_sha2";

/// Writes a digest of the bytes in lower-case hexadecimal, the way `SHA1` and
/// `SHA2` answer it. A size `SHA2` does not have answers nothing: measured on
/// MySQL 8.4.11, `SHA2('abc', 1)` is NULL, and 0 means 256.
fn written_digest(bytes: &[u8], bits: i64) -> Option<String> {
    use sha2::Digest;
    let digest = match bits {
        160 => sha1::Sha1::digest(bytes).to_vec(),
        224 => sha2::Sha224::digest(bytes).to_vec(),
        0 | 256 => sha2::Sha256::digest(bytes).to_vec(),
        384 => sha2::Sha384::digest(bytes).to_vec(),
        512 => sha2::Sha512::digest(bytes).to_vec(),
        _ => return None,
    };
    Some(digest.iter().map(|byte| format!("{byte:02x}")).collect())
}

/// Reads part of a word the way `SUBSTRING` does, counting characters.
///
/// Measured on MySQL 8.4.11: a place of 0 answers nothing, a negative place
/// counts back from the end and answers nothing when it reaches past the start,
/// a place past the end answers nothing, and a count at or below zero answers
/// nothing — `SUBSTR('apple', 0, 3)`, `SUBSTR('apple', -6)`, `SUBSTR('apple', 6)`
/// and `SUBSTR('apple', 2, 0)` are all empty — where the engine's `substr`
/// reads the first two as the start.
pub(crate) const MYSQL_SUBSTRING: &str = "mysql_substring";

fn mysql_substring(text: &str, from: i64, count: Option<i64>) -> String {
    if from == 0 || count.is_some_and(|count| count <= 0) {
        return String::new();
    }
    let characters = text.chars().count() as i64;
    let start = if from < 0 {
        characters + from
    } else {
        from - 1
    };
    if start < 0 || start >= characters {
        return String::new();
    }
    let taken = count.unwrap_or(characters).min(characters - start);
    text.chars()
        .skip(start as usize)
        .take(taken as usize)
        .collect()
}

/// Reads the part of a word before the count-th delimiter, or after it
/// counting from the end, the way `SUBSTRING_INDEX` does.
///
/// Measured on MySQL 8.4.11: the delimiter is matched by its bytes, so case
/// counts — `SUBSTRING_INDEX('aXbxcXd', 'x', 1)` is `aXb` — and copies of it are
/// found from the left without overlapping, whichever way the count runs:
/// `SUBSTRING_INDEX('aaaaa', 'aa', -2)` is `aaa`, the part after the first of
/// the two copies found from the left. A count of 0 or an empty delimiter
/// answers nothing, and a count past the copies there are answers the whole
/// word.
pub(crate) const MYSQL_SUBSTRING_INDEX: &str = "mysql_substring_index";

fn mysql_substring_index(text: &str, delimiter: &str, count: i64) -> String {
    if count == 0 || delimiter.is_empty() {
        return String::new();
    }
    let found = text
        .match_indices(delimiter)
        .map(|(at, _)| at)
        .collect::<Vec<_>>();
    if count > 0 {
        return match usize::try_from(count - 1).ok().and_then(|at| found.get(at)) {
            Some(&at) => text[..at].to_owned(),
            None => text.to_owned(),
        };
    }
    let from_the_end = count.unsigned_abs();
    match u64::try_from(found.len())
        .ok()
        .and_then(|copies| copies.checked_sub(from_the_end))
        .and_then(|at| found.get(usize::try_from(at).ok()?))
    {
        Some(&at) => text[at + delimiter.len()..].to_owned(),
        None => text.to_owned(),
    }
}

/// Rounds a whole number or a real one the way `ROUND` does. The engine's
/// `round` takes no places left of the point, and rounds a real number half
/// away from zero where MySQL rounds it half to even.
pub(crate) const MYSQL_ROUND: &str = "mysql_round";

/// Rounds a whole number to a place left of the point, half away from zero.
///
/// Measured on MySQL 8.4.11: `ROUND(15, -1)` is 20 and `ROUND(-25, -1)` is -30,
/// and a place at or right of the point leaves the number as it is. An answer
/// past a `BIGINT` is MySQL's 1690, and answers nothing here.
fn mysql_round_whole(whole: i64, places: i64) -> Option<i64> {
    if places >= 0 {
        return Some(whole);
    }
    // A place past the nineteenth digit rounds every BIGINT to zero.
    let Ok(digits) = u32::try_from(places.unsigned_abs()) else {
        return Some(0);
    };
    if digits > 19 {
        return Some(0);
    }
    let step = 10_i128.pow(digits);
    let magnitude = i128::from(whole).abs();
    let rounded = (magnitude + step / 2) / step * step;
    i64::try_from(if whole < 0 { -rounded } else { rounded }).ok()
}

/// Rounds a real number the way MySQL's `my_double_round` does: it scales by
/// the power of ten the places name, rounds half to even, and scales back.
///
/// Measured on MySQL 8.4.11: `ROUND(2.5)` is 2, `ROUND(0.15e0, 1)` is 0.2
/// because 0.15 times ten is 1.5 exactly, `ROUND(1.005e0, 2)` is 1 because
/// 1.005 times a hundred falls short of 100.5, and `ROUND(2.675e0, 2)` is 2.68.
/// A scale past the largest double answers the number itself to the right of
/// the point and zero to the left of it.
fn mysql_round_real(value: f64, places: i64) -> f64 {
    let digits = places.unsigned_abs();
    // MySQL reads the power from a table of written constants up to 1e308,
    // and past that works out an infinity.
    let scale = if digits <= 308 {
        format!("1e{digits}")
            .parse::<f64>()
            .expect("a written power of ten is a number")
    } else {
        f64::INFINITY
    };
    if places < 0 {
        if scale.is_infinite() {
            return 0.0;
        }
        return (value / scale).round_ties_even() * scale;
    }
    let scaled = value * scale;
    if scaled.is_infinite() {
        return value;
    }
    scaled.round_ties_even() / scale
}

/// Writes a whole number in binary or in octal, the way `BIN` and `OCT` do.
///
/// Measured on MySQL 8.4.11: `OCT(-3)` answers 1777777777777777777775, the
/// number read as an unsigned one, so a negative is written by its bits rather
/// than with a sign in front.
fn written_in_radix(number: i64, radix: u32) -> String {
    let bits = number as u64;
    if bits == 0 {
        return "0".to_owned();
    }
    let digits = b"01234567";
    let mut written = Vec::new();
    let mut left = bits;
    while left > 0 {
        written.push(digits[(left % u64::from(radix)) as usize]);
        left /= u64::from(radix);
    }
    written.reverse();
    String::from_utf8(written).expect("radix digits are ASCII")
}

/// Writes a whole number in binary, and the same in octal. The engine has
/// neither, and MySQL reads a negative one as its bits.
pub(crate) const MYSQL_BIN: &str = "mysql_bin";
pub(crate) const MYSQL_OCT: &str = "mysql_oct";
/// Finds a word among the ones that follow it, and reads one out by its place.
/// The engine has neither, and MySQL matches the word by the collation.
pub(crate) const MYSQL_FIELD: &str = "mysql_field";
pub(crate) const MYSQL_ELT: &str = "mysql_elt";

/// Answers whether a pattern matches, which is what `REGEXP` and `RLIKE` ask.
/// The engine keeps its own matching in an extension this frontend does not
/// register, and MySQL's is held to a collation rather than to the pattern.
pub(crate) const MYSQL_REGEXP: &str = "mysql_regexp";
pub(crate) const MYSQL_UCA9_LIKE: &str = "mysql_uca9_like";
pub(crate) const MYSQL_UCA400_LIKE: &str = "mysql_uca400_like";
pub(crate) const MYSQL_LOWER: &str = "mysql_lower";
pub(crate) const MYSQL_UPPER: &str = "mysql_upper";
pub(crate) const MYSQL_INSTR: &str = "mysql_instr";
pub(crate) const MYSQL_LOCATE: &str = "mysql_locate";
pub(crate) const MYSQL_TEXT_GREATEST: &str = "mysql_text_greatest";
pub(crate) const MYSQL_TEXT_LEAST: &str = "mysql_text_least";
pub(crate) const MYSQL_TEXT_NULLIF: &str = "mysql_text_nullif";

fn checked_mysql_case(value: &Value, uppercase: bool) -> Result<Value> {
    let Value::Text(value) = value else {
        return if matches!(value, Value::Null) {
            Ok(Value::Null)
        } else {
            Err(LimboError::InvalidArgument(
                "MySQL case conversion requires text".to_string(),
            ))
        };
    };
    let value = value.as_str();
    if !value.is_ascii() {
        return Err(LimboError::InvalidArgument(
            "Unicode LOWER/UPPER requires MySQL-compatible case conversion".to_string(),
        ));
    }
    Ok(Value::build_text(if uppercase {
        value.to_ascii_uppercase()
    } else {
        value.to_ascii_lowercase()
    }))
}

fn checked_mysql_search(haystack: &Value, needle: &Value, start: i64) -> Result<Value> {
    if matches!(haystack, Value::Null) || matches!(needle, Value::Null) {
        return Ok(Value::Null);
    }
    let (Value::Text(haystack), Value::Text(needle)) = (haystack, needle) else {
        return Err(LimboError::InvalidArgument(
            "MySQL INSTR/LOCATE requires text".to_string(),
        ));
    };
    let (haystack, needle) = (haystack.as_str(), needle.as_str());
    if !haystack.is_ascii() || !needle.is_ascii() {
        return Err(LimboError::InvalidArgument(
            "Unicode INSTR/LOCATE requires MySQL-compatible case folding".to_string(),
        ));
    }
    let Ok(start) = usize::try_from(start) else {
        return Ok(Value::from_i64(0));
    };
    let Some(offset) = start.checked_sub(1) else {
        return Ok(Value::from_i64(0));
    };
    let Some(tail) = haystack.get(offset..) else {
        return Ok(Value::from_i64(0));
    };
    let found = tail
        .to_ascii_lowercase()
        .find(&needle.to_ascii_lowercase())
        .map(|position| i64::try_from(offset + position + 1))
        .transpose()
        .map_err(|_| LimboError::IntegerOverflow)?
        .unwrap_or(0);
    Ok(Value::from_i64(found))
}

fn checked_mysql_text_extreme(values: &[Value], greatest: bool) -> Result<Value> {
    if values.iter().any(|value| matches!(value, Value::Null)) {
        return Ok(Value::Null);
    }
    let mut best = match values.first() {
        Some(Value::Text(text)) => text,
        _ => {
            return Err(LimboError::InvalidArgument(
                "MySQL text GREATEST/LEAST requires text values".to_string(),
            ))
        }
    };
    for value in values.iter().skip(1) {
        let Value::Text(candidate) = value else {
            return Err(LimboError::InvalidArgument(
                "MySQL text GREATEST/LEAST requires text values".to_string(),
            ));
        };
        let order = turso_core::mysql_uca9_compare(candidate.as_str(), best.as_str());
        if (greatest && !order.is_lt()) || (!greatest && order.is_lt()) {
            best = candidate;
        }
    }
    Ok(Value::build_text(best.as_str().to_owned()))
}

fn checked_mysql_text_nullif(first: &Value, second: &Value) -> Result<Value> {
    let Value::Text(first) = first else {
        return Err(LimboError::InvalidArgument(
            "MySQL text NULLIF requires text values".to_string(),
        ));
    };
    match second {
        Value::Null => Ok(Value::build_text(first.as_str().to_owned())),
        Value::Text(second)
            if turso_core::mysql_uca9_compare(first.as_str(), second.as_str()).is_eq() =>
        {
            Ok(Value::Null)
        }
        Value::Text(_) => Ok(Value::build_text(first.as_str().to_owned())),
        _ => Err(LimboError::InvalidArgument(
            "MySQL text NULLIF requires text values".to_string(),
        )),
    }
}

/// The text a `REGEXP` matches against.
///
/// Measured on MySQL 8.4.11: `5 REGEXP '5'` answers 1, so a number is matched
/// as the text it is written as. Nothing else is a value to match.
fn matched_text(value: &Value) -> Option<String> {
    match value {
        Value::Text(text) => Some(text.as_str().to_owned()),
        Value::Numeric(turso_core::Numeric::Integer(number)) => Some(number.to_string()),
        _ => None,
    }
}

fn checked_regexp_match(value: &str, pattern: &str) -> Result<bool> {
    if !value.is_ascii() || !pattern.is_ascii() {
        return Err(LimboError::InvalidArgument(
            "Unicode REGEXP requires MySQL-compatible full case folding".to_string(),
        ));
    }
    Ok(compiled_pattern(pattern)?.is_match(value))
}

/// Compiles a `REGEXP` pattern, remembering the last one compiled.
///
/// A statement matches one pattern against every row it reads, so the pattern
/// is compiled once and the rows after the first find it already built.
///
/// Measured on MySQL 8.4.11 under the default collation: `'Alpha' REGEXP
/// 'alpha'` answers 1, so the match ignores case — and `'café' REGEXP 'cafe'`
/// answers 0, so it does not ignore accents, which the same collation does
/// ignore when comparing. The case-folding flag goes in front of the pattern
/// rather than around it, so a pattern that turns it off again still can.
fn compiled_pattern(pattern: &str) -> Result<Arc<regex::Regex>> {
    static LAST: Mutex<Option<(String, Arc<regex::Regex>)>> = Mutex::new(None);
    let mut last = LAST.lock();
    if let Some((remembered, compiled)) = last.as_ref() {
        if remembered == pattern {
            return Ok(compiled.clone());
        }
    }
    let compiled = regex::Regex::new(&format!("(?i){pattern}")).map_err(|error| {
        LimboError::InvalidArgument(format!("REGEXP pattern is not one this reads: {error}"))
    })?;
    let compiled = Arc::new(compiled);
    *last = Some((pattern.to_owned(), compiled.clone()));
    Ok(compiled)
}
/// Writes a number for a person to read, grouped in threes. The engine has no
/// grouping of any kind, so the whole of it is written by the dialect.
pub(crate) const MYSQL_FORMAT: &str = "mysql_format";
/// Cuts a number off at a count of places. The engine rounds where MySQL cuts,
/// and cutting the double behind a written decimal answers the digit below the
/// one MySQL answers, so this is cut by the dialect.
pub(crate) const MYSQL_TRUNCATE: &str = "mysql_truncate";
/// Answers whether one document holds another. The engine has no containment
/// of its own, so the whole of it is answered by the dialect.
pub(crate) const MYSQL_JSON_CONTAINS: &str = "mysql_json_contains";
/// Compares a stored JSON number with a signed SQL integer without losing
/// integers beyond the exact range of binary64.
pub(crate) const MYSQL_JSON_EQUALS_INTEGER: &str = "mysql_json_equals_integer";
pub(crate) const MYSQL_JSON_COMPARE_INTEGER: &str = "mysql_json_compare_integer";
pub(crate) const MYSQL_JSON_COMPARE_STRING: &str = "mysql_json_compare_string";
/// Answers whether two documents share anything, and the two ways MySQL merges
/// one into another. The engine has none of the three, so each is answered by
/// the dialect.
pub(crate) const MYSQL_JSON_OVERLAPS: &str = "mysql_json_overlaps";
/// Finds the paths to the strings a pattern matches. The engine has no search
/// of its own, so the whole of it is answered by the dialect.
pub(crate) const MYSQL_JSON_SEARCH: &str = "mysql_json_search";
pub(crate) const MYSQL_JSON_MERGE_PATCH: &str = "mysql_json_merge_patch";
pub(crate) const MYSQL_JSON_MERGE_PRESERVE: &str = "mysql_json_merge_preserve";
/// Reads what a path names in a document, and takes the quotes off what was
/// found, the way `JSON_EXTRACT` and `JSON_UNQUOTE` do. The engine's own `->`
/// and `->>` read `$[0]` over something that is not an array as nothing where
/// MySQL reads the value itself, and answer the JSON null as no value where
/// MySQL answers the word `null`.
pub(crate) const MYSQL_JSON_EXTRACT: &str = "mysql_json_extract";
pub(crate) const MYSQL_JSON_UNQUOTE: &str = "mysql_json_unquote";
/// Compares the text a JSON reading answers with a value, the way MySQL
/// compares it: against a word under `utf8mb4_bin`, and against a number as
/// two doubles.
pub(crate) const MYSQL_JSON_TEXT_COMPARE: &str = "mysql_json_text_compare";
/// `JSON_CONTAINS` where the document looked for may be bound, which has to
/// refuse text that is not a document the way MySQL does rather than answer
/// nothing.
pub(crate) const MYSQL_JSON_HOLDS: &str = "mysql_json_holds";

/// Compares the text a JSON reading answers with a value, or answers nothing
/// when either is NULL.
///
/// Measured on MySQL 8.4.11: the text carries `utf8mb4_bin`, so against a word
/// it tells `en` from `EN` and pads with spaces — `en` equals `en  `. Against a
/// number both sides are read as doubles, the text by the number it begins
/// with, so `1.50` equals 1.5, `true` equals 0 and `1abc` equals 1.
fn compare_json_text(text: &Value, operand: &Value) -> Result<Option<std::cmp::Ordering>> {
    let text = match text {
        Value::Null => return Ok(None),
        Value::Text(text) => text.as_str(),
        _ => {
            return Err(LimboError::InternalError(
                "a JSON reading answered something other than text".to_string(),
            ))
        }
    };
    let number = match operand {
        Value::Null => return Ok(None),
        Value::Text(operand) => {
            return Ok(Some(compare_padded_with_spaces(
                text.as_bytes(),
                operand.as_str().as_bytes(),
            )))
        }
        Value::Numeric(Numeric::Integer(integer)) => *integer as f64,
        Value::Numeric(Numeric::Float(float)) => f64::from(*float),
        _ => {
            return Err(LimboError::InvalidArgument(
                "a JSON reading is compared with a word or a number".to_string(),
            ))
        }
    };
    let read = text_as_a_double(text).ok_or_else(|| {
        LimboError::InvalidArgument(
            "a JSON reading names a number too large for a double".to_string(),
        )
    })?;
    Ok(read.partial_cmp(&number))
}

/// Compares two byte strings under `utf8mb4_bin`, whose order is the order of
/// the bytes and which reads the shorter one as though it went on in spaces.
fn compare_padded_with_spaces(left: &[u8], right: &[u8]) -> std::cmp::Ordering {
    let longest = left.len().max(right.len());
    (0..longest)
        .map(|at| {
            let left = left.get(at).copied().unwrap_or(b' ');
            let right = right.get(at).copied().unwrap_or(b' ');
            left.cmp(&right)
        })
        .find(|order| order.is_ne())
        .unwrap_or(std::cmp::Ordering::Equal)
}

/// Reads text as the number it begins with, the way MySQL reads a word it
/// compares with a number.
///
/// Measured on MySQL 8.4.11: spaces, tabs and line breaks in front are passed
/// over; a sign, digits, a point and an exponent are read for as long as they
/// make a number, so `1e5x` is 100000, `1e` is 1 and `.5` is 0.5; and anything
/// else is 0 — `abc`, `0x10`, `inf`, `nan` and a full-width digit alike. A
/// number too large for a double answers nothing, the one case this does not
/// read MySQL's way.
fn text_as_a_double(text: &str) -> Option<f64> {
    let bytes = text.as_bytes();
    let mut at = bytes
        .iter()
        .position(|byte| !matches!(byte, b' ' | b'\t' | b'\n' | 0x0b | 0x0c | b'\r'))
        .unwrap_or(bytes.len());
    let start = at;
    if matches!(bytes.get(at), Some(b'+' | b'-')) {
        at += 1;
    }
    let digits_before = count_digits(&bytes[at..]);
    at += digits_before;
    let mut digits_after = 0;
    if bytes.get(at) == Some(&b'.') {
        digits_after = count_digits(&bytes[at + 1..]);
        if digits_before + digits_after > 0 {
            at += 1 + digits_after;
        }
    }
    if digits_before + digits_after == 0 {
        return Some(0.0);
    }
    if matches!(bytes.get(at), Some(b'e' | b'E')) {
        let mut exponent_at = at + 1;
        if matches!(bytes.get(exponent_at), Some(b'+' | b'-')) {
            exponent_at += 1;
        }
        let exponent_digits = count_digits(&bytes[exponent_at..]);
        if exponent_digits > 0 {
            at = exponent_at + exponent_digits;
        }
    }
    let read = text[start..at]
        .parse::<f64>()
        .expect("the digits read make a number");
    read.is_finite().then_some(read)
}

fn count_digits(bytes: &[u8]) -> usize {
    bytes
        .iter()
        .take_while(|byte| byte.is_ascii_digit())
        .count()
}

const MYSQL_JSON_READINGS: [&str; 5] = [
    MYSQL_JSON_DOCUMENT,
    MYSQL_JSON_TYPE,
    MYSQL_JSON_LENGTH,
    MYSQL_JSON_KEYS,
    MYSQL_JSON_QUOTE,
];

/// Answers one JSON reading over one value.
///
/// A value that is not text is not a document, and every one of these answers
/// no value for that, which is what MySQL answers for a NULL column.
fn json_reading(name: &str, value: &Value) -> Value {
    let Value::Text(text) = value else {
        return value.clone();
    };
    let text = text.as_str();
    if name.eq_ignore_ascii_case(MYSQL_JSON_QUOTE) {
        return Value::build_text(turso_mysql_parser::json_quote(text));
    }
    if name.eq_ignore_ascii_case(MYSQL_JSON_TYPE) {
        return match turso_mysql_parser::json_type(text) {
            Some(kind) => Value::build_text(kind.to_owned()),
            None => Value::Null,
        };
    }
    if name.eq_ignore_ascii_case(MYSQL_JSON_LENGTH) {
        return match turso_mysql_parser::json_length(text)
            .and_then(|length| i64::try_from(length).ok())
        {
            Some(length) => Value::from_i64(length),
            None => Value::Null,
        };
    }
    if name.eq_ignore_ascii_case(MYSQL_JSON_KEYS) {
        return match turso_mysql_parser::json_keys(text) {
            Some(keys) => Value::build_text(keys),
            None => Value::Null,
        };
    }
    match turso_mysql_parser::normalize_json(text) {
        Ok(canonical) => Value::build_text(canonical),
        Err(_) => value.clone(),
    }
}

struct MySqlIntegerValidator;

impl AssignmentValidator for MySqlIntegerValidator {
    fn check_assignment(
        &self,
        table_name: &str,
        table_sql: Option<&str>,
        operation: AssignmentOperation,
        values: &[Value],
    ) -> Result<Option<Vec<Value>>> {
        check_mysql_assignment(table_name, table_sql, operation, values, None)
    }
}

/// Puts a `JSON` value into the form MySQL stores a document in.
fn document_value(table_name: &str, column_index: usize, value: &Value) -> Result<Value> {
    let refuse = || {
        LimboError::from(AssignmentError::NotADocument {
            table: table_name.to_string(),
            column: column_index + 1,
        })
    };
    let Value::Text(text) = value else {
        return Err(refuse());
    };
    let canonical = turso_mysql_parser::normalize_json(text.as_str()).map_err(|_| refuse())?;
    Ok(Value::build_text(canonical))
}

/// Puts a `DATE`, a `DATETIME` or a `TIME` into the form MySQL stores it in.
///
/// A value written as a number reads the same way its digits do: measured on
/// 8.4.11, the number 20260906 and the text `'20260906'` both name the sixth
/// of September, and 123456 and `'123456'` both name `12:34:56`.
fn temporal_value(
    table_name: &str,
    column_index: usize,
    type_name: &str,
    value: &Value,
    precision: u8,
) -> Result<Value> {
    let refuse = || {
        LimboError::from(AssignmentError::IncorrectTemporal {
            table: table_name.to_string(),
            column: column_index + 1,
            type_name: type_name.to_string(),
        })
    };
    let written = value_as_written(value).ok_or_else(refuse)?;
    let read = match type_name {
        "DATE" => turso_mysql_parser::normalize_date(&written),
        "TIME" => turso_mysql_parser::normalize_time_with_precision(&written, precision),
        "TIMESTAMP" => turso_mysql_parser::normalize_datetime_with_precision(&written, precision)
            .filter(|moment| {
                ("1970-01-01 00:00:01"..="2038-01-19 03:14:07").contains(&&moment[..19])
            }),
        _ => turso_mysql_parser::normalize_datetime_with_precision(&written, precision),
    };
    Ok(Value::build_text(read.ok_or_else(refuse)?))
}

/// Puts a `YEAR` into the year MySQL stores.
///
/// Measured: a year written as text and one written as a number differ at the
/// zero, where the text is 2000 and the number is the zero year, so the two
/// are read apart rather than through one path.
fn year_value(table_name: &str, column_index: usize, value: &Value) -> Result<Value> {
    let year = match value {
        Value::Text(text) => turso_mysql_parser::normalize_year(text.as_str()),
        Value::Numeric(Numeric::Integer(number)) => turso_mysql_parser::year_from_number(*number),
        // Measured: 69.7 is 1970, so a year written with a fraction is rounded
        // rather than cut.
        Value::Numeric(Numeric::Float(number)) => {
            turso_mysql_parser::year_from_number(number.round() as i64)
        }
        _ => None,
    };
    let year = year.ok_or_else(|| AssignmentError::OutOfRange {
        table: table_name.to_string(),
        column: column_index + 1,
        type_name: "YEAR".to_string(),
        value: match value {
            Value::Numeric(Numeric::Integer(number)) => *number,
            _ => 0,
        },
    })?;
    Ok(Value::from_i64(i64::from(year)))
}

/// Reads a value as the text MySQL would have read, so that a number written
/// as a number and the same digits written as text name the same moment.
fn value_as_written(value: &Value) -> Option<String> {
    match value {
        Value::Text(text) => Some(text.as_str().to_string()),
        Value::Numeric(Numeric::Integer(number)) => Some(number.to_string()),
        Value::Numeric(Numeric::Float(number)) => Some(number.to_string()),
        _ => None,
    }
}

/// Puts an `ENUM` value into the member spelling its column declares.
///
/// Measured on MySQL 8.4.11: a member is matched ignoring case and ignoring
/// trailing spaces, so `'SMALL'` and `'small  '` both store `small`. Text
/// naming no member is read as a position instead — `'0'` is the empty error
/// member and `'1'` is the first declared one — while a number written as a
/// number is a position and nothing else, and there the zero is refused.
fn member_value(
    table_name: &str,
    column_index: usize,
    members: &[String],
    value: &Value,
) -> Result<Value> {
    let refuse = || {
        LimboError::from(AssignmentError::NotAMember {
            table: table_name.to_string(),
            column: column_index + 1,
        })
    };
    if let Value::Text(text) = value {
        let written = text.as_str().trim_end_matches(' ');
        if let Some(member) = members
            .iter()
            .find(|member| member.eq_ignore_ascii_case(written))
        {
            return Ok(Value::build_text(member.clone()));
        }
        let position = written.parse::<u64>().map_err(|_| refuse())?;
        return Ok(Value::build_text(
            member_at(members, position).ok_or_else(refuse)?,
        ));
    }
    let position = whole_number(value).ok_or_else(refuse)?;
    if position == 0 {
        return Err(refuse());
    }
    Ok(Value::build_text(
        member_at(members, position).ok_or_else(refuse)?,
    ))
}

/// Puts a `SET` value into the members its column declares, in declared order.
///
/// Measured on MySQL 8.4.11: `'exec,read'` stores `read,exec`, `'read,read'`
/// stores `read`, and the match ignores case and trailing spaces on the value
/// as a whole — a space around a comma is not trimmed and answers 1265.
/// Text naming no member is read as a bit for each declared member, which is
/// what a number written as a number always is.
fn member_subset_value(
    table_name: &str,
    column_index: usize,
    members: &[String],
    value: &Value,
) -> Result<Value> {
    let refuse = || {
        LimboError::from(AssignmentError::NotAMember {
            table: table_name.to_string(),
            column: column_index + 1,
        })
    };
    if let Value::Text(text) = value {
        let written = text.as_str().trim_end_matches(' ');
        if written.is_empty() {
            return Ok(Value::build_text(String::new()));
        }
        if let Some(chosen) = chosen_members(members, written) {
            return Ok(Value::build_text(join_members(members, chosen)));
        }
        let bits = written.parse::<u64>().map_err(|_| refuse())?;
        return Ok(Value::build_text(
            member_bits(members, bits).ok_or_else(refuse)?,
        ));
    }
    let bits = whole_number(value).ok_or_else(refuse)?;
    Ok(Value::build_text(
        member_bits(members, bits).ok_or_else(refuse)?,
    ))
}

/// Returns which members a comma-separated value names, or nothing if any
/// part of it names none.
fn chosen_members(members: &[String], written: &str) -> Option<Vec<bool>> {
    let mut chosen = vec![false; members.len()];
    for part in written.split(',') {
        let found = members
            .iter()
            .position(|member| member.eq_ignore_ascii_case(part))?;
        chosen[found] = true;
    }
    Some(chosen)
}

fn join_members(members: &[String], chosen: Vec<bool>) -> String {
    members
        .iter()
        .zip(chosen)
        .filter_map(|(member, taken)| taken.then_some(member.as_str()))
        .collect::<Vec<_>>()
        .join(",")
}

/// Reads a `SET` value written as one bit for each declared member.
fn member_bits(members: &[String], bits: u64) -> Option<String> {
    if members.len() < 64 && bits >= 1u64 << members.len() {
        return None;
    }
    let chosen = (0..members.len())
        .map(|index| bits & (1 << index) != 0)
        .collect();
    Some(join_members(members, chosen))
}

/// Returns the member at a declared position, where zero is the empty error
/// member MySQL keeps in front of them.
fn member_at(members: &[String], position: u64) -> Option<String> {
    if position == 0 {
        return Some(String::new());
    }
    members.get(usize::try_from(position - 1).ok()?).cloned()
}

/// Reads a number written as a number, which MySQL cuts towards zero: measured
/// on 8.4.11, an `ENUM` takes 2.9 as its second member and 3.4 as its third.
fn whole_number(value: &Value) -> Option<u64> {
    match value {
        Value::Numeric(Numeric::Integer(number)) => u64::try_from(*number).ok(),
        Value::Numeric(Numeric::Float(number)) => {
            let cut = number.trunc();
            (cut >= 0.0 && cut < u64::MAX as f64).then_some(cut as u64)
        }
        _ => None,
    }
}

/// Checks a record a MySQL table is about to store, and answers the record to
/// store in its place when MySQL would not have stored it as written.
///
/// MySQL keeps a `JSON` document, an `ENUM` or `SET` member and every temporal
/// value in a form of its own, so most of the work here is reading the value
/// the way MySQL reads it and writing back what it read.
pub(crate) fn check_mysql_assignment(
    table_name: &str,
    table_sql: Option<&str>,
    operation: AssignmentOperation,
    values: &[Value],
    injected_counted_column_ordinal: Option<usize>,
) -> Result<Option<Vec<Value>>> {
    let Some(table_sql) = table_sql else {
        return Ok(None);
    };
    let Some(rules) = assignment_rules(table_sql)? else {
        return Ok(None);
    };
    let AssignmentRules {
        spec,
        counted,
        allocator_column,
    } = rules.as_ref();
    let allocator_column = *allocator_column;
    // A trigger's row numbered by the session's counter is the row an
    // injected id would have written, the number living in the key.
    let (operation, injected_counted_column_ordinal) = match operation {
        AssignmentOperation::InsertWithSuppliedRowid => match allocator_column {
            Some((ordinal, false)) if injected_counted_column_ordinal.is_none() => {
                (AssignmentOperation::Insert, Some(ordinal))
            }
            _ => {
                return Err(LimboError::Corrupt(format!(
                    "a row number was supplied for {table_name}, whose key is not its counter"
                )));
            }
        },
        operation => (operation, injected_counted_column_ordinal),
    };
    if *counted
        && operation == AssignmentOperation::Insert
        && injected_counted_column_ordinal.is_none()
    {
        return Err(LimboError::ParseError(
            "MySQL AUTO_INCREMENT inserts are not enabled".to_string(),
        ));
    }
    if operation == AssignmentOperation::Insert
        && allocator_column.is_some()
        && allocator_column.map(|(ordinal, _)| ordinal) != injected_counted_column_ordinal
    {
        return Err(LimboError::Corrupt(
            "AUTO_INCREMENT assignment validator has a different counted column".to_string(),
        ));
    }
    let injected_rowid_alias_ordinal = injected_counted_column_ordinal
        .filter(|_| !allocator_column.is_some_and(|(_, stored_primary_key)| stored_primary_key));
    let expected_values = spec.len();
    if expected_values != values.len() {
        return Err(LimboError::Corrupt(format!(
            "MySQL table {table_name} has {expected_values} stored columns but the record has {} values",
            values.len()
        )));
    }
    // The rowid alias holds no value of its own in a written record, the
    // number living in the key instead, so an insert that put one there wrote
    // the row differently than the counted path meant to. An upsert's update
    // half rebuilds the record from the row it found, where the alias carries
    // that row's own number, so the rule is the insert's alone.
    if let Some(ordinal) = injected_rowid_alias_ordinal {
        if operation == AssignmentOperation::Insert
            && !matches!(values.get(ordinal), Some(Value::Null))
        {
            return Err(LimboError::Corrupt(
                "a counted table's insert did not keep its rowid alias separate".to_string(),
            ));
        }
    }
    let mut rewritten: Option<Vec<Value>> = None;
    for (column_index, value) in values.iter().enumerate() {
        if injected_rowid_alias_ordinal == Some(column_index) || matches!(value, Value::Null) {
            continue;
        }
        if let Some(length) = spec.binary_length(column_index) {
            reject_overlong_binary(table_name, column_index, length, value)?;
            continue;
        }
        if spec.is_bit(column_index) {
            reject_what_one_bit_cannot_hold(table_name, column_index, value)?;
            continue;
        }
        if let Some(length) = spec.character_length(column_index) {
            let stored = text_column_value(
                table_name,
                column_index,
                length,
                spec.is_fixed_width(column_index),
                value,
            )?;
            if let Some(stored) = stored {
                if stored != *value {
                    rewritten.get_or_insert_with(|| values.to_vec())[column_index] = stored;
                }
            }
            continue;
        }
        // Everything MySQL keeps in a form of its own is read and written back
        // here rather than checked, which keeps each of them to one read.
        let stored = if spec.is_json(column_index) {
            Some(document_value(table_name, column_index, value)?)
        } else if spec.is_float(column_index) {
            if spec.is_unsigned_real(column_index) {
                reject_negative_real(table_name, column_index, value)?;
            }
            Some(float_value(table_name, column_index, value)?)
        } else if let Some(members) = spec.enum_members(column_index) {
            Some(member_value(table_name, column_index, members, value)?)
        } else if let Some(members) = spec.set_members(column_index) {
            Some(member_subset_value(
                table_name,
                column_index,
                members,
                value,
            )?)
        } else if spec.is_timestamp(column_index) {
            Some(temporal_value(
                table_name,
                column_index,
                "TIMESTAMP",
                value,
                spec.temporal_precision(column_index).unwrap_or(0),
            )?)
        } else if spec.is_datetime(column_index) {
            Some(temporal_value(
                table_name,
                column_index,
                "DATETIME",
                value,
                spec.temporal_precision(column_index).unwrap_or(0),
            )?)
        } else if spec.is_date(column_index) {
            Some(temporal_value(table_name, column_index, "DATE", value, 0)?)
        } else if spec.is_time(column_index) {
            Some(temporal_value(
                table_name,
                column_index,
                "TIME",
                value,
                spec.temporal_precision(column_index).unwrap_or(0),
            )?)
        } else if spec.is_year(column_index) {
            Some(year_value(table_name, column_index, value)?)
        } else {
            None
        };
        if let Some(stored) = stored {
            if stored != *value {
                rewritten.get_or_insert_with(|| values.to_vec())[column_index] = stored;
            }
            continue;
        }
        if spec.is_unsigned_real(column_index) {
            reject_negative_real(table_name, column_index, value)?;
            continue;
        }
        let Some(integer_type) = spec.column(column_index) else {
            continue;
        };
        let type_name = mysql_integer_name(integer_type).to_string();
        if integer_type == turso_mysql_parser::MySqlIntegerType::BigIntUnsigned {
            let Value::Blob(blob) = value else {
                return Err(AssignmentError::IncorrectType {
                    table: table_name.to_string(),
                    column: column_index + 1,
                    type_name,
                }
                .into());
            };
            turso_core::mysql_uint64_from_blob(blob).map_err(|_| {
                AssignmentError::IncorrectType {
                    table: table_name.to_string(),
                    column: column_index + 1,
                    type_name,
                }
            })?;
            continue;
        }
        let Value::Numeric(Numeric::Integer(value)) = value else {
            return Err(AssignmentError::IncorrectType {
                table: table_name.to_string(),
                column: column_index + 1,
                type_name,
            }
            .into());
        };
        let (min, max) = integer_type.bounds();
        if i128::from(*value) < min || i128::from(*value) > max {
            return Err(AssignmentError::OutOfRange {
                table: table_name.to_string(),
                column: column_index + 1,
                type_name,
                value: *value,
            }
            .into());
        }
    }
    Ok(rewritten)
}

#[cfg(test)]
thread_local! {
    /// How many times this thread read a table's rules from its DDL.
    static RULE_READS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// What a stored table's DDL says about checking a row written to it.
struct AssignmentRules {
    spec: turso_mysql_parser::MySqlNumericSpec,
    /// Whether the table counts its own ids.
    counted: bool,
    /// The counted column and whether it is a `BIGINT UNSIGNED` kept in the
    /// row rather than as the rowid.
    allocator_column: Option<(usize, bool)>,
}

/// Reads the rules for rows written to a table from its stored DDL, once for
/// each DDL text.
///
/// Every row a statement writes is checked against them, and reading them
/// parses the whole DDL: measured, that was 95% of an `UPDATE` of every row.
/// The rules follow from the text alone, so the text is what they are kept
/// under, and a table changed by any means has a different one.
fn assignment_rules(table_sql: &str) -> Result<Option<Arc<AssignmentRules>>> {
    /// A few hundred tables' rules, kept per thread so that no row waits on
    /// another session's.
    const MOST_KEPT: usize = 256;
    thread_local! {
        static KEPT: std::cell::RefCell<std::collections::HashMap<String, Arc<AssignmentRules>>> =
            std::cell::RefCell::default();
    }
    if let Some(rules) = KEPT.with(|kept| kept.borrow().get(table_sql).cloned()) {
        return Ok(Some(rules));
    }
    let Some(decoded) = decode_persisted_schema_sql(SchemaSqlKind::Table, table_sql)? else {
        return Ok(None);
    };
    let mode = SessionSqlMode {
        ansi_quotes: decoded.context.sql_mode.ansi_quotes,
        no_backslash_escapes: decoded.context.sql_mode.no_backslash_escapes,
    };
    #[cfg(test)]
    RULE_READS.with(|reads| reads.set(reads.get() + 1));
    let spec = parse_mysql_numeric_spec(decoded.normalized_ddl, mode)
        .map_err(|error| LimboError::Corrupt(error.to_string()))?;
    let allocator_column = decoded
        .v2_metadata()
        .map(|_| {
            parse_auto_increment_create_table(decoded.normalized_ddl, mode)
                .map(|table| {
                    (
                        table.allocator_column_ordinal,
                        table.allocator_column_type
                            == turso_mysql_parser::MySqlIntegerType::BigIntUnsigned,
                    )
                })
                .map_err(|error| LimboError::Corrupt(error.to_string()))
        })
        .transpose()?;
    let rules = Arc::new(AssignmentRules {
        spec,
        counted: decoded.v2_metadata().is_some(),
        allocator_column,
    });
    KEPT.with(|kept| {
        let mut kept = kept.borrow_mut();
        if kept.len() >= MOST_KEPT {
            kept.clear();
        }
        kept.insert(table_sql.to_owned(), Arc::clone(&rules));
    });
    Ok(Some(rules))
}

/// A FLOAT is rounded when stored, so later comparisons and aggregates read
/// the same binary32 value MySQL stored.
fn float_value(table_name: &str, column_index: usize, value: &Value) -> Result<Value> {
    let number = match value {
        Value::Numeric(Numeric::Integer(number)) => *number as f64,
        Value::Numeric(Numeric::Float(number)) => f64::from(*number),
        Value::Text(text) => {
            text.as_str()
                .parse::<f64>()
                .map_err(|_| AssignmentError::IncorrectType {
                    table: table_name.to_string(),
                    column: column_index + 1,
                    type_name: "FLOAT".to_string(),
                })?
        }
        _ => {
            return Err(AssignmentError::IncorrectType {
                table: table_name.to_string(),
                column: column_index + 1,
                type_name: "FLOAT".to_string(),
            }
            .into())
        }
    };
    let rounded = number as f32;
    if !rounded.is_finite() {
        return Err(AssignmentError::OutOfRange {
            table: table_name.to_string(),
            column: column_index + 1,
            type_name: "FLOAT".to_string(),
            value: 0,
        }
        .into());
    }
    Ok(Value::from_f64(f64::from(rounded)))
}

/// Puts a `VARCHAR` or a `CHAR` value into the form its column stores.
///
/// MySQL counts characters, not bytes: measured on 8.4.11, `VARCHAR(4)` stores
/// four multi-byte characters, and five characters answer 1406. Two things it
/// does are rewrites rather than refusals. An overflow made only of trailing
/// spaces is cut back to the declared width and reported as note 1265, so
/// `'abcd  '` stores `abcd` in a `VARCHAR(4)`. And a `CHAR` gives back no
/// trailing space at all, whatever it was written with, so `'ab  '` in a
/// `CHAR(4)` reads back as two characters.
fn text_column_value(
    table_name: &str,
    column_index: usize,
    length: u32,
    fixed_width: bool,
    value: &Value,
) -> Result<Option<Value>> {
    let Value::Text(text) = value else {
        return Ok(None);
    };
    let written = text.as_str();
    let kept = if fixed_width {
        written.trim_end_matches(' ')
    } else {
        written
    };
    let characters = kept.chars().count();
    if characters <= length as usize {
        return Ok((kept != written).then(|| Value::build_text(kept.to_owned())));
    }
    let width = length as usize;
    let cut: String = kept.chars().take(width).collect();
    if kept.chars().skip(width).all(|character| character == ' ') {
        return Ok(Some(Value::build_text(cut)));
    }
    Err(AssignmentError::TooLong {
        table: table_name.to_string(),
        column: column_index + 1,
        type_name: format!("{}({length})", if fixed_width { "CHAR" } else { "VARCHAR" }),
    }
    .into())
}

/// Refuses a value wider than its `VARBINARY` column.
///
/// The declared count is bytes, not characters, which is the whole of the
/// difference from a `VARCHAR`.
fn reject_overlong_binary(
    table_name: &str,
    column_index: usize,
    length: u32,
    value: &Value,
) -> Result<()> {
    let bytes = match value {
        Value::Blob(blob) => blob.len(),
        Value::Text(text) => text.as_str().len(),
        _ => return Ok(()),
    };
    if bytes <= length as usize {
        return Ok(());
    }
    Err(AssignmentError::TooLong {
        table: table_name.to_string(),
        column: column_index + 1,
        type_name: format!("VARBINARY({length})"),
    }
    .into())
}

/// Refuses a value a `BIT(1)` column cannot hold.
///
/// Measured on MySQL 8.4.11: 0, 1, `TRUE` and `FALSE` are stored, and 2, -1,
/// 1.5, `'1'` and `'a'` all answer 1406 — a word is its bytes, and the byte of
/// `'1'` is wider than one bit. The empty word, which MySQL stores as 0, is
/// refused the same way here.
fn reject_what_one_bit_cannot_hold(
    table_name: &str,
    column_index: usize,
    value: &Value,
) -> Result<()> {
    if matches!(value, Value::Numeric(Numeric::Integer(0 | 1))) {
        return Ok(());
    }
    Err(AssignmentError::TooLong {
        table: table_name.to_string(),
        column: column_index + 1,
        type_name: "BIT(1)".to_string(),
    }
    .into())
}

/// Refuses a negative value in an unsigned `DOUBLE`, `FLOAT` or `DECIMAL`
/// column.
///
/// Measured on MySQL 8.4.11: a negative answers 1264 where zero is taken, the
/// same rule an unsigned integer column is held to.
fn reject_negative_real(table_name: &str, column_index: usize, value: &Value) -> Result<()> {
    let negative = match value {
        Value::Numeric(Numeric::Float(float)) => f64::from(*float) < 0.0,
        Value::Numeric(Numeric::Integer(integer)) => *integer < 0,
        Value::Text(text) => {
            let written = text.as_str();
            written.parse::<f64>().is_ok()
                && written.starts_with('-')
                && written.split(['e', 'E']).next().is_some_and(|mantissa| {
                    mantissa
                        .bytes()
                        .any(|byte| byte.is_ascii_digit() && byte != b'0')
                })
        }
        _ => false,
    };
    if !negative {
        return Ok(());
    }
    Err(AssignmentError::OutOfRange {
        table: table_name.to_string(),
        column: column_index + 1,
        type_name: "UNSIGNED".to_string(),
        value: 0,
    }
    .into())
}

fn mysql_integer_name(integer_type: turso_mysql_parser::MySqlIntegerType) -> &'static str {
    match integer_type {
        turso_mysql_parser::MySqlIntegerType::TinyInt => "TINYINT",
        turso_mysql_parser::MySqlIntegerType::SmallInt => "SMALLINT",
        turso_mysql_parser::MySqlIntegerType::MediumInt => "MEDIUMINT",
        turso_mysql_parser::MySqlIntegerType::Int => "INT",
        turso_mysql_parser::MySqlIntegerType::BigInt => "BIGINT",
        turso_mysql_parser::MySqlIntegerType::TinyIntUnsigned => "TINYINT UNSIGNED",
        turso_mysql_parser::MySqlIntegerType::SmallIntUnsigned => "SMALLINT UNSIGNED",
        turso_mysql_parser::MySqlIntegerType::MediumIntUnsigned => "MEDIUMINT UNSIGNED",
        turso_mysql_parser::MySqlIntegerType::IntUnsigned => "INT UNSIGNED",
        turso_mysql_parser::MySqlIntegerType::BigIntUnsigned => "BIGINT UNSIGNED",
    }
}

impl MySqlDialect {
    fn format_index_sql(&self, input: &str, stmt: &Stmt) -> Result<String> {
        let Some(decoded) = decode_persisted_schema_sql(SchemaSqlKind::Index, input)? else {
            return Err(LimboError::ParseError(
                "MySQL CREATE INDEX requires SchemaSqlSessionContext".to_string(),
            ));
        };
        let stored = parse_marked_index(decoded)?;
        if &stored != stmt {
            return Err(LimboError::Corrupt(
                "MySQL replay SQL does not match its translated index definition".to_string(),
            ));
        }
        Ok(input.to_string())
    }

    fn format_view_sql(&self, input: &str, stmt: &Stmt) -> Result<String> {
        let Some(decoded) = decode_persisted_schema_sql(SchemaSqlKind::View, input)? else {
            return Err(LimboError::ParseError(
                "MySQL CREATE VIEW requires SchemaSqlSessionContext".to_string(),
            ));
        };
        let stored = parse_marked_view(decoded)?;
        if &stored != stmt {
            return Err(LimboError::Corrupt(
                "MySQL replay SQL does not match its translated view definition".to_string(),
            ));
        }
        Ok(input.to_string())
    }

    fn format_trigger_sql(&self, input: &str, stmt: &Stmt) -> Result<String> {
        let Some(decoded) = decode_persisted_schema_sql(SchemaSqlKind::Trigger, input)? else {
            return Err(LimboError::ParseError(
                "MySQL CREATE TRIGGER requires SchemaSqlSessionContext".to_string(),
            ));
        };
        let stored = parse_marked_trigger(decoded)?;
        if &stored != stmt {
            return Err(LimboError::Corrupt(
                "MySQL replay SQL does not match its translated trigger definition".to_string(),
            ));
        }
        Ok(input.to_string())
    }
}

fn catalog_table_sql_name(sql: &str, decoded: Option<DecodedSchemaSql<'_>>) -> Result<String> {
    let statement = if let Some(decoded) = decoded {
        let mode = session_sql_mode(decoded.context.sql_mode);
        match parse_create_table_ast(decoded.normalized_ddl, mode) {
            Ok(statement) => statement,
            Err(error) => parse_auto_increment_create_table(decoded.normalized_ddl, mode)
                .map(|checked| checked.sqlite_statement)
                .map_err(|_| {
                    LimboError::Corrupt(format!("invalid persisted MySQL table SQL: {error}"))
                })?,
        }
    } else {
        turso_core::dialect::sqlite::parse_table_sql_ast(sql)?
    };
    let Stmt::CreateTable { tbl_name, .. } = statement else {
        return Err(LimboError::Corrupt(
            "MySQL schema catalog table SQL is not CREATE TABLE".to_string(),
        ));
    };
    Ok(tbl_name.name.as_str().to_string())
}

fn parse_marked_table(decoded: DecodedSchemaSql<'_>) -> Result<Stmt> {
    let session_context = SchemaSqlSessionContext {
        sql_mode: decoded.context.sql_mode,
        character_set_client: decoded.context.character_set_client,
        collation_connection: decoded.context.collation_connection,
        default_character_set: decoded.context.default_character_set,
        default_collation: decoded.context.default_collation,
    };
    if !session_context.supports_current_table_loader() {
        return Err(LimboError::Corrupt(
            "persisted MySQL table uses an unsupported default collation".to_string(),
        ));
    }
    let mode = session_sql_mode(decoded.context.sql_mode);
    if decoded.v2_metadata().is_some() {
        // A v2 envelope is an allocator identity, not a general table marker.
        // Check its AUTO_INCREMENT shape before the generic parser can lower
        // an ordinary PRIMARY KEY table and accidentally accept the wrong row.
        let statement = parse_auto_increment_create_table(decoded.normalized_ddl, mode)
            .map(|checked| checked.sqlite_statement)
            .map_err(|error| {
                LimboError::Corrupt(format!(
                    "invalid persisted MySQL AUTO_INCREMENT table SQL: {error}"
                ))
            })?;
        return refuse_legacy_text_collation(decoded, statement);
    }
    if decoded.v2_metadata().is_none() {
        if let Ok(checked) = parse_checked_primary_key_create_table(decoded.normalized_ddl, mode) {
            if checked.normalized_mysql_ddl != decoded.normalized_ddl {
                return Err(LimboError::Corrupt(
                    "persisted MySQL PRIMARY KEY table SQL is not canonical".to_string(),
                ));
            }
            return refuse_legacy_text_collation(decoded, checked.sqlite_statement);
        }
    }
    match parse_create_table_ast(decoded.normalized_ddl, mode) {
        Ok(statement) => refuse_legacy_text_collation(decoded, statement),
        Err(error) => Err(LimboError::Corrupt(format!(
            "invalid persisted MySQL table SQL: {error}"
        ))),
    }
}

fn refuse_legacy_text_collation(decoded: DecodedSchemaSql<'_>, statement: Stmt) -> Result<Stmt> {
    if decoded.uses_uca9() {
        return Ok(statement);
    }
    let Stmt::CreateTable {
        body: CreateTableBody::ColumnsAndConstraints { columns, .. },
        ..
    } = &statement
    else {
        return Ok(statement);
    };
    let has_text_collation = columns.iter().any(|column| {
        column.constraints.iter().any(|constraint| {
            matches!(&constraint.constraint, ColumnConstraint::Collate { collation_name }
                if collation_name.as_str().eq_ignore_ascii_case("MYSQL_UCA9_AI_CI"))
        })
    });
    if has_text_collation {
        return Err(LimboError::Corrupt(
            "legacy MySQL text table needs collation migration before it can be opened".to_string(),
        ));
    }
    Ok(statement)
}

fn parse_marked_index(decoded: DecodedSchemaSql<'_>) -> Result<Stmt> {
    let session_context = SchemaSqlSessionContext {
        sql_mode: decoded.context.sql_mode,
        character_set_client: decoded.context.character_set_client,
        collation_connection: decoded.context.collation_connection,
        default_character_set: decoded.context.default_character_set,
        default_collation: decoded.context.default_collation,
    };
    if !session_context.supports_current_table_loader() {
        return Err(LimboError::Corrupt(
            "persisted MySQL index uses an unsupported default collation".to_string(),
        ));
    }
    parse_create_index_ast(
        decoded.normalized_ddl,
        session_sql_mode(decoded.context.sql_mode),
    )
    .map_err(|error| LimboError::Corrupt(format!("invalid persisted MySQL index SQL: {error}")))
}

fn parse_marked_view(decoded: DecodedSchemaSql<'_>) -> Result<Stmt> {
    let session_context = SchemaSqlSessionContext {
        sql_mode: decoded.context.sql_mode,
        character_set_client: decoded.context.character_set_client,
        collation_connection: decoded.context.collation_connection,
        default_character_set: decoded.context.default_character_set,
        default_collation: decoded.context.default_collation,
    };
    if !session_context.supports_current_table_loader() {
        return Err(LimboError::Corrupt(
            "persisted MySQL view uses an unsupported default collation".to_string(),
        ));
    }
    parse_create_view_ast(
        decoded.normalized_ddl,
        session_sql_mode(decoded.context.sql_mode),
    )
    .map_err(|error| LimboError::Corrupt(format!("invalid persisted MySQL view SQL: {error}")))
}

fn parse_marked_trigger(decoded: DecodedSchemaSql<'_>) -> Result<Stmt> {
    let session_context = SchemaSqlSessionContext {
        sql_mode: decoded.context.sql_mode,
        character_set_client: decoded.context.character_set_client,
        collation_connection: decoded.context.collation_connection,
        default_character_set: decoded.context.default_character_set,
        default_collation: decoded.context.default_collation,
    };
    if !session_context.supports_current_table_loader() {
        return Err(LimboError::Corrupt(
            "persisted MySQL trigger uses an unsupported default collation".to_string(),
        ));
    }
    parse_create_trigger_ast(
        decoded.normalized_ddl,
        session_sql_mode(decoded.context.sql_mode),
    )
    .map_err(|error| LimboError::Corrupt(format!("invalid persisted MySQL trigger SQL: {error}")))
}

fn session_sql_mode(mode: SchemaSqlMode) -> SessionSqlMode {
    SessionSqlMode {
        ansi_quotes: mode.ansi_quotes,
        no_backslash_escapes: mode.no_backslash_escapes,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema_sql::{
        encode_schema_sql, encode_schema_sql_v2, encode_schema_sql_v3, CharacterSet, Collation,
        SchemaSqlContext, SchemaSqlMode, SchemaSqlV2Metadata,
    };
    use turso_parser::{ast::Cmd, parser::Parser};

    /// Measured on MySQL 8.4.11 as `'<text>' = <number>`.
    #[test]
    fn text_is_read_as_the_number_it_begins_with() {
        for (text, number) in [
            ("1abc", 1.0),
            ("abc", 0.0),
            ("1e2", 100.0),
            (".5", 0.5),
            ("1.", 1.0),
            ("+1", 1.0),
            ("-", 0.0),
            (" 12abc", 12.0),
            ("1e", 1.0),
            ("1e+", 1.0),
            ("0x10", 0.0),
            ("\t1", 1.0),
            ("\n1", 1.0),
            ("\r1", 1.0),
            ("\u{b}1", 1.0),
            ("\u{c}1", 1.0),
            ("１", 0.0),
            ("inf", 0.0),
            ("nan", 0.0),
            ("1_000", 1.0),
            ("-0", 0.0),
            ("1 ", 1.0),
            ("1e5x", 100000.0),
            ("-.5", -0.5),
            ("- 1", 0.0),
            ("00012", 12.0),
            ("1.5e-1", 0.15),
            ("1E1", 10.0),
            ("0.1", 0.1),
            ("1.e5", 100000.0),
        ] {
            assert_eq!(text_as_a_double(text), Some(number), "{text:?}");
        }
        assert_eq!(text_as_a_double("1e400"), None);
    }

    /// `utf8mb4_bin` reads the shorter side as though it went on in spaces, so
    /// a tab sorts below the space it is compared with. Measured on MySQL
    /// 8.4.11 against `JSON_UNQUOTE`: `'a' > 'a\t'` is 1.
    #[test]
    fn utf8mb4_bin_pads_with_spaces() {
        use std::cmp::Ordering;
        assert_eq!(compare_padded_with_spaces(b"en", b"en  "), Ordering::Equal);
        assert_eq!(compare_padded_with_spaces(b"en ", b"en"), Ordering::Equal);
        assert_eq!(compare_padded_with_spaces(b"en", b"EN"), Ordering::Greater);
        assert_eq!(compare_padded_with_spaces(b"a", b"a\t"), Ordering::Greater);
        assert_eq!(compare_padded_with_spaces(b"a", b"ab"), Ordering::Less);
    }

    #[test]
    fn regexp_refuses_unicode_case_folds_it_cannot_match() {
        assert!(checked_regexp_match("Alpha", "alpha").unwrap());
        assert!(checked_regexp_match("ß", "ss")
            .unwrap_err()
            .to_string()
            .contains("full case folding"));
    }

    #[test]
    fn mysql_case_calls_keep_ascii_values_and_refuse_unicode() {
        assert_eq!(
            checked_mysql_case(&Value::build_text("AbC"), false).unwrap(),
            Value::build_text("abc")
        );
        assert_eq!(
            checked_mysql_case(&Value::build_text("AbC"), true).unwrap(),
            Value::build_text("ABC")
        );
        assert_eq!(
            checked_mysql_case(&Value::Null, false).unwrap(),
            Value::Null
        );
        // MySQL 8.4.11 answers LOWER('É') = 'é' and UPPER('é') = 'É'.
        for (value, uppercase) in [("É", false), ("é", true)] {
            assert!(checked_mysql_case(&Value::build_text(value), uppercase)
                .unwrap_err()
                .to_string()
                .contains("case conversion"));
        }
    }

    #[test]
    fn mysql_search_matches_ascii_without_case_and_refuses_unicode() {
        // MySQL 8.4.11 answers INSTR('ABC','a') = 1 and
        // LOCATE('B','aBcB',3) = 4.
        assert_eq!(
            checked_mysql_search(&Value::build_text("ABC"), &Value::build_text("a"), 1).unwrap(),
            Value::from_i64(1)
        );
        assert_eq!(
            checked_mysql_search(&Value::build_text("aBcB"), &Value::build_text("B"), 3).unwrap(),
            Value::from_i64(4)
        );
        for (start, expected) in [(0, 0), (1, 1), (4, 4), (5, 0)] {
            assert_eq!(
                checked_mysql_search(&Value::build_text("abc"), &Value::build_text(""), start)
                    .unwrap(),
                Value::from_i64(expected)
            );
        }
        // MySQL 8.4.11 answers LOCATE('SS','straße') = 5.
        assert!(
            checked_mysql_search(&Value::build_text("straße"), &Value::build_text("SS"), 1)
                .unwrap_err()
                .to_string()
                .contains("case folding")
        );
    }

    fn trusted_context(database_id: u8) -> SchemaCatalogValidationContext {
        SchemaCatalogValidationContext::new([database_id; 16])
    }

    fn table_context() -> SchemaSqlContext {
        SchemaSqlContext {
            kind: SchemaSqlKind::Table,
            sql_mode: SchemaSqlMode {
                ansi_quotes: false,
                no_backslash_escapes: false,
            },
            character_set_client: CharacterSet::Binary,
            collation_connection: Collation::Binary,
            default_character_set: CharacterSet::Binary,
            default_collation: Collation::Binary,
        }
    }

    fn index_context() -> SchemaSqlContext {
        SchemaSqlContext {
            kind: SchemaSqlKind::Index,
            ..table_context()
        }
    }

    fn view_context() -> SchemaSqlContext {
        SchemaSqlContext {
            kind: SchemaSqlKind::View,
            ..table_context()
        }
    }

    fn trigger_context() -> SchemaSqlContext {
        SchemaSqlContext {
            kind: SchemaSqlKind::Trigger,
            ..table_context()
        }
    }

    fn stored_table(ddl: &str) -> String {
        encode_schema_sql(table_context(), ddl).unwrap()
    }

    fn stored_index(ddl: &str) -> String {
        encode_schema_sql(index_context(), ddl).unwrap()
    }

    fn stored_view(ddl: &str) -> String {
        encode_schema_sql(view_context(), ddl).unwrap()
    }

    fn stored_trigger(ddl: &str) -> String {
        encode_schema_sql(trigger_context(), ddl).unwrap()
    }

    fn catalog_table_row(name: &str, sql: Option<&str>) -> SchemaCatalogRow {
        SchemaCatalogRow {
            object_type: "table".to_string(),
            name: name.to_string(),
            table_name: name.to_string(),
            root_page: 2,
            sql: sql.map(str::to_string),
        }
    }

    #[test]
    fn old_text_tables_require_collation_migration_before_replay() {
        let ddl = "CREATE TABLE `words` (`word` VARCHAR(20))";
        let old = encode_schema_sql(table_context(), ddl).unwrap();
        let old = decode_persisted_schema_sql(SchemaSqlKind::Table, &old)
            .unwrap()
            .unwrap();
        assert!(parse_marked_table(old)
            .unwrap_err()
            .to_string()
            .contains("collation migration"));

        let current = encode_schema_sql_v3(table_context(), None, ddl).unwrap();
        let current = decode_persisted_schema_sql(SchemaSqlKind::Table, &current)
            .unwrap()
            .unwrap();
        assert!(parse_marked_table(current).is_ok());

        let old_numeric =
            encode_schema_sql(table_context(), "CREATE TABLE `numbers` (`value` INTEGER)").unwrap();
        let old_numeric = decode_persisted_schema_sql(SchemaSqlKind::Table, &old_numeric)
            .unwrap()
            .unwrap();
        assert!(parse_marked_table(old_numeric).is_ok());
    }

    #[test]
    fn catalog_validation_ignores_internal_tables_and_checks_user_tables() {
        let dialect = MySqlDialect;
        let user_sql = stored_table("CREATE TABLE `users` (`id` INTEGER NOT NULL)");
        let rows = [
            catalog_table_row(
                "sqlite_sequence",
                Some("CREATE TABLE sqlite_sequence(name,seq)"),
            ),
            catalog_table_row(
                "__turso_internal_seq_users",
                Some("CREATE TABLE __turso_internal_seq_users(value INTEGER)"),
            ),
            catalog_table_row("users", Some(&user_sql)),
        ];

        dialect.validate_schema_catalog(&rows, None).unwrap();

        let error = dialect
            .validate_schema_catalog(
                &[catalog_table_row(
                    "users",
                    Some("CREATE TABLE users (id INTEGER)"),
                )],
                None,
            )
            .unwrap_err();
        assert!(matches!(error, LimboError::Corrupt(_)));
    }

    #[test]
    fn catalog_validation_rejects_reserved_name_spoofing() {
        let dialect = MySqlDialect;
        for name in ["sqlite_sequence", "__turso_internal_seq_users"] {
            let marked_user_sql = stored_table("CREATE TABLE `users` (`id` INTEGER)");
            let error = dialect
                .validate_schema_catalog(&[catalog_table_row(name, Some(&marked_user_sql))], None)
                .unwrap_err();

            assert!(
                matches!(error, LimboError::Corrupt(message) if message.contains("SQL defines table users"))
            );
        }
    }

    #[test]
    fn catalog_validation_requires_consistent_internal_row_identity() {
        let dialect = MySqlDialect;
        let internal_sql = "CREATE TABLE sqlite_sequence(name,seq)";

        let mut mismatched_table_name = catalog_table_row("sqlite_sequence", Some(internal_sql));
        mismatched_table_name.table_name = "other".to_string();
        assert!(matches!(
            dialect.validate_schema_catalog(&[mismatched_table_name], None),
            Err(LimboError::Corrupt(message)) if message.contains("table_name")
        ));

        let mut missing_root_page = catalog_table_row("sqlite_sequence", Some(internal_sql));
        missing_root_page.root_page = 0;
        assert!(matches!(
            dialect.validate_schema_catalog(&[missing_root_page], None),
            Err(LimboError::Corrupt(message)) if message.contains("root page")
        ));

        let mismatched_sql = catalog_table_row(
            "sqlite_sequence",
            Some("CREATE TABLE __turso_internal_seq_users(value INTEGER)"),
        );
        assert!(matches!(
            dialect.validate_schema_catalog(&[mismatched_sql], None),
            Err(LimboError::Corrupt(message)) if message.contains("SQL defines table")
        ));
    }

    #[test]
    fn catalog_validation_rejects_v2_until_database_identity_is_durable() {
        let dialect = MySqlDialect;
        let stored = encode_schema_sql_v2(
            table_context(),
            SchemaSqlV2Metadata::new([1; 16], [2; 16]).unwrap(),
            "CREATE TABLE `users` (`id` INT NOT NULL AUTO_INCREMENT PRIMARY KEY)",
        )
        .unwrap();

        let error = dialect
            .validate_schema_catalog(&[catalog_table_row("users", Some(&stored))], None)
            .unwrap_err();
        assert!(matches!(
            error,
            LimboError::Corrupt(message)
                if message.contains("requires a durable database identity")
        ));
    }

    #[test]
    fn catalog_validation_accepts_v2_with_the_trusted_database_identity() {
        let dialect = MySqlDialect;
        let stored = encode_schema_sql_v2(
            table_context(),
            SchemaSqlV2Metadata::new([7; 16], [2; 16]).unwrap(),
            "CREATE TABLE `users` (`id` INT NOT NULL AUTO_INCREMENT PRIMARY KEY)",
        )
        .unwrap();
        let context = trusted_context(7);

        dialect
            .validate_schema_catalog(&[catalog_table_row("users", Some(&stored))], Some(&context))
            .unwrap();
    }

    #[test]
    fn catalog_validation_rejects_v2_for_another_trusted_database_identity() {
        let dialect = MySqlDialect;
        let stored = encode_schema_sql_v2(
            table_context(),
            SchemaSqlV2Metadata::new([7; 16], [2; 16]).unwrap(),
            "CREATE TABLE `users` (`id` INT NOT NULL AUTO_INCREMENT PRIMARY KEY)",
        )
        .unwrap();
        let context = trusted_context(8);

        assert!(matches!(
            dialect.validate_schema_catalog(
                &[catalog_table_row("users", Some(&stored))],
                Some(&context),
            ),
            Err(LimboError::Corrupt(message)) if message.contains("different database identities")
        ));
    }

    #[test]
    fn marked_table_uses_the_stored_session_mode_and_mysql_translation() {
        let dialect = MySqlDialect;
        let mut context = table_context();
        context.sql_mode.ansi_quotes = true;
        let stored = encode_schema_sql(
            context,
            "CREATE TABLE \"app\".\"users\" (\"id\" INTEGER NOT NULL UNIQUE)",
        )
        .unwrap();

        let table = dialect.parse_table_sql(&stored, 7).unwrap();

        assert_eq!(table.name, "users");
        assert_eq!(table.root_page, 7);
    }

    #[test]
    fn invalid_marked_table_is_database_corruption() {
        let dialect = MySqlDialect;
        let error = dialect
            .parse_table_sql(
                "/*@turso:mysql-schema:v2:eyJ9*/ CREATE TABLE t (id INTEGER)",
                1,
            )
            .unwrap_err();

        assert!(matches!(error, LimboError::Corrupt(_)));
    }

    #[test]
    fn unsupported_default_collation_fails_closed() {
        let dialect = MySqlDialect;
        let mut context = table_context();
        context.default_character_set = CharacterSet::Utf8mb4;
        context.default_collation = Collation::Utf8mb4_0900AiCi;
        let stored = encode_schema_sql(context, "CREATE TABLE `users` (`name` TEXT)").unwrap();

        assert!(matches!(
            dialect.parse_table_sql(&stored, 1),
            Err(LimboError::Corrupt(_))
        ));
    }

    #[test]
    fn unmarked_internal_table_uses_sqlite_fallback() {
        let dialect = MySqlDialect;
        let table = dialect
            .parse_table_sql("CREATE TABLE sqlite_sequence(name,seq)", 1)
            .unwrap();

        assert_eq!(table.name, "sqlite_sequence");
    }

    #[test]
    fn owner_and_name_identify_mysql_files() {
        let dialect = MySqlDialect;

        assert_eq!(dialect.name(), "mysql");
        assert_eq!(dialect.database_file_owner(), DatabaseFileOwner::MySql);
        assert_eq!(
            dialect.database_file_application_id(),
            Some(DatabaseFileOwner::mysql_application_id(
                DatabaseFileOwner::MYSQL_LOWER_CASE_TABLE_NAMES,
            ))
        );
        assert_eq!(
            dialect.database_file_application_id().unwrap() as u32,
            0x5452_0224
        );
    }

    #[test]
    fn table_replay_preserves_normalized_mysql_and_removes_a_safe_qualifier() {
        let dialect = MySqlDialect;
        let stored = stored_table("CREATE TABLE `app` . `users` (`id` INTEGER NOT NULL)");

        let replay = dialect.table_sql_for_replay(&stored).unwrap();
        let decoded = decode_persisted_schema_sql(SchemaSqlKind::Table, &replay)
            .unwrap()
            .unwrap();
        assert_eq!(
            decoded.normalized_ddl,
            "CREATE TABLE `users` (`id` INTEGER NOT NULL)"
        );
    }

    #[test]
    fn table_replay_preserves_v2_identities() {
        let dialect = MySqlDialect;
        let metadata = SchemaSqlV2Metadata::new([0x11; 16], [0x22; 16]).unwrap();
        let stored = encode_schema_sql_v2(
            table_context(),
            metadata,
            "CREATE TABLE `users` (`id` INTEGER NOT NULL AUTO_INCREMENT PRIMARY KEY, `counter` INTEGER)",
        )
        .unwrap();

        let replay = dialect.table_sql_for_replay(&stored).unwrap();
        let decoded = decode_persisted_schema_sql(SchemaSqlKind::Table, &replay)
            .unwrap()
            .unwrap();
        assert_eq!(decoded.v2_metadata(), Some(metadata));
        assert_eq!(
            decoded.normalized_ddl,
            "CREATE TABLE `users` (`id` INTEGER NOT NULL AUTO_INCREMENT PRIMARY KEY, `counter` INTEGER)"
        );
    }

    #[test]
    fn v2_auto_increment_table_loads_and_replays_without_losing_its_ddl() {
        let dialect = MySqlDialect;
        let metadata = SchemaSqlV2Metadata::new([0x11; 16], [0x22; 16]).unwrap();
        let ddl = "CREATE TABLE `users` (`id` INT NOT NULL AUTO_INCREMENT PRIMARY KEY)";
        let stored = encode_schema_sql_v2(table_context(), metadata, ddl).unwrap();

        let table = dialect.parse_table_sql(&stored, 7).unwrap();
        assert_eq!(table.name, "users");
        assert_eq!(table.root_page, 7);

        let replay = dialect.table_sql_for_replay(&stored).unwrap();
        let decoded = decode_persisted_schema_sql(SchemaSqlKind::Table, &replay)
            .unwrap()
            .unwrap();
        assert_eq!(decoded.v2_metadata(), Some(metadata));
        assert_eq!(decoded.normalized_ddl, ddl);
    }

    #[test]
    fn ordinary_primary_key_table_loads_without_a_rowid_alias_and_replays_its_mysql_ddl() {
        let dialect = MySqlDialect;
        let ddl = "CREATE TABLE `users` (`id` INTEGER NOT NULL PRIMARY KEY) ENGINE = InnoDB";
        let stored = stored_table(ddl);

        let table = dialect.parse_table_sql(&stored, 7).unwrap();
        assert_eq!(table.name, "users");
        assert_eq!(table.root_page, 7);
        assert!(table.has_rowid);
        assert!(table.get_rowid_alias_column().is_none());
        assert!(table.unique_sets.iter().any(|set| set.is_primary_key));

        let replay = dialect.table_sql_for_replay(&stored).unwrap();
        let decoded = decode_persisted_schema_sql(SchemaSqlKind::Table, &replay)
            .unwrap()
            .unwrap();
        assert_eq!(decoded.normalized_ddl, ddl);
        assert_eq!(decoded.v2_metadata(), None);
    }

    #[test]
    fn ordinary_primary_key_table_rejects_noncanonical_persisted_ddl() {
        let dialect = MySqlDialect;
        let stored = stored_table("CREATE TABLE `users` (`id` INT PRIMARY KEY) ENGINE=InnoDB");

        assert!(matches!(
            dialect.parse_table_sql(&stored, 7),
            Err(LimboError::Corrupt(message))
                if message.contains("PRIMARY KEY table SQL is not canonical")
        ));
    }

    #[test]
    fn direct_table_traits_reject_v2_ordinary_primary_key_rows() {
        let dialect = MySqlDialect;
        let stored = encode_schema_sql_v2(
            table_context(),
            SchemaSqlV2Metadata::new([0x11; 16], [0x22; 16]).unwrap(),
            "CREATE TABLE `users` (`id` INT NOT NULL PRIMARY KEY)",
        )
        .unwrap();

        assert!(matches!(
            dialect.parse_table_sql(&stored, 7),
            Err(LimboError::Corrupt(_))
        ));
        assert!(matches!(
            dialect.parse_table_sql_ast(&stored),
            Err(LimboError::Corrupt(_))
        ));
        assert!(matches!(
            dialect.parse_schema_sql(SchemaSqlKind::Table, &stored),
            Err(LimboError::Corrupt(_))
        ));
        assert!(matches!(
            dialect.table_sql_for_replay(&stored),
            Err(LimboError::Corrupt(_))
        ));
        assert!(matches!(
            dialect.parse(&stored),
            Err(LimboError::Corrupt(_))
        ));
    }

    /// An ordinary PRIMARY KEY table is no longer refused here, which is what
    /// lets an `ALTER TABLE` run against one. The bare dialect still cannot
    /// write a marked table — that needs the session context — so what it
    /// answers is the same thing it answers for every other marked table.
    #[test]
    fn dialect_passes_an_ordinary_primary_key_table_to_the_writer() {
        let dialect = MySqlDialect;
        let stored =
            stored_table("CREATE TABLE `users` (`id` INT NOT NULL PRIMARY KEY) ENGINE = InnoDB");
        let mut parser = Parser::new(b"CREATE TABLE users (id INT NOT NULL PRIMARY KEY, a INT)");
        let Some(Cmd::Stmt(altered)) = parser.next_cmd().unwrap() else {
            panic!("expected CREATE TABLE statement");
        };

        assert!(matches!(
            dialect.format_rewritten_schema_sql(SchemaSqlKind::Table, &stored, &altered),
            Err(LimboError::ParseError(message))
                if message.contains("SchemaSqlSessionContext")
        ));
    }

    /// Every row a statement writes is checked, so a table's rules are read
    /// from its DDL once rather than once a row.
    #[test]
    fn a_tables_rules_are_read_once_for_each_ddl() {
        let reads = || RULE_READS.with(std::cell::Cell::get);
        let before = reads();
        let stored = stored_table("CREATE TABLE `numbers` (`value` TINYINT)");
        for value in [1, 2, 3] {
            MySqlIntegerValidator
                .check_assignment(
                    "numbers",
                    Some(&stored),
                    AssignmentOperation::Insert,
                    &[Value::from_i64(value)],
                )
                .unwrap();
        }
        assert_eq!(reads(), before + 1);

        let altered = stored_table("CREATE TABLE `numbers` (`value` SMALLINT)");
        MySqlIntegerValidator
            .check_assignment(
                "numbers",
                Some(&altered),
                AssignmentOperation::Insert,
                &[Value::from_i64(1_000)],
            )
            .unwrap();
        assert_eq!(reads(), before + 2);
        assert!(MySqlIntegerValidator
            .check_assignment(
                "numbers",
                Some(&stored),
                AssignmentOperation::Insert,
                &[Value::from_i64(1_000)],
            )
            .is_err());
    }

    #[test]
    fn assignment_validation_rejects_uninjected_v2_auto_increment_inserts() {
        let stored = encode_schema_sql_v2(
            table_context(),
            SchemaSqlV2Metadata::new([0x11; 16], [0x22; 16]).unwrap(),
            "CREATE TABLE `users` (`id` INT NOT NULL AUTO_INCREMENT PRIMARY KEY)",
        )
        .unwrap();

        assert!(matches!(
            MySqlIntegerValidator.check_assignment(
                "users",
                Some(&stored),
                AssignmentOperation::Insert,
                &[Value::from_i64(1)],
            ),
            Err(LimboError::ParseError(message)) if message == "MySQL AUTO_INCREMENT inserts are not enabled"
        ));

        MySqlIntegerValidator
            .check_assignment(
                "users",
                Some(&stored),
                AssignmentOperation::Update,
                &[Value::from_i64(1)],
            )
            .unwrap();
    }

    #[test]
    fn assignment_validation_checks_signed_mediumint_boundaries_and_nulls() {
        let stored =
            stored_table("CREATE TABLE `numbers` (`value` MEDIUMINT, `nullable` MEDIUMINT)");

        for values in [
            vec![Value::from_i64(-8_388_608), Value::Null],
            vec![Value::from_i64(8_388_607), Value::from_i64(0)],
            vec![Value::Null, Value::Null],
        ] {
            MySqlIntegerValidator
                .check_assignment(
                    "numbers",
                    Some(&stored),
                    AssignmentOperation::Insert,
                    &values,
                )
                .unwrap();
        }

        for value in [-8_388_609, 8_388_608] {
            assert!(matches!(
                MySqlIntegerValidator.check_assignment(
                    "numbers",
                    Some(&stored),
                    AssignmentOperation::Update,
                    &[Value::from_i64(value), Value::Null],
                ),
                Err(LimboError::Assignment(error))
                    if matches!(error.as_ref(), AssignmentError::OutOfRange { type_name, .. } if type_name == "MEDIUMINT")
            ));
        }
    }

    #[test]
    fn float_assignment_rounds_the_value_before_storage() {
        let stored = stored_table(
            "CREATE TABLE `numbers` (`value` FLOAT, `positive` FLOAT UNSIGNED, `double` DOUBLE)",
        );
        let values = [
            Value::from_f64(0.1),
            Value::from_i64(16_777_217),
            Value::from_f64(0.1),
        ];
        let rewritten = MySqlIntegerValidator
            .check_assignment(
                "numbers",
                Some(&stored),
                AssignmentOperation::Insert,
                &values,
            )
            .unwrap()
            .expect("FLOAT values must be rounded before storage");
        assert_eq!(rewritten[0], Value::from_f64(f64::from(0.1_f32)));
        assert_eq!(rewritten[1], Value::from_f64(16_777_216.0));
        assert_eq!(rewritten[2], Value::from_f64(0.1));

        let decimal_literal = MySqlIntegerValidator
            .check_assignment(
                "numbers",
                Some(&stored),
                AssignmentOperation::Insert,
                &[Value::build_text("0.1"), Value::Null, Value::Null],
            )
            .unwrap()
            .expect("a written fraction is rounded to FLOAT storage");
        assert_eq!(decimal_literal[0], Value::from_f64(f64::from(0.1_f32)));

        assert_eq!(
            MySqlIntegerValidator
                .check_assignment(
                    "numbers",
                    Some(&stored),
                    AssignmentOperation::Update,
                    &[rewritten[0].clone(), Value::Null, Value::Null],
                )
                .unwrap(),
            None
        );
        assert!(matches!(
            MySqlIntegerValidator.check_assignment(
                "numbers",
                Some(&stored),
                AssignmentOperation::Insert,
                &[Value::Null, Value::from_f64(-0.1), Value::Null],
            ),
            Err(LimboError::Assignment(error))
                if matches!(error.as_ref(), AssignmentError::OutOfRange { .. })
        ));
        assert!(matches!(
            MySqlIntegerValidator.check_assignment(
                "numbers",
                Some(&stored),
                AssignmentOperation::Insert,
                &[Value::Null, Value::build_text("-0.1"), Value::Null],
            ),
            Err(LimboError::Assignment(error))
                if matches!(error.as_ref(), AssignmentError::OutOfRange { .. })
        ));
        assert!(matches!(
            reject_negative_real("numbers", 1, &Value::build_text("-0.001")),
            Err(LimboError::Assignment(error))
                if matches!(error.as_ref(), AssignmentError::OutOfRange { .. })
        ));
        assert!(reject_negative_real("numbers", 1, &Value::build_text("-0.000")).is_ok());
        assert!(matches!(
            MySqlIntegerValidator.check_assignment(
                "numbers",
                Some(&stored),
                AssignmentOperation::Insert,
                &[Value::from_f64(1e40), Value::Null, Value::Null],
            ),
            Err(LimboError::Assignment(error))
                if matches!(error.as_ref(), AssignmentError::OutOfRange { .. })
        ));
    }

    #[test]
    fn timestamp_assignment_stays_within_the_mysql_utc_range() {
        let stored =
            stored_table("CREATE TABLE `moments` (`stamp` TIMESTAMP NULL, `plain` DATETIME)");

        for stamp in ["1970-01-01 00:00:01", "2038-01-19 03:14:07"] {
            MySqlIntegerValidator
                .check_assignment(
                    "moments",
                    Some(&stored),
                    AssignmentOperation::Insert,
                    &[
                        Value::build_text(stamp.to_owned()),
                        Value::build_text(stamp.to_owned()),
                    ],
                )
                .unwrap();
        }

        for stamp in ["1970-01-01 00:00:00", "2038-01-19 03:14:08"] {
            assert!(matches!(
                MySqlIntegerValidator.check_assignment(
                    "moments",
                    Some(&stored),
                    AssignmentOperation::Update,
                    &[
                        Value::build_text(stamp.to_owned()),
                        Value::build_text(stamp.to_owned()),
                    ],
                ),
                Err(LimboError::Assignment(error))
                    if matches!(error.as_ref(), AssignmentError::IncorrectTemporal { type_name, .. } if type_name == "TIMESTAMP")
            ));
        }
    }

    #[test]
    fn unqualified_table_replay_keeps_the_stored_mysql_ddl() {
        let dialect = MySqlDialect;
        let stored = stored_table("CREATE TABLE `users` (`id` INTEGER NOT NULL)");

        let replay = dialect
            .schema_sql_for_replay(SchemaSqlKind::Table, &stored)
            .unwrap();
        assert_eq!(
            decode_persisted_schema_sql(SchemaSqlKind::Table, &replay)
                .unwrap()
                .unwrap()
                .normalized_ddl,
            "CREATE TABLE `users` (`id` INTEGER NOT NULL)"
        );
    }

    #[test]
    fn marked_index_loads_through_the_generic_parser_and_replays_mysql_sql() {
        let dialect = MySqlDialect;
        let stored = stored_index("CREATE UNIQUE INDEX `idx_users_name` ON `users` (`name`)");

        let (command, consumed) = dialect.parse(&stored).unwrap();
        assert_eq!(consumed, stored.len());
        assert!(matches!(command, Some(Cmd::Stmt(Stmt::CreateIndex { .. }))));

        let replay = dialect
            .schema_sql_for_replay(SchemaSqlKind::Index, &stored)
            .unwrap();
        let decoded = decode_persisted_schema_sql(SchemaSqlKind::Index, &replay)
            .unwrap()
            .unwrap();
        assert_eq!(
            decoded.normalized_ddl,
            "CREATE UNIQUE INDEX `idx_users_name` ON `users` (`name`)"
        );
    }

    #[test]
    fn marked_view_loads_through_the_generic_parser_and_replays_mysql_sql() {
        let dialect = MySqlDialect;
        let stored = stored_view("CREATE VIEW `users_view` AS SELECT `name` FROM `users`");

        let (command, consumed) = dialect.parse(&stored).unwrap();
        assert_eq!(consumed, stored.len());
        assert!(matches!(command, Some(Cmd::Stmt(Stmt::CreateView { .. }))));

        let replay = dialect
            .schema_sql_for_replay(SchemaSqlKind::View, &stored)
            .unwrap();
        let decoded = decode_persisted_schema_sql(SchemaSqlKind::View, &replay)
            .unwrap()
            .unwrap();
        assert_eq!(
            decoded.normalized_ddl,
            "CREATE VIEW `users_view` AS SELECT `name` FROM `users`"
        );
    }

    #[test]
    fn marked_trigger_loads_through_the_generic_parser_and_replays_mysql_sql() {
        let dialect = MySqlDialect;
        let stored = stored_trigger(
            "CREATE TRIGGER `copy_user` AFTER INSERT ON `users` FOR EACH ROW BEGIN INSERT INTO `audit` (`name`) VALUES (NEW.`name`); END",
        );

        let (command, consumed) = dialect.parse(&stored).unwrap();
        assert_eq!(consumed, stored.len());
        assert!(matches!(
            command,
            Some(Cmd::Stmt(Stmt::CreateTrigger { .. }))
        ));

        let replay = dialect
            .schema_sql_for_replay(SchemaSqlKind::Trigger, &stored)
            .unwrap();
        let decoded = decode_persisted_schema_sql(SchemaSqlKind::Trigger, &replay)
            .unwrap()
            .unwrap();
        assert_eq!(
            decoded.normalized_ddl,
            "CREATE TRIGGER `copy_user` AFTER INSERT ON `users` FOR EACH ROW BEGIN INSERT INTO `audit` (`name`) VALUES (NEW.`name`); END"
        );
    }
}
