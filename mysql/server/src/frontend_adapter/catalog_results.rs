//! Turning a catalog question's answer into a result set.
//!
//! `SHOW TABLES`, `SHOW COLUMNS`, `SHOW INDEX`, `SHOW CREATE TABLE`, the
//! `information_schema` queries and the administrative statements all end here.
//! None of them read a user table: each one describes the schema, so each one
//! builds its columns by hand rather than from what the engine reports.

use super::*;

pub(super) fn admin_result_to_execution_result(
    result: MySqlAdminCommandResult,
) -> Result<CommandExecutionResult, FrontendErrorKind> {
    match result {
        MySqlAdminCommandResult::Created { .. }
        | MySqlAdminCommandResult::Dropped { .. }
        | MySqlAdminCommandResult::Selected { .. } => {
            Ok(CommandExecutionResult::Ok(CommandOkResult::default()))
        }
        MySqlAdminCommandResult::Listed { databases } => {
            if databases.len() > MAX_DISPATCH_RESULT_ROWS {
                return Err(FrontendErrorKind::Internal);
            }
            Ok(CommandExecutionResult::ResultSet(TextResultSet {
                columns: vec![database_list_column()],
                rows: databases
                    .into_iter()
                    .map(|database| Some(database.into_bytes()))
                    .map(|value| vec![value])
                    .collect(),
                warnings: 0,
                status_flags: 0x0002,
            }))
        }
    }
}

pub(super) fn information_schema_schemata_result_to_execution_result(
    databases: Vec<String>,
) -> Result<CommandExecutionResult, FrontendErrorKind> {
    let result = admin_result_to_execution_result(MySqlAdminCommandResult::Listed { databases })?;
    let CommandExecutionResult::ResultSet(mut result) = result else {
        unreachable!("SCHEMATA provider always returns a result set");
    };
    result.columns = vec![information_schema_schemata_column()];
    Ok(CommandExecutionResult::ResultSet(result))
}

pub(super) fn information_schema_schemata_column() -> ColumnDefinitionConfig {
    let mut column = ColumnDefinitionConfig::new("SCHEMA_NAME", MYSQL_TYPE_VAR_STRING);
    "information_schema".clone_into(&mut column.schema);
    "SCHEMATA".clone_into(&mut column.table);
    "SCHEMATA".clone_into(&mut column.original_table);
    "SCHEMA_NAME".clone_into(&mut column.original_name);
    column.character_set = u16::from(DEFAULT_UTF8MB4_COLLATION);
    column.column_length = 256;
    column.flags =
        MYSQL_NOT_NULL_FLAG | MYSQL_BINARY_FLAG | MYSQL_NO_DEFAULT_VALUE_FLAG | MYSQL_PART_KEY_FLAG;
    column
}

pub(super) fn database_list_column() -> ColumnDefinitionConfig {
    let mut column = ColumnDefinitionConfig::new("Database", MYSQL_TYPE_VAR_STRING);
    column.character_set = u16::from(DEFAULT_UTF8MB4_COLLATION);
    column.column_length = 64;
    column
}

pub(super) fn show_tables_result_to_execution_result(
    database: &str,
    tables: impl IntoIterator<Item = String>,
    status_flags: u16,
) -> Result<CommandExecutionResult, FrontendErrorKind> {
    let tables = tables.into_iter().collect::<Vec<_>>();
    if tables.len() > MAX_DISPATCH_RESULT_ROWS {
        return Err(FrontendErrorKind::Internal);
    }

    let mut retained_bytes = 0usize;
    let rows = tables
        .into_iter()
        .map(|name| {
            if name.len() > MAX_TEXT_ROW_VALUE_LENGTH {
                return Err(FrontendErrorKind::Internal);
            }
            retained_bytes = retained_bytes
                .checked_add(name.len())
                .and_then(|total| {
                    total.checked_add(
                        std::mem::size_of::<Vec<Option<Vec<u8>>>>()
                            + std::mem::size_of::<Option<Vec<u8>>>(),
                    )
                })
                .ok_or(FrontendErrorKind::Internal)?;
            if retained_bytes > MAX_FRONTEND_ADAPTER_RESULT_BYTES {
                return Err(FrontendErrorKind::Internal);
            }
            Ok(vec![Some(name.into_bytes())])
        })
        .collect::<Result<Vec<_>, _>>()?;

    Ok(CommandExecutionResult::ResultSet(TextResultSet {
        columns: vec![show_tables_column(database)],
        rows,
        warnings: 0,
        status_flags,
    }))
}

pub(super) fn show_full_tables_result_to_execution_result(
    database: &str,
    tables: impl IntoIterator<Item = turso_mysql::MySqlTable>,
    status_flags: u16,
) -> Result<CommandExecutionResult, FrontendErrorKind> {
    let CommandExecutionResult::ResultSet(mut result) =
        information_schema_tables_result_to_execution_result(database, tables, status_flags)?
    else {
        unreachable!("catalog provider always returns a result set");
    };
    for row in &mut result.rows {
        row.remove(0);
    }
    let mut name = show_tables_column(database);
    name.flags = MYSQL_NOT_NULL_FLAG | MYSQL_BINARY_FLAG | MYSQL_NO_DEFAULT_VALUE_FLAG;
    let mut kind = ColumnDefinitionConfig::new("Table_type", MYSQL_TYPE_STRING);
    kind.character_set = u16::from(DEFAULT_UTF8MB4_COLLATION);
    kind.column_length = 44;
    kind.flags =
        MYSQL_NOT_NULL_FLAG | MYSQL_BINARY_FLAG | MYSQL_ENUM_FLAG | MYSQL_NO_DEFAULT_VALUE_FLAG;
    for column in [&mut name, &mut kind] {
        column.catalog = "def".into();
        column.table = "TABLES".into();
        column.original_table = "tables".into();
        column.original_name = column.name.clone();
    }
    result.columns = vec![name, kind];
    Ok(CommandExecutionResult::ResultSet(result))
}

pub(super) fn information_schema_tables_result_to_execution_result(
    database: &str,
    tables: impl IntoIterator<Item = turso_mysql::MySqlTable>,
    status_flags: u16,
) -> Result<CommandExecutionResult, FrontendErrorKind> {
    let tables = tables.into_iter().collect::<Vec<_>>();
    if tables.len() > MAX_DISPATCH_RESULT_ROWS {
        return Err(FrontendErrorKind::Internal);
    }

    let mut retained_bytes = 0usize;
    let mut rows = Vec::with_capacity(tables.len());
    for table in tables {
        let table_type = match table.kind() {
            MySqlTableKind::BaseTable => b"BASE TABLE".as_slice(),
            MySqlTableKind::View => b"VIEW".as_slice(),
        };
        let row = vec![
            Some(database.as_bytes().to_vec()),
            Some(table.name().as_bytes().to_vec()),
            Some(table_type.to_vec()),
        ];
        if row
            .iter()
            .flatten()
            .any(|value| value.len() > MAX_TEXT_ROW_VALUE_LENGTH)
        {
            return Err(FrontendErrorKind::Internal);
        }
        checked_text_result_row_payload_len(&row)?;

        let row_bytes = row
            .iter()
            .flatten()
            .map(Vec::len)
            .try_fold(0usize, usize::checked_add)
            .and_then(|bytes| bytes.checked_add(std::mem::size_of::<Vec<Option<Vec<u8>>>>()))
            .and_then(|bytes| {
                std::mem::size_of::<Option<Vec<u8>>>()
                    .checked_mul(row.len())
                    .and_then(|row_storage| bytes.checked_add(row_storage))
            })
            .ok_or(FrontendErrorKind::Internal)?;
        retained_bytes = retained_bytes
            .checked_add(row_bytes)
            .ok_or(FrontendErrorKind::Internal)?;
        if retained_bytes > MAX_FRONTEND_ADAPTER_RESULT_BYTES {
            return Err(FrontendErrorKind::Internal);
        }
        rows.push(row);
    }

    Ok(CommandExecutionResult::ResultSet(TextResultSet {
        columns: information_schema_tables_columns(),
        rows,
        warnings: 0,
        status_flags,
    }))
}

fn information_schema_tables_columns() -> Vec<ColumnDefinitionConfig> {
    // TABLE_SCHEMA's original table really is `schemata` in MySQL. Every value here comes from the
    // pinned MySQL 8.4.11 golden `information-schema-tables.json`.
    [
        (
            "TABLE_SCHEMA",
            "schemata",
            MYSQL_TYPE_VAR_STRING,
            256,
            MYSQL_NOT_NULL_FLAG | MYSQL_BINARY_FLAG | MYSQL_NO_DEFAULT_VALUE_FLAG,
        ),
        (
            "TABLE_NAME",
            "tables",
            MYSQL_TYPE_VAR_STRING,
            256,
            MYSQL_NOT_NULL_FLAG | MYSQL_BINARY_FLAG | MYSQL_NO_DEFAULT_VALUE_FLAG,
        ),
        (
            "TABLE_TYPE",
            "tables",
            MYSQL_TYPE_STRING,
            44,
            MYSQL_NOT_NULL_FLAG | MYSQL_BINARY_FLAG | MYSQL_ENUM_FLAG | MYSQL_NO_DEFAULT_VALUE_FLAG,
        ),
    ]
    .into_iter()
    .map(
        |(name, original_table, column_type, column_length, flags)| {
            let mut column = ColumnDefinitionConfig::new(name, column_type);
            "information_schema".clone_into(&mut column.schema);
            "TABLES".clone_into(&mut column.table);
            original_table.clone_into(&mut column.original_table);
            name.clone_into(&mut column.original_name);
            column.character_set = u16::from(DEFAULT_UTF8MB4_COLLATION);
            column.column_length = column_length;
            column.flags = flags;
            column
        },
    )
    .collect()
}

pub(super) fn information_schema_columns_result_to_execution_result(
    columns: Vec<MySqlColumnMetadata>,
    status_flags: u16,
) -> Result<CommandExecutionResult, FrontendErrorKind> {
    if columns.len() > MAX_DISPATCH_RESULT_ROWS {
        return Err(FrontendErrorKind::Internal);
    }

    let mut retained_bytes = 0usize;
    let mut rows = Vec::with_capacity(columns.len());
    for (ordinal, column) in columns.into_iter().enumerate() {
        if column.name().len() > MAX_TEXT_ROW_VALUE_LENGTH
            || column.extra().len() > MAX_TEXT_ROW_VALUE_LENGTH
        {
            return Err(FrontendErrorKind::Internal);
        }
        let column_type = show_column_type_name(&column)?;
        let extra = show_column_extra(column.extra())?;
        let default = match column.default_value() {
            Some(MySqlColumnDefault::Text(value)) if value.len() > MAX_TEXT_ROW_VALUE_LENGTH => {
                return Err(FrontendErrorKind::Internal);
            }
            _ => show_column_default_value(column.default_value())?,
        };
        let ordinal = (ordinal + 1).to_string().into_bytes();
        let nullable = if column.nullable() {
            b"YES".as_slice()
        } else {
            b"NO".as_slice()
        };
        let key = match column.key() {
            MySqlColumnKey::None => b"".as_slice(),
            MySqlColumnKey::Multiple => b"MUL".as_slice(),
            MySqlColumnKey::Unique => b"UNI".as_slice(),
            MySqlColumnKey::Primary => b"PRI".as_slice(),
        };
        let value_lengths = [
            column.name().len(),
            ordinal.len(),
            default.as_ref().map_or(0, Vec::len),
            nullable.len(),
            column_type.len(),
            key.len(),
            extra.len(),
        ];
        if value_lengths
            .iter()
            .any(|length| *length > MAX_TEXT_ROW_VALUE_LENGTH)
        {
            return Err(FrontendErrorKind::Internal);
        }
        let payload_len = value_lengths
            .iter()
            .try_fold(0usize, |payload_len, length| {
                length_encoded_value_len(*length)
                    .map_err(|_| FrontendErrorKind::Internal)?
                    .checked_add(payload_len)
                    .ok_or(FrontendErrorKind::Internal)
            })?;
        if payload_len > MAX_RESPONSE_PACKET_PAYLOAD_LENGTH {
            return Err(FrontendErrorKind::Internal);
        }
        let row_bytes = value_lengths
            .iter()
            .try_fold(0usize, |row_bytes, length| {
                row_bytes
                    .checked_add(*length)
                    .ok_or(FrontendErrorKind::Internal)
            })?
            .checked_add(std::mem::size_of::<Vec<Option<Vec<u8>>>>())
            .and_then(|bytes| {
                std::mem::size_of::<Option<Vec<u8>>>()
                    .checked_mul(value_lengths.len())
                    .and_then(|row_storage| bytes.checked_add(row_storage))
            })
            .ok_or(FrontendErrorKind::Internal)?;
        retained_bytes = retained_bytes
            .checked_add(row_bytes)
            .ok_or(FrontendErrorKind::Internal)?;
        if retained_bytes > MAX_FRONTEND_ADAPTER_RESULT_BYTES {
            return Err(FrontendErrorKind::Internal);
        }

        rows.push(vec![
            Some(column.name().as_bytes().to_vec()),
            Some(ordinal),
            default,
            Some(nullable.to_vec()),
            Some(column_type.to_vec()),
            Some(key.to_vec()),
            Some(extra.to_vec()),
        ]);
    }

    Ok(CommandExecutionResult::ResultSet(TextResultSet {
        columns: information_schema_columns_columns(),
        rows,
        warnings: 0,
        status_flags,
    }))
}

pub(super) fn information_schema_columns_columns() -> Vec<ColumnDefinitionConfig> {
    let column_name = information_schema_column_definition(
        "COLUMN_NAME",
        MYSQL_TYPE_VAR_STRING,
        256,
        DEFAULT_UTF8MB4_COLLATION.into(),
        false,
    );

    let mut ordinal_position = information_schema_column_definition(
        "ORDINAL_POSITION",
        MYSQL_TYPE_LONG,
        10,
        MYSQL_BINARY_COLLATION,
        true,
    );
    ordinal_position.flags =
        MYSQL_NOT_NULL_FLAG | MYSQL_UNSIGNED_FLAG | MYSQL_NO_DEFAULT_VALUE_FLAG;

    let mut column_default = information_schema_column_definition(
        "COLUMN_DEFAULT",
        MYSQL_TYPE_BLOB,
        262_140,
        DEFAULT_UTF8MB4_COLLATION.into(),
        true,
    );
    column_default.flags = MYSQL_BLOB_FLAG | MYSQL_BINARY_FLAG;

    let mut is_nullable = information_schema_column_definition(
        "IS_NULLABLE",
        MYSQL_TYPE_VAR_STRING,
        12,
        DEFAULT_UTF8MB4_COLLATION.into(),
        false,
    );
    is_nullable.flags = MYSQL_NOT_NULL_FLAG;

    let mut column_type = information_schema_column_definition(
        "COLUMN_TYPE",
        MYSQL_TYPE_BLOB,
        67_108_860,
        DEFAULT_UTF8MB4_COLLATION.into(),
        true,
    );
    column_type.flags =
        MYSQL_NOT_NULL_FLAG | MYSQL_BLOB_FLAG | MYSQL_BINARY_FLAG | MYSQL_NO_DEFAULT_VALUE_FLAG;

    let mut column_key = information_schema_column_definition(
        "COLUMN_KEY",
        MYSQL_TYPE_STRING,
        12,
        DEFAULT_UTF8MB4_COLLATION.into(),
        true,
    );
    column_key.flags =
        MYSQL_NOT_NULL_FLAG | MYSQL_BINARY_FLAG | MYSQL_ENUM_FLAG | MYSQL_NO_DEFAULT_VALUE_FLAG;

    let extra = information_schema_column_definition(
        "EXTRA",
        MYSQL_TYPE_VAR_STRING,
        1024,
        DEFAULT_UTF8MB4_COLLATION.into(),
        false,
    );

    vec![
        column_name,
        ordinal_position,
        column_default,
        is_nullable,
        column_type,
        column_key,
        extra,
    ]
}

fn information_schema_column_definition(
    name: &str,
    column_type: u8,
    column_length: u32,
    character_set: u16,
    has_original_table: bool,
) -> ColumnDefinitionConfig {
    let mut column = ColumnDefinitionConfig::new(name, column_type);
    "def".clone_into(&mut column.catalog);
    "information_schema".clone_into(&mut column.schema);
    "COLUMNS".clone_into(&mut column.table);
    if has_original_table {
        "columns".clone_into(&mut column.original_table);
    }
    name.clone_into(&mut column.original_name);
    column.character_set = character_set;
    column.column_length = column_length;
    column
}

/// Refuses a `database.` qualifier that names anything but the selected
/// database.
///
/// MySQL resolves such a qualifier against any database the caller can reach,
/// which means authorizing against the named one rather than the selected one.
/// Until that is built, only the redundant qualifier clients write right after
/// `USE` is taken.
pub(super) fn reject_other_database_qualifier(
    qualifier: Option<&MySqlDatabaseName>,
    selected_database: &str,
) -> Result<(), FrontendErrorKind> {
    match qualifier {
        None => Ok(()),
        Some(qualifier) if qualifier.as_str().eq_ignore_ascii_case(selected_database) => Ok(()),
        Some(_) => Err(FrontendErrorKind::Unsupported),
    }
}

/// The fifteen columns `SHOW INDEX` returns, in MySQL's order.
///
/// Cardinality is always NULL: it is a statistic MySQL gathers and Turso does
/// not, and MySQL itself sends NULL when it has none.
pub(super) fn show_index_result_to_execution_result(
    table: &str,
    entries: Vec<MySqlIndexEntry>,
    status_flags: u16,
) -> Result<CommandExecutionResult, FrontendErrorKind> {
    if entries.len() > MAX_DISPATCH_RESULT_ROWS {
        return Err(FrontendErrorKind::Internal);
    }
    let mut rows = Vec::with_capacity(entries.len());
    for entry in entries {
        if entry.key_name().len() > MAX_TEXT_ROW_VALUE_LENGTH
            || entry.column_name().len() > MAX_TEXT_ROW_VALUE_LENGTH
        {
            return Err(FrontendErrorKind::Internal);
        }
        rows.push(vec![
            Some(table.as_bytes().to_vec()),
            Some(if entry.unique() {
                b"0".to_vec()
            } else {
                b"1".to_vec()
            }),
            Some(entry.key_name().as_bytes().to_vec()),
            Some(entry.sequence_in_index().to_string().into_bytes()),
            Some(entry.column_name().as_bytes().to_vec()),
            Some(b"A".to_vec()),
            None,
            None,
            None,
            Some(if entry.nullable() {
                b"YES".to_vec()
            } else {
                Vec::new()
            }),
            Some(b"BTREE".to_vec()),
            Some(Vec::new()),
            Some(Vec::new()),
            Some(b"YES".to_vec()),
            None,
        ]);
    }
    Ok(CommandExecutionResult::ResultSet(TextResultSet {
        columns: show_index_columns(),
        rows,
        warnings: 0,
        status_flags,
    }))
}

fn show_index_columns() -> Vec<ColumnDefinitionConfig> {
    [
        ("Table", MYSQL_TYPE_VAR_STRING, 256u32),
        ("Non_unique", MYSQL_TYPE_LONGLONG, 1),
        ("Key_name", MYSQL_TYPE_VAR_STRING, 256),
        ("Seq_in_index", MYSQL_TYPE_LONGLONG, 21),
        ("Column_name", MYSQL_TYPE_VAR_STRING, 256),
        ("Collation", MYSQL_TYPE_VAR_STRING, 4),
        ("Cardinality", MYSQL_TYPE_LONGLONG, 21),
        ("Sub_part", MYSQL_TYPE_LONGLONG, 21),
        ("Packed", MYSQL_TYPE_VAR_STRING, 40),
        ("Null", MYSQL_TYPE_VAR_STRING, 12),
        ("Index_type", MYSQL_TYPE_VAR_STRING, 44),
        ("Comment", MYSQL_TYPE_VAR_STRING, 32),
        ("Index_comment", MYSQL_TYPE_VAR_STRING, 1024),
        ("Visible", MYSQL_TYPE_VAR_STRING, 12),
        ("Expression", MYSQL_TYPE_BLOB, abs_expression_length()),
    ]
    .into_iter()
    .map(|(name, column_type, column_length)| {
        let mut column = ColumnDefinitionConfig::new(name, column_type);
        column.character_set = if column_type == MYSQL_TYPE_LONGLONG {
            MYSQL_BINARY_COLLATION
        } else {
            u16::from(DEFAULT_UTF8MB4_COLLATION)
        };
        column.column_length = column_length;
        column
    })
    .collect()
}

const fn abs_expression_length() -> u32 {
    MAX_TEXT_ROW_VALUE_LENGTH as u32
}

pub(super) fn show_create_table_error_kind(error: MySqlShowCreateTableError) -> FrontendErrorKind {
    match error {
        MySqlShowCreateTableError::MissingTable => FrontendErrorKind::MissingObject,
        MySqlShowCreateTableError::NotTable => FrontendErrorKind::NotView,
        MySqlShowCreateTableError::Unsupported => FrontendErrorKind::Unsupported,
        MySqlShowCreateTableError::Engine(error) => frontend_error_kind(error),
    }
}

pub(super) fn show_create_table_result_to_execution_result(
    result: MySqlShowCreateTableResult,
    status_flags: u16,
) -> Result<CommandExecutionResult, FrontendErrorKind> {
    if result.table().len() > MAX_TEXT_ROW_VALUE_LENGTH
        || result.create_statement().len() > MAX_TEXT_ROW_VALUE_LENGTH
    {
        return Err(FrontendErrorKind::Internal);
    }
    let rows = vec![vec![
        Some(result.table().as_bytes().to_vec()),
        Some(result.create_statement().as_bytes().to_vec()),
    ]];
    Ok(CommandExecutionResult::ResultSet(TextResultSet {
        columns: show_create_table_columns(result.create_statement().len()),
        rows,
        warnings: 0,
        status_flags,
    }))
}

/// MySQL sizes the `Create Table` column from the statement it is about:
/// `max(1024, byte length) * 4`, the 4 being utf8mb4's widest character.
fn show_create_table_columns(statement_length: usize) -> Vec<ColumnDefinitionConfig> {
    let statement_width = u32::try_from(statement_length.max(1024) * 4).unwrap_or(u32::MAX);
    [("Table", 256u32), ("Create Table", statement_width)]
        .into_iter()
        .map(|(name, column_length)| {
            let mut column = ColumnDefinitionConfig::new(name, MYSQL_TYPE_VAR_STRING);
            column.character_set = u16::from(DEFAULT_UTF8MB4_COLLATION);
            column.column_length = column_length;
            column.decimals = 31;
            column.flags = MYSQL_NOT_NULL_FLAG;
            column
        })
        .collect()
}

pub(super) fn show_columns_result_to_execution_result(
    columns: Vec<MySqlColumnMetadata>,
    status_flags: u16,
) -> Result<CommandExecutionResult, FrontendErrorKind> {
    if columns.len() > MAX_DISPATCH_RESULT_ROWS {
        return Err(FrontendErrorKind::Internal);
    }

    let mut retained_bytes = 0usize;
    let mut rows = Vec::with_capacity(columns.len());
    for column in columns {
        if column.name().len() > MAX_TEXT_ROW_VALUE_LENGTH
            || column.extra().len() > MAX_TEXT_ROW_VALUE_LENGTH
        {
            return Err(FrontendErrorKind::Internal);
        }
        let row = vec![
            Some(column.name().as_bytes().to_vec()),
            Some(show_column_type_name(&column)?),
            Some(if column.nullable() {
                b"YES".to_vec()
            } else {
                b"NO".to_vec()
            }),
            Some(match column.key() {
                MySqlColumnKey::None => Vec::new(),
                MySqlColumnKey::Multiple => b"MUL".to_vec(),
                MySqlColumnKey::Unique => b"UNI".to_vec(),
                MySqlColumnKey::Primary => b"PRI".to_vec(),
            }),
            show_column_default_value(column.default_value())?,
            Some(show_column_extra(column.extra())?.to_vec()),
        ];
        checked_text_result_row_payload_len(&row)?;

        let row_bytes = row
            .iter()
            .flatten()
            .map(Vec::len)
            .try_fold(0usize, usize::checked_add)
            .and_then(|bytes| bytes.checked_add(std::mem::size_of::<Vec<Option<Vec<u8>>>>()))
            .and_then(|bytes| {
                std::mem::size_of::<Option<Vec<u8>>>()
                    .checked_mul(row.len())
                    .and_then(|row_storage| bytes.checked_add(row_storage))
            })
            .ok_or(FrontendErrorKind::Internal)?;
        retained_bytes = retained_bytes
            .checked_add(row_bytes)
            .ok_or(FrontendErrorKind::Internal)?;
        if retained_bytes > MAX_FRONTEND_ADAPTER_RESULT_BYTES {
            return Err(FrontendErrorKind::Internal);
        }
        rows.push(row);
    }

    Ok(CommandExecutionResult::ResultSet(TextResultSet {
        columns: show_columns_columns(),
        rows,
        warnings: 0,
        status_flags,
    }))
}

pub(super) fn show_columns_columns() -> Vec<ColumnDefinitionConfig> {
    [
        ("Field", 64),
        ("Type", MAX_TEXT_ROW_VALUE_LENGTH as u32),
        ("Null", 3),
        ("Key", 3),
        ("Default", MAX_TEXT_ROW_VALUE_LENGTH as u32),
        ("Extra", 40),
    ]
    .into_iter()
    .map(|(name, column_length)| {
        let mut column = ColumnDefinitionConfig::new(name, MYSQL_TYPE_VAR_STRING);
        column.character_set = u16::from(DEFAULT_UTF8MB4_COLLATION);
        column.column_length = column_length;
        column
    })
    .collect()
}

fn checked_text_result_row_payload_len(
    values: &[Option<Vec<u8>>],
) -> Result<usize, FrontendErrorKind> {
    let payload_len = values.iter().try_fold(0usize, |payload_len, value| {
        let value_len = match value {
            None => 1,
            Some(bytes) => {
                length_encoded_value_len(bytes.len()).map_err(|_| FrontendErrorKind::Internal)?
            }
        };
        payload_len
            .checked_add(value_len)
            .ok_or(FrontendErrorKind::Internal)
    })?;
    if payload_len > MAX_RESPONSE_PACKET_PAYLOAD_LENGTH {
        return Err(FrontendErrorKind::Internal);
    }
    Ok(payload_len)
}

/// Renders the type the way MySQL 8.4.11 reports it here, lower case and
/// carrying the declared length where the type has one.
fn show_column_type_name(column: &MySqlColumnMetadata) -> Result<Vec<u8>, FrontendErrorKind> {
    if let Some((precision, scale)) = column.decimal_size() {
        return match column.type_name() {
            "DECIMAL" => Ok(format!("decimal({precision},{scale})").into_bytes()),
            _ => Err(FrontendErrorKind::Internal),
        };
    }
    if let Some(length) = column.character_length() {
        return match column.type_name() {
            "VARCHAR" => Ok(format!("varchar({length})").into_bytes()),
            "CHAR" => Ok(format!("char({length})").into_bytes()),
            _ => Err(FrontendErrorKind::Internal),
        };
    }
    let name: &[u8] = match column.type_name() {
        "TINYINT" => b"tinyint",
        "SMALLINT" => b"smallint",
        "MEDIUMINT" => b"mediumint",
        "INT" | "INTEGER" => b"int",
        "BIGINT" => b"bigint",
        "TEXT" => b"text",
        "BLOB" => b"blob",
        "DOUBLE" => b"double",
        "FLOAT" => b"float",
        "BOOLEAN" => b"tinyint(1)",
        "DATETIME" => b"datetime",
        "TIMESTAMP" => b"timestamp",
        _ => return Err(FrontendErrorKind::Internal),
    };
    Ok(name.to_vec())
}

pub(super) fn show_column_extra(extra: &str) -> Result<&'static [u8], FrontendErrorKind> {
    match extra {
        "" => Ok(b""),
        "AUTO_INCREMENT" => Ok(b"auto_increment"),
        _ => Err(FrontendErrorKind::Internal),
    }
}

pub(super) fn show_column_default_value(
    default_value: Option<&MySqlColumnDefault>,
) -> Result<Option<Vec<u8>>, FrontendErrorKind> {
    let Some(default_value) = default_value else {
        return Ok(None);
    };
    let value = match default_value {
        MySqlColumnDefault::Null => return Ok(None),
        MySqlColumnDefault::Integer { value, .. } => {
            let value = value.to_string();
            if value.len() > MAX_TEXT_ROW_VALUE_LENGTH {
                return Err(FrontendErrorKind::Internal);
            }
            return Ok(Some(value.into_bytes()));
        }
        MySqlColumnDefault::Text(text) => text.as_bytes(),
        MySqlColumnDefault::Boolean(value) => {
            return Ok(Some(if *value { b"1".to_vec() } else { b"0".to_vec() }));
        }
    };
    if value.len() > MAX_TEXT_ROW_VALUE_LENGTH {
        return Err(FrontendErrorKind::Internal);
    }
    Ok(Some(value.to_vec()))
}

pub(super) fn show_tables_column(database: &str) -> ColumnDefinitionConfig {
    let mut column =
        ColumnDefinitionConfig::new(format!("Tables_in_{database}"), MYSQL_TYPE_VAR_STRING);
    column.character_set = u16::from(DEFAULT_UTF8MB4_COLLATION);
    column.column_length = 256;
    column
}
