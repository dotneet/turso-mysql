//! The `information_schema` reads Prisma's schema engine makes before it
//! migrates, pulls or diffs a database, as prisma-engines 7.1 (the engines of
//! Prisma 6.19) writes them in `sql-schema-describer/src/mysql.rs`.
//!
//! They are recognized rather than translated. Most compare and order names
//! by their bytes with `BINARY`, which this server's checked `SELECT` does not
//! read, so the server works out each answer from plain reads of the catalog
//! and does the byte comparisons and orderings itself.

use super::*;
use information_schema::same_catalog_tokens;

/// One of the fixed catalog reads, each prepared with its schema bound.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrismaInformationSchemaQuery {
    /// The tables `migrate deploy` looks through before it makes
    /// `_prisma_migrations`: the names alone.
    MigrationTableNames,
    /// The tables the describer describes, with their options and comments.
    TableNames,
    /// Every `CHECK` constraint the session can see, in every database.
    CheckConstraints,
    /// Every column of every table and view of the database.
    Columns,
    /// Every column of every foreign key of the database.
    ForeignKeys,
    /// Every column of every index of the database.
    Indexes,
}

impl PrismaInformationSchemaQuery {
    /// How many `?` the read binds, each a database's name.
    pub fn parameter_count(self) -> usize {
        match self {
            Self::MigrationTableNames | Self::TableNames | Self::ForeignKeys => 2,
            Self::Columns | Self::Indexes => 1,
            Self::CheckConstraints => 0,
        }
    }
}

const MIGRATION_TABLE_NAMES: &str = "SELECT DISTINCT BINARY table_info.table_name AS table_name \
    FROM information_schema.tables AS table_info \
    JOIN information_schema.columns AS column_info \
    ON BINARY column_info.table_name = BINARY table_info.table_name \
    WHERE table_info.table_schema = ? AND column_info.table_schema = ? \
    AND table_info.table_type = 'BASE TABLE' \
    ORDER BY BINARY table_info.table_name";

const TABLE_NAMES: &str = "SELECT DISTINCT BINARY table_info.table_name AS table_name, \
    table_info.create_options AS create_options, \
    table_info.table_comment AS table_comment \
    FROM information_schema.tables AS table_info \
    JOIN information_schema.columns AS column_info \
    ON BINARY column_info.table_name = BINARY table_info.table_name \
    WHERE table_info.table_schema = ? AND column_info.table_schema = ? \
    AND table_info.table_type = 'BASE TABLE' \
    ORDER BY BINARY table_info.table_name";

const CHECK_CONSTRAINTS: &str = "SELECT tc.table_schema AS namespace, \
    tc.table_name AS table_name, tc.constraint_name AS constraint_name, \
    LOWER(tc.constraint_type) AS constraint_type, \
    cc.check_clause AS constraint_definition \
    FROM INFORMATION_SCHEMA.TABLE_CONSTRAINTS tc \
    LEFT JOIN INFORMATION_SCHEMA.CHECK_CONSTRAINTS cc \
    ON cc.constraint_schema = tc.table_schema AND cc.constraint_name = tc.constraint_name \
    WHERE constraint_type = 'CHECK' \
    ORDER BY namespace, table_name, constraint_type, constraint_name;";

const COLUMNS: &str = "SELECT column_name column_name, data_type data_type, \
    column_type full_data_type, character_maximum_length character_maximum_length, \
    numeric_precision numeric_precision, numeric_scale numeric_scale, \
    datetime_precision datetime_precision, column_default column_default, \
    is_nullable is_nullable, extra extra, table_name table_name, \
    IF(column_comment = '', NULL, column_comment) AS column_comment \
    FROM information_schema.columns WHERE table_schema = ? ORDER BY ordinal_position";

const FOREIGN_KEYS: &str = "SELECT kcu.constraint_name constraint_name, \
    kcu.column_name column_name, kcu.referenced_table_name referenced_table_name, \
    kcu.referenced_column_name referenced_column_name, \
    kcu.ordinal_position ordinal_position, kcu.table_name table_name, \
    rc.delete_rule delete_rule, rc.update_rule update_rule \
    FROM information_schema.key_column_usage AS kcu \
    INNER JOIN information_schema.referential_constraints AS rc \
    ON BINARY kcu.constraint_name = BINARY rc.constraint_name \
    WHERE BINARY kcu.table_schema = ? AND BINARY rc.constraint_schema = ? \
    AND kcu.referenced_column_name IS NOT NULL \
    ORDER BY BINARY kcu.table_schema, BINARY kcu.table_name, \
    BINARY kcu.constraint_name, kcu.ordinal_position";

const INDEXES: &str = "SELECT table_name AS table_name, index_name AS index_name, \
    column_name AS column_name, sub_part AS partial, seq_in_index AS seq_in_index, \
    collation AS column_order, non_unique AS non_unique, index_type AS index_type \
    FROM information_schema.statistics WHERE table_schema = ? \
    ORDER BY BINARY table_name, BINARY index_name, seq_in_index";

/// Recognizes one of Prisma's catalog reads, whatever its spacing and with
/// the comments Prisma writes into them, or `None` for any other statement.
pub fn parse_optional_prisma_information_schema_query(
    sql: &str,
    mode: SessionSqlMode,
) -> Result<Option<PrismaInformationSchemaQuery>, ParseError> {
    // Every one of them names `information_schema`, which most statements
    // do not, so the rest are passed over without being read.
    if !mentions_ignoring_case(sql, "information_schema") {
        return Ok(None);
    }
    let tokens = tokenize_information_schema_query(sql, mode)?;
    for (template, query) in [
        (
            MIGRATION_TABLE_NAMES,
            PrismaInformationSchemaQuery::MigrationTableNames,
        ),
        (TABLE_NAMES, PrismaInformationSchemaQuery::TableNames),
        (
            CHECK_CONSTRAINTS,
            PrismaInformationSchemaQuery::CheckConstraints,
        ),
        (COLUMNS, PrismaInformationSchemaQuery::Columns),
        (FOREIGN_KEYS, PrismaInformationSchemaQuery::ForeignKeys),
        (INDEXES, PrismaInformationSchemaQuery::Indexes),
    ] {
        if same_catalog_tokens(&tokens, &tokenize_information_schema_query(template, mode)?) {
            return Ok(Some(query));
        }
    }
    Ok(None)
}
