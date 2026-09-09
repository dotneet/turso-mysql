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
        let whole = [
            Some(database.as_bytes().to_vec()),
            Some(table.name().as_bytes().to_vec()),
            Some(table_type.to_vec()),
        ];
        // The row holds what the query named, in the order it named it.
        let row = projected
            .iter()
            .map(|column| {
                whole[match column {
                    MySqlInformationSchemaTablesColumn::TableSchema => 0,
                    MySqlInformationSchemaTablesColumn::TableName => 1,
                    MySqlInformationSchemaTablesColumn::TableType => 2,
                }]
                .clone()
            })
            .collect::<Vec<_>>();
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
    ];
    projected
        .iter()
        .map(|column| {
            let (name, original_table, column_type, column_length, flags) = whole[match column {
                MySqlInformationSchemaTablesColumn::TableSchema => 0,
                MySqlInformationSchemaTablesColumn::TableName => 1,
                MySqlInformationSchemaTablesColumn::TableType => 2,
            }];
            let mut column = ColumnDefinitionConfig::new(name, column_type);
            "information_schema".clone_into(&mut column.schema);
            "TABLES".clone_into(&mut column.table);
            original_table.clone_into(&mut column.original_table);
            name.clone_into(&mut column.original_name);
            column.character_set = u16::from(DEFAULT_UTF8MB4_COLLATION);
            column.column_length = column_length;
            column.flags = flags;
            column
        })
        .collect()
}

/// The shapes MySQL reports for the `information_schema.STATISTICS` columns
/// this answers, in the order MySQL declares them.
///
/// Every value comes from the pinned MySQL 8.4.11 golden
/// `information-schema-statistics.json`. `CARDINALITY` is the one column MySQL
/// has that is missing here: it is an estimate the engine keeps no equivalent
/// of, and answering a made-up one is worse than answering none.
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
        if column.name().len() > MAX_TEXT_ROW_VALUE_LENGTH
            || column.extra().len() > MAX_TEXT_ROW_VALUE_LENGTH
            || column.comment().len() > MAX_TEXT_ROW_VALUE_LENGTH
        {
            return Err(FrontendErrorKind::Internal);
        }
        let column_type = show_column_type_name(&column)?;
        let extra = show_column_extra(column.extra())?;
        let default = match column.default_value() {
            Some(MySqlColumnDefault::Text(value)) if value.len() > MAX_TEXT_ROW_VALUE_LENGTH => {
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

        let whole = [
            Some(column.name().as_bytes().to_vec()),
            Some(ordinal),
            default,
            Some(nullable.to_vec()),
            Some(column_type.to_vec()),
            Some(key.to_vec()),
            Some(extra.to_vec()),
            Some(column.comment().as_bytes().to_vec()),
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

/// Where one column sits among the eight, which is the order MySQL declares
/// them in and the order both the row and the definitions are built in.
fn information_schema_columns_position(column: MySqlInformationSchemaColumnsColumn) -> usize {
    match column {
        MySqlInformationSchemaColumnsColumn::ColumnName => 0,
        MySqlInformationSchemaColumnsColumn::OrdinalPosition => 1,
        MySqlInformationSchemaColumnsColumn::ColumnDefault => 2,
        MySqlInformationSchemaColumnsColumn::IsNullable => 3,
        MySqlInformationSchemaColumnsColumn::ColumnType => 4,
        MySqlInformationSchemaColumnsColumn::ColumnKey => 5,
        MySqlInformationSchemaColumnsColumn::Extra => 6,
        MySqlInformationSchemaColumnsColumn::ColumnComment => 7,
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

    let whole = [
        column_name,
        ordinal_position,
        column_default,
        is_nullable,
        column_type,
        column_key,
        extra,
        column_comment,
    ];
    projected
        .iter()
        .map(|column| whole[information_schema_columns_position(*column)].clone())
        .collect()
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
                Some(b"utf8mb4_0900_ai_ci".to_vec()),
                None,
                Some(Vec::new()),
                Some(Vec::new()),
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

/// Builds the rows `SHOW COLUMNS` reports, with or without the `FULL` extras.
///
/// Measured on MySQL 8.4.11: `FULL` puts `Collation` third and appends
/// `Privileges` and `Comment`. The collation is the text one for a `VARCHAR`,
/// `CHAR` or `TEXT` and NULL for every other type, a `VARBINARY` and a `BLOB`
/// included. The comment is the text the column was declared with, empty where
/// it was declared with none.
///
/// `Privileges` is answered NULL. MySQL reports the connected user's grants on
/// the column, and this server's grants are per database and per table rather
/// than per column, so it does not keep the figure — the same answer `SHOW
/// TABLE STATUS` gives for the storage figures InnoDB keeps and this does not.
/// The column is nullable in MySQL too, so NULL is a value a client can read.
pub(super) fn show_columns_result(
    columns: Vec<MySqlColumnMetadata>,
    status_flags: u16,
    full: bool,
) -> Result<CommandExecutionResult, FrontendErrorKind> {
    if columns.len() > MAX_DISPATCH_RESULT_ROWS {
        return Err(FrontendErrorKind::Internal);
    }

    let mut retained_bytes = 0usize;
    let mut rows = Vec::with_capacity(columns.len());
    for column in columns {
        if column.name().len() > MAX_TEXT_ROW_VALUE_LENGTH
            || column.extra().len() > MAX_TEXT_ROW_VALUE_LENGTH
            || column.comment().len() > MAX_TEXT_ROW_VALUE_LENGTH
        {
            return Err(FrontendErrorKind::Internal);
        }
        let mut row = vec![
            Some(column.name().as_bytes().to_vec()),
            Some(show_column_type_name(&column)?),
        ];
        if full {
            row.push(
                matches!(column.type_name(), "VARCHAR" | "CHAR" | "TEXT")
                    .then(|| b"utf8mb4_0900_ai_ci".to_vec()),
            );
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
            Some(show_column_extra(column.extra())?.to_vec()),
        ]);
        if full {
            row.push(None);
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
    if payload_len > MAX_RESPONSE_PACKET_PAYLOAD_LENGTH {
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

pub(super) fn show_column_extra(extra: &str) -> Result<&'static [u8], FrontendErrorKind> {
    match extra {
        "" => Ok(b""),
        "AUTO_INCREMENT" => Ok(b"auto_increment"),
        // Measured on MySQL 8.4.11: a column defaulting to the moment it is
        // written reports this, in capitals where `auto_increment` is not.
        "DEFAULT_GENERATED" => Ok(b"DEFAULT_GENERATED"),
        // Measured: the words are reported in lower case where
        // `DEFAULT_GENERATED` is in capitals, and the two run together where
        // the column carries both.
        "on update CURRENT_TIMESTAMP" => Ok(b"on update CURRENT_TIMESTAMP"),
        "DEFAULT_GENERATED on update CURRENT_TIMESTAMP" => {
            Ok(b"DEFAULT_GENERATED on update CURRENT_TIMESTAMP")
        }
        _ => Err(FrontendErrorKind::Internal),
    }
}

pub(super) fn show_column_default_value(
    column: &MySqlColumnMetadata,
) -> Result<Option<Vec<u8>>, FrontendErrorKind> {
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
        MySqlColumnDefault::Boolean(value) => {
            return Ok(Some(if *value { b"1".to_vec() } else { b"0".to_vec() }));
        }
    };
    if value.len() > MAX_TEXT_ROW_VALUE_LENGTH {
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
