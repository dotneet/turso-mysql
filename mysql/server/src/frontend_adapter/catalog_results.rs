//! Turning a catalog question's answer into a result set.
//!
//! `SHOW TABLES`, `SHOW COLUMNS`, `SHOW INDEX`, `SHOW CREATE TABLE`, the
//! `information_schema` queries and the administrative statements all end here.
//! None of them read a user table: each one describes the schema, so each one
//! builds its columns by hand rather than from what the engine reports.

use super::*;

/// The longest value a catalog listing sends. Every value in one describes the
/// schema, so each row is kept to one small packet.
pub(super) const MAX_CATALOG_VALUE_LENGTH: usize = crate::MAX_RESPONSE_PACKET_PAYLOAD_LENGTH;

pub(super) fn admin_result_to_execution_result(
    result: MySqlAdminCommandResult,
) -> Result<CommandExecutionResult, FrontendErrorKind> {
    match result {
        MySqlAdminCommandResult::Created { .. }
        | MySqlAdminCommandResult::AlreadyExists { .. }
        | MySqlAdminCommandResult::Altered { .. }
        | MySqlAdminCommandResult::Dropped { .. }
        | MySqlAdminCommandResult::AlreadyGone { .. }
        | MySqlAdminCommandResult::Selected { .. } => {
            Ok(CommandExecutionResult::Ok(CommandOkResult::default()))
        }
        MySqlAdminCommandResult::CreateStatement {
            database,
            statement,
        } => Ok(CommandExecutionResult::ResultSet(TextResultSet {
            columns: show_create_database_columns(),
            rows: vec![vec![
                Some(database.into_bytes()),
                Some(statement.into_bytes()),
            ]],
            warnings: 0,
            status_flags: 0x0002,
        })),
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

/// Measured on MySQL 8.4.11: both columns are `VAR_STRING` with 31 decimals
/// and only the not-null flag, and `Create Database` is 4096 long whatever the
/// statement's length.
fn show_create_database_columns() -> Vec<ColumnDefinitionConfig> {
    [("Database", 256u32), ("Create Database", 4096)]
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

pub(super) fn gorm_current_database_result(
    database: &str,
    status_flags: u16,
) -> PreparedStatementExecutionResult {
    PreparedStatementExecutionResult::ResultSet(BinaryResultSet {
        columns: vec![information_schema_schemata_column()],
        rows: vec![vec![BinaryResultValue::Text(database.to_owned())]],
        warnings: 0,
        status_flags,
    })
}

pub(super) fn gorm_catalog_count_result(
    count: i64,
    status_flags: u16,
) -> PreparedStatementExecutionResult {
    PreparedStatementExecutionResult::ResultSet(BinaryResultSet {
        columns: vec![gorm_catalog_count_column()],
        rows: vec![vec![BinaryResultValue::Integer(count)]],
        warnings: 0,
        status_flags,
    })
}

pub(super) fn gorm_catalog_count_column() -> ColumnDefinitionConfig {
    let mut column = column_definition("count(*)".to_owned(), MYSQL_TYPE_LONGLONG);
    column.column_length = 21;
    column.flags = MYSQL_NOT_NULL_FLAG | MYSQL_BINARY_FLAG | MYSQL_NUM_FLAG;
    column
}

pub(super) fn gorm_columns_definitions() -> Vec<ColumnDefinitionConfig> {
    use MySqlInformationSchemaColumnsColumn as Column;
    let mut columns = information_schema_columns_columns(&[
        Column::ColumnName,
        Column::ColumnDefault,
        Column::DataType,
        Column::CharacterMaximumLength,
        Column::ColumnType,
        Column::ColumnKey,
        Column::Extra,
        Column::ColumnComment,
        Column::NumericPrecision,
        Column::NumericScale,
    ]);
    let mut nullable = ColumnDefinitionConfig::new("is_nullable = 'YES'", MYSQL_TYPE_LONG);
    nullable.character_set = MYSQL_BINARY_COLLATION;
    nullable.column_length = 1;
    nullable.flags = MYSQL_NOT_NULL_FLAG | MYSQL_NUM_FLAG;
    columns.insert(2, nullable);
    let mut datetime_precision = information_schema_column_definition(
        "DATETIME_PRECISION",
        MYSQL_TYPE_LONG,
        10,
        MYSQL_BINARY_COLLATION,
        true,
    );
    datetime_precision.flags = MYSQL_UNSIGNED_FLAG | MYSQL_NUM_FLAG;
    columns.push(datetime_precision);
    columns
}

pub(super) fn gorm_columns_result(
    columns: Vec<MySqlColumnMetadata>,
    status_flags: u16,
) -> Result<PreparedStatementExecutionResult, FrontendErrorKind> {
    use MySqlInformationSchemaColumnsColumn as Column;
    let nullable = columns
        .iter()
        .map(MySqlColumnMetadata::nullable)
        .collect::<Vec<_>>();
    let datetime_precision = columns
        .iter()
        .map(|column| column.temporal_precision().map(i64::from))
        .collect::<Vec<_>>();
    let result = information_schema_columns_result_to_execution_result(
        columns,
        &[
            Column::ColumnName,
            Column::ColumnDefault,
            Column::DataType,
            Column::CharacterMaximumLength,
            Column::ColumnType,
            Column::ColumnKey,
            Column::Extra,
            Column::ColumnComment,
            Column::NumericPrecision,
            Column::NumericScale,
        ],
        status_flags,
    )?;
    let CommandExecutionResult::ResultSet(text) = result else {
        unreachable!("the COLUMNS provider always returns a result set");
    };
    let rows = text
        .rows
        .into_iter()
        .zip(nullable)
        .zip(datetime_precision)
        .map(|((row, nullable), datetime_precision)| {
            let mut values = row
                .into_iter()
                .enumerate()
                .map(|(position, value)| match value {
                    None => Ok(BinaryResultValue::Null),
                    Some(value) if matches!(position, 3 | 8 | 9) => {
                        let value = std::str::from_utf8(&value)
                            .map_err(|_| FrontendErrorKind::Internal)?
                            .parse::<i64>()
                            .map_err(|_| FrontendErrorKind::Internal)?;
                        if position == 3 {
                            Ok(BinaryResultValue::Integer(value))
                        } else {
                            Ok(BinaryResultValue::UnsignedInteger(
                                u64::try_from(value).map_err(|_| FrontendErrorKind::Internal)?,
                            ))
                        }
                    }
                    Some(value) if matches!(position, 1 | 2 | 4 | 7) => {
                        Ok(BinaryResultValue::Blob(value))
                    }
                    Some(value) => Ok(BinaryResultValue::Text(
                        String::from_utf8(value).map_err(|_| FrontendErrorKind::Internal)?,
                    )),
                })
                .collect::<Result<Vec<_>, FrontendErrorKind>>()?;
            values.insert(2, BinaryResultValue::Integer(i64::from(nullable)));
            values.push(match datetime_precision {
                Some(precision) => BinaryResultValue::Integer(precision),
                None => BinaryResultValue::Null,
            });
            Ok(values)
        })
        .collect::<Result<Vec<_>, FrontendErrorKind>>()?;
    Ok(PreparedStatementExecutionResult::ResultSet(
        BinaryResultSet {
            columns: gorm_columns_definitions(),
            rows,
            warnings: 0,
            status_flags,
        },
    ))
}

pub(super) fn database_list_column() -> ColumnDefinitionConfig {
    let mut column = ColumnDefinitionConfig::new("Database", MYSQL_TYPE_VAR_STRING);
    column.character_set = u16::from(DEFAULT_UTF8MB4_COLLATION);
    column.column_length = 64;
    column
}

pub(super) fn show_tables_result_to_execution_result(
    database: &str,
    pattern: Option<&str>,
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
            if name.len() > MAX_CATALOG_VALUE_LENGTH {
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
        columns: vec![show_tables_column(database, pattern)],
        rows,
        warnings: 0,
        status_flags,
    }))
}

pub(super) fn show_full_tables_result_to_execution_result(
    database: &str,
    pattern: Option<&str>,
    tables: impl IntoIterator<Item = turso_mysql::MySqlTable>,
    status_flags: u16,
) -> Result<CommandExecutionResult, FrontendErrorKind> {
    // `SHOW FULL TABLES` answers the same three columns and then drops the
    // schema, so it asks for all three in MySQL's own order.
    let CommandExecutionResult::ResultSet(mut result) =
        information_schema_tables_result_to_execution_result(
            database,
            tables,
            &[
                MySqlInformationSchemaTablesColumn::TableSchema,
                MySqlInformationSchemaTablesColumn::TableName,
                MySqlInformationSchemaTablesColumn::TableType,
            ],
            status_flags,
        )?
    else {
        unreachable!("catalog provider always returns a result set");
    };
    for row in &mut result.rows {
        row.remove(0);
    }
    let mut name = show_tables_column(database, pattern);
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
    projected: &[MySqlInformationSchemaTablesColumn],
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
        let base_table = table.kind() == MySqlTableKind::BaseTable;
        // Measured on MySQL 8.4.11: a view has no engine, collation or
        // storage, and its comment is `VIEW`.
        let whole = [
            Some(database.as_bytes().to_vec()),
            Some(table.name().as_bytes().to_vec()),
            Some(table_type.to_vec()),
            base_table.then(|| b"InnoDB".to_vec()),
            None,
            None,
            table
                .collation()
                .map(|collation| collation.name().as_bytes().to_vec()),
            Some(
                table
                    .comment()
                    .map_or(b"VIEW".to_vec(), |comment| comment.as_bytes().to_vec()),
            ),
        ];
        // The row holds what the query named, in the order it named it.
        let row = projected
            .iter()
            .map(|column| whole[information_schema_tables_ordinal(*column)].clone())
            .collect::<Vec<_>>();
        if row
            .iter()
            .flatten()
            .any(|value| value.len() > MAX_CATALOG_VALUE_LENGTH)
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
        columns: information_schema_tables_columns(projected),
        rows,
        warnings: 0,
        status_flags,
    }))
}

pub(super) fn information_schema_tables_columns(
    projected: &[MySqlInformationSchemaTablesColumn],
) -> Vec<ColumnDefinitionConfig> {
    // TABLE_SCHEMA's original table really is `schemata` in MySQL. Every value here comes from the
    // pinned MySQL 8.4.11 golden `information-schema-tables.json`.
    let whole = [
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
        // The rest were measured against the pinned MySQL 8.4.11 oracle.
        ("ENGINE", "", MYSQL_TYPE_VAR_STRING, 256, 0),
        (
            "DATA_LENGTH",
            "",
            MYSQL_TYPE_LONGLONG,
            21,
            MYSQL_UNSIGNED_FLAG | MYSQL_NUM_FLAG,
        ),
        (
            "INDEX_LENGTH",
            "",
            MYSQL_TYPE_LONGLONG,
            21,
            MYSQL_UNSIGNED_FLAG | MYSQL_NUM_FLAG,
        ),
        (
            "TABLE_COLLATION",
            "collations",
            MYSQL_TYPE_VAR_STRING,
            256,
            MYSQL_NO_DEFAULT_VALUE_FLAG,
        ),
        (
            "TABLE_COMMENT",
            "",
            MYSQL_TYPE_BLOB,
            24_576,
            MYSQL_BLOB_FLAG,
        ),
    ];
    projected
        .iter()
        .map(|column| {
            let (name, original_table, column_type, column_length, flags) =
                whole[information_schema_tables_ordinal(*column)];
            let mut column = ColumnDefinitionConfig::new(name, column_type);
            "information_schema".clone_into(&mut column.schema);
            "TABLES".clone_into(&mut column.table);
            original_table.clone_into(&mut column.original_table);
            name.clone_into(&mut column.original_name);
            column.character_set = if column_type == MYSQL_TYPE_LONGLONG {
                MYSQL_BINARY_COLLATION
            } else {
                u16::from(DEFAULT_UTF8MB4_COLLATION)
            };
            column.column_length = column_length;
            column.flags = flags;
            column
        })
        .collect()
}

/// Where one `information_schema.TABLES` column sits among the ones this
/// answers, which is the order MySQL declares them in.
fn information_schema_tables_ordinal(column: MySqlInformationSchemaTablesColumn) -> usize {
    match column {
        MySqlInformationSchemaTablesColumn::TableSchema => 0,
        MySqlInformationSchemaTablesColumn::TableName => 1,
        MySqlInformationSchemaTablesColumn::TableType => 2,
        MySqlInformationSchemaTablesColumn::Engine => 3,
        MySqlInformationSchemaTablesColumn::DataLength => 4,
        MySqlInformationSchemaTablesColumn::IndexLength => 5,
        MySqlInformationSchemaTablesColumn::TableCollation => 6,
        MySqlInformationSchemaTablesColumn::TableComment => 7,
    }
}

/// The shapes MySQL reports for the `information_schema.STATISTICS` columns
/// this answers, in the order MySQL declares them.
///
/// Every value comes from the pinned MySQL 8.4.11 golden
/// `information-schema-statistics.json`.
pub(super) fn information_schema_statistics_columns() -> Vec<ColumnDefinitionConfig> {
    [
        (
            "TABLE_CATALOG",
            "catalogs",
            MYSQL_TYPE_VAR_STRING,
            256u32,
            MYSQL_NOT_NULL_FLAG | MYSQL_BINARY_FLAG | MYSQL_NO_DEFAULT_VALUE_FLAG,
        ),
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
        ("NON_UNIQUE", "", MYSQL_TYPE_LONG, 2, MYSQL_NOT_NULL_FLAG),
        (
            "INDEX_SCHEMA",
            "schemata",
            MYSQL_TYPE_VAR_STRING,
            256,
            MYSQL_NOT_NULL_FLAG | MYSQL_BINARY_FLAG | MYSQL_NO_DEFAULT_VALUE_FLAG,
        ),
        ("INDEX_NAME", "", MYSQL_TYPE_VAR_STRING, 256, 0),
        (
            "SEQ_IN_INDEX",
            "index_column_usage",
            MYSQL_TYPE_LONG,
            10,
            MYSQL_NOT_NULL_FLAG | MYSQL_UNSIGNED_FLAG | MYSQL_NO_DEFAULT_VALUE_FLAG,
        ),
        ("COLUMN_NAME", "", MYSQL_TYPE_VAR_STRING, 256, 0),
        ("COLLATION", "", MYSQL_TYPE_VAR_STRING, 4, 0),
        ("CARDINALITY", "", MYSQL_TYPE_LONGLONG, 21, 0),
        ("SUB_PART", "", MYSQL_TYPE_LONGLONG, 21, 0),
        // Measured: MySQL reports the column it never fills as the null type.
        ("PACKED", "", MYSQL_TYPE_NULL, 0, MYSQL_BINARY_FLAG),
        (
            "NULLABLE",
            "",
            MYSQL_TYPE_VAR_STRING,
            12,
            MYSQL_NOT_NULL_FLAG,
        ),
        (
            "INDEX_TYPE",
            "",
            MYSQL_TYPE_VAR_STRING,
            44,
            MYSQL_NOT_NULL_FLAG | MYSQL_BINARY_FLAG,
        ),
        (
            "COMMENT",
            "",
            MYSQL_TYPE_VAR_STRING,
            32,
            MYSQL_NOT_NULL_FLAG,
        ),
        (
            "INDEX_COMMENT",
            "indexes",
            MYSQL_TYPE_VAR_STRING,
            8192,
            MYSQL_NOT_NULL_FLAG | MYSQL_BINARY_FLAG | MYSQL_NO_DEFAULT_VALUE_FLAG,
        ),
        (
            "IS_VISIBLE",
            "",
            MYSQL_TYPE_VAR_STRING,
            12,
            MYSQL_NOT_NULL_FLAG,
        ),
        (
            "EXPRESSION",
            "",
            MYSQL_TYPE_BLOB,
            u32::MAX,
            MYSQL_BLOB_FLAG | MYSQL_BINARY_FLAG,
        ),
    ]
    .into_iter()
    .map(
        |(name, original_table, column_type, column_length, flags)| {
            let mut column = ColumnDefinitionConfig::new(name, column_type);
            "information_schema".clone_into(&mut column.schema);
            "STATISTICS".clone_into(&mut column.table);
            original_table.clone_into(&mut column.original_table);
            name.clone_into(&mut column.original_name);
            column.character_set = if matches!(
                column_type,
                MYSQL_TYPE_LONG | MYSQL_TYPE_LONGLONG | MYSQL_TYPE_NULL
            ) {
                MYSQL_BINARY_COLLATION
            } else {
                u16::from(DEFAULT_UTF8MB4_COLLATION)
            };
            column.column_length = column_length;
            column.flags = flags;
            column
        },
    )
    .collect()
}

/// The shapes MySQL reports for the `information_schema.KEY_COLUMN_USAGE`
/// columns, in the order MySQL declares them.
///
/// Every value comes from the pinned MySQL 8.4.11 golden
/// `information-schema-key-column-usage.json`. This is the whole table: MySQL
/// has these twelve columns and no more.
pub(super) fn information_schema_key_column_usage_columns() -> Vec<ColumnDefinitionConfig> {
    let named = MYSQL_NOT_NULL_FLAG | MYSQL_BINARY_FLAG | MYSQL_NO_DEFAULT_VALUE_FLAG;
    [
        (
            "CONSTRAINT_CATALOG",
            "catalogs",
            MYSQL_TYPE_VAR_STRING,
            256u32,
            named,
        ),
        (
            "CONSTRAINT_SCHEMA",
            "schemata",
            MYSQL_TYPE_VAR_STRING,
            256,
            named,
        ),
        ("CONSTRAINT_NAME", "", MYSQL_TYPE_VAR_STRING, 256, 0),
        (
            "TABLE_CATALOG",
            "catalogs",
            MYSQL_TYPE_VAR_STRING,
            256,
            named,
        ),
        (
            "TABLE_SCHEMA",
            "schemata",
            MYSQL_TYPE_VAR_STRING,
            256,
            named,
        ),
        ("TABLE_NAME", "tables", MYSQL_TYPE_VAR_STRING, 256, named),
        ("COLUMN_NAME", "", MYSQL_TYPE_VAR_STRING, 256, 0),
        (
            "ORDINAL_POSITION",
            "",
            MYSQL_TYPE_LONG,
            10,
            MYSQL_NOT_NULL_FLAG | MYSQL_UNSIGNED_FLAG,
        ),
        (
            "POSITION_IN_UNIQUE_CONSTRAINT",
            "",
            MYSQL_TYPE_LONG,
            10,
            MYSQL_UNSIGNED_FLAG,
        ),
        (
            "REFERENCED_TABLE_SCHEMA",
            "",
            MYSQL_TYPE_VAR_STRING,
            256,
            MYSQL_BINARY_FLAG,
        ),
        (
            "REFERENCED_TABLE_NAME",
            "",
            MYSQL_TYPE_VAR_STRING,
            256,
            MYSQL_BINARY_FLAG,
        ),
        ("REFERENCED_COLUMN_NAME", "", MYSQL_TYPE_VAR_STRING, 256, 0),
    ]
    .into_iter()
    .map(
        |(name, original_table, column_type, column_length, flags)| {
            let mut column = ColumnDefinitionConfig::new(name, column_type);
            "information_schema".clone_into(&mut column.schema);
            "KEY_COLUMN_USAGE".clone_into(&mut column.table);
            original_table.clone_into(&mut column.original_table);
            name.clone_into(&mut column.original_name);
            column.character_set = if column_type == MYSQL_TYPE_LONG {
                MYSQL_BINARY_COLLATION
            } else {
                u16::from(DEFAULT_UTF8MB4_COLLATION)
            };
            column.column_length = column_length;
            column.flags = flags;
            column
        },
    )
    .collect()
}

/// The shapes MySQL reports for the `information_schema.TABLE_CONSTRAINTS`
/// columns, in the order MySQL declares them.
///
/// Every value comes from the pinned MySQL 8.4.11 golden
/// `information-schema-table-constraints.json`. This is the whole table.
pub(super) fn information_schema_table_constraints_columns() -> Vec<ColumnDefinitionConfig> {
    let named = MYSQL_NOT_NULL_FLAG | MYSQL_BINARY_FLAG | MYSQL_NO_DEFAULT_VALUE_FLAG;
    let read_back = MYSQL_NOT_NULL_FLAG | MYSQL_BINARY_FLAG;
    catalog_text_columns(
        "TABLE_CONSTRAINTS",
        &[
            (
                "CONSTRAINT_CATALOG",
                "catalogs",
                MYSQL_TYPE_VAR_STRING,
                256,
                named,
            ),
            (
                "CONSTRAINT_SCHEMA",
                "schemata",
                MYSQL_TYPE_VAR_STRING,
                256,
                named,
            ),
            ("CONSTRAINT_NAME", "", MYSQL_TYPE_VAR_STRING, 256, 0),
            (
                "TABLE_SCHEMA",
                "schemata",
                MYSQL_TYPE_VAR_STRING,
                256,
                named,
            ),
            ("TABLE_NAME", "tables", MYSQL_TYPE_VAR_STRING, 256, named),
            ("CONSTRAINT_TYPE", "", MYSQL_TYPE_VAR_STRING, 44, read_back),
            ("ENFORCED", "", MYSQL_TYPE_VAR_STRING, 12, read_back),
        ],
    )
}

/// The shapes MySQL reports for the
/// `information_schema.REFERENTIAL_CONSTRAINTS` columns, in the order MySQL
/// declares them.
///
/// Every value comes from the pinned MySQL 8.4.11 golden
/// `information-schema-table-constraints.json`. This is the whole table.
pub(super) fn information_schema_referential_constraints_columns() -> Vec<ColumnDefinitionConfig> {
    let named = MYSQL_NOT_NULL_FLAG | MYSQL_BINARY_FLAG | MYSQL_NO_DEFAULT_VALUE_FLAG;
    // Measured: MySQL declares the three rule columns as ENUMs, so they report
    // the string type and the ENUM flag where every other column here reports
    // a var_string.
    let rule =
        MYSQL_NOT_NULL_FLAG | MYSQL_BINARY_FLAG | MYSQL_ENUM_FLAG | MYSQL_NO_DEFAULT_VALUE_FLAG;
    catalog_text_columns(
        "REFERENTIAL_CONSTRAINTS",
        &[
            (
                "CONSTRAINT_CATALOG",
                "catalogs",
                MYSQL_TYPE_VAR_STRING,
                256,
                named,
            ),
            (
                "CONSTRAINT_SCHEMA",
                "schemata",
                MYSQL_TYPE_VAR_STRING,
                256,
                named,
            ),
            ("CONSTRAINT_NAME", "", MYSQL_TYPE_VAR_STRING, 256, 0),
            (
                "UNIQUE_CONSTRAINT_CATALOG",
                "foreign_keys",
                MYSQL_TYPE_VAR_STRING,
                256,
                named,
            ),
            (
                "UNIQUE_CONSTRAINT_SCHEMA",
                "foreign_keys",
                MYSQL_TYPE_VAR_STRING,
                256,
                named,
            ),
            (
                "UNIQUE_CONSTRAINT_NAME",
                "foreign_keys",
                MYSQL_TYPE_VAR_STRING,
                256,
                0,
            ),
            ("MATCH_OPTION", "foreign_keys", MYSQL_TYPE_STRING, 28, rule),
            ("UPDATE_RULE", "foreign_keys", MYSQL_TYPE_STRING, 44, rule),
            ("DELETE_RULE", "foreign_keys", MYSQL_TYPE_STRING, 44, rule),
            ("TABLE_NAME", "tables", MYSQL_TYPE_VAR_STRING, 256, named),
            (
                "REFERENCED_TABLE_NAME",
                "foreign_keys",
                MYSQL_TYPE_VAR_STRING,
                256,
                named,
            ),
        ],
    )
}

/// The shapes MySQL reports for the `information_schema.CHECK_CONSTRAINTS`
/// columns, in the order MySQL declares them.
///
/// Measured on MySQL 8.4.11 through `SELECT *` with an `ORDER BY`, the reading
/// every other table here is pinned to.
pub(super) fn information_schema_check_constraints_columns() -> Vec<ColumnDefinitionConfig> {
    let named = MYSQL_NOT_NULL_FLAG | MYSQL_BINARY_FLAG | MYSQL_NO_DEFAULT_VALUE_FLAG;
    catalog_text_columns(
        "CHECK_CONSTRAINTS",
        &[
            (
                "CONSTRAINT_CATALOG",
                "catalogs",
                MYSQL_TYPE_VAR_STRING,
                256,
                named,
            ),
            (
                "CONSTRAINT_SCHEMA",
                "schemata",
                MYSQL_TYPE_VAR_STRING,
                256,
                named,
            ),
            (
                "CONSTRAINT_NAME",
                "check_constraints",
                MYSQL_TYPE_VAR_STRING,
                256,
                MYSQL_NOT_NULL_FLAG | MYSQL_NO_DEFAULT_VALUE_FLAG,
            ),
            (
                "CHECK_CLAUSE",
                "check_constraints",
                MYSQL_TYPE_BLOB,
                u32::MAX,
                named | MYSQL_BLOB_FLAG,
            ),
        ],
    )
}

/// The shapes MySQL reports for the `information_schema.ROUTINES` columns, in
/// the order MySQL declares them.
///
/// Measured on MySQL 8.4.11 through `SELECT *` with an `ORDER BY`, the reading
/// every other table here is pinned to. Three columns are constants in MySQL's
/// own definition of the table, and those name no table at all.
pub(super) fn information_schema_routines_columns() -> Vec<ColumnDefinitionConfig> {
    let named = MYSQL_NOT_NULL_FLAG | MYSQL_BINARY_FLAG | MYSQL_NO_DEFAULT_VALUE_FLAG;
    let listed = MYSQL_NOT_NULL_FLAG | MYSQL_NO_DEFAULT_VALUE_FLAG;
    let chosen =
        MYSQL_NOT_NULL_FLAG | MYSQL_BINARY_FLAG | MYSQL_ENUM_FLAG | MYSQL_NO_DEFAULT_VALUE_FLAG;
    let blob = MYSQL_BLOB_FLAG | MYSQL_BINARY_FLAG;
    [
        (
            "SPECIFIC_NAME",
            "routines",
            MYSQL_TYPE_VAR_STRING,
            256u32,
            listed,
        ),
        (
            "ROUTINE_CATALOG",
            "catalogs",
            MYSQL_TYPE_VAR_STRING,
            256,
            named,
        ),
        (
            "ROUTINE_SCHEMA",
            "schemata",
            MYSQL_TYPE_VAR_STRING,
            256,
            named,
        ),
        (
            "ROUTINE_NAME",
            "routines",
            MYSQL_TYPE_VAR_STRING,
            256,
            listed,
        ),
        ("ROUTINE_TYPE", "routines", MYSQL_TYPE_STRING, 36, chosen),
        ("DATA_TYPE", "", MYSQL_TYPE_BLOB, 201_326_580, blob),
        ("CHARACTER_MAXIMUM_LENGTH", "", MYSQL_TYPE_LONGLONG, 21, 0),
        ("CHARACTER_OCTET_LENGTH", "", MYSQL_TYPE_LONGLONG, 21, 0),
        (
            "NUMERIC_PRECISION",
            "routines",
            MYSQL_TYPE_LONG,
            10,
            MYSQL_UNSIGNED_FLAG,
        ),
        (
            "NUMERIC_SCALE",
            "routines",
            MYSQL_TYPE_LONG,
            10,
            MYSQL_UNSIGNED_FLAG,
        ),
        (
            "DATETIME_PRECISION",
            "routines",
            MYSQL_TYPE_LONG,
            10,
            MYSQL_UNSIGNED_FLAG,
        ),
        ("CHARACTER_SET_NAME", "", MYSQL_TYPE_VAR_STRING, 256, 0),
        ("COLLATION_NAME", "", MYSQL_TYPE_VAR_STRING, 256, 0),
        ("DTD_IDENTIFIER", "", MYSQL_TYPE_BLOB, 201_326_580, blob),
        (
            "ROUTINE_BODY",
            "",
            MYSQL_TYPE_VAR_STRING,
            32,
            MYSQL_NOT_NULL_FLAG,
        ),
        ("ROUTINE_DEFINITION", "", MYSQL_TYPE_BLOB, u32::MAX, blob),
        ("EXTERNAL_NAME", "", MYSQL_TYPE_NULL, 0, MYSQL_BINARY_FLAG),
        (
            "EXTERNAL_LANGUAGE",
            "routines",
            MYSQL_TYPE_VAR_STRING,
            256,
            MYSQL_NOT_NULL_FLAG | MYSQL_BINARY_FLAG,
        ),
        (
            "PARAMETER_STYLE",
            "",
            MYSQL_TYPE_VAR_STRING,
            12,
            MYSQL_NOT_NULL_FLAG,
        ),
        (
            "IS_DETERMINISTIC",
            "",
            MYSQL_TYPE_VAR_STRING,
            12,
            MYSQL_NOT_NULL_FLAG,
        ),
        ("SQL_DATA_ACCESS", "routines", MYSQL_TYPE_STRING, 68, chosen),
        ("SQL_PATH", "", MYSQL_TYPE_NULL, 0, MYSQL_BINARY_FLAG),
        ("SECURITY_TYPE", "routines", MYSQL_TYPE_STRING, 28, chosen),
        ("CREATED", "routines", MYSQL_TYPE_TIMESTAMP, 19, named),
        ("LAST_ALTERED", "routines", MYSQL_TYPE_TIMESTAMP, 19, named),
        (
            "SQL_MODE",
            "routines",
            MYSQL_TYPE_STRING,
            2080,
            MYSQL_NOT_NULL_FLAG | MYSQL_BINARY_FLAG | MYSQL_SET_FLAG | MYSQL_NO_DEFAULT_VALUE_FLAG,
        ),
        (
            "ROUTINE_COMMENT",
            "routines",
            MYSQL_TYPE_BLOB,
            262_140,
            named | MYSQL_BLOB_FLAG,
        ),
        ("DEFINER", "routines", MYSQL_TYPE_VAR_STRING, 1152, named),
        (
            "CHARACTER_SET_CLIENT",
            "character_sets",
            MYSQL_TYPE_VAR_STRING,
            256,
            listed,
        ),
        (
            "COLLATION_CONNECTION",
            "collations",
            MYSQL_TYPE_VAR_STRING,
            256,
            listed,
        ),
        (
            "DATABASE_COLLATION",
            "collations",
            MYSQL_TYPE_VAR_STRING,
            256,
            listed,
        ),
    ]
    .into_iter()
    .map(
        |(name, original_table, column_type, column_length, flags)| {
            let mut column = ColumnDefinitionConfig::new(name, column_type);
            // MySQL defines these three as constants, which belong to no
            // table.
            if !matches!(name, "EXTERNAL_NAME" | "SQL_PATH" | "PARAMETER_STYLE") {
                "information_schema".clone_into(&mut column.schema);
                "ROUTINES".clone_into(&mut column.table);
            }
            original_table.clone_into(&mut column.original_table);
            name.clone_into(&mut column.original_name);
            column.character_set = if matches!(
                column_type,
                MYSQL_TYPE_LONG | MYSQL_TYPE_LONGLONG | MYSQL_TYPE_NULL | MYSQL_TYPE_TIMESTAMP
            ) {
                MYSQL_BINARY_COLLATION
            } else {
                u16::from(DEFAULT_UTF8MB4_COLLATION)
            };
            column.column_length = column_length;
            // Measured: every number and the null type also carry the
            // numeric flag.
            column.flags = if matches!(
                column_type,
                MYSQL_TYPE_LONG | MYSQL_TYPE_LONGLONG | MYSQL_TYPE_NULL
            ) {
                flags | MYSQL_NUM_FLAG
            } else {
                flags
            };
            if name == "PARAMETER_STYLE" {
                column.decimals = 31;
            }
            column
        },
    )
    .collect()
}

/// Every column of `information_schema.VIEWS`, in the order MySQL declares
/// them.
///
/// The seven a dump client reads keep the shapes measured for it; the other
/// three were measured on MySQL 8.4.11 through `SELECT *`.
pub(super) fn information_schema_views_columns() -> Vec<ColumnDefinitionConfig> {
    let fields = [
        (
            "TABLE_CATALOG",
            "information_schema",
            MYSQL_TYPE_VAR_STRING,
            256,
            MYSQL_NOT_NULL_FLAG
                | MYSQL_UNIQUE_KEY_FLAG
                | MYSQL_BINARY_FLAG
                | MYSQL_NO_DEFAULT_VALUE_FLAG
                | MYSQL_PART_KEY_FLAG,
            0,
        ),
        (
            "TABLE_SCHEMA",
            "information_schema",
            MYSQL_TYPE_VAR_STRING,
            256,
            MYSQL_NOT_NULL_FLAG | MYSQL_BINARY_FLAG | MYSQL_NO_DEFAULT_VALUE_FLAG,
            0,
        ),
        (
            "TABLE_NAME",
            "information_schema",
            MYSQL_TYPE_VAR_STRING,
            256,
            MYSQL_NOT_NULL_FLAG | MYSQL_BINARY_FLAG | MYSQL_NO_DEFAULT_VALUE_FLAG,
            0,
        ),
        (
            "VIEW_DEFINITION",
            "",
            MYSQL_TYPE_BLOB,
            u32::MAX,
            MYSQL_BINARY_FLAG,
            31,
        ),
        (
            "CHECK_OPTION",
            "information_schema",
            MYSQL_TYPE_STRING,
            32,
            MYSQL_BINARY_FLAG | MYSQL_ENUM_FLAG,
            0,
        ),
        (
            "IS_UPDATABLE",
            "information_schema",
            MYSQL_TYPE_STRING,
            12,
            MYSQL_BINARY_FLAG | MYSQL_ENUM_FLAG,
            0,
        ),
        (
            "DEFINER",
            "information_schema",
            MYSQL_TYPE_VAR_STRING,
            1152,
            MYSQL_BINARY_FLAG | MYSQL_PART_KEY_FLAG,
            0,
        ),
        (
            "SECURITY_TYPE",
            "",
            MYSQL_TYPE_VAR_STRING,
            28,
            MYSQL_BINARY_FLAG,
            31,
        ),
        (
            "CHARACTER_SET_CLIENT",
            "information_schema",
            MYSQL_TYPE_VAR_STRING,
            256,
            MYSQL_NOT_NULL_FLAG
                | MYSQL_UNIQUE_KEY_FLAG
                | MYSQL_NO_DEFAULT_VALUE_FLAG
                | MYSQL_PART_KEY_FLAG,
            0,
        ),
        (
            "COLLATION_CONNECTION",
            "information_schema",
            MYSQL_TYPE_VAR_STRING,
            256,
            MYSQL_NOT_NULL_FLAG
                | MYSQL_UNIQUE_KEY_FLAG
                | MYSQL_NO_DEFAULT_VALUE_FLAG
                | MYSQL_PART_KEY_FLAG,
            0,
        ),
    ];
    fields
        .into_iter()
        .map(|(name, schema, kind, length, flags, decimals)| {
            let mut column = ColumnDefinitionConfig::new(name, kind);
            schema.clone_into(&mut column.schema);
            "views".clone_into(&mut column.table);
            "VIEWS".clone_into(&mut column.original_table);
            name.clone_into(&mut column.original_name);
            column.character_set = u16::from(DEFAULT_UTF8MB4_COLLATION);
            column.column_length = length;
            column.flags = flags;
            column.decimals = decimals;
            column
        })
        .collect()
}

/// Builds the columns of one `information_schema` table whose every column
/// holds text, which is every one of them but `STATISTICS` and
/// `KEY_COLUMN_USAGE`.
fn catalog_text_columns(
    table: &str,
    columns: &[(&str, &str, u8, u32, u16)],
) -> Vec<ColumnDefinitionConfig> {
    columns
        .iter()
        .map(
            |(name, original_table, column_type, column_length, flags)| {
                let mut column = ColumnDefinitionConfig::new(*name, *column_type);
                "information_schema".clone_into(&mut column.schema);
                table.clone_into(&mut column.table);
                (*original_table).clone_into(&mut column.original_table);
                (*name).clone_into(&mut column.original_name);
                column.character_set = u16::from(DEFAULT_UTF8MB4_COLLATION);
                column.column_length = *column_length;
                column.flags = *flags;
                column
            },
        )
        .collect()
}

pub(super) fn information_schema_columns_result_to_execution_result(
    columns: Vec<MySqlColumnMetadata>,
    projected: &[MySqlInformationSchemaColumnsColumn],
    status_flags: u16,
) -> Result<CommandExecutionResult, FrontendErrorKind> {
    if columns.len() > MAX_DISPATCH_RESULT_ROWS {
        return Err(FrontendErrorKind::Internal);
    }

    let mut retained_bytes = 0usize;
    let mut rows = Vec::with_capacity(columns.len());
    for (ordinal, column) in columns.into_iter().enumerate() {
        if column.name().len() > MAX_CATALOG_VALUE_LENGTH
            || column.extra().len() > MAX_CATALOG_VALUE_LENGTH
            || column.comment().len() > MAX_CATALOG_VALUE_LENGTH
        {
            return Err(FrontendErrorKind::Internal);
        }
        let column_type = show_column_type_name(&column)?;
        let (character_maximum_length, numeric_precision, numeric_scale, collation_name) =
            information_schema_column_sizes(&column, &column_type);
        let extra = show_column_extra(column.extra())?;
        let default = match column.default_value() {
            Some(MySqlColumnDefault::Text(value)) if value.len() > MAX_CATALOG_VALUE_LENGTH => {
                return Err(FrontendErrorKind::Internal);
            }
            _ => show_column_default_value(&column)?,
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
            column.comment().len(),
            character_maximum_length.as_ref().map_or(0, Vec::len),
            numeric_precision.as_ref().map_or(0, Vec::len),
            numeric_scale.as_ref().map_or(0, Vec::len),
            collation_name.as_ref().map_or(0, Vec::len),
        ];
        if value_lengths
            .iter()
            .any(|length| *length > MAX_CATALOG_VALUE_LENGTH)
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
        if payload_len > crate::MAX_RESPONSE_PACKET_PAYLOAD_LENGTH {
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

        let whole = [
            Some(column.name().as_bytes().to_vec()),
            Some(ordinal),
            default,
            Some(nullable.to_vec()),
            Some(the_type_without_its_own_words(&column_type).to_vec()),
            Some(column_type.to_vec()),
            Some(key.to_vec()),
            Some(extra.to_vec()),
            Some(column.comment().as_bytes().to_vec()),
            character_maximum_length,
            numeric_precision,
            numeric_scale,
            collation_name,
        ];
        // The row holds what the query named, in the order it named it.
        rows.push(
            projected
                .iter()
                .map(|column| whole[information_schema_columns_position(*column)].clone())
                .collect::<Vec<_>>(),
        );
    }

    Ok(CommandExecutionResult::ResultSet(TextResultSet {
        columns: information_schema_columns_columns(projected),
        rows,
        warnings: 0,
        status_flags,
    }))
}

/// Where one column sits among the nine, which is the order MySQL declares
/// them in and the order both the row and the definitions are built in.
fn information_schema_columns_position(column: MySqlInformationSchemaColumnsColumn) -> usize {
    match column {
        MySqlInformationSchemaColumnsColumn::ColumnName => 0,
        MySqlInformationSchemaColumnsColumn::OrdinalPosition => 1,
        MySqlInformationSchemaColumnsColumn::ColumnDefault => 2,
        MySqlInformationSchemaColumnsColumn::IsNullable => 3,
        MySqlInformationSchemaColumnsColumn::DataType => 4,
        MySqlInformationSchemaColumnsColumn::ColumnType => 5,
        MySqlInformationSchemaColumnsColumn::ColumnKey => 6,
        MySqlInformationSchemaColumnsColumn::Extra => 7,
        MySqlInformationSchemaColumnsColumn::ColumnComment => 8,
        MySqlInformationSchemaColumnsColumn::CharacterMaximumLength => 9,
        MySqlInformationSchemaColumnsColumn::NumericPrecision => 10,
        MySqlInformationSchemaColumnsColumn::NumericScale => 11,
        MySqlInformationSchemaColumnsColumn::CollationName => 12,
    }
}

type InformationSchemaColumnSizes = (
    Option<Vec<u8>>,
    Option<Vec<u8>>,
    Option<Vec<u8>>,
    Option<Vec<u8>>,
);

fn information_schema_column_sizes(
    column: &MySqlColumnMetadata,
    column_type: &[u8],
) -> InformationSchemaColumnSizes {
    let data_type = the_type_without_its_own_words(column_type);
    let characters = match data_type {
        b"char" | b"varchar" | b"varbinary" => column.character_length().map(u64::from),
        b"tinytext" | b"tinyblob" => Some(255),
        b"text" | b"blob" => Some(65_535),
        b"mediumtext" | b"mediumblob" => Some(16_777_215),
        b"longtext" | b"longblob" => Some(4_294_967_295),
        b"enum" => turso_mysql_parser::enum_members(column.type_name()).and_then(|members| {
            members
                .iter()
                .map(|member| member.chars().count())
                .max()
                .map(|length| length as u64)
        }),
        b"set" => turso_mysql_parser::set_members(column.type_name()).map(|members| {
            members
                .iter()
                .map(|member| member.chars().count() as u64)
                .sum::<u64>()
                + members.len().saturating_sub(1) as u64
        }),
        _ => None,
    };
    let precision = if let Some((precision, _)) = column.decimal_size() {
        Some(precision)
    } else {
        match data_type {
            b"tinyint" => Some(3),
            b"smallint" => Some(5),
            b"mediumint" => Some(7),
            b"int" | b"integer" => Some(10),
            b"bigint" if column_type.ends_with(b" unsigned") => Some(20),
            b"bigint" => Some(19),
            b"float" => Some(12),
            b"double" => Some(22),
            b"bit" => Some(1),
            _ => None,
        }
    };
    let scale = if let Some((_, scale)) = column.decimal_size() {
        Some(scale)
    } else if matches!(
        data_type,
        b"tinyint" | b"smallint" | b"mediumint" | b"int" | b"integer" | b"bigint"
    ) {
        Some(0)
    } else {
        None
    };
    let collation = column.collation_name().map(|name| name.as_bytes().to_vec());
    (
        characters.map(|value| value.to_string().into_bytes()),
        precision.map(|value| value.to_string().into_bytes()),
        scale.map(|value| value.to_string().into_bytes()),
        collation,
    )
}

/// The type a column holds, without the size and the sign the declaration
/// carried.
///
/// Measured on MySQL 8.4.11 over one of every type: `DATA_TYPE` is
/// `COLUMN_TYPE` up to the first `(` or space, so `varchar(8)` reads
/// `varchar`, `int unsigned` reads `int`, `decimal(8,2)` reads `decimal`,
/// `tinyint(1)` reads `tinyint` and `enum('a','b')` reads `enum`.
fn the_type_without_its_own_words(column_type: &[u8]) -> &[u8] {
    match column_type
        .iter()
        .position(|byte| *byte == b'(' || *byte == b' ')
    {
        Some(at) => &column_type[..at],
        None => column_type,
    }
}

pub(super) fn information_schema_columns_columns(
    projected: &[MySqlInformationSchemaColumnsColumn],
) -> Vec<ColumnDefinitionConfig> {
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

    // Measured on MySQL 8.4.11: a blob of 201326580 carrying only the blob and
    // binary flags, nullable where `COLUMN_TYPE` is not, and naming no
    // original table where `COLUMN_TYPE` names one.
    let mut data_type = information_schema_column_definition(
        "DATA_TYPE",
        MYSQL_TYPE_BLOB,
        201_326_580,
        DEFAULT_UTF8MB4_COLLATION.into(),
        false,
    );
    data_type.flags = MYSQL_BLOB_FLAG | MYSQL_BINARY_FLAG;

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

    // Measured on MySQL 8.4.11: a blob of 24576, not null, and naming no
    // original table.
    let mut column_comment = information_schema_column_definition(
        "COLUMN_COMMENT",
        MYSQL_TYPE_BLOB,
        24_576,
        DEFAULT_UTF8MB4_COLLATION.into(),
        false,
    );
    column_comment.flags = MYSQL_NOT_NULL_FLAG | MYSQL_BLOB_FLAG | MYSQL_BINARY_FLAG;

    let mut character_maximum_length = information_schema_column_definition(
        "CHARACTER_MAXIMUM_LENGTH",
        MYSQL_TYPE_LONGLONG,
        21,
        MYSQL_BINARY_COLLATION,
        false,
    );
    character_maximum_length.flags = MYSQL_NUM_FLAG;

    let mut numeric_precision = information_schema_column_definition(
        "NUMERIC_PRECISION",
        MYSQL_TYPE_LONGLONG,
        10,
        MYSQL_BINARY_COLLATION,
        false,
    );
    numeric_precision.flags = MYSQL_UNSIGNED_FLAG | MYSQL_NUM_FLAG;

    let mut numeric_scale = information_schema_column_definition(
        "NUMERIC_SCALE",
        MYSQL_TYPE_LONGLONG,
        10,
        MYSQL_BINARY_COLLATION,
        false,
    );
    numeric_scale.flags = MYSQL_UNSIGNED_FLAG | MYSQL_NUM_FLAG;

    let collation_name = information_schema_column_definition(
        "COLLATION_NAME",
        MYSQL_TYPE_VAR_STRING,
        256,
        DEFAULT_UTF8MB4_COLLATION.into(),
        false,
    );

    let whole = [
        column_name,
        ordinal_position,
        column_default,
        is_nullable,
        data_type,
        column_type,
        column_key,
        extra,
        column_comment,
        character_maximum_length,
        numeric_precision,
        numeric_scale,
        collation_name,
    ];
    projected
        .iter()
        .map(|column| whole[information_schema_columns_position(*column)].clone())
        .collect()
}

/// Every column of `information_schema.COLUMNS`, in the order MySQL declares
/// them, which is what a wildcard over the table answers.
///
/// The thirteen the catalogue reader answers keep the shapes pinned there. The
/// other nine were measured on MySQL 8.4.11 through `SELECT *` with an `ORDER
/// BY`, the reading every other table here is pinned to.
pub(super) fn information_schema_columns_every_column() -> Vec<ColumnDefinitionConfig> {
    let answered = information_schema_columns_columns(&[
        MySqlInformationSchemaColumnsColumn::ColumnName,
        MySqlInformationSchemaColumnsColumn::OrdinalPosition,
        MySqlInformationSchemaColumnsColumn::ColumnDefault,
        MySqlInformationSchemaColumnsColumn::IsNullable,
        MySqlInformationSchemaColumnsColumn::DataType,
        MySqlInformationSchemaColumnsColumn::CharacterMaximumLength,
        MySqlInformationSchemaColumnsColumn::NumericPrecision,
        MySqlInformationSchemaColumnsColumn::NumericScale,
        MySqlInformationSchemaColumnsColumn::CollationName,
        MySqlInformationSchemaColumnsColumn::ColumnType,
        MySqlInformationSchemaColumnsColumn::ColumnKey,
        MySqlInformationSchemaColumnsColumn::Extra,
        MySqlInformationSchemaColumnsColumn::ColumnComment,
    ]);
    let mut answered = answered.into_iter();
    let mut next_answered = || {
        answered
            .next()
            .expect("the catalogue reader answers thirteen columns")
    };
    let named = |name: &str, original_table: &str| {
        let mut column = information_schema_column_definition(
            name,
            MYSQL_TYPE_VAR_STRING,
            256,
            DEFAULT_UTF8MB4_COLLATION.into(),
            false,
        );
        original_table.clone_into(&mut column.original_table);
        column.flags = MYSQL_NOT_NULL_FLAG | MYSQL_BINARY_FLAG | MYSQL_NO_DEFAULT_VALUE_FLAG;
        column
    };
    let mut character_octet_length = information_schema_column_definition(
        "CHARACTER_OCTET_LENGTH",
        MYSQL_TYPE_LONGLONG,
        21,
        MYSQL_BINARY_COLLATION,
        false,
    );
    character_octet_length.flags = MYSQL_NUM_FLAG;
    let mut datetime_precision = information_schema_column_definition(
        "DATETIME_PRECISION",
        MYSQL_TYPE_LONG,
        10,
        MYSQL_BINARY_COLLATION,
        true,
    );
    datetime_precision.flags = MYSQL_UNSIGNED_FLAG | MYSQL_NUM_FLAG;
    let character_set_name = information_schema_column_definition(
        "CHARACTER_SET_NAME",
        MYSQL_TYPE_VAR_STRING,
        256,
        DEFAULT_UTF8MB4_COLLATION.into(),
        false,
    );
    let privileges = information_schema_column_definition(
        "PRIVILEGES",
        MYSQL_TYPE_VAR_STRING,
        616,
        DEFAULT_UTF8MB4_COLLATION.into(),
        false,
    );
    let mut generation_expression = information_schema_column_definition(
        "GENERATION_EXPRESSION",
        MYSQL_TYPE_BLOB,
        u32::MAX,
        DEFAULT_UTF8MB4_COLLATION.into(),
        false,
    );
    generation_expression.flags = MYSQL_NOT_NULL_FLAG | MYSQL_BLOB_FLAG | MYSQL_BINARY_FLAG;
    let mut srs_id = information_schema_column_definition(
        "SRS_ID",
        MYSQL_TYPE_LONG,
        10,
        MYSQL_BINARY_COLLATION,
        true,
    );
    srs_id.flags = MYSQL_UNSIGNED_FLAG | MYSQL_NUM_FLAG;

    let column_name = next_answered();
    let ordinal_position = next_answered();
    let column_default = next_answered();
    let is_nullable = next_answered();
    let data_type = next_answered();
    let character_maximum_length = next_answered();
    let numeric_precision = next_answered();
    let numeric_scale = next_answered();
    let collation_name = next_answered();
    let column_type = next_answered();
    let column_key = next_answered();
    let extra = next_answered();
    let column_comment = next_answered();
    vec![
        named("TABLE_CATALOG", "catalogs"),
        named("TABLE_SCHEMA", "schemata"),
        named("TABLE_NAME", "tables"),
        column_name,
        ordinal_position,
        column_default,
        is_nullable,
        data_type,
        character_maximum_length,
        character_octet_length,
        numeric_precision,
        numeric_scale,
        datetime_precision,
        character_set_name,
        collation_name,
        column_type,
        column_key,
        extra,
        privileges,
        column_comment,
        generation_expression,
        srs_id,
    ]
}

/// The rows `information_schema.COLUMNS` answers for one table, every column
/// of each, in the order MySQL declares them.
///
/// Measured on MySQL 8.4.11 over one of every type: a `CHAR`, `VARCHAR`,
/// `ENUM` or `SET` holds four bytes a character, a `TEXT` or a binary type is
/// measured in bytes already, so its two lengths are the same number, and only
/// a `TIME`, `DATETIME` or `TIMESTAMP` has a `DATETIME_PRECISION` — a `DATE`
/// has none. No column here is generated or spatial, so every
/// `GENERATION_EXPRESSION` is empty and every `SRS_ID` is NULL.
pub(super) fn information_schema_columns_rows(
    database: &str,
    table: &str,
    columns: Vec<MySqlColumnMetadata>,
    privileges: &str,
) -> Result<Vec<Vec<Value>>, FrontendErrorKind> {
    let text = |bytes: Vec<u8>| {
        String::from_utf8(bytes)
            .map(Value::build_text)
            .map_err(|_| FrontendErrorKind::Internal)
    };
    let number = |bytes: Option<Vec<u8>>| -> Result<Value, FrontendErrorKind> {
        match bytes {
            None => Ok(Value::Null),
            Some(bytes) => std::str::from_utf8(&bytes)
                .ok()
                .and_then(|digits| digits.parse::<i64>().ok())
                .map(Value::from_i64)
                .ok_or(FrontendErrorKind::Internal),
        }
    };
    let mut rows = Vec::with_capacity(columns.len());
    for (ordinal, column) in columns.into_iter().enumerate() {
        let column_type = show_column_type_name(&column)?;
        let data_type = the_type_without_its_own_words(&column_type).to_vec();
        let (character_maximum_length, numeric_precision, numeric_scale, collation_name) =
            information_schema_column_sizes(&column, &column_type);
        let character_octet_length = match data_type.as_slice() {
            b"char" | b"varchar" | b"enum" | b"set" => character_maximum_length
                .as_ref()
                .map(|characters| {
                    let characters = std::str::from_utf8(characters)
                        .ok()
                        .and_then(|digits| digits.parse::<u64>().ok())
                        .ok_or(FrontendErrorKind::Internal)?;
                    Ok((characters * 4).to_string().into_bytes())
                })
                .transpose()?,
            _ => character_maximum_length.clone(),
        };
        let datetime_precision = match data_type.as_slice() {
            b"time" | b"datetime" | b"timestamp" => {
                Value::from_i64(i64::from(column.temporal_precision().unwrap_or(0)))
            }
            _ => Value::Null,
        };
        let character_set_name = match &collation_name {
            Some(_) => Value::build_text("utf8mb4"),
            None => Value::Null,
        };
        let key = match column.key() {
            MySqlColumnKey::None => "",
            MySqlColumnKey::Multiple => "MUL",
            MySqlColumnKey::Unique => "UNI",
            MySqlColumnKey::Primary => "PRI",
        };
        rows.push(vec![
            Value::build_text("def"),
            Value::build_text(database.to_owned()),
            Value::build_text(table.to_owned()),
            Value::build_text(column.name().to_owned()),
            Value::from_i64(ordinal as i64 + 1),
            match show_column_default_value(&column)? {
                Some(default) => text(default)?,
                None => Value::Null,
            },
            Value::build_text(if column.nullable() { "YES" } else { "NO" }),
            text(data_type)?,
            number(character_maximum_length)?,
            number(character_octet_length)?,
            number(numeric_precision)?,
            number(numeric_scale)?,
            datetime_precision,
            character_set_name,
            match collation_name {
                Some(collation) => text(collation)?,
                None => Value::Null,
            },
            text(column_type)?,
            Value::build_text(key),
            text(show_column_extra(column.extra())?)?,
            Value::build_text(privileges.to_owned()),
            Value::build_text(column.comment().to_owned()),
            Value::build_text(""),
            Value::Null,
        ]);
    }
    Ok(rows)
}

/// Every column of `information_schema.SCHEMATA`, in the order MySQL declares
/// them.
///
/// Measured on MySQL 8.4.11 through `SELECT *` with an `ORDER BY`, the reading
/// every other table here is pinned to. `SQL_PATH` is a constant in MySQL's
/// own definition of the table, and names no table at all.
pub(super) fn information_schema_schemata_columns() -> Vec<ColumnDefinitionConfig> {
    let named = MYSQL_NOT_NULL_FLAG | MYSQL_BINARY_FLAG | MYSQL_NO_DEFAULT_VALUE_FLAG;
    let listed = MYSQL_NOT_NULL_FLAG | MYSQL_NO_DEFAULT_VALUE_FLAG;
    let mut columns = catalog_text_columns(
        "SCHEMATA",
        &[
            (
                "CATALOG_NAME",
                "catalogs",
                MYSQL_TYPE_VAR_STRING,
                256,
                named,
            ),
            ("SCHEMA_NAME", "schemata", MYSQL_TYPE_VAR_STRING, 256, named),
            (
                "DEFAULT_CHARACTER_SET_NAME",
                "character_sets",
                MYSQL_TYPE_VAR_STRING,
                256,
                listed,
            ),
            (
                "DEFAULT_COLLATION_NAME",
                "collations",
                MYSQL_TYPE_VAR_STRING,
                256,
                listed,
            ),
            (
                "SQL_PATH",
                "",
                MYSQL_TYPE_NULL,
                0,
                MYSQL_BINARY_FLAG | MYSQL_NUM_FLAG,
            ),
            (
                "DEFAULT_ENCRYPTION",
                "schemata",
                MYSQL_TYPE_STRING,
                12,
                named | MYSQL_ENUM_FLAG,
            ),
        ],
    );
    let sql_path = &mut columns[4];
    sql_path.schema.clear();
    sql_path.table.clear();
    sql_path.character_set = MYSQL_BINARY_COLLATION;
    columns
}

/// The row `information_schema.SCHEMATA` answers for one database.
///
/// Every database here is `utf8mb4`, and none is encrypted.
pub(super) fn information_schema_schemata_row(
    database: String,
    collation: turso_mysql_parser::MySqlTableCollation,
) -> Vec<Value> {
    vec![
        Value::build_text("def"),
        Value::build_text(database),
        Value::build_text("utf8mb4"),
        Value::build_text(collation.name()),
        Value::Null,
        Value::build_text("NO"),
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
        if entry.key_name().len() > MAX_CATALOG_VALUE_LENGTH
            || entry.column_name().len() > MAX_CATALOG_VALUE_LENGTH
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

/// Reports what one `ANALYZE TABLE` did.
///
/// Measured on MySQL 8.4.11: one row of `<database>.<table>`, `analyze`,
/// `status`, `OK`, over four columns of VAR_STRING 128, VAR_STRING 10,
/// VAR_STRING 10 and MEDIUM_BLOB 393216, all latin1 and none of them flagged.
pub(super) fn analyze_table_result_to_execution_result(
    database: &str,
    table: &str,
    status_flags: u16,
) -> CommandExecutionResult {
    maintenance_result(database, table, "analyze", None, status_flags)
}

/// Reports what one `CHECK TABLE` found.
///
/// Measured on MySQL 8.4.11: `status` and `OK` when nothing is wrong, over the
/// same four columns `ANALYZE TABLE` answers. What the check found instead goes
/// in `Msg_text` under `error`, which is where MySQL puts it too.
pub(super) fn check_table_result_to_execution_result(
    database: &str,
    table: &str,
    problem: Option<String>,
    status_flags: u16,
) -> CommandExecutionResult {
    maintenance_result(database, table, "check", problem, status_flags)
}

/// The one row a maintenance statement answers.
fn maintenance_result(
    database: &str,
    table: &str,
    operation: &str,
    problem: Option<String>,
    status_flags: u16,
) -> CommandExecutionResult {
    let column = |name: &str, column_type: u8, column_length: u32| {
        let mut column = ColumnDefinitionConfig::new(name, column_type);
        column.character_set = u16::from(DEFAULT_UTF8MB4_COLLATION);
        column.column_length = column_length;
        column
    };
    let (message_type, message) = match problem {
        None => (b"status".to_vec(), b"OK".to_vec()),
        Some(problem) => (b"error".to_vec(), problem.into_bytes()),
    };
    CommandExecutionResult::ResultSet(TextResultSet {
        columns: vec![
            column("Table", MYSQL_TYPE_VAR_STRING, 512),
            column("Op", MYSQL_TYPE_VAR_STRING, 40),
            column("Msg_type", MYSQL_TYPE_VAR_STRING, 40),
            column("Msg_text", MYSQL_TYPE_MEDIUM_BLOB, 1_572_864),
        ],
        rows: vec![vec![
            Some(format!("{database}.{table}").into_bytes()),
            Some(operation.as_bytes().to_vec()),
            Some(message_type),
            Some(message),
        ]],
        warnings: 0,
        status_flags,
    })
}

/// Describes each table in the selected database.
///
/// MySQL fills the storage figures from InnoDB's own accounting. This server has
/// none of it, so those columns answer NULL rather than a number invented to
/// look like one — which is a shape MySQL itself produces here, for a view. The
/// columns it can answer honestly it does: the name, the engine it already
/// reports, the row count, the auto-increment counter, and the collation it
/// already claims.
pub(super) fn show_table_status_result_to_execution_result(
    rows: Vec<ShowTableStatusRow>,
    status_flags: u16,
) -> Result<CommandExecutionResult, FrontendErrorKind> {
    if rows.len() > MAX_DISPATCH_RESULT_ROWS {
        return Err(FrontendErrorKind::Internal);
    }
    let rows = rows
        .into_iter()
        .map(|row| {
            vec![
                Some(row.name.into_bytes()),
                Some(b"InnoDB".to_vec()),
                None,
                None,
                Some(row.rows.to_string().into_bytes()),
                None,
                None,
                None,
                None,
                None,
                row.auto_increment
                    .map(|value| value.to_string().into_bytes()),
                None,
                None,
                None,
                Some(row.collation.as_bytes().to_vec()),
                None,
                Some(Vec::new()),
                Some(row.comment.into_bytes()),
            ]
        })
        .collect::<Vec<_>>();
    for row in &rows {
        checked_text_result_row_payload_len(row)?;
    }
    Ok(CommandExecutionResult::ResultSet(TextResultSet {
        columns: show_table_status_columns(),
        rows,
        warnings: 0,
        status_flags,
    }))
}

/// What one `SHOW TABLE STATUS` row can be answered from.
pub(super) struct ShowTableStatusRow {
    pub name: String,
    pub rows: u64,
    pub auto_increment: Option<u64>,
    /// The collation the table was declared with.
    pub collation: &'static str,
    /// The table's `COMMENT`, empty where it has none.
    pub comment: String,
}

/// The eighteen columns, with the shapes measured on MySQL 8.4.11.
fn show_table_status_columns() -> Vec<ColumnDefinitionConfig> {
    [
        ("Name", MYSQL_TYPE_VAR_STRING, 256u32),
        ("Engine", MYSQL_TYPE_VAR_STRING, 256),
        ("Version", MYSQL_TYPE_LONG, 3),
        ("Row_format", MYSQL_TYPE_STRING, 40),
        ("Rows", MYSQL_TYPE_LONGLONG, 21),
        ("Avg_row_length", MYSQL_TYPE_LONGLONG, 21),
        ("Data_length", MYSQL_TYPE_LONGLONG, 21),
        ("Max_data_length", MYSQL_TYPE_LONGLONG, 21),
        ("Index_length", MYSQL_TYPE_LONGLONG, 21),
        ("Data_free", MYSQL_TYPE_LONGLONG, 21),
        ("Auto_increment", MYSQL_TYPE_LONGLONG, 21),
        ("Create_time", MYSQL_TYPE_TIMESTAMP, 19),
        ("Update_time", MYSQL_TYPE_DATETIME, 19),
        ("Check_time", MYSQL_TYPE_DATETIME, 19),
        ("Collation", MYSQL_TYPE_VAR_STRING, 256),
        ("Checksum", MYSQL_TYPE_LONGLONG, 21),
        ("Create_options", MYSQL_TYPE_VAR_STRING, 1024),
        ("Comment", MYSQL_TYPE_BLOB, 24_576),
    ]
    .into_iter()
    .map(|(name, column_type, column_length)| {
        let mut column = ColumnDefinitionConfig::new(name, column_type);
        // Measured over a utf8mb4 connection: the text columns carry the
        // connection's own collation and the numeric and temporal ones the
        // binary collation.
        column.character_set = if matches!(
            column_type,
            MYSQL_TYPE_VAR_STRING | MYSQL_TYPE_STRING | MYSQL_TYPE_BLOB
        ) {
            u16::from(DEFAULT_UTF8MB4_COLLATION)
        } else {
            MYSQL_BINARY_COLLATION
        };
        column.column_length = column_length;
        column
    })
    .collect()
}

fn show_index_columns() -> Vec<ColumnDefinitionConfig> {
    [
        ("Table", MYSQL_TYPE_VAR_STRING, 256u32),
        ("Non_unique", MYSQL_TYPE_LONG, 2),
        ("Key_name", MYSQL_TYPE_VAR_STRING, 256),
        ("Seq_in_index", MYSQL_TYPE_LONG, 10),
        ("Column_name", MYSQL_TYPE_VAR_STRING, 256),
        ("Collation", MYSQL_TYPE_VAR_STRING, 4),
        ("Cardinality", MYSQL_TYPE_LONGLONG, 21),
        ("Sub_part", MYSQL_TYPE_LONGLONG, 21),
        // Measured: MySQL reports the column it never fills as the null type.
        ("Packed", MYSQL_TYPE_NULL, 0),
        ("Null", MYSQL_TYPE_VAR_STRING, 12),
        ("Index_type", MYSQL_TYPE_VAR_STRING, 44),
        ("Comment", MYSQL_TYPE_VAR_STRING, 32),
        ("Index_comment", MYSQL_TYPE_VAR_STRING, 8192),
        ("Visible", MYSQL_TYPE_VAR_STRING, 12),
        ("Expression", MYSQL_TYPE_BLOB, abs_expression_length()),
    ]
    .into_iter()
    .map(|(name, column_type, column_length)| {
        let mut column = ColumnDefinitionConfig::new(name, column_type);
        column.character_set = if matches!(
            column_type,
            MYSQL_TYPE_LONGLONG | MYSQL_TYPE_LONG | MYSQL_TYPE_NULL
        ) {
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
    MAX_CATALOG_VALUE_LENGTH as u32
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
    if result.table().len() > MAX_CATALOG_VALUE_LENGTH
        || result.create_statement().len() > MAX_CATALOG_VALUE_LENGTH
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

pub(super) fn show_triggers_result(
    triggers: Vec<MySqlTriggerMetadata>,
    status_flags: u16,
) -> Result<CommandExecutionResult, FrontendErrorKind> {
    if triggers.len() > MAX_DISPATCH_RESULT_ROWS {
        return Err(FrontendErrorKind::Internal);
    }
    let mut rows = Vec::with_capacity(triggers.len());
    for trigger in triggers {
        let fields = [
            trigger.name,
            trigger.event.written().to_owned(),
            trigger.table,
            trigger.statement,
            trigger.timing.written().to_owned(),
            trigger.creator.created_at,
            trigger.creator.sql_mode,
            format!("{}@%", trigger.creator.username),
            trigger.creator.character_set_client,
            trigger.creator.collation_connection,
            trigger.creator.database_collation,
        ];
        if fields
            .iter()
            .any(|field| field.len() > MAX_CATALOG_VALUE_LENGTH)
        {
            return Err(FrontendErrorKind::Internal);
        }
        rows.push(
            fields
                .into_iter()
                .map(|field| Some(field.into_bytes()))
                .collect(),
        );
    }
    Ok(CommandExecutionResult::ResultSet(TextResultSet {
        columns: show_triggers_columns(),
        rows,
        warnings: 0,
        status_flags,
    }))
}

fn show_triggers_columns() -> Vec<ColumnDefinitionConfig> {
    let required = MYSQL_NOT_NULL_FLAG | MYSQL_NO_DEFAULT_VALUE_FLAG;
    let fields = [
        ("Trigger", "triggers", MYSQL_TYPE_VAR_STRING, 256, required),
        (
            "Event",
            "triggers",
            MYSQL_TYPE_STRING,
            24,
            required | MYSQL_BINARY_FLAG | MYSQL_ENUM_FLAG,
        ),
        (
            "Table",
            "tables",
            MYSQL_TYPE_VAR_STRING,
            256,
            required | MYSQL_BINARY_FLAG,
        ),
        (
            "Statement",
            "triggers",
            MYSQL_TYPE_BLOB,
            u32::MAX,
            required | MYSQL_BLOB_FLAG | MYSQL_BINARY_FLAG,
        ),
        (
            "Timing",
            "triggers",
            MYSQL_TYPE_STRING,
            24,
            required | MYSQL_BINARY_FLAG | MYSQL_ENUM_FLAG,
        ),
        (
            "Created",
            "triggers",
            MYSQL_TYPE_TIMESTAMP,
            22,
            required | MYSQL_BINARY_FLAG,
        ),
        (
            "sql_mode",
            "triggers",
            MYSQL_TYPE_STRING,
            2080,
            required | MYSQL_BINARY_FLAG | MYSQL_SET_FLAG,
        ),
        (
            "Definer",
            "triggers",
            MYSQL_TYPE_VAR_STRING,
            1152,
            required | MYSQL_BINARY_FLAG,
        ),
        (
            "character_set_client",
            "character_sets",
            MYSQL_TYPE_VAR_STRING,
            256,
            required,
        ),
        (
            "collation_connection",
            "collations",
            MYSQL_TYPE_VAR_STRING,
            256,
            required,
        ),
        (
            "Database Collation",
            "collations",
            MYSQL_TYPE_VAR_STRING,
            256,
            required,
        ),
    ];
    fields
        .into_iter()
        .map(|(name, original_table, kind, length, flags)| {
            let mut column = ColumnDefinitionConfig::new(name, kind);
            "TRIGGERS".clone_into(&mut column.table);
            original_table.clone_into(&mut column.original_table);
            name.clone_into(&mut column.original_name);
            column.character_set = if name == "Created" {
                MYSQL_BINARY_COLLATION
            } else {
                u16::from(DEFAULT_UTF8MB4_COLLATION)
            };
            column.column_length = length;
            column.decimals = if name == "Created" { 2 } else { 0 };
            column.flags = flags;
            column
        })
        .collect()
}

pub(super) fn show_create_trigger_result(
    trigger: MySqlTriggerMetadata,
    status_flags: u16,
) -> Result<CommandExecutionResult, FrontendErrorKind> {
    let fields = [
        trigger.name,
        trigger.creator.sql_mode,
        trigger.create_statement,
        trigger.creator.character_set_client,
        trigger.creator.collation_connection,
        trigger.creator.database_collation,
        trigger.creator.created_at,
    ];
    if fields
        .iter()
        .any(|field| field.len() > MAX_CATALOG_VALUE_LENGTH)
    {
        return Err(FrontendErrorKind::Internal);
    }
    let lengths = [
        768,
        u32::try_from(fields[1].len().saturating_mul(4)).unwrap_or(u32::MAX),
        u32::try_from(fields[2].len().max(1024).saturating_mul(4)).unwrap_or(u32::MAX),
        128,
        128,
        128,
        0,
    ];
    let names = [
        "Trigger",
        "sql_mode",
        "SQL Original Statement",
        "character_set_client",
        "collation_connection",
        "Database Collation",
        "Created",
    ];
    let columns = names
        .into_iter()
        .zip(lengths)
        .enumerate()
        .map(|(index, (name, length))| {
            let mut column = ColumnDefinitionConfig::new(
                name,
                if index == 6 {
                    MYSQL_TYPE_TIMESTAMP
                } else {
                    MYSQL_TYPE_VAR_STRING
                },
            );
            column.character_set = if index == 6 {
                MYSQL_BINARY_COLLATION
            } else {
                u16::from(DEFAULT_UTF8MB4_COLLATION)
            };
            column.column_length = length;
            column.decimals = if index == 6 { 0 } else { 31 };
            column.flags = if index == 6 {
                MYSQL_NOT_NULL_FLAG | MYSQL_BINARY_FLAG
            } else if index == 2 {
                0
            } else {
                MYSQL_NOT_NULL_FLAG
            };
            column
        })
        .collect();
    Ok(CommandExecutionResult::ResultSet(TextResultSet {
        columns,
        rows: vec![fields
            .into_iter()
            .map(|field| Some(field.into_bytes()))
            .collect()],
        warnings: 0,
        status_flags,
    }))
}

pub(super) fn show_create_view_result(
    view: MySqlViewMetadata,
    status_flags: u16,
) -> Result<CommandExecutionResult, FrontendErrorKind> {
    let fields = [
        view.name,
        view.create_statement,
        view.creator.character_set_client,
        view.creator.collation_connection,
    ];
    if fields
        .iter()
        .any(|field| field.len() > MAX_CATALOG_VALUE_LENGTH)
    {
        return Err(FrontendErrorKind::Internal);
    }
    let lengths = [
        256,
        u32::try_from(fields[1].len().max(1024).saturating_mul(4)).unwrap_or(u32::MAX),
        128,
        128,
    ];
    let columns = [
        "View",
        "Create View",
        "character_set_client",
        "collation_connection",
    ]
    .into_iter()
    .zip(lengths)
    .map(|(name, length)| {
        let mut column = ColumnDefinitionConfig::new(name, MYSQL_TYPE_VAR_STRING);
        column.character_set = u16::from(DEFAULT_UTF8MB4_COLLATION);
        column.column_length = length;
        column.decimals = 31;
        column.flags = MYSQL_NOT_NULL_FLAG;
        column
    })
    .collect();
    Ok(CommandExecutionResult::ResultSet(TextResultSet {
        columns,
        rows: vec![fields
            .into_iter()
            .map(|field| Some(field.into_bytes()))
            .collect()],
        warnings: 0,
        status_flags,
    }))
}

/// Builds the rows `SHOW COLUMNS` reports, with or without the `FULL` extras.
///
/// Measured on MySQL 8.4.11: `FULL` puts `Collation` third and appends
/// `Privileges` and `Comment`. The collation is the text one for a `VARCHAR`,
/// `CHAR` or `TEXT` and NULL for every other type, a `VARBINARY` and a `BLOB`
/// included. The comment is the text the column was declared with, empty where
/// it was declared with none.
///
/// The connected user's effective table grants supply `Privileges`. This
/// frontend has no separate column grants.
pub(super) fn show_columns_result(
    columns: Vec<MySqlColumnMetadata>,
    status_flags: u16,
    full: bool,
    privileges: &[u8],
) -> Result<CommandExecutionResult, FrontendErrorKind> {
    if columns.len() > MAX_DISPATCH_RESULT_ROWS {
        return Err(FrontendErrorKind::Internal);
    }

    let mut retained_bytes = 0usize;
    let mut rows = Vec::with_capacity(columns.len());
    for column in columns {
        if column.name().len() > MAX_CATALOG_VALUE_LENGTH
            || column.extra().len() > MAX_CATALOG_VALUE_LENGTH
            || column.comment().len() > MAX_CATALOG_VALUE_LENGTH
        {
            return Err(FrontendErrorKind::Internal);
        }
        let mut row = vec![
            Some(column.name().as_bytes().to_vec()),
            Some(show_column_type_name(&column)?),
        ];
        if full {
            row.push(column.collation_name().map(|name| name.as_bytes().to_vec()));
        }
        row.extend([
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
            show_column_default_value(&column)?,
            Some(show_column_extra(column.extra())?),
        ]);
        if full {
            row.push(Some(privileges.to_vec()));
            row.push(Some(column.comment().as_bytes().to_vec()));
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
        columns: if full {
            show_full_columns_columns()
        } else {
            show_columns_columns()
        },
        rows,
        warnings: 0,
        status_flags,
    }))
}

pub(super) fn show_columns_columns() -> Vec<ColumnDefinitionConfig> {
    show_columns_column_definitions(&[
        ("Field", MYSQL_TYPE_VAR_STRING, 256),
        ("Type", MYSQL_TYPE_BLOB, 67_108_860),
        ("Null", MYSQL_TYPE_VAR_STRING, 12),
        ("Key", MYSQL_TYPE_STRING, 12),
        ("Default", MYSQL_TYPE_BLOB, 262_140),
        ("Extra", MYSQL_TYPE_VAR_STRING, 1024),
    ])
}

/// Measured over a utf8mb4 connection: `Collation` is 256 wide and sits
/// third, `Privileges` is 616 and `Comment` follows it.
pub(super) fn show_full_columns_columns() -> Vec<ColumnDefinitionConfig> {
    show_columns_column_definitions(&[
        ("Field", MYSQL_TYPE_VAR_STRING, 256),
        ("Type", MYSQL_TYPE_BLOB, 67_108_860),
        ("Collation", MYSQL_TYPE_VAR_STRING, 256),
        ("Null", MYSQL_TYPE_VAR_STRING, 12),
        ("Key", MYSQL_TYPE_STRING, 12),
        ("Default", MYSQL_TYPE_BLOB, 262_140),
        ("Extra", MYSQL_TYPE_VAR_STRING, 1024),
        ("Privileges", MYSQL_TYPE_VAR_STRING, 616),
        ("Comment", MYSQL_TYPE_BLOB, 24_576),
    ])
}

/// Measured over a utf8mb4 connection: `Type` and `Default` are blobs rather
/// than var-strings and `Key` is a fixed-width string, and every one of them
/// carries the connection's own collation.
fn show_columns_column_definitions(names: &[(&str, u8, u32)]) -> Vec<ColumnDefinitionConfig> {
    names
        .iter()
        .copied()
        .map(|(name, column_type, column_length)| {
            let mut column = ColumnDefinitionConfig::new(name, column_type);
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
    if payload_len > crate::MAX_RESPONSE_PACKET_PAYLOAD_LENGTH {
        return Err(FrontendErrorKind::Internal);
    }
    Ok(payload_len)
}

/// Renders the type the way MySQL 8.4.11 reports it here, lower case and
/// carrying the declared length where the type has one.
fn show_column_type_name(column: &MySqlColumnMetadata) -> Result<Vec<u8>, FrontendErrorKind> {
    turso_mysql::show_create_table::type_name(column)
        .map(String::into_bytes)
        .ok_or(FrontendErrorKind::Internal)
}

pub(super) fn show_column_extra(extra: &str) -> Result<Vec<u8>, FrontendErrorKind> {
    match extra {
        "" => Ok(Vec::new()),
        "AUTO_INCREMENT" => Ok(b"auto_increment".to_vec()),
        // Measured on MySQL 8.4.11: a column defaulting to the moment it is
        // written reports this, in capitals where `auto_increment` is not.
        "DEFAULT_GENERATED" => Ok(b"DEFAULT_GENERATED".to_vec()),
        // Measured: the words are reported in lower case where
        // `DEFAULT_GENERATED` is in capitals, the two run together where the
        // column carries both, and a column holding fractional seconds names
        // its digits — `on update CURRENT_TIMESTAMP(6)`.
        extra
            if extra
                .strip_prefix("DEFAULT_GENERATED ")
                .unwrap_or(extra)
                .strip_prefix("on update CURRENT_TIMESTAMP")
                .is_some_and(|digits| {
                    digits.is_empty() || matches!(digits.as_bytes(), [b'(', b'1'..=b'6', b')'])
                }) =>
        {
            Ok(extra.as_bytes().to_vec())
        }
        _ => Err(FrontendErrorKind::Internal),
    }
}

pub(super) fn show_column_default_value(
    column: &MySqlColumnMetadata,
) -> Result<Option<Vec<u8>>, FrontendErrorKind> {
    // Measured on MySQL 8.4.11: a column holding fractional seconds names
    // its digits, `CURRENT_TIMESTAMP(3)`.
    if matches!(column.default_value(), Some(MySqlColumnDefault::Moment)) {
        return Ok(Some(
            turso_mysql::show_create_table::the_moment(column.temporal_precision()).into_bytes(),
        ));
    }
    if column.type_name() == "BIT" {
        if let Some(default @ MySqlColumnDefault::Integer { .. }) = column.default_value() {
            return turso_mysql::show_create_table::bit_literal(default)
                .map(|literal| Some(literal.as_bytes().to_vec()))
                .ok_or(FrontendErrorKind::Internal);
        }
    }
    show_default_at_scale(column.default_value(), column.decimal_size())
}

/// The same, told the scale rather than the column, so a default can be read
/// on its own.
pub(super) fn show_default_at_scale(
    default_value: Option<&MySqlColumnDefault>,
    scale: Option<(u32, u32)>,
) -> Result<Option<Vec<u8>>, FrontendErrorKind> {
    let Some(default_value) = default_value else {
        return Ok(None);
    };
    let written;
    let value = match default_value {
        MySqlColumnDefault::Null => return Ok(None),
        // A column that holds its values at a scale of its own reports its
        // default at that scale — measured, a `DECIMAL(10,2)` written
        // `DEFAULT 3` reports `3.00`.
        MySqlColumnDefault::Integer { value, .. } => {
            written =
                turso_mysql::show_create_table::at_the_columns_scale(scale, &value.to_string());
            written.as_bytes()
        }
        MySqlColumnDefault::Number(text) => {
            written = turso_mysql::show_create_table::at_the_columns_scale(scale, text);
            written.as_bytes()
        }
        MySqlColumnDefault::Text(text) => text.as_bytes(),
        // Measured on MySQL 8.4.11: `COLUMN_DEFAULT` reads `CURRENT_TIMESTAMP`
        // for one of these, the same words `SHOW COLUMNS` reports.
        MySqlColumnDefault::Moment => b"CURRENT_TIMESTAMP",
        // Measured on MySQL 8.4.11: an expression default reads as the call
        // without its parentheses.
        MySqlColumnDefault::MomentCall => b"now()",
        MySqlColumnDefault::Boolean(value) => {
            return Ok(Some(if *value { b"1".to_vec() } else { b"0".to_vec() }));
        }
    };
    if value.len() > MAX_CATALOG_VALUE_LENGTH {
        return Err(FrontendErrorKind::Internal);
    }
    Ok(Some(value.to_vec()))
}

/// Names the one column `SHOW TABLES` answers.
///
/// Measured on MySQL 8.4.11: a `LIKE` puts the pattern in the column name, so
/// `SHOW TABLES LIKE 'alpha%'` answers a column called
/// `Tables_in_probe (alpha%)`.
pub(super) fn show_tables_column(database: &str, pattern: Option<&str>) -> ColumnDefinitionConfig {
    let name = match pattern {
        Some(pattern) => format!("Tables_in_{database} ({pattern})"),
        None => format!("Tables_in_{database}"),
    };
    let mut column = ColumnDefinitionConfig::new(name, MYSQL_TYPE_VAR_STRING);
    column.character_set = u16::from(DEFAULT_UTF8MB4_COLLATION);
    column.column_length = 256;
    column
}

pub(super) fn connector_j_tables_result(
    schema: &str,
    mut tables: Vec<turso_mysql::MySqlTable>,
    status_flags: u16,
) -> Result<CommandExecutionResult, FrontendErrorKind> {
    if tables.len() > MAX_DISPATCH_RESULT_ROWS {
        return Err(FrontendErrorKind::Internal);
    }
    tables.sort_unstable_by(|left, right| {
        let kind = |table: &turso_mysql::MySqlTable| match table.kind() {
            MySqlTableKind::BaseTable => 0,
            MySqlTableKind::View => 1,
        };
        kind(left)
            .cmp(&kind(right))
            .then_with(|| left.name().cmp(right.name()))
    });
    let rows = tables
        .into_iter()
        .map(|table| {
            vec![
                Some(schema.as_bytes().to_vec()),
                None,
                Some(table.name().as_bytes().to_vec()),
                Some(
                    match table.kind() {
                        MySqlTableKind::BaseTable => b"TABLE".as_slice(),
                        MySqlTableKind::View => b"VIEW".as_slice(),
                    }
                    .to_vec(),
                ),
                Some(Vec::new()),
                None,
                None,
                None,
                None,
                None,
            ]
        })
        .collect();
    Ok(CommandExecutionResult::ResultSet(TextResultSet {
        columns: connector_j_tables_columns(),
        rows,
        warnings: 0,
        status_flags,
    }))
}

pub(super) fn connector_j_information_schema_collation_result(
    status_flags: u16,
) -> CommandExecutionResult {
    let mut column = connector_j_information_schema_column(
        "DEFAULT_COLLATION_NAME",
        "SCHEMATA",
        "SCHEMATA",
        MYSQL_TYPE_VAR_STRING,
        64,
        MYSQL_NOT_NULL_FLAG
            | MYSQL_UNIQUE_KEY_FLAG
            | MYSQL_NO_DEFAULT_VALUE_FLAG
            | MYSQL_PART_KEY_FLAG,
    );
    "DEFAULT_COLLATION_NAME".clone_into(&mut column.original_name);
    CommandExecutionResult::ResultSet(TextResultSet {
        columns: vec![column],
        rows: vec![vec![Some(b"utf8mb3_general_ci".to_vec())]],
        warnings: 0,
        status_flags,
    })
}

pub(super) fn connector_j_catalogs_result(
    mut databases: Vec<String>,
    status_flags: u16,
) -> Result<CommandExecutionResult, FrontendErrorKind> {
    if !databases
        .iter()
        .any(|database| database.eq_ignore_ascii_case("information_schema"))
    {
        databases.push("information_schema".to_owned());
    }
    if databases.len() > MAX_DISPATCH_RESULT_ROWS {
        return Err(FrontendErrorKind::Internal);
    }
    databases.sort_unstable();
    let mut column = connector_j_information_schema_column(
        "TABLE_CAT",
        "SCHEMATA",
        "schemata",
        MYSQL_TYPE_VAR_STRING,
        64,
        MYSQL_NOT_NULL_FLAG | MYSQL_BINARY_FLAG | MYSQL_NO_DEFAULT_VALUE_FLAG,
    );
    "SCHEMA_NAME".clone_into(&mut column.original_name);
    Ok(CommandExecutionResult::ResultSet(TextResultSet {
        columns: vec![column],
        rows: databases
            .into_iter()
            .map(|database| vec![Some(database.into_bytes())])
            .collect(),
        warnings: 0,
        status_flags,
    }))
}

pub(super) fn connector_j_schemas_result(status_flags: u16) -> CommandExecutionResult {
    let mut schema = connector_j_information_schema_column(
        "TABLE_SCHEM",
        "SCHEMATA",
        "SCHEMATA",
        MYSQL_TYPE_VAR_STRING,
        64,
        MYSQL_NOT_NULL_FLAG | MYSQL_BINARY_FLAG | MYSQL_NO_DEFAULT_VALUE_FLAG | MYSQL_PART_KEY_FLAG,
    );
    "SCHEMA_NAME".clone_into(&mut schema.original_name);
    let mut catalog = connector_j_information_schema_column(
        "TABLE_CATALOG",
        "SCHEMATA",
        "SCHEMATA",
        MYSQL_TYPE_VAR_STRING,
        64,
        MYSQL_NOT_NULL_FLAG
            | MYSQL_UNIQUE_KEY_FLAG
            | MYSQL_BINARY_FLAG
            | MYSQL_NO_DEFAULT_VALUE_FLAG
            | MYSQL_PART_KEY_FLAG,
    );
    "CATALOG_NAME".clone_into(&mut catalog.original_name);
    CommandExecutionResult::ResultSet(TextResultSet {
        columns: vec![schema, catalog],
        rows: Vec::new(),
        warnings: 0,
        status_flags,
    })
}

pub(super) fn connector_j_reserved_keywords_result(status_flags: u16) -> CommandExecutionResult {
    let mut column = connector_j_information_schema_column(
        "WORD",
        "KEYWORDS",
        "KEYWORDS",
        MYSQL_TYPE_VAR_STRING,
        128,
        0,
    );
    "WORD".clone_into(&mut column.original_name);
    CommandExecutionResult::ResultSet(TextResultSet {
        columns: vec![column],
        rows: super::reserved_keywords_84::WORDS
            .iter()
            .map(|word| vec![Some(word.as_bytes().to_vec())])
            .collect(),
        warnings: 0,
        status_flags,
    })
}

fn connector_j_tables_columns() -> Vec<ColumnDefinitionConfig> {
    let mut catalog = connector_j_information_schema_column(
        "TABLE_CAT",
        "TABLES",
        "schemata",
        MYSQL_TYPE_VAR_STRING,
        64,
        MYSQL_NOT_NULL_FLAG | MYSQL_BINARY_FLAG | MYSQL_NO_DEFAULT_VALUE_FLAG,
    );
    "TABLE_SCHEMA".clone_into(&mut catalog.original_name);
    let mut table_name = connector_j_information_schema_column(
        "TABLE_NAME",
        "TABLES",
        "tables",
        MYSQL_TYPE_VAR_STRING,
        64,
        MYSQL_NOT_NULL_FLAG | MYSQL_BINARY_FLAG | MYSQL_NO_DEFAULT_VALUE_FLAG,
    );
    "TABLE_NAME".clone_into(&mut table_name.original_name);
    let table_type = connector_j_information_schema_column(
        "TABLE_TYPE",
        "",
        "",
        MYSQL_TYPE_VAR_STRING,
        15,
        MYSQL_NOT_NULL_FLAG | MYSQL_BINARY_FLAG,
    );
    let remarks = connector_j_information_schema_column(
        "REMARKS",
        "TABLES",
        "",
        MYSQL_TYPE_BLOB,
        6144,
        MYSQL_BLOB_FLAG,
    );
    let mut columns = vec![
        catalog,
        connector_j_null_column("TABLE_SCHEM"),
        table_name,
        table_type,
        remarks,
    ];
    columns.extend([
        connector_j_null_column("TYPE_CAT"),
        connector_j_null_column("TYPE_SCHEM"),
        connector_j_null_column("TYPE_NAME"),
        connector_j_null_column("SELF_REFERENCING_COL_NAME"),
        connector_j_null_column("REF_GENERATION"),
    ]);
    columns
}

fn connector_j_information_schema_column(
    name: &str,
    table: &str,
    original_table: &str,
    column_type: u8,
    column_length: u32,
    flags: u16,
) -> ColumnDefinitionConfig {
    let mut column = ColumnDefinitionConfig::new(name, column_type);
    if !table.is_empty() {
        "information_schema".clone_into(&mut column.schema);
        table.clone_into(&mut column.table);
        original_table.clone_into(&mut column.original_table);
    }
    column.character_set = if matches!(
        column_type,
        MYSQL_TYPE_NULL | MYSQL_TYPE_LONG | MYSQL_TYPE_LONGLONG
    ) {
        MYSQL_BINARY_COLLATION
    } else {
        8
    };
    column.column_length = column_length;
    column.flags = flags;
    column
}

fn connector_j_null_column(name: &str) -> ColumnDefinitionConfig {
    connector_j_information_schema_column(
        name,
        "",
        "",
        MYSQL_TYPE_NULL,
        0,
        MYSQL_BINARY_FLAG | MYSQL_NUM_FLAG,
    )
}

pub(super) fn connector_j_columns_result(
    schema: &str,
    tables: Vec<(String, Vec<(usize, MySqlColumnMetadata)>)>,
    status_flags: u16,
) -> Result<CommandExecutionResult, FrontendErrorKind> {
    let mut rows = Vec::new();
    for (table, columns) in tables {
        for (ordinal, column) in columns {
            if rows.len() >= MAX_DISPATCH_RESULT_ROWS {
                return Err(FrontendErrorKind::Internal);
            }
            let (jdbc_type, type_name, size, decimal_digits, octet_length) =
                connector_j_column_type(&column)?;
            let nullable = column.nullable();
            let text = |value: &str| Some(value.as_bytes().to_vec());
            rows.push(vec![
                text(schema),
                None,
                text(&table),
                text(column.name()),
                connector_j_number(jdbc_type),
                text(&type_name),
                connector_j_number(size),
                connector_j_number(65_535),
                decimal_digits.map(|value| value.to_string().into_bytes()),
                connector_j_number(10),
                connector_j_number(usize::from(nullable)),
                text(column.comment()),
                show_column_default_value(&column)?,
                connector_j_number(0),
                connector_j_number(0),
                octet_length.map(|value| value.to_string().into_bytes()),
                connector_j_number(ordinal + 1),
                text(if nullable { "YES" } else { "NO" }),
                None,
                None,
                None,
                None,
                text(if column.extra().eq_ignore_ascii_case("auto_increment") {
                    "YES"
                } else {
                    "NO"
                }),
                text("NO"),
            ]);
        }
    }
    Ok(CommandExecutionResult::ResultSet(TextResultSet {
        columns: connector_j_columns_columns(),
        rows,
        warnings: 0,
        status_flags,
    }))
}

fn connector_j_number(value: impl ToString) -> Option<Vec<u8>> {
    Some(value.to_string().into_bytes())
}

type ConnectorJColumnType = (i32, String, u32, Option<u32>, Option<u32>);

fn connector_j_column_type(
    column: &MySqlColumnMetadata,
) -> Result<ConnectorJColumnType, FrontendErrorKind> {
    let rendered = show_column_type_name(column)?;
    let base = the_type_without_its_own_words(&rendered);
    let unsigned = rendered.ends_with(b" unsigned");
    let (jdbc_type, name, size, digits, octets) = match base {
        b"int" | b"integer" => (4, "INT", 10, None, None),
        b"bigint" => (-5, "BIGINT", if unsigned { 20 } else { 19 }, None, None),
        b"smallint" => (5, "SMALLINT", 5, None, None),
        b"mediumint" => (4, "MEDIUMINT", if unsigned { 8 } else { 7 }, None, None),
        b"tinyint" if !unsigned && rendered.windows(3).any(|window| window == b"(1)") => {
            (-7, "BIT", 1, None, None)
        }
        b"tinyint" => (-6, "TINYINT", 3, None, None),
        b"varchar" => {
            let length = column
                .character_length()
                .ok_or(FrontendErrorKind::Unsupported)?;
            let width = connector_j_character_octet_width(column)?;
            (12, "VARCHAR", length, None, length.checked_mul(width))
        }
        b"char" => {
            let length = column
                .character_length()
                .ok_or(FrontendErrorKind::Unsupported)?;
            let width = connector_j_character_octet_width(column)?;
            (1, "CHAR", length, None, length.checked_mul(width))
        }
        b"text" => (-1, "TEXT", 65_535, None, Some(65_535)),
        b"decimal" => {
            let (precision, scale) = column
                .decimal_size()
                .ok_or(FrontendErrorKind::Unsupported)?;
            (3, "DECIMAL", precision, Some(scale), None)
        }
        b"timestamp" | b"datetime" => {
            let precision = connector_j_temporal_precision(&rendered)?;
            let size = 19 + if precision > 0 { precision + 1 } else { 0 };
            (
                93,
                if base == b"timestamp" {
                    "TIMESTAMP"
                } else {
                    "DATETIME"
                },
                size,
                None,
                None,
            )
        }
        b"date" => (91, "DATE", 10, None, None),
        b"time" => {
            let precision = connector_j_temporal_precision(&rendered)?;
            let size = 8 + if precision > 0 { precision + 1 } else { 0 };
            (92, "TIME", size, None, None)
        }
        _ => return Err(FrontendErrorKind::Unsupported),
    };
    let name = if unsigned {
        format!("{name} UNSIGNED")
    } else {
        name.to_owned()
    };
    Ok((jdbc_type, name, size, digits, octets))
}

fn connector_j_character_octet_width(
    column: &MySqlColumnMetadata,
) -> Result<u32, FrontendErrorKind> {
    match column.collation_name() {
        None => Ok(4),
        Some(name) if name.starts_with("utf8mb4_") => Ok(4),
        Some(name) if name.starts_with("utf8mb3_") => Ok(3),
        Some(name) if name.starts_with("latin1_") || name.starts_with("ascii_") => Ok(1),
        Some(_) => Err(FrontendErrorKind::Unsupported),
    }
}

fn connector_j_temporal_precision(rendered: &[u8]) -> Result<u32, FrontendErrorKind> {
    let Some(open) = rendered.iter().position(|byte| *byte == b'(') else {
        return Ok(0);
    };
    let close = rendered
        .iter()
        .position(|byte| *byte == b')')
        .ok_or(FrontendErrorKind::Unsupported)?;
    std::str::from_utf8(&rendered[open + 1..close])
        .ok()
        .and_then(|value| value.parse::<u32>().ok())
        .filter(|precision| *precision <= 6)
        .ok_or(FrontendErrorKind::Unsupported)
}

fn connector_j_columns_columns() -> Vec<ColumnDefinitionConfig> {
    let specs = [
        (
            "TABLE_SCHEMA",
            "COLUMNS",
            "COLUMNS",
            MYSQL_TYPE_VAR_STRING,
            64,
            MYSQL_NOT_NULL_FLAG
                | MYSQL_BINARY_FLAG
                | MYSQL_NO_DEFAULT_VALUE_FLAG
                | MYSQL_PART_KEY_FLAG,
        ),
        (
            "NULL",
            "",
            "",
            MYSQL_TYPE_NULL,
            0,
            MYSQL_BINARY_FLAG | MYSQL_NUM_FLAG,
        ),
        (
            "TABLE_NAME",
            "COLUMNS",
            "COLUMNS",
            MYSQL_TYPE_VAR_STRING,
            64,
            MYSQL_NOT_NULL_FLAG
                | MYSQL_BINARY_FLAG
                | MYSQL_NO_DEFAULT_VALUE_FLAG
                | MYSQL_PART_KEY_FLAG,
        ),
        (
            "COLUMN_NAME",
            "COLUMNS",
            "COLUMNS",
            MYSQL_TYPE_VAR_STRING,
            64,
            0,
        ),
        (
            "DATA_TYPE",
            "",
            "",
            MYSQL_TYPE_LONGLONG,
            5,
            MYSQL_NOT_NULL_FLAG | MYSQL_BINARY_FLAG | MYSQL_NUM_FLAG,
        ),
        (
            "TYPE_NAME",
            "",
            "",
            MYSQL_TYPE_LONG_BLOB,
            50_331_645,
            MYSQL_BINARY_FLAG,
        ),
        ("COLUMN_SIZE", "", "", MYSQL_TYPE_VAR_STRING, 21, 0),
        (
            "BUFFER_LENGTH",
            "",
            "",
            MYSQL_TYPE_LONGLONG,
            6,
            MYSQL_NOT_NULL_FLAG | MYSQL_BINARY_FLAG | MYSQL_NUM_FLAG,
        ),
        ("DECIMAL_DIGITS", "", "", MYSQL_TYPE_VAR_STRING, 10, 0),
        (
            "NUM_PREC_RADIX",
            "",
            "",
            MYSQL_TYPE_LONGLONG,
            3,
            MYSQL_NOT_NULL_FLAG | MYSQL_BINARY_FLAG | MYSQL_NUM_FLAG,
        ),
        (
            "NULLABLE",
            "",
            "",
            MYSQL_TYPE_LONGLONG,
            2,
            MYSQL_NOT_NULL_FLAG | MYSQL_BINARY_FLAG | MYSQL_NUM_FLAG,
        ),
        (
            "REMARKS",
            "COLUMNS",
            "COLUMNS",
            MYSQL_TYPE_VAR_STRING,
            2048,
            MYSQL_NOT_NULL_FLAG | MYSQL_BINARY_FLAG,
        ),
        (
            "COLUMN_DEF",
            "COLUMNS",
            "COLUMNS",
            MYSQL_TYPE_BLOB,
            65_535,
            MYSQL_BLOB_FLAG | MYSQL_BINARY_FLAG,
        ),
        (
            "SQL_DATA_TYPE",
            "",
            "",
            MYSQL_TYPE_LONGLONG,
            2,
            MYSQL_NOT_NULL_FLAG | MYSQL_BINARY_FLAG | MYSQL_NUM_FLAG,
        ),
        (
            "SQL_DATETIME_SUB",
            "",
            "",
            MYSQL_TYPE_LONGLONG,
            2,
            MYSQL_NOT_NULL_FLAG | MYSQL_BINARY_FLAG | MYSQL_NUM_FLAG,
        ),
        (
            "CHAR_OCTET_LENGTH",
            "",
            "",
            MYSQL_TYPE_LONGLONG,
            21,
            MYSQL_BINARY_FLAG | MYSQL_NUM_FLAG,
        ),
        (
            "ORDINAL_POSITION",
            "COLUMNS",
            "COLUMNS",
            MYSQL_TYPE_LONG,
            10,
            MYSQL_NOT_NULL_FLAG
                | MYSQL_UNSIGNED_FLAG
                | MYSQL_NO_DEFAULT_VALUE_FLAG
                | MYSQL_NUM_FLAG
                | MYSQL_PART_KEY_FLAG,
        ),
        (
            "IS_NULLABLE",
            "COLUMNS",
            "COLUMNS",
            MYSQL_TYPE_VAR_STRING,
            3,
            MYSQL_NOT_NULL_FLAG,
        ),
        (
            "SCOPE_CATALOG",
            "",
            "",
            MYSQL_TYPE_NULL,
            0,
            MYSQL_BINARY_FLAG | MYSQL_NUM_FLAG,
        ),
        (
            "SCOPE_SCHEMA",
            "",
            "",
            MYSQL_TYPE_NULL,
            0,
            MYSQL_BINARY_FLAG | MYSQL_NUM_FLAG,
        ),
        (
            "SCOPE_TABLE",
            "",
            "",
            MYSQL_TYPE_NULL,
            0,
            MYSQL_BINARY_FLAG | MYSQL_NUM_FLAG,
        ),
        (
            "SOURCE_DATA_TYPE",
            "",
            "",
            MYSQL_TYPE_NULL,
            0,
            MYSQL_BINARY_FLAG | MYSQL_NUM_FLAG,
        ),
        (
            "IS_AUTOINCREMENT",
            "",
            "",
            MYSQL_TYPE_VAR_STRING,
            3,
            MYSQL_NOT_NULL_FLAG,
        ),
        (
            "IS_GENERATEDCOLUMN",
            "",
            "",
            MYSQL_TYPE_VAR_STRING,
            3,
            MYSQL_NOT_NULL_FLAG,
        ),
    ];
    specs
        .into_iter()
        .map(|(name, table, original_table, kind, length, flags)| {
            let mut column = connector_j_information_schema_column(
                name,
                table,
                original_table,
                kind,
                length,
                flags,
            );
            match name {
                "TABLE_SCHEMA" | "TABLE_NAME" | "COLUMN_NAME" | "ORDINAL_POSITION"
                | "IS_NULLABLE" => name,
                "REMARKS" => "COLUMN_COMMENT",
                "COLUMN_DEF" => "COLUMN_DEFAULT",
                _ => "",
            }
            .clone_into(&mut column.original_name);
            if matches!(name, "COLUMN_NAME" | "REMARKS" | "IS_NULLABLE") {
                column.schema.clear();
            }
            column
        })
        .collect()
}

pub(super) fn connector_j_foreign_keys_result(
    rows: Vec<Vec<Option<Vec<u8>>>>,
    exported: bool,
    status_flags: u16,
) -> Result<CommandExecutionResult, FrontendErrorKind> {
    if rows.len() > MAX_DISPATCH_RESULT_ROWS {
        return Err(FrontendErrorKind::Internal);
    }
    let specs = [
        (
            "PKTABLE_CAT",
            "A",
            "KEY_COLUMN_USAGE",
            MYSQL_TYPE_VAR_STRING,
            64,
            MYSQL_BINARY_FLAG,
        ),
        (
            "PKTABLE_SCHEM",
            "",
            "",
            MYSQL_TYPE_NULL,
            0,
            MYSQL_BINARY_FLAG | MYSQL_NUM_FLAG,
        ),
        (
            "PKTABLE_NAME",
            "A",
            "KEY_COLUMN_USAGE",
            MYSQL_TYPE_VAR_STRING,
            64,
            MYSQL_BINARY_FLAG,
        ),
        (
            "PKCOLUMN_NAME",
            "A",
            "KEY_COLUMN_USAGE",
            MYSQL_TYPE_VAR_STRING,
            64,
            0,
        ),
        (
            "FKTABLE_CAT",
            "A",
            "KEY_COLUMN_USAGE",
            MYSQL_TYPE_VAR_STRING,
            64,
            MYSQL_NOT_NULL_FLAG
                | MYSQL_BINARY_FLAG
                | MYSQL_NO_DEFAULT_VALUE_FLAG
                | MYSQL_PART_KEY_FLAG,
        ),
        (
            "FKTABLE_SCHEM",
            "",
            "",
            MYSQL_TYPE_NULL,
            0,
            MYSQL_BINARY_FLAG | MYSQL_NUM_FLAG,
        ),
        (
            "FKTABLE_NAME",
            "A",
            "KEY_COLUMN_USAGE",
            MYSQL_TYPE_VAR_STRING,
            64,
            MYSQL_NOT_NULL_FLAG
                | MYSQL_BINARY_FLAG
                | MYSQL_NO_DEFAULT_VALUE_FLAG
                | MYSQL_PART_KEY_FLAG,
        ),
        (
            "FKCOLUMN_NAME",
            "A",
            "KEY_COLUMN_USAGE",
            MYSQL_TYPE_VAR_STRING,
            64,
            0,
        ),
        (
            "KEY_SEQ",
            "A",
            "KEY_COLUMN_USAGE",
            MYSQL_TYPE_LONG,
            10,
            MYSQL_NOT_NULL_FLAG | MYSQL_UNSIGNED_FLAG | MYSQL_NUM_FLAG,
        ),
        (
            "UPDATE_RULE",
            "",
            "",
            MYSQL_TYPE_LONGLONG,
            2,
            MYSQL_NOT_NULL_FLAG | MYSQL_BINARY_FLAG | MYSQL_NUM_FLAG,
        ),
        (
            "DELETE_RULE",
            "",
            "",
            MYSQL_TYPE_LONGLONG,
            2,
            MYSQL_NOT_NULL_FLAG | MYSQL_BINARY_FLAG | MYSQL_NUM_FLAG,
        ),
        (
            "FK_NAME",
            "A",
            "KEY_COLUMN_USAGE",
            MYSQL_TYPE_VAR_STRING,
            64,
            0,
        ),
        (
            "PK_NAME",
            if exported { "TC" } else { "R" },
            if exported {
                "TABLE_CONSTRAINTS"
            } else {
                "REFERENTIAL_CONSTRAINTS"
            },
            MYSQL_TYPE_VAR_STRING,
            64,
            0,
        ),
        (
            "DEFERRABILITY",
            "",
            "",
            MYSQL_TYPE_LONGLONG,
            2,
            MYSQL_NOT_NULL_FLAG | MYSQL_BINARY_FLAG | MYSQL_NUM_FLAG,
        ),
    ];
    let columns = specs
        .into_iter()
        .map(|(name, table, original_table, kind, length, flags)| {
            let mut column = connector_j_information_schema_column(
                name,
                table,
                original_table,
                kind,
                length,
                flags,
            );
            match name {
                "PKTABLE_CAT" => "REFERENCED_TABLE_SCHEMA",
                "PKTABLE_NAME" => "REFERENCED_TABLE_NAME",
                "PKCOLUMN_NAME" => "REFERENCED_COLUMN_NAME",
                "FKTABLE_CAT" => "TABLE_SCHEMA",
                "FKTABLE_NAME" => "TABLE_NAME",
                "FKCOLUMN_NAME" => "COLUMN_NAME",
                "KEY_SEQ" => "ORDINAL_POSITION",
                "FK_NAME" => "CONSTRAINT_NAME",
                "PK_NAME" if exported => "CONSTRAINT_NAME",
                "PK_NAME" => "UNIQUE_CONSTRAINT_NAME",
                _ => "",
            }
            .clone_into(&mut column.original_name);
            if name == "FKCOLUMN_NAME" {
                column.schema.clear();
            }
            column
        })
        .collect();
    Ok(CommandExecutionResult::ResultSet(TextResultSet {
        columns,
        rows,
        warnings: 0,
        status_flags,
    }))
}

pub(super) fn connector_j_primary_keys_result(
    rows: Vec<Vec<Option<Vec<u8>>>>,
    status_flags: u16,
) -> Result<CommandExecutionResult, FrontendErrorKind> {
    if rows.len() > MAX_DISPATCH_RESULT_ROWS {
        return Err(FrontendErrorKind::Internal);
    }
    let specs = [
        (
            "TABLE_CAT",
            "STATISTICS",
            MYSQL_TYPE_VAR_STRING,
            64,
            MYSQL_NOT_NULL_FLAG
                | MYSQL_BINARY_FLAG
                | MYSQL_NO_DEFAULT_VALUE_FLAG
                | MYSQL_PART_KEY_FLAG,
        ),
        (
            "TABLE_SCHEM",
            "",
            MYSQL_TYPE_NULL,
            0,
            MYSQL_BINARY_FLAG | MYSQL_NUM_FLAG,
        ),
        (
            "TABLE_NAME",
            "STATISTICS",
            MYSQL_TYPE_VAR_STRING,
            64,
            MYSQL_NOT_NULL_FLAG
                | MYSQL_BINARY_FLAG
                | MYSQL_NO_DEFAULT_VALUE_FLAG
                | MYSQL_PART_KEY_FLAG,
        ),
        ("COLUMN_NAME", "STATISTICS", MYSQL_TYPE_VAR_STRING, 64, 0),
        (
            "KEY_SEQ",
            "STATISTICS",
            MYSQL_TYPE_LONG,
            10,
            MYSQL_NOT_NULL_FLAG
                | MYSQL_PRI_KEY_FLAG
                | MYSQL_UNSIGNED_FLAG
                | MYSQL_NO_DEFAULT_VALUE_FLAG
                | MYSQL_NUM_FLAG
                | MYSQL_PART_KEY_FLAG,
        ),
        ("PK_NAME", "", MYSQL_TYPE_VAR_STRING, 7, MYSQL_NOT_NULL_FLAG),
    ];
    let columns = specs
        .into_iter()
        .map(|(name, table, kind, length, flags)| {
            let mut column =
                connector_j_information_schema_column(name, table, table, kind, length, flags);
            match name {
                "TABLE_CAT" => "TABLE_SCHEMA",
                "TABLE_NAME" | "COLUMN_NAME" => name,
                "KEY_SEQ" => "SEQ_IN_INDEX",
                _ => "",
            }
            .clone_into(&mut column.original_name);
            if name == "COLUMN_NAME" {
                column.schema.clear();
            }
            column
        })
        .collect();
    Ok(CommandExecutionResult::ResultSet(TextResultSet {
        columns,
        rows,
        warnings: 0,
        status_flags,
    }))
}

pub(super) fn connector_j_index_info_result(
    rows: Vec<Vec<Option<Vec<u8>>>>,
    status_flags: u16,
) -> Result<CommandExecutionResult, FrontendErrorKind> {
    if rows.len() > MAX_DISPATCH_RESULT_ROWS {
        return Err(FrontendErrorKind::Internal);
    }
    let specs = [
        (
            "TABLE_CAT",
            "STATISTICS",
            MYSQL_TYPE_VAR_STRING,
            64,
            MYSQL_NOT_NULL_FLAG
                | MYSQL_BINARY_FLAG
                | MYSQL_NO_DEFAULT_VALUE_FLAG
                | MYSQL_PART_KEY_FLAG,
        ),
        (
            "TABLE_SCHEM",
            "",
            MYSQL_TYPE_NULL,
            0,
            MYSQL_BINARY_FLAG | MYSQL_NUM_FLAG,
        ),
        (
            "TABLE_NAME",
            "STATISTICS",
            MYSQL_TYPE_VAR_STRING,
            64,
            MYSQL_NOT_NULL_FLAG
                | MYSQL_BINARY_FLAG
                | MYSQL_NO_DEFAULT_VALUE_FLAG
                | MYSQL_PART_KEY_FLAG,
        ),
        (
            "NON_UNIQUE",
            "STATISTICS",
            MYSQL_TYPE_LONGLONG,
            2,
            MYSQL_NOT_NULL_FLAG | MYSQL_BINARY_FLAG | MYSQL_NUM_FLAG,
        ),
        (
            "INDEX_QUALIFIER",
            "",
            MYSQL_TYPE_NULL,
            0,
            MYSQL_BINARY_FLAG | MYSQL_NUM_FLAG,
        ),
        ("INDEX_NAME", "STATISTICS", MYSQL_TYPE_VAR_STRING, 64, 0),
        (
            "TYPE",
            "",
            MYSQL_TYPE_LONGLONG,
            2,
            MYSQL_NOT_NULL_FLAG | MYSQL_BINARY_FLAG | MYSQL_NUM_FLAG,
        ),
        (
            "ORDINAL_POSITION",
            "STATISTICS",
            MYSQL_TYPE_LONG,
            10,
            MYSQL_NOT_NULL_FLAG
                | MYSQL_PRI_KEY_FLAG
                | MYSQL_UNSIGNED_FLAG
                | MYSQL_NO_DEFAULT_VALUE_FLAG
                | MYSQL_NUM_FLAG
                | MYSQL_PART_KEY_FLAG,
        ),
        ("COLUMN_NAME", "STATISTICS", MYSQL_TYPE_VAR_STRING, 64, 0),
        ("ASC_OR_DESC", "STATISTICS", MYSQL_TYPE_VAR_STRING, 1, 0),
        (
            "CARDINALITY",
            "STATISTICS",
            MYSQL_TYPE_LONGLONG,
            21,
            MYSQL_BINARY_FLAG | MYSQL_NUM_FLAG,
        ),
        (
            "PAGES",
            "",
            MYSQL_TYPE_LONGLONG,
            2,
            MYSQL_NOT_NULL_FLAG | MYSQL_BINARY_FLAG | MYSQL_NUM_FLAG,
        ),
        (
            "FILTER_CONDITION",
            "",
            MYSQL_TYPE_NULL,
            0,
            MYSQL_BINARY_FLAG | MYSQL_NUM_FLAG,
        ),
    ];
    let columns = specs
        .into_iter()
        .map(|(name, table, kind, length, flags)| {
            let mut column =
                connector_j_information_schema_column(name, table, table, kind, length, flags);
            match name {
                "TABLE_CAT" => "TABLE_SCHEMA",
                "TABLE_NAME" | "NON_UNIQUE" | "INDEX_NAME" | "COLUMN_NAME" => name,
                "ORDINAL_POSITION" => "SEQ_IN_INDEX",
                "ASC_OR_DESC" => "COLLATION",
                "CARDINALITY" => "CARDINALITY",
                _ => "",
            }
            .clone_into(&mut column.original_name);
            if matches!(
                name,
                "NON_UNIQUE" | "INDEX_NAME" | "COLUMN_NAME" | "ASC_OR_DESC" | "CARDINALITY"
            ) {
                column.schema.clear();
            }
            column
        })
        .collect();
    Ok(CommandExecutionResult::ResultSet(TextResultSet {
        columns,
        rows,
        warnings: 0,
        status_flags,
    }))
}

/// Answers `SHOW EVENTS`, `SHOW FUNCTION STATUS` and `SHOW PROCEDURE STATUS`,
/// which list stored programs. This server keeps none, so each lists no row.
///
/// The columns were measured on MySQL 8.4.11, original tables and names
/// included, which name the data dictionary's own tables.
pub(super) fn show_stored_programs_result(
    kind: MySqlStoredProgramKind,
    status_flags: u16,
) -> CommandExecutionResult {
    let columns = match kind {
        MySqlStoredProgramKind::Events => show_events_columns(),
        MySqlStoredProgramKind::Functions | MySqlStoredProgramKind::Procedures => {
            show_routine_status_columns()
        }
    };
    CommandExecutionResult::ResultSet(TextResultSet {
        columns,
        rows: Vec::new(),
        warnings: 0,
        status_flags,
    })
}

fn show_events_columns() -> Vec<ColumnDefinitionConfig> {
    let required = MYSQL_NOT_NULL_FLAG | MYSQL_NO_DEFAULT_VALUE_FLAG;
    let key = required | MYSQL_PART_KEY_FLAG;
    let words = |name: &str, length: u32, flags: u16| {
        let mut column = ColumnDefinitionConfig::new(name, MYSQL_TYPE_VAR_STRING);
        column.character_set = u16::from(DEFAULT_UTF8MB4_COLLATION);
        column.column_length = length;
        column.flags = flags;
        column
    };
    let computed = |mut column: ColumnDefinitionConfig| {
        column.decimals = NOT_FIXED_DECIMALS;
        column
    };
    let moment = |name: &str| {
        let mut column = ColumnDefinitionConfig::new(name, MYSQL_TYPE_DATETIME);
        column.character_set = u16::from(DEFAULT_UTF8MB4_COLLATION);
        column.column_length = 76;
        column.flags = MYSQL_BINARY_FLAG;
        column
    };
    let mut interval_field = ColumnDefinitionConfig::new("Interval field", MYSQL_TYPE_STRING);
    interval_field.character_set = u16::from(DEFAULT_UTF8MB4_COLLATION);
    interval_field.column_length = 72;
    interval_field.flags = MYSQL_ENUM_FLAG | MYSQL_BINARY_FLAG;
    let mut originator = ColumnDefinitionConfig::new("Originator", MYSQL_TYPE_LONG);
    originator.character_set = MYSQL_BINARY_COLLATION;
    originator.column_length = 10;
    originator.flags = required | MYSQL_UNSIGNED_FLAG;
    // Each column with the data dictionary table it comes from; the ones MySQL
    // works out rather than reads name none.
    [
        (words("Db", 256, key | MYSQL_BINARY_FLAG), "sch"),
        (words("Name", 256, key), "evt"),
        (
            words(
                "Definer",
                1152,
                key | MYSQL_BINARY_FLAG | MYSQL_MULTIPLE_KEY_FLAG,
            ),
            "evt",
        ),
        (words("Time zone", 256, required | MYSQL_BINARY_FLAG), "evt"),
        (computed(words("Type", 36, MYSQL_NOT_NULL_FLAG)), ""),
        (moment("Execute at"), ""),
        (computed(words("Interval value", 1024, 0)), ""),
        (interval_field, "evt"),
        (moment("Starts"), ""),
        (moment("Ends"), ""),
        (
            computed(words("Status", 84, MYSQL_NOT_NULL_FLAG | MYSQL_BINARY_FLAG)),
            "",
        ),
        (originator, "evt"),
        (
            words("character_set_client", 256, key | MYSQL_UNIQUE_KEY_FLAG),
            "cs_client",
        ),
        (
            words("collation_connection", 256, key | MYSQL_UNIQUE_KEY_FLAG),
            "coll_conn",
        ),
        (
            words("Database Collation", 256, key | MYSQL_UNIQUE_KEY_FLAG),
            "coll_db",
        ),
    ]
    .into_iter()
    .map(|(mut column, original_table)| {
        if !original_table.is_empty() {
            "information_schema".clone_into(&mut column.schema);
        }
        "EVENTS".clone_into(&mut column.table);
        original_table.clone_into(&mut column.original_table);
        column.original_name = column.name.clone();
        column
    })
    .collect()
}

fn show_routine_status_columns() -> Vec<ColumnDefinitionConfig> {
    let required = MYSQL_NOT_NULL_FLAG | MYSQL_NO_DEFAULT_VALUE_FLAG;
    let text = u16::from(DEFAULT_UTF8MB4_COLLATION);
    // Name, original table, type, character set, length, flags.
    let fields: [(&str, &str, u8, u16, u32, u16); 12] = [
        (
            "Db",
            "schemata",
            MYSQL_TYPE_VAR_STRING,
            text,
            256,
            required | MYSQL_BINARY_FLAG,
        ),
        (
            "Name",
            "routines",
            MYSQL_TYPE_VAR_STRING,
            text,
            256,
            required,
        ),
        (
            "Type",
            "routines",
            MYSQL_TYPE_STRING,
            text,
            36,
            required | MYSQL_ENUM_FLAG | MYSQL_BINARY_FLAG,
        ),
        (
            "Language",
            "routines",
            MYSQL_TYPE_VAR_STRING,
            text,
            256,
            MYSQL_NOT_NULL_FLAG | MYSQL_BINARY_FLAG,
        ),
        (
            "Definer",
            "routines",
            MYSQL_TYPE_VAR_STRING,
            text,
            1152,
            required | MYSQL_BINARY_FLAG,
        ),
        (
            "Modified",
            "routines",
            MYSQL_TYPE_TIMESTAMP,
            MYSQL_BINARY_COLLATION,
            19,
            required | MYSQL_BINARY_FLAG,
        ),
        (
            "Created",
            "routines",
            MYSQL_TYPE_TIMESTAMP,
            MYSQL_BINARY_COLLATION,
            19,
            required | MYSQL_BINARY_FLAG,
        ),
        (
            "Security_type",
            "routines",
            MYSQL_TYPE_STRING,
            text,
            28,
            required | MYSQL_ENUM_FLAG | MYSQL_BINARY_FLAG,
        ),
        (
            "Comment",
            "routines",
            MYSQL_TYPE_BLOB,
            text,
            262_140,
            required | MYSQL_BLOB_FLAG | MYSQL_BINARY_FLAG,
        ),
        (
            "character_set_client",
            "character_sets",
            MYSQL_TYPE_VAR_STRING,
            text,
            256,
            required,
        ),
        (
            "collation_connection",
            "collations",
            MYSQL_TYPE_VAR_STRING,
            text,
            256,
            required,
        ),
        (
            "Database Collation",
            "collations",
            MYSQL_TYPE_VAR_STRING,
            text,
            256,
            required,
        ),
    ];
    fields
        .into_iter()
        .map(
            |(name, original_table, kind, character_set, length, flags)| {
                let mut column = ColumnDefinitionConfig::new(name, kind);
                "ROUTINES".clone_into(&mut column.table);
                original_table.clone_into(&mut column.original_table);
                name.clone_into(&mut column.original_name);
                column.character_set = character_set;
                column.column_length = length;
                column.flags = flags;
                column
            },
        )
        .collect()
}

/// Answers `mysqldump`'s question whether a table has histograms. MySQL keeps
/// one only after `ANALYZE TABLE ... UPDATE HISTOGRAM`, which this server
/// refuses, so no table here has one and the answer is no rows.
///
/// Measured on MySQL 8.4.11: `COLUMN_NAME` is the catalog table's own NOT
/// NULL `VAR_STRING` of 256, and the reading a nullable `JSON` of 4294967292
/// with 31 decimals, in the connection's collation.
pub(super) fn histogram_listing_result(
    query: &MySqlHistogramQuery,
    status_flags: u16,
) -> CommandExecutionResult {
    let mut column_name = ColumnDefinitionConfig::new("COLUMN_NAME", MYSQL_TYPE_VAR_STRING);
    "information_schema".clone_into(&mut column_name.schema);
    "COLUMN_STATISTICS".clone_into(&mut column_name.table);
    "COLUMN_STATISTICS".clone_into(&mut column_name.original_table);
    "COLUMN_NAME".clone_into(&mut column_name.original_name);
    column_name.character_set = u16::from(DEFAULT_UTF8MB4_COLLATION);
    column_name.column_length = 256;
    column_name.flags = MYSQL_NOT_NULL_FLAG | MYSQL_NO_DEFAULT_VALUE_FLAG | MYSQL_PART_KEY_FLAG;
    let mut reading = ColumnDefinitionConfig::new(query.reading_column_name(), MYSQL_TYPE_JSON);
    reading.character_set = u16::from(DEFAULT_UTF8MB4_COLLATION);
    reading.column_length = u32::MAX - 3;
    reading.flags = MYSQL_BINARY_FLAG;
    reading.decimals = NOT_FIXED_DECIMALS;
    CommandExecutionResult::ResultSet(TextResultSet {
        columns: vec![column_name, reading],
        rows: Vec::new(),
        warnings: 0,
        status_flags,
    })
}

/// Answers `SHOW STATUS` for the counters this server keeps.
///
/// MySQL answers some 330 counters about the whole server. This one keeps
/// three of them and answers those, and no row for any other name — what
/// `SHOW VARIABLES` does here too, and what MySQL does for a counter its
/// build leaves out. Measured on MySQL 8.4.11: each is the server's whether
/// the session or the global scope is asked, the rows come in name order, and
/// the columns are `SHOW VARIABLES`' own, read from `session_status` or
/// `global_status`. A session the runtime did not accept has no reading of
/// `Threads_connected`, and refuses it.
pub(super) fn show_status_result(
    command: &MySqlShowStatusCommand,
    sessions: &MySqlSessionRegistry,
    counted: bool,
    status_flags: u16,
) -> Result<CommandExecutionResult, FrontendErrorKind> {
    let mut rows = Vec::new();
    for (name, value) in kept_status_counters(sessions, counted) {
        if !command.selects(name) {
            continue;
        }
        let value = value.ok_or(FrontendErrorKind::Unsupported)?;
        rows.push(vec![
            Some(name.as_bytes().to_vec()),
            Some(value.into_bytes()),
        ]);
    }
    Ok(CommandExecutionResult::ResultSet(TextResultSet {
        columns: crate::session_variables::name_and_value_columns(match command.scope() {
            MySqlVariableScope::Session => "session_status",
            MySqlVariableScope::Global => "global_status",
        }),
        rows,
        warnings: 0,
        status_flags,
    }))
}

/// Answers the read of one status counter's value out of
/// `performance_schema.session_status` or `global_status` — Laravel's
/// `db:show` counts the connections this way — for a counter this server
/// keeps, and refuses one it does not rather than answering no row for it.
///
/// Measured on MySQL 8.4.11, over both protocols: the counter's name is
/// matched without regard to case, and the one column is a nullable
/// `VAR_STRING` of 4096 named after its alias, whose origin is the table's
/// `VARIABLE_VALUE`.
pub(super) fn status_counter_read_result(
    read: &MySqlStatusCounterRead,
    sessions: &MySqlSessionRegistry,
    counted: bool,
    status_flags: u16,
) -> Result<CommandExecutionResult, FrontendErrorKind> {
    let (_, value) = kept_status_counters(sessions, counted)
        .into_iter()
        .find(|(name, _)| name.eq_ignore_ascii_case(read.counter()))
        .ok_or(FrontendErrorKind::Unsupported)?;
    let value = value.ok_or(FrontendErrorKind::Unsupported)?;
    let table = match read.scope() {
        MySqlVariableScope::Session => "session_status",
        MySqlVariableScope::Global => "global_status",
    };
    let mut column = ColumnDefinitionConfig::new(read.column_name(), MYSQL_TYPE_VAR_STRING);
    column.schema = "performance_schema".into();
    column.table = table.into();
    column.original_table = table.into();
    column.original_name = "VARIABLE_VALUE".into();
    column.character_set = u16::from(DEFAULT_UTF8MB4_COLLATION);
    column.column_length = 4096;
    Ok(CommandExecutionResult::ResultSet(TextResultSet {
        columns: vec![column],
        rows: vec![vec![Some(value.into_bytes())]],
        warnings: 0,
        status_flags,
    }))
}

/// The status counters this server keeps, by name, and what each reads.
///
/// `Uptime` counts from when this server opened its databases, and
/// `Uptime_since_flush_status` with it, `FLUSH STATUS` being refused here.
/// `Threads_connected` counts the sessions that have logged in, where MySQL
/// also counts a connection still in its handshake. A session the runtime
/// did not accept is not counted, and has no reading of it.
fn kept_status_counters(
    sessions: &MySqlSessionRegistry,
    counted: bool,
) -> [(&'static str, Option<String>); 3] {
    let uptime = sessions.uptime().as_secs().to_string();
    [
        (
            "Threads_connected",
            counted.then(|| sessions.logged_in().to_string()),
        ),
        ("Uptime", Some(uptime.clone())),
        ("Uptime_since_flush_status", Some(uptime)),
    ]
}

/// Answers `SHOW [FULL] PROCESSLIST` with the sessions of the asking account.
///
/// Measured on MySQL 8.4.11 for an account without `PROCESS`, which is what
/// every account here is: it lists every connection of its own account in
/// the order of their IDs, a waiting one as `Sleep` with an empty state and
/// no statement, and one running a statement as `Query` — `Execute` for a
/// prepared one — with the statement. `Time` is the whole seconds since the
/// connection last started or finished a command. The asking connection's
/// own row reads `init`; another connection running a statement reads
/// `executing` here, where MySQL names the step it is at, and a prepared
/// statement is shown with its `?` where MySQL writes the values bound to
/// it. Without `FULL` the statement is cut to its first 100 characters.
pub(super) fn show_processlist_result(
    sessions: Vec<MySqlSessionSnapshot>,
    asking: u32,
    full: bool,
    status_flags: u16,
) -> Result<CommandExecutionResult, FrontendErrorKind> {
    if sessions.len() > MAX_DISPATCH_RESULT_ROWS {
        return Err(FrontendErrorKind::Internal);
    }
    let rows = sessions
        .into_iter()
        .map(|session| {
            let (command, state, info) = match &session.running {
                None => ("Sleep", Some(String::new()), None),
                Some(running) => {
                    let state = if session.id == asking {
                        "init"
                    } else {
                        "executing"
                    };
                    let text = if full {
                        running.text.clone()
                    } else {
                        running.text.chars().take(100).collect()
                    };
                    (running.command, Some(state.to_owned()), Some(text))
                }
            };
            vec![
                Some(session.id.to_string().into_bytes()),
                Some(session.account.into_bytes()),
                Some(session.host.into_bytes()),
                session.database.map(String::into_bytes),
                Some(command.as_bytes().to_vec()),
                Some(session.seconds.to_string().into_bytes()),
                state.map(String::into_bytes),
                info.map(String::into_bytes),
            ]
        })
        .collect();
    Ok(CommandExecutionResult::ResultSet(TextResultSet {
        columns: show_processlist_columns(full),
        rows,
        warnings: 0,
        status_flags,
    }))
}

/// Measured on MySQL 8.4.11, none of them naming a table.
fn show_processlist_columns(full: bool) -> Vec<ColumnDefinitionConfig> {
    let text = u16::from(DEFAULT_UTF8MB4_COLLATION);
    let info = if full {
        (MYSQL_TYPE_LONG_BLOB, 805_306_368)
    } else {
        (MYSQL_TYPE_VAR_STRING, 400)
    };
    let columns: [(&str, u8, u16, u32, u16, u8); 8] = [
        (
            "Id",
            MYSQL_TYPE_LONGLONG,
            MYSQL_BINARY_COLLATION,
            22,
            MYSQL_NOT_NULL_FLAG | MYSQL_BINARY_FLAG,
            0,
        ),
        (
            "User",
            MYSQL_TYPE_VAR_STRING,
            text,
            128,
            MYSQL_NOT_NULL_FLAG,
            NOT_FIXED_DECIMALS,
        ),
        (
            "Host",
            MYSQL_TYPE_VAR_STRING,
            text,
            1020,
            MYSQL_NOT_NULL_FLAG,
            NOT_FIXED_DECIMALS,
        ),
        (
            "db",
            MYSQL_TYPE_VAR_STRING,
            text,
            256,
            0,
            NOT_FIXED_DECIMALS,
        ),
        (
            "Command",
            MYSQL_TYPE_VAR_STRING,
            text,
            64,
            MYSQL_NOT_NULL_FLAG,
            NOT_FIXED_DECIMALS,
        ),
        (
            "Time",
            MYSQL_TYPE_LONG,
            MYSQL_BINARY_COLLATION,
            8,
            MYSQL_NOT_NULL_FLAG | MYSQL_BINARY_FLAG,
            0,
        ),
        (
            "State",
            MYSQL_TYPE_VAR_STRING,
            text,
            120,
            0,
            NOT_FIXED_DECIMALS,
        ),
        ("Info", info.0, text, info.1, 0, NOT_FIXED_DECIMALS),
    ];
    columns
        .into_iter()
        .map(|(name, kind, character_set, length, flags, decimals)| {
            let mut column = ColumnDefinitionConfig::new(name, kind);
            column.character_set = character_set;
            column.column_length = length;
            column.flags = flags;
            column.decimals = decimals;
            column
        })
        .collect()
}

/// The `group_concat_max_len` Laravel's catalog reads are answered under,
/// MySQL's own, which no joined list of a key's columns here outgrows.
pub(super) const LARAVEL_GROUP_CONCAT_MAX_LEN: u64 = 1024;

/// Which protocol a result's columns are described for. MySQL describes
/// Laravel's catalog reads differently over each: measured on 8.4.11, a
/// column read through a view names its origin as the view's alias over the
/// text protocol and as the catalog table's own column over the binary one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum CatalogProtocol {
    Text,
    Binary,
}

/// One row of a Laravel catalog read: words, and for an index whether it is
/// unique.
pub(super) struct LaravelCatalogRow {
    words: Vec<String>,
    unique: Option<bool>,
}

/// One index or foreign key a catalog read groups rows into: its name read
/// without regard to case, which orders the groups, and the grouping columns.
type CatalogGroup = (String, Vec<String>);

/// One column of a key: its place in the key and the column names it lists.
type KeyMember = (u64, Vec<String>);

/// Groups the rows a plain catalog read answered into one row for each index
/// or foreign key. The plain read's rows come as the grouping columns, then
/// each column's place in the key, then the column names it lists.
pub(super) fn laravel_catalog_rows(
    query: &LaravelInformationSchemaQuery,
    rows: Vec<crate::TextResultRow>,
) -> Result<Vec<LaravelCatalogRow>, FrontendErrorKind> {
    let grouped_by = match query {
        LaravelInformationSchemaQuery::Indexes { .. } => 3,
        LaravelInformationSchemaQuery::ForeignKeys { .. } => 5,
    };
    let mut groups: std::collections::BTreeMap<CatalogGroup, Vec<KeyMember>> =
        std::collections::BTreeMap::new();
    for row in rows {
        let row = row
            .into_iter()
            .map(|value| {
                value
                    .ok_or(FrontendErrorKind::Unsupported)
                    .and_then(|value| {
                        String::from_utf8(value).map_err(|_| FrontendErrorKind::Internal)
                    })
            })
            .collect::<Result<Vec<_>, _>>()?;
        let (key, rest) = row.split_at(grouped_by);
        let [place, listed @ ..] = rest else {
            return Err(FrontendErrorKind::Internal);
        };
        let place = place.parse().map_err(|_| FrontendErrorKind::Internal)?;
        groups
            .entry((key[0].to_ascii_lowercase(), key.to_vec()))
            .or_default()
            .push((place, listed.to_vec()));
    }
    let mut answered = Vec::with_capacity(groups.len());
    for ((_, key), mut members) in groups {
        members.sort_by_key(|(place, _)| *place);
        let lists = (0..members[0].1.len())
            .map(|list| {
                members
                    .iter()
                    .map(|(_, listed)| listed[list].as_str())
                    .collect::<Vec<_>>()
                    .join(",")
            })
            .collect::<Vec<_>>();
        if lists
            .iter()
            .any(|list| list.len() as u64 > LARAVEL_GROUP_CONCAT_MAX_LEN)
        {
            return Err(FrontendErrorKind::Unsupported);
        }
        answered.push(match query {
            LaravelInformationSchemaQuery::Indexes { .. } => {
                let [name, kind, non_unique] = key.as_slice() else {
                    return Err(FrontendErrorKind::Internal);
                };
                LaravelCatalogRow {
                    words: vec![name.clone(), lists[0].clone(), kind.clone()],
                    unique: Some(non_unique == "0"),
                }
            }
            LaravelInformationSchemaQuery::ForeignKeys { .. } => {
                let [name, foreign_schema, foreign_table, on_update, on_delete] = key.as_slice()
                else {
                    return Err(FrontendErrorKind::Internal);
                };
                LaravelCatalogRow {
                    words: vec![
                        name.clone(),
                        lists[0].clone(),
                        foreign_schema.clone(),
                        foreign_table.clone(),
                        lists[1].clone(),
                        on_update.clone(),
                        on_delete.clone(),
                    ],
                    unique: None,
                }
            }
        });
    }
    Ok(answered)
}

pub(super) fn laravel_catalog_text_result(
    query: &LaravelInformationSchemaQuery,
    rows: Vec<LaravelCatalogRow>,
    status_flags: u16,
) -> TextResultSet {
    TextResultSet {
        columns: laravel_catalog_columns(query, CatalogProtocol::Text),
        rows: rows
            .into_iter()
            .map(|row| {
                let mut values = row
                    .words
                    .into_iter()
                    .map(|word| Some(word.into_bytes()))
                    .collect::<Vec<_>>();
                if let Some(unique) = row.unique {
                    values.push(Some(u8::from(unique).to_string().into_bytes()));
                }
                values
            })
            .collect(),
        warnings: 0,
        status_flags,
    }
}

pub(super) fn laravel_catalog_binary_result(
    query: &LaravelInformationSchemaQuery,
    rows: Vec<LaravelCatalogRow>,
    status_flags: u16,
) -> BinaryResultSet {
    let columns = laravel_catalog_columns(query, CatalogProtocol::Binary);
    BinaryResultSet {
        rows: rows
            .into_iter()
            .map(|row| {
                let mut values = row
                    .words
                    .into_iter()
                    .zip(&columns)
                    .map(|(word, column)| {
                        if column.column_type == MYSQL_TYPE_LONG_BLOB {
                            BinaryResultValue::Blob(word.into_bytes())
                        } else {
                            BinaryResultValue::Text(word)
                        }
                    })
                    .collect::<Vec<_>>();
                if let Some(unique) = row.unique {
                    values.push(BinaryResultValue::Integer(i64::from(unique)));
                }
                values
            })
            .collect(),
        columns,
        warnings: 0,
        status_flags,
    }
}

/// The columns of one of Laravel's catalog reads, as MySQL 8.4.11 describes
/// them over each protocol, measured with `group_concat_max_len` at 1024.
pub(super) fn laravel_catalog_columns(
    query: &LaravelInformationSchemaQuery,
    protocol: CatalogProtocol,
) -> Vec<ColumnDefinitionConfig> {
    let binary = protocol == CatalogProtocol::Binary;
    // A column read out of a catalog table: over the text protocol it names
    // its alias as its origin and the view it was read through as its table,
    // and over the binary one the catalog table's own column, with the
    // decimals a word carries when it is worked out.
    let read = |name: &str, table: &str, original: &str, original_table: &str, length, flags| {
        let mut column = ColumnDefinitionConfig::new(name, MYSQL_TYPE_VAR_STRING);
        column.table = table.into();
        column.character_set = u16::from(DEFAULT_UTF8MB4_COLLATION);
        column.column_length = length;
        column.flags = flags;
        if binary {
            column.original_name = original.into();
            column.original_table = original_table.into();
        } else {
            column.original_name = name.into();
            column.schema = "information_schema".into();
        }
        column
    };
    let joined = |name: &str| {
        let mut column = ColumnDefinitionConfig::new(name, MYSQL_TYPE_LONG_BLOB);
        column.character_set = u16::from(DEFAULT_UTF8MB4_COLLATION);
        column.column_length = 36864;
        column.decimals = NOT_FIXED_DECIMALS;
        column
    };
    match query {
        LaravelInformationSchemaQuery::Indexes { .. } => {
            let mut name = read("name", "statistics", "INDEX_NAME", "STATISTICS", 256, 0);
            let mut kind = read(
                "type",
                "statistics",
                "INDEX_TYPE",
                "STATISTICS",
                44,
                MYSQL_NOT_NULL_FLAG | MYSQL_BINARY_FLAG,
            );
            if binary {
                name.decimals = NOT_FIXED_DECIMALS;
                kind.decimals = NOT_FIXED_DECIMALS;
            }
            let mut unique = ColumnDefinitionConfig::new(
                "unique",
                if binary {
                    MYSQL_TYPE_LONGLONG
                } else {
                    MYSQL_TYPE_LONG
                },
            );
            unique.character_set = MYSQL_BINARY_COLLATION;
            unique.column_length = 1;
            unique.flags = if binary {
                MYSQL_NOT_NULL_FLAG | MYSQL_BINARY_FLAG | MYSQL_NUM_FLAG
            } else {
                unique.original_name = "unique".into();
                MYSQL_NOT_NULL_FLAG | MYSQL_NUM_FLAG
            };
            vec![name, joined("columns"), kind, unique]
        }
        LaravelInformationSchemaQuery::ForeignKeys { .. } => {
            let key = |name: &str, original: &str, flags| {
                let mut column = read(name, "kc", original, "KEY_COLUMN_USAGE", 256, flags);
                column.schema = "information_schema".into();
                column
            };
            let rule = |name: &str, original: &str| {
                let mut column = read(
                    name,
                    "rc",
                    original,
                    "REFERENTIAL_CONSTRAINTS",
                    44,
                    MYSQL_NOT_NULL_FLAG
                        | MYSQL_BINARY_FLAG
                        | MYSQL_ENUM_FLAG
                        | MYSQL_NO_DEFAULT_VALUE_FLAG,
                );
                column.column_type = MYSQL_TYPE_STRING;
                column.schema = "information_schema".into();
                if !binary {
                    column.original_table = "foreign_keys".into();
                }
                column
            };
            vec![
                key("name", "CONSTRAINT_NAME", 0),
                joined("columns"),
                key(
                    "foreign_schema",
                    "REFERENCED_TABLE_SCHEMA",
                    MYSQL_BINARY_FLAG,
                ),
                key("foreign_table", "REFERENCED_TABLE_NAME", MYSQL_BINARY_FLAG),
                joined("foreign_columns"),
                rule("on_update", "UPDATE_RULE"),
                rule("on_delete", "DELETE_RULE"),
            ]
        }
    }
}
