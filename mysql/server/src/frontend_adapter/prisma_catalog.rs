//! Answering the `information_schema` reads Prisma's schema engine prepares.
//!
//! Each answer is worked out from plain reads of the catalog, which the text
//! path authorizes as it authorizes any read, and every comparison and order
//! Prisma writes with `BINARY` is made here on the names' bytes. Every column
//! is described the way MySQL 8.4.11 described it to `mysql_async`, the driver
//! Prisma speaks through.

use super::*;
use turso_mysql_parser::PrismaInformationSchemaQuery;

/// One row as the catalog's text path read it, a value's text or NULL.
type ReadRow = Vec<Option<Vec<u8>>>;

/// A table's name and its comment.
type NamedTable = (Vec<u8>, Option<Vec<u8>>);

impl<A> AuthorizedDatabaseCommandAdapter<A>
where
    A: DatabaseAuthorizer,
{
    /// Answers one of Prisma's catalog reads for the databases it bound.
    pub(super) fn prisma_catalog_result(
        &mut self,
        query: PrismaInformationSchemaQuery,
        schemas: &[String],
    ) -> Result<BinaryResultSet, FrontendErrorKind> {
        let rows = match query {
            PrismaInformationSchemaQuery::MigrationTableNames => self
                .prisma_table_names(schemas)?
                .into_iter()
                .map(|(name, _)| vec![Some(name)])
                .collect(),
            // Measured on MySQL 8.4.11: a table written with none of the
            // options `CREATE_OPTIONS` lists — and this server refuses every
            // one of them — answers an empty word.
            PrismaInformationSchemaQuery::TableNames => self
                .prisma_table_names(schemas)?
                .into_iter()
                .map(|(name, comment)| vec![Some(name), Some(Vec::new()), comment])
                .collect(),
            PrismaInformationSchemaQuery::CheckConstraints => self.prisma_check_constraints()?,
            PrismaInformationSchemaQuery::Columns => self.prisma_columns(schemas)?,
            PrismaInformationSchemaQuery::ForeignKeys => self.prisma_foreign_keys(schemas)?,
            PrismaInformationSchemaQuery::Indexes => self.prisma_indexes(schemas)?,
        };
        let columns = prisma_catalog_columns(query);
        let rows = rows
            .into_iter()
            .map(|row| binary_row(&columns, row))
            .collect::<Result<Vec<_>, _>>()?;
        Ok(BinaryResultSet {
            columns,
            rows,
            warnings: 0,
            status_flags: self.status_flags(),
        })
    }

    /// The base tables of the first database that have a column in the
    /// second, each once, in the order of their names' bytes, beside their
    /// comments.
    fn prisma_table_names(
        &mut self,
        schemas: &[String],
    ) -> Result<Vec<NamedTable>, FrontendErrorKind> {
        let [tables_in, columns_in] = schemas else {
            return Err(FrontendErrorKind::Internal);
        };
        let tables = self.read_the_catalog(&format!(
            "SELECT table_name, table_comment FROM information_schema.tables \
             WHERE table_schema = {} AND table_type = 'BASE TABLE'",
            written_name(tables_in)?
        ))?;
        let with_columns = self
            .read_the_catalog(&format!(
                "SELECT DISTINCT table_name FROM information_schema.columns \
                 WHERE table_schema = {}",
                written_name(columns_in)?
            ))?
            .into_iter()
            .map(|mut row| row.swap_remove(0))
            .collect::<Vec<_>>();
        let mut named = Vec::new();
        for row in tables {
            let [name, comment] = fields(row)?;
            let name = name.ok_or(FrontendErrorKind::Internal)?;
            if with_columns
                .iter()
                .any(|other| other.as_deref() == Some(name.as_slice()))
            {
                named.push((name, comment));
            }
        }
        named.sort_by(|(left, _), (right, _)| left.cmp(right));
        named.dedup_by(|(left, _), (right, _)| left == right);
        Ok(named)
    }

    /// Every `CHECK` constraint with its clause, the constraint's type spelled
    /// in lower case as `LOWER` spells it.
    fn prisma_check_constraints(&mut self) -> Result<Vec<ReadRow>, FrontendErrorKind> {
        let rows = self.read_the_catalog(
            "SELECT tc.table_schema, tc.table_name, tc.constraint_name, cc.check_clause \
             FROM information_schema.table_constraints tc \
             LEFT JOIN information_schema.check_constraints cc \
             ON cc.constraint_schema = tc.table_schema \
             AND cc.constraint_name = tc.constraint_name \
             WHERE tc.constraint_type = 'CHECK' \
             ORDER BY tc.table_schema, tc.table_name, tc.constraint_name",
        )?;
        rows.into_iter()
            .map(|row| {
                let [schema, table, name, clause] = fields(row)?;
                Ok(vec![schema, table, name, Some(b"check".to_vec()), clause])
            })
            .collect()
    }

    /// Every column of the database, an empty comment answered as NULL the
    /// way Prisma's `IF` asks.
    fn prisma_columns(&mut self, schemas: &[String]) -> Result<Vec<ReadRow>, FrontendErrorKind> {
        let [schema] = schemas else {
            return Err(FrontendErrorKind::Internal);
        };
        let mut rows = self.read_the_catalog(&format!(
            "SELECT column_name, data_type, column_type, character_maximum_length, \
             numeric_precision, numeric_scale, datetime_precision, column_default, \
             is_nullable, extra, table_name, column_comment \
             FROM information_schema.columns WHERE table_schema = {} \
             ORDER BY ordinal_position",
            written_name(schema)?
        ))?;
        for row in &mut rows {
            let comment = row.last_mut().ok_or(FrontendErrorKind::Internal)?;
            if comment.as_deref() == Some(b"".as_slice()) {
                *comment = None;
            }
        }
        Ok(rows)
    }

    /// Every column of every foreign key of the first database whose
    /// constraint the second database names, joined and ordered on the
    /// names' bytes.
    fn prisma_foreign_keys(
        &mut self,
        schemas: &[String],
    ) -> Result<Vec<ReadRow>, FrontendErrorKind> {
        let [keys_in, rules_in] = schemas else {
            return Err(FrontendErrorKind::Internal);
        };
        let keys = self.read_the_catalog(
            "SELECT table_schema, table_name, constraint_name, column_name, \
             referenced_table_name, referenced_column_name, ordinal_position \
             FROM information_schema.key_column_usage \
             WHERE referenced_column_name IS NOT NULL",
        )?;
        let rules = self
            .read_the_catalog(
                "SELECT constraint_schema, constraint_name, delete_rule, update_rule \
                 FROM information_schema.referential_constraints",
            )?
            .into_iter()
            .map(fields)
            .collect::<Result<Vec<[Option<Vec<u8>>; 4]>, _>>()?;
        let mut joined = Vec::new();
        for key in keys {
            let [schema, table, name, column, referenced_table, referenced_column, position] =
                fields(key)?;
            if schema.as_deref() != Some(keys_in.as_bytes()) {
                continue;
            }
            let place = whole_number(position.as_deref())?;
            for [rule_schema, rule_name, delete_rule, update_rule] in &rules {
                if rule_schema.as_deref() != Some(rules_in.as_bytes()) || *rule_name != name {
                    continue;
                }
                joined.push((
                    (table.clone(), name.clone(), place),
                    vec![
                        name.clone(),
                        column.clone(),
                        referenced_table.clone(),
                        referenced_column.clone(),
                        position.clone(),
                        table.clone(),
                        delete_rule.clone(),
                        update_rule.clone(),
                    ],
                ));
            }
        }
        joined.sort_by(|(left, _), (right, _)| left.cmp(right));
        Ok(joined.into_iter().map(|(_, row)| row).collect())
    }

    /// Every column of every index of the database, ordered on the table's
    /// and the index's names' bytes and then the column's place in the index.
    fn prisma_indexes(&mut self, schemas: &[String]) -> Result<Vec<ReadRow>, FrontendErrorKind> {
        let [schema] = schemas else {
            return Err(FrontendErrorKind::Internal);
        };
        let rows = self.read_the_catalog(&format!(
            "SELECT table_name, index_name, column_name, sub_part, seq_in_index, collation, \
             non_unique, index_type FROM information_schema.statistics \
             WHERE table_schema = {}",
            written_name(schema)?
        ))?;
        let mut ordered = rows
            .into_iter()
            .map(|row| {
                let place = whole_number(row.get(4).and_then(Option::as_deref))?;
                Ok(((row[0].clone(), row[1].clone(), place), row))
            })
            .collect::<Result<Vec<_>, FrontendErrorKind>>()?;
        ordered.sort_by(|(left, _), (right, _)| left.cmp(right));
        Ok(ordered.into_iter().map(|(_, row)| row).collect())
    }

    /// Reads the catalog through the text path, which authorizes the read as
    /// it authorizes any.
    fn read_the_catalog(&mut self, sql: &str) -> Result<Vec<ReadRow>, FrontendErrorKind> {
        let CommandExecutionResult::ResultSet(read) = self.execute_query_statement(sql)? else {
            return Err(FrontendErrorKind::Internal);
        };
        Ok(read.rows)
    }
}

/// A database's name written into a catalog read. A name holding a quote or a
/// backslash is refused rather than escaped: no database here has one.
fn written_name(name: &str) -> Result<String, FrontendErrorKind> {
    if name.contains(['\'', '\\']) {
        return Err(FrontendErrorKind::Unsupported);
    }
    Ok(format!("'{name}'"))
}

fn fields<const N: usize>(row: ReadRow) -> Result<[Option<Vec<u8>>; N], FrontendErrorKind> {
    <[Option<Vec<u8>>; N]>::try_from(row).map_err(|_| FrontendErrorKind::Internal)
}

fn whole_number(value: Option<&[u8]>) -> Result<i64, FrontendErrorKind> {
    value
        .and_then(|value| std::str::from_utf8(value).ok())
        .and_then(|value| value.parse().ok())
        .ok_or(FrontendErrorKind::Internal)
}

/// One row as the binary protocol sends it, each value in the form its
/// column's type crosses in.
fn binary_row(
    columns: &[ColumnDefinitionConfig],
    row: ReadRow,
) -> Result<Vec<BinaryResultValue>, FrontendErrorKind> {
    if row.len() != columns.len() {
        return Err(FrontendErrorKind::Internal);
    }
    columns
        .iter()
        .zip(row)
        .map(|(column, value)| {
            let Some(value) = value else {
                return Ok(BinaryResultValue::Null);
            };
            match column.column_type {
                MYSQL_TYPE_VAR_STRING | MYSQL_TYPE_STRING => String::from_utf8(value)
                    .map(BinaryResultValue::Text)
                    .map_err(|_| FrontendErrorKind::Internal),
                MYSQL_TYPE_BLOB => Ok(BinaryResultValue::Blob(value)),
                MYSQL_TYPE_LONGLONG if column.flags & MYSQL_UNSIGNED_FLAG != 0 => {
                    u64::try_from(whole_number(Some(&value))?)
                        .map(BinaryResultValue::UnsignedInteger)
                        .map_err(|_| FrontendErrorKind::Internal)
                }
                MYSQL_TYPE_LONG | MYSQL_TYPE_LONGLONG => {
                    whole_number(Some(&value)).map(BinaryResultValue::Integer)
                }
                _ => Err(FrontendErrorKind::Internal),
            }
        })
        .collect()
}

/// The columns one of Prisma's catalog reads answers, measured on MySQL
/// 8.4.11 through `mysql_async` over the binary protocol.
pub(super) fn prisma_catalog_columns(
    query: PrismaInformationSchemaQuery,
) -> Vec<ColumnDefinitionConfig> {
    const NAME: u32 = 256;
    let key = MYSQL_NOT_NULL_FLAG | MYSQL_BINARY_FLAG | MYSQL_NO_DEFAULT_VALUE_FLAG;
    let blob = MYSQL_BLOB_FLAG | MYSQL_BINARY_FLAG;
    let words = |name: &str, table: &str, original_table: &str, length: u32, flags: u16| {
        read_column(
            name,
            MYSQL_TYPE_VAR_STRING,
            table,
            original_table,
            length,
            flags,
        )
    };
    let number = |name: &str, table: &str, original_table: &str, kind: u8, length: u32, flags| {
        let mut column = read_column(name, kind, table, original_table, length, flags);
        column.character_set = MYSQL_BINARY_COLLATION;
        column
    };
    match query {
        PrismaInformationSchemaQuery::MigrationTableNames => vec![bytes_of_the_name()],
        PrismaInformationSchemaQuery::TableNames => vec![
            bytes_of_the_name(),
            words("create_options", "table_info", "", 1024, 0),
            read_column(
                "table_comment",
                MYSQL_TYPE_BLOB,
                "table_info",
                "",
                24_576,
                MYSQL_BLOB_FLAG,
            ),
        ],
        PrismaInformationSchemaQuery::CheckConstraints => vec![
            words("namespace", "tc", "schemata", NAME, key),
            words("table_name", "tc", "tables", NAME, key),
            words("constraint_name", "tc", "", NAME, 0),
            worked_out(
                "constraint_type",
                MYSQL_TYPE_VAR_STRING,
                44,
                MYSQL_BINARY_FLAG,
            ),
            read_column(
                "constraint_definition",
                MYSQL_TYPE_BLOB,
                "cc",
                "check_constraints",
                u32::MAX,
                blob | MYSQL_NO_DEFAULT_VALUE_FLAG,
            ),
        ],
        PrismaInformationSchemaQuery::Columns => vec![
            words("column_name", "columns", "", NAME, 0),
            read_column(
                "data_type",
                MYSQL_TYPE_BLOB,
                "columns",
                "",
                201_326_580,
                blob,
            ),
            read_column(
                "full_data_type",
                MYSQL_TYPE_BLOB,
                "columns",
                "columns",
                67_108_860,
                blob | MYSQL_NOT_NULL_FLAG | MYSQL_NO_DEFAULT_VALUE_FLAG,
            ),
            number(
                "character_maximum_length",
                "columns",
                "",
                MYSQL_TYPE_LONGLONG,
                21,
                MYSQL_NUM_FLAG,
            ),
            number(
                "numeric_precision",
                "columns",
                "",
                MYSQL_TYPE_LONGLONG,
                10,
                MYSQL_UNSIGNED_FLAG | MYSQL_NUM_FLAG,
            ),
            number(
                "numeric_scale",
                "columns",
                "",
                MYSQL_TYPE_LONGLONG,
                10,
                MYSQL_UNSIGNED_FLAG | MYSQL_NUM_FLAG,
            ),
            number(
                "datetime_precision",
                "columns",
                "columns",
                MYSQL_TYPE_LONG,
                10,
                MYSQL_UNSIGNED_FLAG | MYSQL_NUM_FLAG,
            ),
            read_column(
                "column_default",
                MYSQL_TYPE_BLOB,
                "columns",
                "columns",
                262_140,
                blob,
            ),
            words("is_nullable", "columns", "", 12, MYSQL_NOT_NULL_FLAG),
            words("extra", "columns", "", 1024, 0),
            words("table_name", "columns", "tables", NAME, key),
            worked_out("column_comment", MYSQL_TYPE_BLOB, 24_576, blob),
        ],
        PrismaInformationSchemaQuery::ForeignKeys => vec![
            words("constraint_name", "kcu", "", NAME, 0),
            words("column_name", "kcu", "", NAME, 0),
            words("referenced_table_name", "kcu", "", NAME, MYSQL_BINARY_FLAG),
            words("referenced_column_name", "kcu", "", NAME, 0),
            number(
                "ordinal_position",
                "kcu",
                "",
                MYSQL_TYPE_LONG,
                10,
                MYSQL_NOT_NULL_FLAG | MYSQL_UNSIGNED_FLAG | MYSQL_NUM_FLAG,
            ),
            words("table_name", "kcu", "tables", NAME, key),
            read_column(
                "delete_rule",
                MYSQL_TYPE_STRING,
                "rc",
                "foreign_keys",
                44,
                key | MYSQL_ENUM_FLAG,
            ),
            read_column(
                "update_rule",
                MYSQL_TYPE_STRING,
                "rc",
                "foreign_keys",
                44,
                key | MYSQL_ENUM_FLAG,
            ),
        ],
        PrismaInformationSchemaQuery::Indexes => vec![
            words("table_name", "statistics", "tables", NAME, key),
            words("index_name", "statistics", "", NAME, 0),
            words("column_name", "statistics", "", NAME, 0),
            number(
                "partial",
                "statistics",
                "",
                MYSQL_TYPE_LONGLONG,
                21,
                MYSQL_NUM_FLAG,
            ),
            number(
                "seq_in_index",
                "statistics",
                "index_column_usage",
                MYSQL_TYPE_LONG,
                10,
                MYSQL_NOT_NULL_FLAG
                    | MYSQL_UNSIGNED_FLAG
                    | MYSQL_NO_DEFAULT_VALUE_FLAG
                    | MYSQL_NUM_FLAG,
            ),
            words("column_order", "statistics", "", 4, 0),
            number(
                "non_unique",
                "statistics",
                "",
                MYSQL_TYPE_LONG,
                2,
                MYSQL_NOT_NULL_FLAG | MYSQL_NUM_FLAG,
            ),
            words(
                "index_type",
                "statistics",
                "",
                44,
                MYSQL_NOT_NULL_FLAG | MYSQL_BINARY_FLAG,
            ),
        ],
    }
}

/// `BINARY table_info.table_name`: the name's bytes, in the binary character
/// set, read from no table.
fn bytes_of_the_name() -> ColumnDefinitionConfig {
    let mut column = worked_out("table_name", MYSQL_TYPE_VAR_STRING, 192, MYSQL_BINARY_FLAG);
    column.character_set = MYSQL_BINARY_COLLATION;
    column
}

/// A column read out of a catalog table under an alias: the alias is both its
/// name and its origin, and the table is the alias the statement gave it.
fn read_column(
    name: &str,
    column_type: u8,
    table: &str,
    original_table: &str,
    length: u32,
    flags: u16,
) -> ColumnDefinitionConfig {
    let mut column = ColumnDefinitionConfig::new(name, column_type);
    column.schema = "information_schema".into();
    column.table = table.into();
    column.original_table = original_table.into();
    column.original_name = name.into();
    column.character_set = u16::from(DEFAULT_UTF8MB4_COLLATION);
    column.column_length = length;
    column.flags = flags;
    column
}

/// A column worked out of a call rather than read from a table.
fn worked_out(name: &str, column_type: u8, length: u32, flags: u16) -> ColumnDefinitionConfig {
    let mut column = ColumnDefinitionConfig::new(name, column_type);
    column.original_name = name.into();
    column.character_set = u16::from(DEFAULT_UTF8MB4_COLLATION);
    column.column_length = length;
    column.flags = flags;
    column
}
