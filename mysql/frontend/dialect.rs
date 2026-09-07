use std::sync::Arc;

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
    render_create_table_mysql_with_mode, render_create_trigger_mysql_with_mode,
    render_create_view_mysql_with_mode, SessionSqlMode,
};
use turso_parser::ast::{Cmd, Stmt};

use crate::schema_sql::{
    decode_persisted_schema_sql, decode_schema_sql_any, encode_schema_sql, reencode_schema_sql,
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
        // the number 0 is the zero year, and a `VARBINARY` holding `'007'`
        // would otherwise read back as `7`.
        for column in table.columns_mut().iter_mut() {
            let holds_text = matches!(
                column.ty_str.to_ascii_uppercase().as_str(),
                "JSON" | "DATE" | "TIME" | "DATETIME" | "TIMESTAMP" | "YEAR" | "VARBINARY"
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
        let normalized =
            render_create_table_mysql_with_mode(&stmt, session_sql_mode(decoded.context.sql_mode))
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
                return encode_schema_sql(decoded.context, &normalized)
                    .map_err(|error| LimboError::Corrupt(error.to_string()));
            }
            SchemaSqlKind::View => {
                let Some(decoded) = decode_persisted_schema_sql(kind, sql)? else {
                    return Dialect::schema_sql_for_replay(&turso_core::SqliteDialect, kind, sql);
                };
                let statement = parse_marked_view(decoded)?;
                let normalized = render_create_view_mysql_with_mode(
                    &statement,
                    session_sql_mode(decoded.context.sql_mode),
                )
                .map_err(|error| {
                    LimboError::Corrupt(format!("cannot replay MySQL view SQL: {error}"))
                })?;
                return encode_schema_sql(decoded.context, &normalized)
                    .map_err(|error| LimboError::Corrupt(error.to_string()));
            }
            SchemaSqlKind::Trigger => {
                let Some(decoded) = decode_persisted_schema_sql(kind, sql)? else {
                    return Dialect::schema_sql_for_replay(&turso_core::SqliteDialect, kind, sql);
                };
                let statement = parse_marked_trigger(decoded)?;
                let normalized = render_create_trigger_mysql_with_mode(
                    &statement,
                    session_sql_mode(decoded.context.sql_mode),
                )
                .map_err(|error| {
                    LimboError::Corrupt(format!("cannot replay MySQL trigger SQL: {error}"))
                })?;
                return encode_schema_sql(decoded.context, &normalized)
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
        if arg_count == 1 && name.eq_ignore_ascii_case(MYSQL_MD5) {
            return Ok(Some(Func::Dialect(MYSQL_MD5.to_string())));
        }
        if arg_count == 2
            && (name.eq_ignore_ascii_case(MYSQL_FORMAT)
                || name.eq_ignore_ascii_case(MYSQL_TRUNCATE)
                || name.eq_ignore_ascii_case(MYSQL_JSON_CONTAINS))
        {
            return Ok(Some(Func::Dialect(name.to_ascii_lowercase())));
        }
        turso_core::dialect::sqlite::resolve_builtin_function(name, arg_count)
    }

    fn exec_scalar_function(
        &self,
        connection: &turso_core::Connection,
        name: &str,
        args: &[Value],
    ) -> Result<Value> {
        if name.eq_ignore_ascii_case("last_insert_id") && args.is_empty() {
            let id = i64::try_from(connection.mysql_last_insert_id())
                .map_err(|_| LimboError::IntegerOverflow)?;
            return Ok(Value::from_i64(id));
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
        if name.eq_ignore_ascii_case(MYSQL_JSON_CONTAINS) {
            let [target, candidate] = args else {
                return Err(LimboError::ParseError(format!(
                    "{name} takes two arguments"
                )));
            };
            let (Value::Text(target), Value::Text(candidate)) = (target, candidate) else {
                return Ok(Value::Null);
            };
            return Ok(
                match turso_mysql_parser::json_contains(target.as_str(), candidate.as_str()) {
                    Some(held) => Value::from_i64(i64::from(held)),
                    None => Value::Null,
                },
            );
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
            let number = match value {
                Value::Numeric(Numeric::Integer(integer)) => *integer as f64,
                Value::Numeric(Numeric::Float(float)) => f64::from(*float),
                _ => return Ok(Value::Null),
            };
            // Measured on MySQL 8.4.11: a negative count answers no fraction
            // at all rather than rounding to a whole ten.
            let decimals = match decimals {
                Value::Numeric(Numeric::Integer(integer)) => u32::try_from(*integer).unwrap_or(0),
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
/// Writes the thirty-two hexadecimal characters `MD5` answers. The engine
/// keeps its digests in an extension this frontend does not register.
pub(crate) const MYSQL_MD5: &str = "mysql_md5";
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
        "TIME" => turso_mysql_parser::normalize_time(&written),
        _ => turso_mysql_parser::normalize_datetime(&written),
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
    injected_rowid_alias_ordinal: Option<usize>,
) -> Result<Option<Vec<Value>>> {
    let Some(table_sql) = table_sql else {
        return Ok(None);
    };
    let Some(decoded) = decode_persisted_schema_sql(SchemaSqlKind::Table, table_sql)? else {
        return Ok(None);
    };
    if decoded.v2_metadata().is_some()
        && operation == AssignmentOperation::Insert
        && injected_rowid_alias_ordinal.is_none()
    {
        return Err(LimboError::ParseError(
            "MySQL AUTO_INCREMENT inserts are not enabled".to_string(),
        ));
    }
    let mode = SessionSqlMode {
        ansi_quotes: decoded.context.sql_mode.ansi_quotes,
        no_backslash_escapes: decoded.context.sql_mode.no_backslash_escapes,
    };
    let spec = parse_mysql_numeric_spec(decoded.normalized_ddl, mode)
        .map_err(|error| LimboError::Corrupt(error.to_string()))?;
    let allocator_column_ordinal = decoded
        .v2_metadata()
        .map(|_| {
            parse_auto_increment_create_table(decoded.normalized_ddl, mode)
                .map(|table| table.allocator_column_ordinal)
                .map_err(|error| LimboError::Corrupt(error.to_string()))
        })
        .transpose()?;
    if operation == AssignmentOperation::Insert
        && allocator_column_ordinal.is_some()
        && allocator_column_ordinal != injected_rowid_alias_ordinal
    {
        return Err(LimboError::Corrupt(
            "AUTO_INCREMENT assignment validator has a different rowid alias column".to_string(),
        ));
    }
    let expected_values = spec.len();
    if expected_values != values.len() {
        return Err(LimboError::Corrupt(format!(
            "MySQL table {table_name} has {expected_values} stored columns but the record has {} values",
            values.len()
        )));
    }
    if let Some(ordinal) = injected_rowid_alias_ordinal {
        if !matches!(values.get(ordinal), Some(Value::Null)) {
            return Err(LimboError::Corrupt(
                "AUTO_INCREMENT injected insert did not keep its rowid alias separate".to_string(),
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
        } else if let Some(members) = spec.enum_members(column_index) {
            Some(member_value(table_name, column_index, members, value)?)
        } else if let Some(members) = spec.set_members(column_index) {
            Some(member_subset_value(
                table_name,
                column_index,
                members,
                value,
            )?)
        } else if spec.is_datetime(column_index) {
            Some(temporal_value(table_name, column_index, "DATETIME", value)?)
        } else if spec.is_date(column_index) {
            Some(temporal_value(table_name, column_index, "DATE", value)?)
        } else if spec.is_time(column_index) {
            Some(temporal_value(table_name, column_index, "TIME", value)?)
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
        let Value::Numeric(Numeric::Integer(value)) = value else {
            return Err(AssignmentError::IncorrectType {
                table: table_name.to_string(),
                column: column_index + 1,
                type_name,
            }
            .into());
        };
        let (min, max) = integer_type.bounds();
        if *value < min || *value > max {
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

/// Refuses a negative value in an unsigned `DOUBLE`, `FLOAT` or `DECIMAL`
/// column.
///
/// Measured on MySQL 8.4.11: a negative answers 1264 where zero is taken, the
/// same rule an unsigned integer column is held to.
fn reject_negative_real(table_name: &str, column_index: usize, value: &Value) -> Result<()> {
    let negative = match value {
        Value::Numeric(Numeric::Float(float)) => f64::from(*float) < 0.0,
        Value::Numeric(Numeric::Integer(integer)) => *integer < 0,
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
        return parse_auto_increment_create_table(decoded.normalized_ddl, mode)
            .map(|checked| checked.sqlite_statement)
            .map_err(|error| {
                LimboError::Corrupt(format!(
                    "invalid persisted MySQL AUTO_INCREMENT table SQL: {error}"
                ))
            });
    }
    if decoded.v2_metadata().is_none() {
        if let Ok(checked) = parse_checked_primary_key_create_table(decoded.normalized_ddl, mode) {
            if checked.normalized_mysql_ddl != decoded.normalized_ddl {
                return Err(LimboError::Corrupt(
                    "persisted MySQL PRIMARY KEY table SQL is not canonical".to_string(),
                ));
            }
            return Ok(checked.sqlite_statement);
        }
    }
    match parse_create_table_ast(decoded.normalized_ddl, mode) {
        Ok(statement) => Ok(statement),
        Err(error) => Err(LimboError::Corrupt(format!(
            "invalid persisted MySQL table SQL: {error}"
        ))),
    }
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
        encode_schema_sql, encode_schema_sql_v2, CharacterSet, Collation, SchemaSqlContext,
        SchemaSqlMode, SchemaSqlV2Metadata,
    };
    use turso_parser::{ast::Cmd, parser::Parser};

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
            "CREATE TABLE `users` (`id` INTEGER NOT NULL AUTO_INCREMENT PRIMARY KEY, `name` TEXT)",
        )
        .unwrap();

        let replay = dialect.table_sql_for_replay(&stored).unwrap();
        let decoded = decode_persisted_schema_sql(SchemaSqlKind::Table, &replay)
            .unwrap()
            .unwrap();
        assert_eq!(decoded.v2_metadata(), Some(metadata));
        assert_eq!(
            decoded.normalized_ddl,
            "CREATE TABLE `users` (`id` INTEGER NOT NULL AUTO_INCREMENT PRIMARY KEY, `name` TEXT)"
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
