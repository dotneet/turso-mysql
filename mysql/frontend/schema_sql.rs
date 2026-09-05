//! Versioned MySQL DDL envelopes stored in `sqlite_schema.sql`.

use std::{collections::BTreeSet, fmt};

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use serde::{Deserialize, Serialize};
pub use turso_core::SchemaSqlKind;
use turso_mysql_parser::{
    parse_auto_increment_create_table, parse_checked_primary_key_create_table,
    parse_create_table_ast, render_create_index_mysql_with_mode,
    render_create_table_mysql_with_mode, render_create_trigger_mysql_with_mode,
    render_create_view_mysql_with_mode, SessionSqlMode,
};

const RESERVED_PREFIX: &str = "/*@turso:mysql-schema:";
const VERSION_PREFIX: &str = "v1:";
const V2_VERSION_PREFIX: &str = "v2:";
const MARKER_END: &str = "*/ ";

/// Largest accepted decoded creation context.
pub const MAX_CONTEXT_JSON_BYTES: usize = 512;

/// Largest accepted normalized MySQL statement in one schema row.
pub const MAX_NORMALIZED_DDL_BYTES: usize = 1024 * 1024;

const MAX_CONTEXT_BASE64_BYTES: usize = MAX_CONTEXT_JSON_BYTES.div_ceil(3) * 4;

/// The supported MySQL character sets whose creation-time meaning can be stored.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CharacterSet {
    Utf8mb4,
    Binary,
}

/// The supported MySQL collations whose creation-time meaning can be stored.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Collation {
    Utf8mb4_0900AiCi,
    Utf8mb4Bin,
    Binary,
}

/// SQL modes that change lexical meaning while parsing persisted DDL.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SchemaSqlMode {
    pub ansi_quotes: bool,
    pub no_backslash_escapes: bool,
}

/// Canonical creation context stored with a normalized MySQL schema statement.
///
/// Field order is part of the v1 encoding: decoding reserializes this struct
/// and requires byte-for-byte equality with the decoded JSON.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SchemaSqlContext {
    pub kind: SchemaSqlKind,
    pub sql_mode: SchemaSqlMode,
    pub character_set_client: CharacterSet,
    pub collation_connection: Collation,
    pub default_character_set: CharacterSet,
    pub default_collation: Collation,
}

/// One of the fixed-width identities carried by a v2 schema envelope.
///
/// The bytes are deliberately opaque to the schema codec. The allocator and
/// database layers may give them meaning, but neither layer may derive them
/// from a display name or a physical root page.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SchemaSqlId([u8; 16]);

impl SchemaSqlId {
    /// Construct an identity, rejecting the reserved all-zero value.
    pub fn from_bytes(bytes: [u8; 16]) -> Result<Self, SchemaSqlError> {
        if bytes.iter().all(|byte| *byte == 0) {
            return Err(SchemaSqlError::InvalidContext);
        }
        Ok(Self(bytes))
    }

    /// Return the identity bytes for the allocator or database layer.
    pub const fn into_bytes(self) -> [u8; 16] {
        self.0
    }
}

/// Immutable v2 identities for one AUTO_INCREMENT table.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SchemaSqlV2Metadata {
    pub database_id: SchemaSqlId,
    pub allocator_id: SchemaSqlId,
}

impl SchemaSqlV2Metadata {
    /// Construct v2 metadata after validating both identities.
    pub fn new(database_id: [u8; 16], allocator_id: [u8; 16]) -> Result<Self, SchemaSqlError> {
        Ok(Self {
            database_id: SchemaSqlId::from_bytes(database_id)?,
            allocator_id: SchemaSqlId::from_bytes(allocator_id)?,
        })
    }
}

/// Per-session creation settings used to format every schema object touched by
/// one statement.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SchemaSqlSessionContext {
    pub sql_mode: SchemaSqlMode,
    pub character_set_client: CharacterSet,
    pub collation_connection: Collation,
    pub default_character_set: CharacterSet,
    pub default_collation: Collation,
}

impl SchemaSqlSessionContext {
    /// Capture these session settings for one schema object envelope.
    pub const fn for_kind(self, kind: SchemaSqlKind) -> SchemaSqlContext {
        SchemaSqlContext {
            kind,
            sql_mode: self.sql_mode,
            character_set_client: self.character_set_client,
            collation_connection: self.collation_connection,
            default_character_set: self.default_character_set,
            default_collation: self.default_collation,
        }
    }

    pub(crate) const fn supports_current_table_loader(self) -> bool {
        matches!(self.character_set_client, CharacterSet::Binary)
            && matches!(self.collation_connection, Collation::Binary)
            && matches!(self.default_character_set, CharacterSet::Binary)
            && matches!(self.default_collation, Collation::Binary)
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
enum StoredSchemaSqlKind {
    Table,
    Index,
    View,
    Trigger,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct StoredSchemaSqlMode {
    ansi_quotes: bool,
    no_backslash_escapes: bool,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct StoredSchemaSqlContext {
    kind: StoredSchemaSqlKind,
    sql_mode: StoredSchemaSqlMode,
    character_set_client: CharacterSet,
    collation_connection: Collation,
    default_character_set: CharacterSet,
    default_collation: Collation,
}

/// Strict v2 context. Keep this separate from [`StoredSchemaSqlContext`]:
/// changing the v1 struct would change its canonical bytes and its accepted
/// compatibility surface.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct StoredSchemaSqlContextV2 {
    kind: StoredSchemaSqlKind,
    sql_mode: StoredSchemaSqlMode,
    character_set_client: CharacterSet,
    collation_connection: Collation,
    default_character_set: CharacterSet,
    default_collation: Collation,
    database_id: String,
    allocator_id: String,
}

/// A validated MySQL schema envelope borrowing the normalized DDL from storage.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub struct DecodedSchemaSql<'a> {
    pub context: SchemaSqlContext,
    pub normalized_ddl: &'a str,
    v2_metadata: Option<SchemaSqlV2Metadata>,
}

impl DecodedSchemaSql<'_> {
    /// Returns the immutable database/table identities carried by a v2 envelope.
    pub const fn v2_metadata(&self) -> Option<SchemaSqlV2Metadata> {
        self.v2_metadata
    }
}

/// One table row supplied to [`validate_schema_sql_catalog`].
///
/// Encoded rows are decoded when validation runs, so malformed reserved
/// markers cannot be mistaken for ordinary SQLite schema SQL. Decoded rows
/// are useful to callers that already loaded and checked the envelope.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SchemaSqlCatalogEntry<'a> {
    Encoded(&'a str),
    Decoded(DecodedSchemaSql<'a>),
}

impl<'a> SchemaSqlCatalogEntry<'a> {
    /// Supply one encoded `sqlite_schema.sql` table row.
    pub const fn encoded(stored: &'a str) -> Self {
        Self::Encoded(stored)
    }

    /// Supply one previously decoded table row.
    pub const fn decoded(decoded: DecodedSchemaSql<'a>) -> Self {
        Self::Decoded(decoded)
    }
}

impl<'a> From<&'a str> for SchemaSqlCatalogEntry<'a> {
    fn from(stored: &'a str) -> Self {
        Self::encoded(stored)
    }
}

impl<'a> From<&'a String> for SchemaSqlCatalogEntry<'a> {
    fn from(stored: &'a String) -> Self {
        Self::encoded(stored)
    }
}

impl<'a> From<DecodedSchemaSql<'a>> for SchemaSqlCatalogEntry<'a> {
    fn from(decoded: DecodedSchemaSql<'a>) -> Self {
        Self::decoded(decoded)
    }
}

/// An invalid or unsupported MySQL schema envelope.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SchemaSqlError {
    UnsupportedVersion,
    MalformedEnvelope,
    ContextTooLong,
    InvalidBase64,
    InvalidContext,
    NonCanonicalContext,
    ObjectKindMismatch,
    CatalogEntryMissingEnvelope,
    CatalogDatabaseIdMismatch,
    CatalogAllocatorIdDuplicate,
    MissingV2Metadata,
    MalformedAutoIncrementDefinition,
    MalformedTableDefinition,
    EmptyStatement,
    StatementTooLong,
}

impl fmt::Display for SchemaSqlError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let message = match self {
            Self::UnsupportedVersion => "unsupported MySQL schema SQL marker version",
            Self::MalformedEnvelope => "malformed MySQL schema SQL marker",
            Self::ContextTooLong => "MySQL schema SQL context exceeds its size limit",
            Self::InvalidBase64 => "MySQL schema SQL context is not base64url without padding",
            Self::InvalidContext => "MySQL schema SQL context has unsupported or invalid fields",
            Self::NonCanonicalContext => "MySQL schema SQL context is not canonical JSON",
            Self::ObjectKindMismatch => "MySQL schema SQL object kind does not match sqlite_schema",
            Self::CatalogEntryMissingEnvelope => {
                "MySQL schema catalog entry is missing its schema SQL envelope"
            }
            Self::CatalogDatabaseIdMismatch => {
                "MySQL schema catalog entries belong to different database identities"
            }
            Self::CatalogAllocatorIdDuplicate => {
                "MySQL AUTO_INCREMENT allocator identity is not unique"
            }
            Self::MissingV2Metadata => {
                "MySQL AUTO_INCREMENT schema SQL requires v2 identity metadata"
            }
            Self::MalformedAutoIncrementDefinition => {
                "MySQL AUTO_INCREMENT schema SQL is malformed or unsupported"
            }
            Self::MalformedTableDefinition => "MySQL schema SQL table definition is malformed",
            Self::EmptyStatement => "MySQL schema SQL statement is empty",
            Self::StatementTooLong => "MySQL schema SQL statement exceeds its size limit",
        };
        f.write_str(message)
    }
}

impl std::error::Error for SchemaSqlError {}

impl turso_core::SchemaSqlFormatter for SchemaSqlSessionContext {
    fn format_schema_sql(
        &self,
        kind: SchemaSqlKind,
        input: &str,
        stmt: &turso_parser::ast::Stmt,
    ) -> turso_core::Result<String> {
        if !matches!(
            kind,
            SchemaSqlKind::Table
                | SchemaSqlKind::Index
                | SchemaSqlKind::View
                | SchemaSqlKind::Trigger
        ) || !self.supports_current_table_loader()
        {
            return Err(turso_core::LimboError::ParseError(
                "the current MySQL schema formatter supports only binary-context tables, indexes, views, and triggers"
                    .to_string(),
            ));
        }
        let normalized = match kind {
            SchemaSqlKind::Table => Ok(input.to_string()),
            SchemaSqlKind::Index => {
                let mode = SessionSqlMode {
                    ansi_quotes: self.sql_mode.ansi_quotes,
                    no_backslash_escapes: self.sql_mode.no_backslash_escapes,
                };
                render_create_index_mysql_with_mode(stmt, mode)
            }
            SchemaSqlKind::View => {
                let mode = SessionSqlMode {
                    ansi_quotes: self.sql_mode.ansi_quotes,
                    no_backslash_escapes: self.sql_mode.no_backslash_escapes,
                };
                render_create_view_mysql_with_mode(stmt, mode)
            }
            SchemaSqlKind::Trigger => {
                let mode = SessionSqlMode {
                    ansi_quotes: self.sql_mode.ansi_quotes,
                    no_backslash_escapes: self.sql_mode.no_backslash_escapes,
                };
                render_create_trigger_mysql_with_mode(stmt, mode)
            }
            _ => unreachable!("checked supported MySQL schema kind"),
        }
        .map_err(|error| turso_core::LimboError::ParseError(error.to_string()))?;
        encode_schema_sql(self.for_kind(kind), &normalized).map_err(schema_sql_error_to_limbo)
    }

    fn format_rewritten_schema_sql(
        &self,
        kind: SchemaSqlKind,
        previous_sql: &str,
        stmt: &turso_parser::ast::Stmt,
    ) -> turso_core::Result<String> {
        if kind == SchemaSqlKind::Trigger {
            return Err(turso_core::LimboError::ParseError(
                "rewriting a marked MySQL trigger is not supported".to_string(),
            ));
        }
        if !matches!(
            kind,
            SchemaSqlKind::Table | SchemaSqlKind::Index | SchemaSqlKind::View
        ) {
            return Err(turso_core::LimboError::ParseError(
                "MySQL schema rewrites currently support only tables, indexes, and views"
                    .to_string(),
            ));
        }
        let decoded = decode_persisted_schema_sql(kind, previous_sql)?.ok_or_else(|| {
            turso_core::LimboError::Corrupt(
                "MySQL schema rewrite requires a marked previous schema row".to_string(),
            )
        })?;
        let previous_session = session_context(decoded.context);
        if !previous_session.supports_current_table_loader() {
            return Err(turso_core::LimboError::Corrupt(
                "persisted MySQL schema uses an unsupported character context".to_string(),
            ));
        }
        let mode = SessionSqlMode {
            ansi_quotes: decoded.context.sql_mode.ansi_quotes,
            no_backslash_escapes: decoded.context.sql_mode.no_backslash_escapes,
        };
        if kind == SchemaSqlKind::Table && decoded.v2_metadata().is_none() {
            match parse_checked_primary_key_create_table(decoded.normalized_ddl, mode) {
                Ok(checked) if checked.normalized_mysql_ddl == decoded.normalized_ddl => {
                    return Err(turso_core::LimboError::ParseError(
                        "rewriting an ordinary MySQL PRIMARY KEY table is not supported"
                            .to_string(),
                    ));
                }
                Ok(_) => {
                    return Err(turso_core::LimboError::Corrupt(
                        "persisted MySQL PRIMARY KEY table SQL is not canonical".to_string(),
                    ));
                }
                Err(_) => {}
            }
        }
        let normalized = match kind {
            SchemaSqlKind::Table => render_create_table_mysql_with_mode(stmt, mode),
            SchemaSqlKind::Index => render_create_index_mysql_with_mode(stmt, mode),
            SchemaSqlKind::View => render_create_view_mysql_with_mode(stmt, mode),
            _ => unreachable!("checked supported MySQL schema kind"),
        }
        .map_err(|error| turso_core::LimboError::ParseError(error.to_string()))?;
        reencode_schema_sql(decoded, &normalized).map_err(schema_sql_error_to_limbo)
    }
}

/// Encode one normalized MySQL statement for durable storage.
pub fn encode_schema_sql(
    context: SchemaSqlContext,
    normalized_ddl: &str,
) -> Result<String, SchemaSqlError> {
    validate_context(context)?;
    validate_statement(normalized_ddl)?;

    let stored_context = StoredSchemaSqlContext::from_context(context)?;
    let context_json = canonical_context_json(&stored_context)?;
    let encoded_context = URL_SAFE_NO_PAD.encode(context_json);
    Ok(format!(
        "{RESERVED_PREFIX}{VERSION_PREFIX}{encoded_context}{MARKER_END}{normalized_ddl}"
    ))
}

/// Encode one table statement with immutable v2 allocator metadata.
///
/// This explicit API is intentionally not used by the formatter yet. Keeping
/// v2 construction separate prevents existing v1 callers from accidentally
/// changing their on-disk bytes while the AUTO_INCREMENT SQL path is still
/// being implemented.
pub fn encode_schema_sql_v2(
    context: SchemaSqlContext,
    metadata: SchemaSqlV2Metadata,
    normalized_ddl: &str,
) -> Result<String, SchemaSqlError> {
    validate_context(context)?;
    if context.kind != SchemaSqlKind::Table {
        return Err(SchemaSqlError::InvalidContext);
    }
    validate_statement(normalized_ddl)?;

    let stored_context = StoredSchemaSqlContextV2::from_context(context, metadata)?;
    let context_json = canonical_context_json_v2(&stored_context)?;
    if context_json.len() > MAX_CONTEXT_JSON_BYTES {
        return Err(SchemaSqlError::ContextTooLong);
    }
    let encoded_context = URL_SAFE_NO_PAD.encode(context_json);
    Ok(format!(
        "{RESERVED_PREFIX}{V2_VERSION_PREFIX}{encoded_context}{MARKER_END}{normalized_ddl}"
    ))
}

/// Re-encode a decoded row without changing its envelope version or metadata.
///
/// Schema rewrites and VACUUM replay must use this helper: encoding a decoded
/// v2 row through [`encode_schema_sql`] would silently discard both durable
/// identities.
pub(crate) fn reencode_schema_sql(
    decoded: DecodedSchemaSql<'_>,
    normalized_ddl: &str,
) -> Result<String, SchemaSqlError> {
    match decoded.v2_metadata {
        Some(metadata) => encode_schema_sql_v2(decoded.context, metadata, normalized_ddl),
        None => encode_schema_sql(decoded.context, normalized_ddl),
    }
}

/// Decode a stored schema row.
///
/// `Ok(None)` is reserved for unmarked SQLite internal schema SQL. Any row
/// beginning with the reserved marker namespace is either a valid v1 or v2 envelope
/// or an error; it never falls back to SQLite parsing.
pub fn decode_schema_sql(
    expected_kind: SchemaSqlKind,
    stored: &str,
) -> Result<Option<DecodedSchemaSql<'_>>, SchemaSqlError> {
    let decoded = decode_schema_sql_any(stored)?;
    if let Some(decoded) = decoded {
        if decoded.context.kind != expected_kind {
            return Err(SchemaSqlError::ObjectKindMismatch);
        }
    }
    Ok(decoded)
}

/// Decode a stored schema row without imposing the sqlite_schema object kind.
///
/// This is used only by the generic dialect parser, which does not receive the
/// sqlite_schema row type. Schema loading must use [`decode_schema_sql`] so it
/// keeps validating that the marker kind agrees with the catalog row.
pub fn decode_schema_sql_any(stored: &str) -> Result<Option<DecodedSchemaSql<'_>>, SchemaSqlError> {
    let Some(after_reserved_prefix) = stored.strip_prefix(RESERVED_PREFIX) else {
        return Ok(None);
    };
    let (version, after_version) =
        if let Some(after_version) = after_reserved_prefix.strip_prefix(VERSION_PREFIX) {
            (SchemaSqlEnvelopeVersion::V1, after_version)
        } else if let Some(after_version) = after_reserved_prefix.strip_prefix(V2_VERSION_PREFIX) {
            (SchemaSqlEnvelopeVersion::V2, after_version)
        } else {
            return Err(SchemaSqlError::UnsupportedVersion);
        };
    let (encoded_context, normalized_ddl) = after_version
        .split_once(MARKER_END)
        .ok_or(SchemaSqlError::MalformedEnvelope)?;

    if encoded_context.len() > MAX_CONTEXT_BASE64_BYTES {
        return Err(SchemaSqlError::ContextTooLong);
    }
    validate_statement(normalized_ddl)?;

    let context_json = URL_SAFE_NO_PAD
        .decode(encoded_context)
        .map_err(|_| SchemaSqlError::InvalidBase64)?;
    if context_json.len() > MAX_CONTEXT_JSON_BYTES {
        return Err(SchemaSqlError::ContextTooLong);
    }

    let (context, v2_metadata) = match version {
        SchemaSqlEnvelopeVersion::V1 => {
            let stored_context: StoredSchemaSqlContext = serde_json::from_slice(&context_json)
                .map_err(|_| SchemaSqlError::InvalidContext)?;
            let context = stored_context.to_context()?;
            validate_context(context)?;
            if canonical_context_json(&stored_context)? != context_json {
                return Err(SchemaSqlError::NonCanonicalContext);
            }
            (context, None)
        }
        SchemaSqlEnvelopeVersion::V2 => {
            let stored_context: StoredSchemaSqlContextV2 = serde_json::from_slice(&context_json)
                .map_err(|_| SchemaSqlError::InvalidContext)?;
            let (context, metadata) = stored_context.to_context()?;
            validate_context(context)?;
            if canonical_context_json_v2(&stored_context)? != context_json {
                return Err(SchemaSqlError::NonCanonicalContext);
            }
            (context, Some(metadata))
        }
    };
    Ok(Some(DecodedSchemaSql {
        context,
        v2_metadata,
        normalized_ddl,
    }))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SchemaSqlEnvelopeVersion {
    V1,
    V2,
}

/// Decode persisted schema SQL for the core schema loader.
///
/// An invalid reserved MySQL envelope means an owned database file is corrupt,
/// while an unmarked row remains an expected SQLite internal object.
pub fn decode_persisted_schema_sql(
    expected_kind: SchemaSqlKind,
    stored: &str,
) -> turso_core::Result<Option<DecodedSchemaSql<'_>>> {
    decode_schema_sql(expected_kind, stored)
        .map_err(|error| turso_core::LimboError::Corrupt(error.to_string()))
}

/// Validate the table envelopes loaded for one logical MySQL database.
///
/// Legacy v1 rows remain valid when they do not declare `AUTO_INCREMENT`.
/// A table that does declare it must carry v2 metadata. Every v2 row must
/// carry the expected database identity, and allocator identities are unique
/// across the AUTO_INCREMENT rows in this catalog.
pub fn validate_schema_sql_catalog<'a, I, E>(
    expected_database_id: SchemaSqlId,
    entries: I,
) -> Result<(), SchemaSqlError>
where
    I: IntoIterator<Item = E>,
    E: Into<SchemaSqlCatalogEntry<'a>>,
{
    let mut allocator_ids = BTreeSet::new();
    for entry in entries {
        let entry = entry.into();
        let decoded = match entry {
            SchemaSqlCatalogEntry::Encoded(stored) => {
                decode_schema_sql(SchemaSqlKind::Table, stored)?
                    .ok_or(SchemaSqlError::CatalogEntryMissingEnvelope)?
            }
            SchemaSqlCatalogEntry::Decoded(decoded) => decoded,
        };
        validate_decoded_catalog_entry(expected_database_id, decoded, &mut allocator_ids)?;
    }
    Ok(())
}

/// Validate already decoded table envelopes for one logical MySQL database.
pub fn validate_decoded_schema_sql_catalog<'a, I>(
    expected_database_id: SchemaSqlId,
    entries: I,
) -> Result<(), SchemaSqlError>
where
    I: IntoIterator<Item = DecodedSchemaSql<'a>>,
{
    validate_schema_sql_catalog(
        expected_database_id,
        entries.into_iter().map(SchemaSqlCatalogEntry::Decoded),
    )
}

/// Decode and validate encoded table rows for one logical MySQL database.
pub fn validate_encoded_schema_sql_catalog<I, S>(
    expected_database_id: SchemaSqlId,
    entries: I,
) -> Result<(), SchemaSqlError>
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let mut allocator_ids = BTreeSet::new();
    for stored in entries {
        let decoded = decode_schema_sql(SchemaSqlKind::Table, stored.as_ref())?
            .ok_or(SchemaSqlError::CatalogEntryMissingEnvelope)?;
        validate_decoded_catalog_entry(expected_database_id, decoded, &mut allocator_ids)?;
    }
    Ok(())
}

fn validate_decoded_catalog_entry(
    expected_database_id: SchemaSqlId,
    decoded: DecodedSchemaSql<'_>,
    allocator_ids: &mut BTreeSet<[u8; 16]>,
) -> Result<(), SchemaSqlError> {
    if decoded.context.kind != SchemaSqlKind::Table {
        return Err(SchemaSqlError::ObjectKindMismatch);
    }
    validate_context(decoded.context)?;
    validate_statement(decoded.normalized_ddl)?;
    match decoded.v2_metadata {
        Some(metadata) => {
            validate_auto_increment_definition(decoded)?;
            if metadata.database_id != expected_database_id {
                return Err(SchemaSqlError::CatalogDatabaseIdMismatch);
            }
            if !allocator_ids.insert(metadata.allocator_id.into_bytes()) {
                return Err(SchemaSqlError::CatalogAllocatorIdDuplicate);
            }
        }
        None => match parse_auto_increment_create_table(
            decoded.normalized_ddl,
            parser_sql_mode(decoded.context.sql_mode),
        ) {
            Ok(_) => return Err(SchemaSqlError::MissingV2Metadata),
            Err(_) => {
                let mode = parser_sql_mode(decoded.context.sql_mode);
                match parse_checked_primary_key_create_table(decoded.normalized_ddl, mode) {
                    Ok(checked) => {
                        if checked.normalized_mysql_ddl != decoded.normalized_ddl {
                            return Err(SchemaSqlError::MalformedTableDefinition);
                        }
                    }
                    Err(_) => {
                        parse_create_table_ast(decoded.normalized_ddl, mode)
                            .map(|_| ())
                            .map_err(|_| SchemaSqlError::MalformedTableDefinition)?;
                    }
                }
            }
        },
    }
    Ok(())
}

impl StoredSchemaSqlContext {
    fn from_context(context: SchemaSqlContext) -> Result<Self, SchemaSqlError> {
        Ok(Self {
            kind: StoredSchemaSqlKind::from_core(context.kind)?,
            sql_mode: StoredSchemaSqlMode {
                ansi_quotes: context.sql_mode.ansi_quotes,
                no_backslash_escapes: context.sql_mode.no_backslash_escapes,
            },
            character_set_client: context.character_set_client,
            collation_connection: context.collation_connection,
            default_character_set: context.default_character_set,
            default_collation: context.default_collation,
        })
    }

    fn to_context(self) -> Result<SchemaSqlContext, SchemaSqlError> {
        Ok(SchemaSqlContext {
            kind: self.kind.to_core()?,
            sql_mode: SchemaSqlMode {
                ansi_quotes: self.sql_mode.ansi_quotes,
                no_backslash_escapes: self.sql_mode.no_backslash_escapes,
            },
            character_set_client: self.character_set_client,
            collation_connection: self.collation_connection,
            default_character_set: self.default_character_set,
            default_collation: self.default_collation,
        })
    }
}

impl StoredSchemaSqlContextV2 {
    fn from_context(
        context: SchemaSqlContext,
        metadata: SchemaSqlV2Metadata,
    ) -> Result<Self, SchemaSqlError> {
        if context.kind != SchemaSqlKind::Table {
            return Err(SchemaSqlError::InvalidContext);
        }
        Ok(Self {
            kind: StoredSchemaSqlKind::from_core(context.kind)?,
            sql_mode: StoredSchemaSqlMode {
                ansi_quotes: context.sql_mode.ansi_quotes,
                no_backslash_escapes: context.sql_mode.no_backslash_escapes,
            },
            character_set_client: context.character_set_client,
            collation_connection: context.collation_connection,
            default_character_set: context.default_character_set,
            default_collation: context.default_collation,
            database_id: encode_schema_id(metadata.database_id),
            allocator_id: encode_schema_id(metadata.allocator_id),
        })
    }

    fn to_context(&self) -> Result<(SchemaSqlContext, SchemaSqlV2Metadata), SchemaSqlError> {
        if self.kind != StoredSchemaSqlKind::Table {
            return Err(SchemaSqlError::InvalidContext);
        }
        Ok((
            SchemaSqlContext {
                kind: self.kind.to_core()?,
                sql_mode: SchemaSqlMode {
                    ansi_quotes: self.sql_mode.ansi_quotes,
                    no_backslash_escapes: self.sql_mode.no_backslash_escapes,
                },
                character_set_client: self.character_set_client,
                collation_connection: self.collation_connection,
                default_character_set: self.default_character_set,
                default_collation: self.default_collation,
            },
            SchemaSqlV2Metadata {
                database_id: decode_schema_id(&self.database_id)?,
                allocator_id: decode_schema_id(&self.allocator_id)?,
            },
        ))
    }
}

impl StoredSchemaSqlKind {
    fn from_core(kind: SchemaSqlKind) -> Result<Self, SchemaSqlError> {
        match kind {
            SchemaSqlKind::Table => Ok(Self::Table),
            SchemaSqlKind::Index => Ok(Self::Index),
            SchemaSqlKind::View => Ok(Self::View),
            SchemaSqlKind::Trigger => Ok(Self::Trigger),
            _ => Err(SchemaSqlError::InvalidContext),
        }
    }

    fn to_core(self) -> Result<SchemaSqlKind, SchemaSqlError> {
        match self {
            Self::Table => Ok(SchemaSqlKind::Table),
            Self::Index => Ok(SchemaSqlKind::Index),
            Self::View => Ok(SchemaSqlKind::View),
            Self::Trigger => Ok(SchemaSqlKind::Trigger),
        }
    }
}

fn canonical_context_json(context: &StoredSchemaSqlContext) -> Result<Vec<u8>, SchemaSqlError> {
    serde_json::to_vec(context).map_err(|_| SchemaSqlError::InvalidContext)
}

fn canonical_context_json_v2(
    context: &StoredSchemaSqlContextV2,
) -> Result<Vec<u8>, SchemaSqlError> {
    serde_json::to_vec(context).map_err(|_| SchemaSqlError::InvalidContext)
}

fn encode_schema_id(id: SchemaSqlId) -> String {
    let mut encoded = String::with_capacity(32);
    for byte in id.0 {
        encoded.push(char::from(b"0123456789abcdef"[(byte >> 4) as usize]));
        encoded.push(char::from(b"0123456789abcdef"[(byte & 0x0f) as usize]));
    }
    encoded
}

fn decode_schema_id(encoded: &str) -> Result<SchemaSqlId, SchemaSqlError> {
    if encoded.len() != 32 {
        return Err(SchemaSqlError::InvalidContext);
    }

    let bytes = encoded.as_bytes();
    let mut decoded = [0; 16];
    for (index, pair) in bytes.chunks_exact(2).enumerate() {
        let high = decode_lower_hex_digit(pair[0]).ok_or(SchemaSqlError::InvalidContext)?;
        let low = decode_lower_hex_digit(pair[1]).ok_or(SchemaSqlError::InvalidContext)?;
        decoded[index] = (high << 4) | low;
    }
    SchemaSqlId::from_bytes(decoded)
}

fn decode_lower_hex_digit(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        _ => None,
    }
}

fn validate_context(context: SchemaSqlContext) -> Result<(), SchemaSqlError> {
    if !collation_belongs_to(context.character_set_client, context.collation_connection)
        || !collation_belongs_to(context.default_character_set, context.default_collation)
    {
        return Err(SchemaSqlError::InvalidContext);
    }
    Ok(())
}

fn session_context(context: SchemaSqlContext) -> SchemaSqlSessionContext {
    SchemaSqlSessionContext {
        sql_mode: context.sql_mode,
        character_set_client: context.character_set_client,
        collation_connection: context.collation_connection,
        default_character_set: context.default_character_set,
        default_collation: context.default_collation,
    }
}

fn collation_belongs_to(character_set: CharacterSet, collation: Collation) -> bool {
    matches!(
        (character_set, collation),
        (
            CharacterSet::Utf8mb4,
            Collation::Utf8mb4_0900AiCi | Collation::Utf8mb4Bin
        ) | (CharacterSet::Binary, Collation::Binary)
    )
}

fn validate_statement(statement: &str) -> Result<(), SchemaSqlError> {
    if statement.trim().is_empty() {
        return Err(SchemaSqlError::EmptyStatement);
    }
    if statement.len() > MAX_NORMALIZED_DDL_BYTES {
        return Err(SchemaSqlError::StatementTooLong);
    }
    Ok(())
}

fn validate_auto_increment_definition(decoded: DecodedSchemaSql<'_>) -> Result<(), SchemaSqlError> {
    parse_auto_increment_create_table(
        decoded.normalized_ddl,
        parser_sql_mode(decoded.context.sql_mode),
    )
    .map(|_| ())
    .map_err(|_| SchemaSqlError::MalformedAutoIncrementDefinition)
}

fn parser_sql_mode(mode: SchemaSqlMode) -> SessionSqlMode {
    SessionSqlMode {
        ansi_quotes: mode.ansi_quotes,
        no_backslash_escapes: mode.no_backslash_escapes,
    }
}

fn schema_sql_error_to_limbo(error: SchemaSqlError) -> turso_core::LimboError {
    match error {
        SchemaSqlError::StatementTooLong => turso_core::LimboError::TooBig,
        _ => turso_core::LimboError::ParseError(error.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use turso_core::SchemaSqlFormatter as _;
    use turso_parser::{ast::Cmd, parser::Parser};

    const TABLE_DDL: &str = "CREATE TABLE `t` (`id` INT NOT NULL) DEFAULT CHARACTER SET = utf8mb4 COLLATE = utf8mb4_0900_ai_ci";

    fn session_context() -> SchemaSqlSessionContext {
        SchemaSqlSessionContext {
            sql_mode: SchemaSqlMode {
                ansi_quotes: true,
                no_backslash_escapes: false,
            },
            character_set_client: CharacterSet::Utf8mb4,
            collation_connection: Collation::Utf8mb4_0900AiCi,
            default_character_set: CharacterSet::Utf8mb4,
            default_collation: Collation::Utf8mb4_0900AiCi,
        }
    }

    fn table_context() -> SchemaSqlContext {
        session_context().for_kind(SchemaSqlKind::Table)
    }

    fn sqlite_table_stmt() -> turso_parser::ast::Stmt {
        let mut parser = Parser::new(b"CREATE TABLE t (id INTEGER)");
        let Some(Cmd::Stmt(stmt)) = parser.next_cmd().unwrap() else {
            panic!("expected CREATE TABLE statement");
        };
        stmt
    }

    #[test]
    fn round_trips_canonical_context_and_normalized_ddl() {
        let context = table_context();
        let stored = encode_schema_sql(context, TABLE_DDL).unwrap();

        assert_eq!(
            stored,
            "/*@turso:mysql-schema:v1:eyJraW5kIjoidGFibGUiLCJzcWxfbW9kZSI6eyJhbnNpX3F1b3RlcyI6dHJ1ZSwibm9fYmFja3NsYXNoX2VzY2FwZXMiOmZhbHNlfSwiY2hhcmFjdGVyX3NldF9jbGllbnQiOiJ1dGY4bWI0IiwiY29sbGF0aW9uX2Nvbm5lY3Rpb24iOiJ1dGY4bWI0XzA5MDBfYWlfY2kiLCJkZWZhdWx0X2NoYXJhY3Rlcl9zZXQiOiJ1dGY4bWI0IiwiZGVmYXVsdF9jb2xsYXRpb24iOiJ1dGY4bWI0XzA5MDBfYWlfY2kifQ*/ CREATE TABLE `t` (`id` INT NOT NULL) DEFAULT CHARACTER SET = utf8mb4 COLLATE = utf8mb4_0900_ai_ci"
        );

        let decoded = decode_schema_sql(SchemaSqlKind::Table, &stored)
            .unwrap()
            .unwrap();
        assert_eq!(decoded.context, context);
        assert_eq!(decoded.v2_metadata(), None);
        assert_eq!(decoded.normalized_ddl, TABLE_DDL);
    }

    fn v2_metadata() -> SchemaSqlV2Metadata {
        SchemaSqlV2Metadata::new([0x01; 16], [0xab; 16]).unwrap()
    }

    fn auto_increment_ddl(table: &str) -> String {
        format!("CREATE TABLE `{table}` (`id` INT NOT NULL AUTO_INCREMENT PRIMARY KEY)")
    }

    fn plain_table_ddl(table: &str) -> String {
        format!("CREATE TABLE `{table}` (`id` INT NOT NULL)")
    }

    fn metadata(database_byte: u8, allocator_byte: u8) -> SchemaSqlV2Metadata {
        SchemaSqlV2Metadata::new([database_byte; 16], [allocator_byte; 16]).unwrap()
    }

    fn encoded_v2_context(context: &str) -> String {
        format!(
            "{RESERVED_PREFIX}{V2_VERSION_PREFIX}{}{MARKER_END}{TABLE_DDL}",
            URL_SAFE_NO_PAD.encode(context)
        )
    }

    #[test]
    fn v2_round_trips_typed_metadata_with_canonical_lowercase_hex() {
        let metadata = v2_metadata();
        let stored = encode_schema_sql_v2(table_context(), metadata, TABLE_DDL).unwrap();
        let (encoded_context, _) = stored
            .strip_prefix("/*@turso:mysql-schema:v2:")
            .unwrap()
            .split_once(MARKER_END)
            .unwrap();
        let context_json = URL_SAFE_NO_PAD.decode(encoded_context).unwrap();
        let expected_json = format!(
            r#"{{"kind":"table","sql_mode":{{"ansi_quotes":true,"no_backslash_escapes":false}},"character_set_client":"utf8mb4","collation_connection":"utf8mb4_0900_ai_ci","default_character_set":"utf8mb4","default_collation":"utf8mb4_0900_ai_ci","database_id":"{}","allocator_id":"{}"}}"#,
            "01".repeat(16),
            "ab".repeat(16),
        );
        assert_eq!(context_json, expected_json.as_bytes());

        let decoded = decode_schema_sql(SchemaSqlKind::Table, &stored)
            .unwrap()
            .unwrap();
        assert_eq!(decoded.context, table_context());
        assert_eq!(decoded.v2_metadata(), Some(metadata));
        assert_eq!(decoded.normalized_ddl, TABLE_DDL);
    }

    #[test]
    fn v2_encode_requires_a_table_and_nonzero_identities() {
        let metadata = v2_metadata();
        let mut index_context = table_context();
        index_context.kind = SchemaSqlKind::Index;
        assert_eq!(
            encode_schema_sql_v2(index_context, metadata, TABLE_DDL),
            Err(SchemaSqlError::InvalidContext)
        );
        assert_eq!(
            SchemaSqlV2Metadata::new([0; 16], [1; 16]),
            Err(SchemaSqlError::InvalidContext)
        );
        assert_eq!(
            SchemaSqlV2Metadata::new([1; 16], [0; 16]),
            Err(SchemaSqlError::InvalidContext)
        );
    }

    #[test]
    fn v2_rejects_unknown_missing_extra_wrong_kind_and_noncanonical_contexts() {
        let valid = format!(
            r#"{{"kind":"table","sql_mode":{{"ansi_quotes":false,"no_backslash_escapes":false}},"character_set_client":"utf8mb4","collation_connection":"utf8mb4_0900_ai_ci","default_character_set":"utf8mb4","default_collation":"utf8mb4_0900_ai_ci","database_id":"{}","allocator_id":"{}"}}"#,
            "01".repeat(16),
            "ab".repeat(16),
        );
        let invalid_contexts = [
            valid.replace(
                &format!(",\"allocator_id\":\"{}\"}}", "ab".repeat(16)),
                &format!(
                    ",\"allocator_id\":\"{}\",\"unknown\":true}}",
                    "ab".repeat(16)
                ),
            ),
            valid.replacen(&format!(",\"allocator_id\":\"{}\"", "ab".repeat(16)), "", 1),
            valid.replacen(
                &format!(",\"database_id\":\"{}\"", "01".repeat(16)),
                &format!(
                    ",\"database_id\":\"{}\",\"database_id\":\"{}\"",
                    "01".repeat(16),
                    "01".repeat(16)
                ),
                1,
            ),
            valid.replacen(r#""kind":"table""#, r#""kind":"index""#, 1),
            format!(" {valid}"),
        ];

        for context in invalid_contexts {
            assert!(
                decode_schema_sql(SchemaSqlKind::Table, &encoded_v2_context(&context)).is_err()
            );
        }

        let mut uppercase = valid.clone();
        uppercase = uppercase.replace(
            &format!(r#""allocator_id":"{}""#, "ab".repeat(16)),
            &format!(r#""allocator_id":"{}""#, "AB".repeat(16)),
        );
        assert!(decode_schema_sql(SchemaSqlKind::Table, &encoded_v2_context(&uppercase)).is_err());

        let zero = valid.replace(
            &format!(r#""database_id":"{}""#, "01".repeat(16)),
            &format!(r#""database_id":"{}""#, "00".repeat(16)),
        );
        assert!(decode_schema_sql(SchemaSqlKind::Table, &encoded_v2_context(&zero)).is_err());

        let short = valid.replace(
            &format!(r#""allocator_id":"{}""#, "ab".repeat(16)),
            r#""allocator_id":"ab""#,
        );
        assert!(decode_schema_sql(SchemaSqlKind::Table, &encoded_v2_context(&short)).is_err());

        let v1_with_v2_field = format!(
            r#"{{"kind":"table","sql_mode":{{"ansi_quotes":false,"no_backslash_escapes":false}},"character_set_client":"utf8mb4","collation_connection":"utf8mb4_0900_ai_ci","default_character_set":"utf8mb4","default_collation":"utf8mb4_0900_ai_ci","allocator_id":"{}"}}"#,
            "ab".repeat(16),
        );
        let v1 = format!(
            "{RESERVED_PREFIX}{VERSION_PREFIX}{}{MARKER_END}{TABLE_DDL}",
            URL_SAFE_NO_PAD.encode(v1_with_v2_field)
        );
        assert!(decode_schema_sql(SchemaSqlKind::Table, &v1).is_err());
    }

    #[test]
    fn v2_rejects_malformed_or_oversized_contexts_without_sqlite_fallback() {
        for encoded in ["not=base64", "eyJ9"] {
            assert!(decode_schema_sql(
                SchemaSqlKind::Table,
                &format!("{RESERVED_PREFIX}{V2_VERSION_PREFIX}{encoded}{MARKER_END}{TABLE_DDL}")
            )
            .is_err());
        }

        let unknown_version = format!(
            "{RESERVED_PREFIX}v3:{}{MARKER_END}{TABLE_DDL}",
            URL_SAFE_NO_PAD.encode("{}")
        );
        assert_eq!(
            decode_schema_sql(SchemaSqlKind::Table, &unknown_version),
            Err(SchemaSqlError::UnsupportedVersion)
        );

        let long_context = format!(r#"{{"unknown":"{}"}}"#, "x".repeat(MAX_CONTEXT_JSON_BYTES));
        assert_eq!(
            decode_schema_sql(SchemaSqlKind::Table, &encoded_v2_context(&long_context)),
            Err(SchemaSqlError::ContextTooLong)
        );
    }

    #[test]
    fn catalog_validation_accepts_v1_non_auto_increment_and_v2_auto_increment_rows() {
        let expected_database_id = SchemaSqlId::from_bytes([1; 16]).unwrap();
        let first = encode_schema_sql_v2(
            table_context(),
            metadata(1, 2),
            &auto_increment_ddl("first"),
        )
        .unwrap();
        let second = encode_schema_sql_v2(
            table_context(),
            metadata(1, 3),
            &auto_increment_ddl("second"),
        )
        .unwrap();
        let legacy = encode_schema_sql(table_context(), &plain_table_ddl("legacy")).unwrap();
        let decoded_second = decode_schema_sql(SchemaSqlKind::Table, &second)
            .unwrap()
            .unwrap();

        validate_schema_sql_catalog(
            expected_database_id,
            [
                SchemaSqlCatalogEntry::encoded(&first),
                SchemaSqlCatalogEntry::decoded(decoded_second),
                SchemaSqlCatalogEntry::encoded(&legacy),
            ],
        )
        .unwrap();
        validate_encoded_schema_sql_catalog(expected_database_id, [&first, &second, &legacy])
            .unwrap();
        validate_decoded_schema_sql_catalog(
            expected_database_id,
            [
                decode_schema_sql(SchemaSqlKind::Table, &first)
                    .unwrap()
                    .unwrap(),
                decode_schema_sql(SchemaSqlKind::Table, &second)
                    .unwrap()
                    .unwrap(),
                decode_schema_sql(SchemaSqlKind::Table, &legacy)
                    .unwrap()
                    .unwrap(),
            ],
        )
        .unwrap();
    }

    #[test]
    fn catalog_validation_accepts_canonical_ordinary_primary_key_rows() {
        let ordinary = encode_schema_sql(
            table_context(),
            "CREATE TABLE `ordinary` (`id` INT NOT NULL PRIMARY KEY) ENGINE = InnoDB",
        )
        .unwrap();

        validate_encoded_schema_sql_catalog(SchemaSqlId::from_bytes([1; 16]).unwrap(), [ordinary])
            .unwrap();
    }

    #[test]
    fn catalog_validation_rejects_noncanonical_ordinary_primary_key_rows() {
        let ordinary = encode_schema_sql(
            table_context(),
            "CREATE TABLE `ordinary` (`id` INT PRIMARY KEY) ENGINE=InnoDB",
        )
        .unwrap();

        assert_eq!(
            validate_encoded_schema_sql_catalog(
                SchemaSqlId::from_bytes([1; 16]).unwrap(),
                [ordinary],
            ),
            Err(SchemaSqlError::MalformedTableDefinition)
        );
    }

    #[test]
    fn catalog_validation_rejects_v1_auto_increment_without_v2_metadata() {
        let legacy = encode_schema_sql(table_context(), &auto_increment_ddl("legacy")).unwrap();
        assert_eq!(
            validate_encoded_schema_sql_catalog(
                SchemaSqlId::from_bytes([1; 16]).unwrap(),
                [legacy],
            ),
            Err(SchemaSqlError::MissingV2Metadata)
        );
    }

    #[test]
    fn catalog_validation_rejects_mixed_database_ids_and_duplicate_allocator_ids() {
        let expected_database_id = SchemaSqlId::from_bytes([1; 16]).unwrap();
        let first = encode_schema_sql_v2(
            table_context(),
            metadata(1, 2),
            &auto_increment_ddl("first"),
        )
        .unwrap();
        let other_database = encode_schema_sql_v2(
            table_context(),
            metadata(9, 3),
            &auto_increment_ddl("other_database"),
        )
        .unwrap();
        assert_eq!(
            validate_encoded_schema_sql_catalog(
                expected_database_id,
                [first.as_str(), other_database.as_str()],
            ),
            Err(SchemaSqlError::CatalogDatabaseIdMismatch)
        );

        let duplicate_allocator = encode_schema_sql_v2(
            table_context(),
            metadata(1, 2),
            &auto_increment_ddl("duplicate"),
        )
        .unwrap();
        assert_eq!(
            validate_encoded_schema_sql_catalog(
                expected_database_id,
                [first.as_str(), duplicate_allocator.as_str()],
            ),
            Err(SchemaSqlError::CatalogAllocatorIdDuplicate)
        );
    }

    #[test]
    fn catalog_validation_rejects_malformed_rows_and_auto_increment_definitions() {
        let expected_database_id = SchemaSqlId::from_bytes([1; 16]).unwrap();
        assert_eq!(
            validate_encoded_schema_sql_catalog(expected_database_id, [TABLE_DDL]),
            Err(SchemaSqlError::CatalogEntryMissingEnvelope)
        );

        let malformed_marker = "/*@turso:mysql-schema:v2:not=base64*/ CREATE TABLE t (id INT)";
        assert_eq!(
            validate_encoded_schema_sql_catalog(expected_database_id, [malformed_marker]),
            Err(SchemaSqlError::InvalidBase64)
        );

        let malformed_auto_increment = encode_schema_sql_v2(
            table_context(),
            metadata(1, 2),
            "CREATE TABLE `t` (`id` INT AUTO_INCREMENT)",
        )
        .unwrap();
        assert_eq!(
            validate_encoded_schema_sql_catalog(expected_database_id, [malformed_auto_increment]),
            Err(SchemaSqlError::MalformedAutoIncrementDefinition)
        );

        let literal_keyword = encode_schema_sql(
            table_context(),
            "CREATE TABLE `ordinary` (`description` TEXT DEFAULT 'AUTO_INCREMENT')",
        )
        .unwrap();
        validate_encoded_schema_sql_catalog(expected_database_id, [literal_keyword]).unwrap();
    }

    #[test]
    fn unmarked_internal_sql_uses_the_sqlite_fallback() {
        assert_eq!(
            decode_schema_sql(
                SchemaSqlKind::Table,
                "CREATE TABLE sqlite_sequence(name,seq)"
            ),
            Ok(None)
        );
    }

    #[test]
    fn session_context_attaches_the_kind_selected_by_core() {
        for kind in [
            SchemaSqlKind::Table,
            SchemaSqlKind::Index,
            SchemaSqlKind::View,
            SchemaSqlKind::Trigger,
        ] {
            assert_eq!(session_context().for_kind(kind).kind, kind);
        }
    }

    #[test]
    fn formatter_envelopes_the_kind_selected_for_the_create() {
        let stmt = sqlite_table_stmt();
        let mut formatter = session_context();
        formatter.character_set_client = CharacterSet::Binary;
        formatter.collation_connection = Collation::Binary;
        formatter.default_character_set = CharacterSet::Binary;
        formatter.default_collation = Collation::Binary;
        let stored = formatter
            .format_schema_sql(SchemaSqlKind::Table, TABLE_DDL, &stmt)
            .unwrap();
        let decoded = decode_schema_sql(SchemaSqlKind::Table, &stored)
            .unwrap()
            .unwrap();

        assert_eq!(decoded.context, formatter.for_kind(SchemaSqlKind::Table));
        assert_eq!(decoded.normalized_ddl, TABLE_DDL);
    }

    #[test]
    fn formatter_rejects_schema_rows_the_dialect_cannot_reopen() {
        let stmt = sqlite_table_stmt();
        assert!(matches!(
            session_context().format_schema_sql(SchemaSqlKind::Table, TABLE_DDL, &stmt),
            Err(turso_core::LimboError::ParseError(_))
        ));
        assert!(matches!(
            session_context().format_schema_sql(SchemaSqlKind::View, TABLE_DDL, &stmt),
            Err(turso_core::LimboError::ParseError(_))
        ));
    }

    #[test]
    fn formatter_reuses_the_previous_context_for_table_rewrites() {
        let mut previous_session = session_context();
        previous_session.character_set_client = CharacterSet::Binary;
        previous_session.collation_connection = Collation::Binary;
        previous_session.default_character_set = CharacterSet::Binary;
        previous_session.default_collation = Collation::Binary;
        let previous = encode_schema_sql(
            previous_session.for_kind(SchemaSqlKind::Table),
            "CREATE TABLE `t` (`id` INTEGER)",
        )
        .unwrap();

        let rewritten = session_context()
            .format_rewritten_schema_sql(SchemaSqlKind::Table, &previous, &sqlite_table_stmt())
            .unwrap();
        let decoded = decode_schema_sql(SchemaSqlKind::Table, &rewritten)
            .unwrap()
            .unwrap();
        assert_eq!(
            decoded.context,
            previous_session.for_kind(SchemaSqlKind::Table)
        );
        assert_eq!(decoded.normalized_ddl, "CREATE TABLE `t` (`id` INTEGER)");
    }

    #[test]
    fn formatter_rejects_rewrites_of_ordinary_primary_key_tables() {
        let mut previous_session = session_context();
        previous_session.character_set_client = CharacterSet::Binary;
        previous_session.collation_connection = Collation::Binary;
        previous_session.default_character_set = CharacterSet::Binary;
        previous_session.default_collation = Collation::Binary;
        let previous = encode_schema_sql(
            previous_session.for_kind(SchemaSqlKind::Table),
            "CREATE TABLE `t` (`id` INT NOT NULL PRIMARY KEY) ENGINE = InnoDB",
        )
        .unwrap();

        assert!(matches!(
            session_context().format_rewritten_schema_sql(
                SchemaSqlKind::Table,
                &previous,
                &sqlite_table_stmt(),
            ),
            Err(turso_core::LimboError::ParseError(message))
                if message.contains("ordinary MySQL PRIMARY KEY table")
        ));
    }

    #[test]
    fn formatter_preserves_v2_identities_for_table_rewrites() {
        let mut previous_session = session_context();
        previous_session.character_set_client = CharacterSet::Binary;
        previous_session.collation_connection = Collation::Binary;
        previous_session.default_character_set = CharacterSet::Binary;
        previous_session.default_collation = Collation::Binary;
        let metadata = v2_metadata();
        let previous = encode_schema_sql_v2(
            previous_session.for_kind(SchemaSqlKind::Table),
            metadata,
            "CREATE TABLE `t` (`id` INTEGER)",
        )
        .unwrap();

        let rewritten = session_context()
            .format_rewritten_schema_sql(SchemaSqlKind::Table, &previous, &sqlite_table_stmt())
            .unwrap();
        assert!(rewritten.starts_with("/*@turso:mysql-schema:v2:"));
        let decoded = decode_schema_sql(SchemaSqlKind::Table, &rewritten)
            .unwrap()
            .unwrap();
        assert_eq!(decoded.v2_metadata(), Some(metadata));
        assert_eq!(decoded.normalized_ddl, "CREATE TABLE `t` (`id` INTEGER)");
    }

    #[test]
    fn persisted_marker_errors_are_reported_as_core_corruption() {
        let error = decode_persisted_schema_sql(
            SchemaSqlKind::Table,
            "/*@turso:mysql-schema:v2:eyJ9*/ CREATE TABLE `t` (`id` INT)",
        )
        .unwrap_err();

        assert!(matches!(error, turso_core::LimboError::Corrupt(_)));
    }

    #[test]
    fn only_total_reserved_prefix_absence_allows_fallback() {
        for stored in [
            "/*@turso:mysql-schema:v2:eyJ9*/ CREATE TABLE `t` (`id` INT)",
            "/*@turso:mysql-schema:v1:not=base64*/ CREATE TABLE `t` (`id` INT)",
            "/*@turso:mysql-schema:v1:eyJ9*/",
        ] {
            assert!(decode_schema_sql(SchemaSqlKind::Table, stored).is_err());
        }
    }

    #[test]
    fn rejects_unknown_missing_duplicate_and_noncanonical_context_fields() {
        let invalid_contexts = [
            r#"{"kind":"table","sql_mode":{"ansi_quotes":false,"no_backslash_escapes":false},"character_set_client":"utf8mb4","collation_connection":"utf8mb4_0900_ai_ci","default_character_set":"utf8mb4","default_collation":"utf8mb4_0900_ai_ci","unknown":true}"#,
            r#"{"kind":"table","sql_mode":{"ansi_quotes":false,"no_backslash_escapes":false},"character_set_client":"utf8mb4","collation_connection":"utf8mb4_0900_ai_ci","default_character_set":"utf8mb4"}"#,
            r#"{"kind":"table","kind":"view","sql_mode":{"ansi_quotes":false,"no_backslash_escapes":false},"character_set_client":"utf8mb4","collation_connection":"utf8mb4_0900_ai_ci","default_character_set":"utf8mb4","default_collation":"utf8mb4_0900_ai_ci"}"#,
            r#"{"sql_mode":{"ansi_quotes":false,"no_backslash_escapes":false},"kind":"table","character_set_client":"utf8mb4","collation_connection":"utf8mb4_0900_ai_ci","default_character_set":"utf8mb4","default_collation":"utf8mb4_0900_ai_ci"}"#,
        ];

        for context in invalid_contexts {
            let stored = format!(
                "{RESERVED_PREFIX}{VERSION_PREFIX}{}{MARKER_END}{TABLE_DDL}",
                URL_SAFE_NO_PAD.encode(context)
            );
            assert!(decode_schema_sql(SchemaSqlKind::Table, &stored).is_err());
        }
    }

    #[test]
    fn rejects_kind_mismatch_and_incompatible_charset_collation() {
        let stored = encode_schema_sql(table_context(), TABLE_DDL).unwrap();
        assert_eq!(
            decode_schema_sql(SchemaSqlKind::View, &stored),
            Err(SchemaSqlError::ObjectKindMismatch)
        );

        let mut invalid = table_context();
        invalid.default_character_set = CharacterSet::Binary;
        assert_eq!(
            encode_schema_sql(invalid, TABLE_DDL),
            Err(SchemaSqlError::InvalidContext)
        );
    }

    #[test]
    fn rejects_context_and_statement_over_their_bounds() {
        let long_context = format!("{{\"x\":\"{}\"}}", "x".repeat(MAX_CONTEXT_JSON_BYTES));
        let stored = format!(
            "{RESERVED_PREFIX}{VERSION_PREFIX}{}{MARKER_END}{TABLE_DDL}",
            URL_SAFE_NO_PAD.encode(long_context)
        );
        assert_eq!(
            decode_schema_sql(SchemaSqlKind::Table, &stored),
            Err(SchemaSqlError::ContextTooLong)
        );

        let ddl = "x".repeat(MAX_NORMALIZED_DDL_BYTES + 1);
        assert_eq!(
            encode_schema_sql(table_context(), &ddl),
            Err(SchemaSqlError::StatementTooLong)
        );
    }

    #[test]
    fn rejects_empty_and_whitespace_only_statements() {
        for ddl in ["", " ", "\n\t"] {
            assert_eq!(
                encode_schema_sql(table_context(), ddl),
                Err(SchemaSqlError::EmptyStatement)
            );

            let stored = format!(
                "{RESERVED_PREFIX}{VERSION_PREFIX}{}{MARKER_END}{ddl}",
                URL_SAFE_NO_PAD.encode(
                    canonical_context_json(
                        &StoredSchemaSqlContext::from_context(table_context()).unwrap()
                    )
                    .unwrap()
                )
            );
            assert_eq!(
                decode_schema_sql(SchemaSqlKind::Table, &stored),
                Err(SchemaSqlError::EmptyStatement)
            );
        }
    }
}
