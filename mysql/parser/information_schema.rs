//! Checking the `information_schema` queries clients send at startup.
//!
//! These are recognized rather than translated. A client asks a narrow,
//! predictable question about the catalog, and anything wider is refused, so
//! this is all validation and no rendering.

use super::*;

pub(crate) fn tokenize_information_schema_query(
    sql: &str,
    mode: SessionSqlMode,
) -> Result<Vec<Token>, ParseError> {
    Tokenizer::new(&SessionMySqlDialect::without_executable_comments(mode), sql)
        .tokenize()
        .map_err(|error| ParseError::Sqlparser(error.to_string()))
}

/// The fixed prepared catalog queries emitted by GORM 1.31.2 with the MySQL driver.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GormInformationSchemaPreparedQuery {
    CurrentDatabase,
    Columns,
    HasTable,
    HasColumn,
    HasIndex,
    HasConstraint,
}

pub fn parse_optional_gorm_information_schema_prepared_query(
    sql: &str,
    mode: SessionSqlMode,
) -> Result<Option<GormInformationSchemaPreparedQuery>, ParseError> {
    const CURRENT_DATABASE: &str = "SELECT SCHEMA_NAME FROM information_schema.SCHEMATA \
        WHERE SCHEMA_NAME LIKE ? ORDER BY SCHEMA_NAME = ? DESC, SCHEMA_NAME LIMIT 1";
    const COLUMNS: &str = "SELECT column_name, column_default, is_nullable = 'YES', \
        data_type, character_maximum_length, column_type, column_key, extra, \
        column_comment, numeric_precision, numeric_scale, datetime_precision \
        FROM information_schema.columns WHERE table_schema = ? AND table_name = ? \
        ORDER BY ORDINAL_POSITION";
    const HAS_TABLE: &str = "SELECT count(*) FROM information_schema.tables \
        WHERE table_schema = ? AND table_name = ? AND table_type = ?";
    const HAS_COLUMN: &str = "SELECT count(*) FROM information_schema.columns \
        WHERE table_schema = ? AND table_name = ? AND column_name = ?";
    const HAS_INDEX: &str = "SELECT count(*) FROM information_schema.statistics \
        WHERE table_schema = ? AND table_name = ? AND index_name = ?";
    const HAS_CONSTRAINT: &str = "SELECT count(*) FROM information_schema.table_constraints \
        WHERE constraint_schema = ? AND table_name = ? AND constraint_name = ?";

    let tokens = tokenize_information_schema_query(sql, mode)?;
    if tokens.iter().any(|token| {
        matches!(
            token,
            Token::Whitespace(
                Whitespace::SingleLineComment { .. } | Whitespace::MultiLineComment(_)
            )
        )
    }) {
        return Ok(None);
    }
    if same_catalog_tokens(
        &tokens,
        &tokenize_information_schema_query(CURRENT_DATABASE, mode)?,
    ) {
        return Ok(Some(GormInformationSchemaPreparedQuery::CurrentDatabase));
    }
    if same_catalog_tokens(&tokens, &tokenize_information_schema_query(COLUMNS, mode)?) {
        return Ok(Some(GormInformationSchemaPreparedQuery::Columns));
    }
    if same_catalog_tokens(
        &tokens,
        &tokenize_information_schema_query(HAS_TABLE, mode)?,
    ) {
        return Ok(Some(GormInformationSchemaPreparedQuery::HasTable));
    }
    if same_catalog_tokens(
        &tokens,
        &tokenize_information_schema_query(HAS_COLUMN, mode)?,
    ) {
        return Ok(Some(GormInformationSchemaPreparedQuery::HasColumn));
    }
    if same_catalog_tokens(
        &tokens,
        &tokenize_information_schema_query(HAS_INDEX, mode)?,
    ) {
        return Ok(Some(GormInformationSchemaPreparedQuery::HasIndex));
    }
    if same_catalog_tokens(
        &tokens,
        &tokenize_information_schema_query(HAS_CONSTRAINT, mode)?,
    ) {
        return Ok(Some(GormInformationSchemaPreparedQuery::HasConstraint));
    }
    Ok(None)
}

/// Connector/J 9.6.0 queries measured against MySQL 8.4.11 with
/// `useInformationSchema=true`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConnectorJInformationSchemaQuery {
    Tables {
        schema: String,
        table_pattern: String,
        types: Vec<String>,
    },
    Columns {
        schema: String,
        table_pattern: Option<String>,
        column_pattern: String,
    },
    PrimaryKeys {
        schema: String,
        table: String,
    },
    IndexInfo {
        schema: String,
        table: String,
    },
    ImportedKeys {
        schema: String,
        table: String,
    },
    ExportedKeys {
        schema: String,
        table: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectorJSchemataListingQuery {
    Catalogs,
    Schemas,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConnectorJForeignKey {
    pub name: String,
    pub child_columns: Vec<String>,
    pub parent_table: String,
    pub parent_columns: Vec<String>,
    pub on_delete: Option<String>,
    pub on_update: Option<String>,
}

pub fn parse_connector_j_foreign_keys(
    create_table_sql: &str,
    mode: SessionSqlMode,
) -> Result<Vec<ConnectorJForeignKey>, ParseError> {
    let Statement::CreateTable(table) = parse_one_statement(create_table_sql, mode)? else {
        return unsupported("Connector/J foreign key metadata source");
    };
    let mut keys = Vec::new();
    for constraint in table.constraints {
        let TableConstraint::ForeignKey(key) = constraint else {
            continue;
        };
        let name = key.name.ok_or(ParseError::Unsupported {
            feature: "unnamed Connector/J foreign key",
        })?;
        let [ObjectNamePart::Identifier(parent)] = key.foreign_table.0.as_slice() else {
            return unsupported("schema-qualified Connector/J foreign key parent");
        };
        if key.columns.is_empty() || key.columns.len() != key.referred_columns.len() {
            return unsupported("Connector/J foreign key column count");
        }
        keys.push(ConnectorJForeignKey {
            name: name.value,
            child_columns: key.columns.into_iter().map(|column| column.value).collect(),
            parent_table: parent.value.clone(),
            parent_columns: key
                .referred_columns
                .into_iter()
                .map(|column| column.value)
                .collect(),
            on_delete: key.on_delete.map(|action| action.to_string()),
            on_update: key.on_update.map(|action| action.to_string()),
        });
    }
    Ok(keys)
}

pub fn parse_optional_connector_j_information_schema_query(
    sql: &str,
    mode: SessionSqlMode,
) -> Result<Option<ConnectorJInformationSchemaQuery>, ParseError> {
    let actual = tokenize_information_schema_query(sql, mode)?;
    if actual.iter().any(|token| {
        matches!(
            token,
            Token::Whitespace(
                Whitespace::SingleLineComment { .. } | Whitespace::MultiLineComment(_)
            )
        )
    }) {
        return Ok(None);
    }

    if let Some(captures) =
        connector_j_template_captures(&actual, CONNECTOR_J_GET_TABLES_TEMPLATE, mode)?
    {
        let [Some(schema), Some(table_pattern), types @ ..] = captures.as_slice() else {
            return Ok(None);
        };
        return Ok(Some(ConnectorJInformationSchemaQuery::Tables {
            schema: schema.clone(),
            table_pattern: table_pattern.clone(),
            types: types.iter().flatten().cloned().collect(),
        }));
    }
    let tables_without_types = CONNECTOR_J_GET_TABLES_TEMPLATE.replace(
        " HAVING TABLE_TYPE IN (__TYPE1__,__TYPE2__,__TYPE3__,__TYPE4__,__TYPE5__)",
        "",
    );
    if let Some(captures) = connector_j_template_captures(&actual, &tables_without_types, mode)? {
        let [Some(schema), Some(table_pattern)] = captures.as_slice() else {
            return Ok(None);
        };
        return Ok(Some(ConnectorJInformationSchemaQuery::Tables {
            schema: schema.clone(),
            table_pattern: table_pattern.clone(),
            types: Vec::new(),
        }));
    }
    if let Some(captures) =
        connector_j_template_captures(&actual, CONNECTOR_J_GET_COLUMNS_TEMPLATE, mode)?
    {
        let [Some(schema), Some(table_pattern), Some(column_pattern)] = captures.as_slice() else {
            return Ok(None);
        };
        return Ok(Some(ConnectorJInformationSchemaQuery::Columns {
            schema: schema.clone(),
            table_pattern: Some(table_pattern.clone()),
            column_pattern: column_pattern.clone(),
        }));
    }
    let columns_without_table_pattern =
        CONNECTOR_J_GET_COLUMNS_TEMPLATE.replace(" AND TABLE_NAME LIKE '__TABLE_PATTERN__'", "");
    if let Some(captures) =
        connector_j_template_captures(&actual, &columns_without_table_pattern, mode)?
    {
        let [Some(schema), Some(column_pattern)] = captures.as_slice() else {
            return Ok(None);
        };
        return Ok(Some(ConnectorJInformationSchemaQuery::Columns {
            schema: schema.clone(),
            table_pattern: None,
            column_pattern: column_pattern.clone(),
        }));
    }
    for (template, kind) in [
        (CONNECTOR_J_GET_PRIMARY_KEYS_TEMPLATE, 0),
        (CONNECTOR_J_GET_INDEX_INFO_TEMPLATE, 1),
        (CONNECTOR_J_GET_IMPORTED_KEYS_TEMPLATE, 2),
        (CONNECTOR_J_GET_EXPORTED_KEYS_TEMPLATE, 3),
    ] {
        let Some(captures) = connector_j_template_captures(&actual, template, mode)? else {
            continue;
        };
        let [Some(schema), Some(table)] = captures.as_slice() else {
            return Ok(None);
        };
        return Ok(Some(match kind {
            0 => ConnectorJInformationSchemaQuery::PrimaryKeys {
                schema: schema.clone(),
                table: table.clone(),
            },
            1 => ConnectorJInformationSchemaQuery::IndexInfo {
                schema: schema.clone(),
                table: table.clone(),
            },
            2 => ConnectorJInformationSchemaQuery::ImportedKeys {
                schema: schema.clone(),
                table: table.clone(),
            },
            3 => ConnectorJInformationSchemaQuery::ExportedKeys {
                schema: schema.clone(),
                table: table.clone(),
            },
            _ => unreachable!("all connector query kinds are enumerated"),
        }));
    }
    Ok(None)
}

pub fn is_connector_j_information_schema_collation_query(
    sql: &str,
    mode: SessionSqlMode,
) -> Result<bool, ParseError> {
    const QUERY: &str = "SELECT DEFAULT_COLLATION_NAME FROM INFORMATION_SCHEMA.SCHEMATA WHERE SCHEMA_NAME = 'information_schema'";
    let actual = tokenize_information_schema_query(sql, mode)?;
    if actual.iter().any(|token| {
        matches!(
            token,
            Token::Whitespace(
                Whitespace::SingleLineComment { .. } | Whitespace::MultiLineComment(_)
            )
        )
    }) {
        return Ok(false);
    }
    Ok(same_catalog_tokens(
        &actual,
        &tokenize_information_schema_query(QUERY, mode)?,
    ))
}

pub fn is_connector_j_reserved_keywords_query(
    sql: &str,
    mode: SessionSqlMode,
) -> Result<bool, ParseError> {
    const QUERY: &str =
        "SELECT WORD FROM INFORMATION_SCHEMA.KEYWORDS WHERE RESERVED = 1 ORDER BY WORD";
    let actual = tokenize_information_schema_query(sql, mode)?;
    if actual.iter().any(|token| {
        matches!(
            token,
            Token::Whitespace(
                Whitespace::SingleLineComment { .. } | Whitespace::MultiLineComment(_)
            )
        )
    }) {
        return Ok(false);
    }
    Ok(same_catalog_tokens(
        &actual,
        &tokenize_information_schema_query(QUERY, mode)?,
    ))
}

pub fn parse_optional_connector_j_schemata_listing_query(
    sql: &str,
    mode: SessionSqlMode,
) -> Result<Option<ConnectorJSchemataListingQuery>, ParseError> {
    const CATALOGS: &str =
        "SELECT SCHEMA_NAME AS TABLE_CAT FROM INFORMATION_SCHEMA.SCHEMATA ORDER BY TABLE_CAT";
    const SCHEMAS: &str = "SELECT SCHEMA_NAME AS TABLE_SCHEM, CATALOG_NAME AS TABLE_CATALOG FROM INFORMATION_SCHEMA.SCHEMATA WHERE FALSE ORDER BY TABLE_CATALOG, TABLE_SCHEM";
    let actual = tokenize_information_schema_query(sql, mode)?;
    if actual.iter().any(|token| {
        matches!(
            token,
            Token::Whitespace(
                Whitespace::SingleLineComment { .. } | Whitespace::MultiLineComment(_)
            )
        )
    }) {
        return Ok(None);
    }
    for (template, kind) in [
        (CATALOGS, ConnectorJSchemataListingQuery::Catalogs),
        (SCHEMAS, ConnectorJSchemataListingQuery::Schemas),
    ] {
        if same_catalog_tokens(&actual, &tokenize_information_schema_query(template, mode)?) {
            return Ok(Some(kind));
        }
    }
    Ok(None)
}

fn connector_j_template_captures(
    actual: &[Token],
    template: &str,
    mode: SessionSqlMode,
) -> Result<Option<Vec<Option<String>>>, ParseError> {
    let expected = tokenize_information_schema_query(template, mode)?;
    let actual = actual
        .iter()
        .filter(|token| !matches!(token, Token::Whitespace(_)))
        .collect::<Vec<_>>();
    let expected = expected
        .iter()
        .filter(|token| !matches!(token, Token::Whitespace(_)))
        .collect::<Vec<_>>();
    if actual.len() != expected.len() {
        return Ok(None);
    }
    let mut captures = Vec::new();
    for (actual, expected) in actual.into_iter().zip(expected) {
        match expected {
            Token::SingleQuotedString(value) if value.starts_with("__") => {
                let Token::SingleQuotedString(value) = actual else {
                    return Ok(None);
                };
                captures.push(Some(value.clone()));
            }
            Token::Word(word) if word.value.starts_with("__TYPE") => match actual {
                Token::SingleQuotedString(value) => captures.push(Some(value.clone())),
                Token::Word(value) if value.value.eq_ignore_ascii_case("null") => {
                    captures.push(None);
                }
                _ => return Ok(None),
            },
            _ if same_catalog_token(actual, expected) => {}
            _ => return Ok(None),
        }
    }
    Ok(Some(captures))
}

/// SQL emitted by Connector/J 9.6.0 with `useInformationSchema=true`, measured on MySQL 8.4.11.
pub const CONNECTOR_J_GET_TABLES_TEMPLATE: &str = r#"SELECT TABLE_SCHEMA AS TABLE_CAT, NULL AS TABLE_SCHEM, TABLE_NAME, CASE WHEN TABLE_TYPE = 'BASE TABLE' THEN CASE WHEN TABLE_SCHEMA = 'mysql' OR TABLE_SCHEMA = 'performance_schema' OR TABLE_SCHEMA = 'sys' THEN 'SYSTEM TABLE' ELSE 'TABLE' END WHEN TABLE_TYPE = 'TEMPORARY' THEN 'LOCAL_TEMPORARY' ELSE TABLE_TYPE END AS TABLE_TYPE, TABLE_COMMENT AS REMARKS, NULL AS TYPE_CAT, NULL AS TYPE_SCHEM, NULL AS TYPE_NAME, NULL AS SELF_REFERENCING_COL_NAME, NULL AS REF_GENERATION FROM INFORMATION_SCHEMA.TABLES WHERE TABLE_SCHEMA = '__SCHEMA__' AND TABLE_NAME LIKE '__TABLE_PATTERN__' HAVING TABLE_TYPE IN (__TYPE1__,__TYPE2__,__TYPE3__,__TYPE4__,__TYPE5__) ORDER BY TABLE_TYPE, TABLE_SCHEMA, TABLE_NAME"#;

pub const CONNECTOR_J_GET_COLUMNS_TEMPLATE: &str = r#"SELECT TABLE_SCHEMA, NULL, TABLE_NAME, COLUMN_NAME, CASE WHEN UPPER(DATA_TYPE) = 'DECIMAL' THEN 3 WHEN UPPER(DATA_TYPE) = 'DECIMAL UNSIGNED' THEN 3 WHEN UPPER(DATA_TYPE) = 'TINYINT' THEN IF(LOCATE('ZEROFILL', UPPER(COLUMN_TYPE)) = 0 AND LOCATE('UNSIGNED', UPPER(COLUMN_TYPE)) = 0 AND LOCATE('(1)', COLUMN_TYPE) != 0, -7, -6) WHEN UPPER(DATA_TYPE) = 'TINYINT UNSIGNED' THEN IF(LOCATE('ZEROFILL', UPPER(COLUMN_TYPE)) = 0 AND LOCATE('UNSIGNED', UPPER(COLUMN_TYPE)) = 0 AND LOCATE('(1)', COLUMN_TYPE) != 0, -7, -6) WHEN UPPER(DATA_TYPE) = 'BOOLEAN' THEN 16 WHEN UPPER(DATA_TYPE) = 'SMALLINT' THEN 5 WHEN UPPER(DATA_TYPE) = 'SMALLINT UNSIGNED' THEN 5 WHEN UPPER(DATA_TYPE) = 'INT' THEN 4 WHEN UPPER(DATA_TYPE) = 'INT UNSIGNED' THEN 4 WHEN UPPER(DATA_TYPE) = 'FLOAT' THEN 7 WHEN UPPER(DATA_TYPE) = 'FLOAT UNSIGNED' THEN 7 WHEN UPPER(DATA_TYPE) = 'DOUBLE' THEN 8 WHEN UPPER(DATA_TYPE) = 'DOUBLE UNSIGNED' THEN 8 WHEN UPPER(DATA_TYPE) = 'NULL' THEN 0 WHEN UPPER(DATA_TYPE) = 'TIMESTAMP' THEN 93 WHEN UPPER(DATA_TYPE) = 'BIGINT' THEN -5 WHEN UPPER(DATA_TYPE) = 'BIGINT UNSIGNED' THEN -5 WHEN UPPER(DATA_TYPE) = 'MEDIUMINT' THEN 4 WHEN UPPER(DATA_TYPE) = 'MEDIUMINT UNSIGNED' THEN 4 WHEN UPPER(DATA_TYPE) = 'DATE' THEN 91 WHEN UPPER(DATA_TYPE) = 'TIME' THEN 92 WHEN UPPER(DATA_TYPE) = 'DATETIME' THEN 93 WHEN UPPER(DATA_TYPE) = 'YEAR' THEN 91 WHEN UPPER(DATA_TYPE) = 'VARCHAR' THEN 12 WHEN UPPER(DATA_TYPE) = 'VARBINARY' THEN -3 WHEN UPPER(DATA_TYPE) = 'BIT' THEN -7 WHEN UPPER(DATA_TYPE) = 'JSON' THEN -1 WHEN UPPER(DATA_TYPE) = 'ENUM' THEN 1 WHEN UPPER(DATA_TYPE) = 'SET' THEN 1 WHEN UPPER(DATA_TYPE) = 'TINYBLOB' THEN -3 WHEN UPPER(DATA_TYPE) = 'TINYTEXT' THEN 12 WHEN UPPER(DATA_TYPE) = 'MEDIUMBLOB' THEN -4 WHEN UPPER(DATA_TYPE) = 'MEDIUMTEXT' THEN -1 WHEN UPPER(DATA_TYPE) = 'LONGBLOB' THEN -4 WHEN UPPER(DATA_TYPE) = 'LONGTEXT' THEN -1 WHEN UPPER(DATA_TYPE) = 'BLOB' THEN -4 WHEN UPPER(DATA_TYPE) = 'TEXT' THEN -1 WHEN UPPER(DATA_TYPE) = 'CHAR' THEN 1 WHEN UPPER(DATA_TYPE) = 'BINARY' THEN -2 WHEN UPPER(DATA_TYPE) = 'GEOMETRY' THEN -2 WHEN UPPER(DATA_TYPE) = 'VECTOR' THEN -4 WHEN UPPER(DATA_TYPE) = 'UNKNOWN' THEN 1111 WHEN UPPER(DATA_TYPE) = 'POINT' THEN -2 WHEN UPPER(DATA_TYPE) = 'LINESTRING' THEN -2 WHEN UPPER(DATA_TYPE) = 'POLYGON' THEN -2 WHEN UPPER(DATA_TYPE) = 'MULTIPOINT' THEN -2 WHEN UPPER(DATA_TYPE) = 'MULTILINESTRING' THEN -2 WHEN UPPER(DATA_TYPE) = 'MULTIPOLYGON' THEN -2 WHEN UPPER(DATA_TYPE) = 'GEOMETRYCOLLECTION' THEN -2 WHEN UPPER(DATA_TYPE) = 'GEOMCOLLECTION' THEN -2 ELSE 1111 END AS DATA_TYPE, UPPER(CASE WHEN UPPER(DATA_TYPE) = 'TINYINT' THEN CASE WHEN LOCATE('ZEROFILL', UPPER(COLUMN_TYPE)) = 0 AND LOCATE('UNSIGNED', UPPER(COLUMN_TYPE)) = 0 AND LOCATE('(1)', COLUMN_TYPE) != 0 THEN 'BIT' WHEN LOCATE('UNSIGNED', UPPER(COLUMN_TYPE)) != 0 AND LOCATE('UNSIGNED', UPPER(DATA_TYPE)) = 0 THEN 'TINYINT UNSIGNED' ELSE DATA_TYPE END WHEN LOCATE('UNSIGNED', UPPER(COLUMN_TYPE)) != 0 AND LOCATE('UNSIGNED', UPPER(DATA_TYPE)) = 0 AND LOCATE('SET', UPPER(DATA_TYPE)) <> 1 AND LOCATE('ENUM', UPPER(DATA_TYPE)) <> 1 THEN CONCAT(DATA_TYPE, ' UNSIGNED') WHEN UPPER(DATA_TYPE) = 'POINT' THEN 'GEOMETRY' WHEN UPPER(DATA_TYPE) = 'LINESTRING' THEN 'GEOMETRY' WHEN UPPER(DATA_TYPE) = 'POLYGON' THEN 'GEOMETRY' WHEN UPPER(DATA_TYPE) = 'MULTIPOINT' THEN 'GEOMETRY' WHEN UPPER(DATA_TYPE) = 'MULTILINESTRING' THEN 'GEOMETRY' WHEN UPPER(DATA_TYPE) = 'MULTIPOLYGON' THEN 'GEOMETRY' WHEN UPPER(DATA_TYPE) = 'GEOMETRYCOLLECTION' THEN 'GEOMETRY' WHEN UPPER(DATA_TYPE) = 'GEOMCOLLECTION' THEN 'GEOMETRY' ELSE UPPER(DATA_TYPE) END) AS TYPE_NAME, UPPER(CASE WHEN UPPER(DATA_TYPE) = 'YEAR' THEN 4 WHEN UPPER(DATA_TYPE) = 'DATE' THEN 10 WHEN UPPER(DATA_TYPE) = 'DATETIME' OR UPPER(DATA_TYPE) = 'TIMESTAMP' THEN 19 + IF(DATETIME_PRECISION > 0, DATETIME_PRECISION + 1, DATETIME_PRECISION) WHEN UPPER(DATA_TYPE) = 'TIME' THEN 8 + IF(DATETIME_PRECISION > 0, DATETIME_PRECISION + 1, DATETIME_PRECISION) WHEN UPPER(DATA_TYPE) = 'TINYINT' AND LOCATE('ZEROFILL', UPPER(COLUMN_TYPE)) = 0 AND LOCATE('UNSIGNED', UPPER(COLUMN_TYPE)) = 0 AND LOCATE('(1)', COLUMN_TYPE) != 0 THEN 1 WHEN UPPER(DATA_TYPE) = 'MEDIUMINT' AND LOCATE('UNSIGNED', UPPER(COLUMN_TYPE)) != 0 THEN 8 WHEN UPPER(DATA_TYPE) = 'JSON' THEN 1073741824 WHEN UPPER(DATA_TYPE) = 'GEOMETRY' THEN 65535 WHEN UPPER(DATA_TYPE) = 'POINT' THEN 65535 WHEN UPPER(DATA_TYPE) = 'LINESTRING' THEN 65535 WHEN UPPER(DATA_TYPE) = 'POLYGON' THEN 65535 WHEN UPPER(DATA_TYPE) = 'MULTIPOINT' THEN 65535 WHEN UPPER(DATA_TYPE) = 'MULTILINESTRING' THEN 65535 WHEN UPPER(DATA_TYPE) = 'MULTIPOLYGON' THEN 65535 WHEN UPPER(DATA_TYPE) = 'GEOMETRYCOLLECTION' THEN 65535 WHEN UPPER(DATA_TYPE) = 'GEOMCOLLECTION' THEN 65535 WHEN CHARACTER_MAXIMUM_LENGTH IS NULL THEN NUMERIC_PRECISION WHEN CHARACTER_MAXIMUM_LENGTH > 2147483647 THEN 2147483647 ELSE CHARACTER_MAXIMUM_LENGTH END) AS COLUMN_SIZE, 65535 AS BUFFER_LENGTH, UPPER(CASE WHEN UPPER(DATA_TYPE) = 'DECIMAL' THEN NUMERIC_SCALE WHEN UPPER(DATA_TYPE) = 'FLOAT' OR UPPER(DATA_TYPE) = 'DOUBLE' THEN IF(NUMERIC_SCALE IS NULL, 0, NUMERIC_SCALE) ELSE NULL END) AS DECIMAL_DIGITS, 10 AS NUM_PREC_RADIX, CASE WHEN IS_NULLABLE COLLATE utf8mb3_general_ci= 'NO' THEN 0 ELSE CASE WHEN IS_NULLABLE COLLATE utf8mb3_general_ci= 'YES' THEN 1 ELSE 2 END END AS NULLABLE, COLUMN_COMMENT AS REMARKS, COLUMN_DEFAULT AS COLUMN_DEF, 0 AS SQL_DATA_TYPE, 0 AS SQL_DATETIME_SUB, CASE WHEN CHARACTER_OCTET_LENGTH > 2147483647 THEN 2147483647 ELSE CHARACTER_OCTET_LENGTH END AS CHAR_OCTET_LENGTH, ORDINAL_POSITION, IS_NULLABLE, NULL AS SCOPE_CATALOG, NULL AS SCOPE_SCHEMA, NULL AS SCOPE_TABLE, NULL AS SOURCE_DATA_TYPE, IF (EXTRA COLLATE utf8mb3_general_ci LIKE '%auto_increment%','YES','NO') AS IS_AUTOINCREMENT, IF (EXTRA COLLATE utf8mb3_general_ci LIKE  '%GENERATED%','YES','NO') AS IS_GENERATEDCOLUMN FROM INFORMATION_SCHEMA.COLUMNS WHERE TABLE_SCHEMA = '__SCHEMA__' AND TABLE_NAME LIKE '__TABLE_PATTERN__' AND COLUMN_NAME LIKE '__COLUMN_PATTERN__' ORDER BY TABLE_SCHEMA, TABLE_NAME, ORDINAL_POSITION"#;

pub const CONNECTOR_J_GET_PRIMARY_KEYS_TEMPLATE: &str = r#"SELECT TABLE_SCHEMA AS TABLE_CAT, NULL AS TABLE_SCHEM, TABLE_NAME, COLUMN_NAME, SEQ_IN_INDEX AS KEY_SEQ, 'PRIMARY' AS PK_NAME FROM INFORMATION_SCHEMA.STATISTICS WHERE TABLE_SCHEMA = '__SCHEMA__' AND TABLE_NAME = '__TABLE__' AND INDEX_NAME = 'PRIMARY' ORDER BY TABLE_SCHEMA, TABLE_NAME, COLUMN_NAME, SEQ_IN_INDEX"#;

pub const CONNECTOR_J_GET_INDEX_INFO_TEMPLATE: &str = r#"SELECT TABLE_SCHEMA AS TABLE_CAT, NULL AS TABLE_SCHEM, TABLE_NAME, NON_UNIQUE, NULL AS INDEX_QUALIFIER, INDEX_NAME, 3 AS TYPE, SEQ_IN_INDEX AS ORDINAL_POSITION, COLUMN_NAME, COLLATION AS ASC_OR_DESC, CARDINALITY, 0 AS PAGES, NULL AS FILTER_CONDITION FROM INFORMATION_SCHEMA.STATISTICS WHERE TABLE_SCHEMA = '__SCHEMA__' AND TABLE_NAME = '__TABLE__' ORDER BY NON_UNIQUE, INDEX_NAME, SEQ_IN_INDEX"#;

pub const CONNECTOR_J_GET_IMPORTED_KEYS_TEMPLATE: &str = r#"SELECT DISTINCT A.REFERENCED_TABLE_SCHEMA AS PKTABLE_CAT, NULL AS PKTABLE_SCHEM, A.REFERENCED_TABLE_NAME AS PKTABLE_NAME, A.REFERENCED_COLUMN_NAME AS PKCOLUMN_NAME, A.TABLE_SCHEMA AS FKTABLE_CAT, NULL AS FKTABLE_SCHEM, A.TABLE_NAME AS FKTABLE_NAME, A.COLUMN_NAME AS FKCOLUMN_NAME, A.ORDINAL_POSITION AS KEY_SEQ, CASE WHEN R.UPDATE_RULE = 'CASCADE' THEN 0 WHEN R.UPDATE_RULE = 'SET NULL' THEN 2 WHEN R.UPDATE_RULE = 'SET DEFAULT' THEN 4 WHEN R.UPDATE_RULE = 'RESTRICT' THEN 1 WHEN R.UPDATE_RULE = 'NO ACTION' THEN 1 ELSE 1 END AS UPDATE_RULE, CASE WHEN R.DELETE_RULE = 'CASCADE' THEN 0 WHEN R.DELETE_RULE = 'SET NULL' THEN 2 WHEN R.DELETE_RULE = 'SET DEFAULT' THEN 4 WHEN R.DELETE_RULE = 'RESTRICT' THEN 1 WHEN R.DELETE_RULE = 'NO ACTION' THEN 1 ELSE 1 END AS DELETE_RULE, A.CONSTRAINT_NAME AS FK_NAME, R.UNIQUE_CONSTRAINT_NAME AS PK_NAME, 7 AS DEFERRABILITY FROM INFORMATION_SCHEMA.KEY_COLUMN_USAGE A JOIN INFORMATION_SCHEMA.TABLE_CONSTRAINTS B USING (CONSTRAINT_SCHEMA, CONSTRAINT_NAME, TABLE_NAME) JOIN INFORMATION_SCHEMA.REFERENTIAL_CONSTRAINTS R ON (R.CONSTRAINT_NAME = B.CONSTRAINT_NAME AND R.TABLE_NAME = B.TABLE_NAME AND R.CONSTRAINT_SCHEMA = B.TABLE_SCHEMA)WHERE B.CONSTRAINT_TYPE = 'FOREIGN KEY' AND A.TABLE_SCHEMA = '__SCHEMA__' AND A.TABLE_NAME = '__TABLE__' AND A.REFERENCED_TABLE_SCHEMA IS NOT NULL ORDER BY A.REFERENCED_TABLE_SCHEMA, A.REFERENCED_TABLE_NAME, A.ORDINAL_POSITION"#;

pub const CONNECTOR_J_GET_EXPORTED_KEYS_TEMPLATE: &str = r#"SELECT DISTINCT A.REFERENCED_TABLE_SCHEMA AS PKTABLE_CAT, NULL AS PKTABLE_SCHEM, A.REFERENCED_TABLE_NAME AS PKTABLE_NAME, A.REFERENCED_COLUMN_NAME AS PKCOLUMN_NAME, A.TABLE_SCHEMA AS FKTABLE_CAT, NULL AS FKTABLE_SCHEM, A.TABLE_NAME AS FKTABLE_NAME, A.COLUMN_NAME AS FKCOLUMN_NAME, A.ORDINAL_POSITION AS KEY_SEQ, CASE WHEN R.UPDATE_RULE = 'CASCADE' THEN 0 WHEN R.UPDATE_RULE = 'SET NULL' THEN 2 WHEN R.UPDATE_RULE = 'SET DEFAULT' THEN 4 WHEN R.UPDATE_RULE = 'RESTRICT' THEN 1 WHEN R.UPDATE_RULE = 'NO ACTION' THEN 1 ELSE 1 END AS UPDATE_RULE, CASE WHEN R.DELETE_RULE = 'CASCADE' THEN 0 WHEN R.DELETE_RULE = 'SET NULL' THEN 2 WHEN R.DELETE_RULE = 'SET DEFAULT' THEN 4 WHEN R.DELETE_RULE = 'RESTRICT' THEN 1 WHEN R.DELETE_RULE = 'NO ACTION' THEN 1 ELSE 1 END AS DELETE_RULE, A.CONSTRAINT_NAME AS FK_NAME, TC.CONSTRAINT_NAME AS PK_NAME, 7 AS DEFERRABILITY FROM INFORMATION_SCHEMA.KEY_COLUMN_USAGE A JOIN INFORMATION_SCHEMA.TABLE_CONSTRAINTS B USING (TABLE_SCHEMA, TABLE_NAME, CONSTRAINT_NAME) JOIN INFORMATION_SCHEMA.REFERENTIAL_CONSTRAINTS R ON (R.CONSTRAINT_NAME = B.CONSTRAINT_NAME AND R.TABLE_NAME = B.TABLE_NAME AND R.CONSTRAINT_SCHEMA = B.TABLE_SCHEMA) LEFT JOIN INFORMATION_SCHEMA.TABLE_CONSTRAINTS TC ON (A.REFERENCED_TABLE_SCHEMA = TC.TABLE_SCHEMA AND A.REFERENCED_TABLE_NAME = TC.TABLE_NAME AND TC.CONSTRAINT_TYPE IN ('UNIQUE', 'PRIMARY KEY')) WHERE B.CONSTRAINT_TYPE = 'FOREIGN KEY' AND A.REFERENCED_TABLE_SCHEMA = '__SCHEMA__' AND A.REFERENCED_TABLE_NAME = '__TABLE__' ORDER BY FKTABLE_CAT, FKTABLE_SCHEM, FKTABLE_NAME, KEY_SEQ"#;

fn same_catalog_tokens(actual: &[Token], expected: &[Token]) -> bool {
    let actual = actual
        .iter()
        .filter(|token| !matches!(token, Token::Whitespace(_)))
        .collect::<Vec<_>>();
    let expected = expected
        .iter()
        .filter(|token| !matches!(token, Token::Whitespace(_)))
        .collect::<Vec<_>>();
    actual.len() == expected.len()
        && actual
            .into_iter()
            .zip(expected)
            .all(|(actual, expected)| same_catalog_token(actual, expected))
}

fn same_catalog_token(actual: &Token, expected: &Token) -> bool {
    match (actual, expected) {
        (Token::Word(actual), Token::Word(expected)) => {
            actual.quote_style == expected.quote_style
                && actual.value.eq_ignore_ascii_case(&expected.value)
        }
        _ => actual == expected,
    }
}

#[cfg(test)]
mod connector_j_tests {
    use super::*;

    #[test]
    fn measured_connector_j_queries_capture_only_catalog_filters() {
        let tables = CONNECTOR_J_GET_TABLES_TEMPLATE
            .replace("'__SCHEMA__'", "'reports'")
            .replace("'__TABLE_PATTERN__'", "'jdbc_%'")
            .replace("__TYPE1__", "'TABLE'")
            .replace("__TYPE2__", "'VIEW'")
            .replace("__TYPE3__", "null")
            .replace("__TYPE4__", "null")
            .replace("__TYPE5__", "null");
        assert_eq!(
            parse_optional_connector_j_information_schema_query(&tables, SessionSqlMode::default())
                .unwrap(),
            Some(ConnectorJInformationSchemaQuery::Tables {
                schema: "reports".to_owned(),
                table_pattern: "jdbc_%".to_owned(),
                types: vec!["TABLE".to_owned(), "VIEW".to_owned()],
            })
        );
        let columns = CONNECTOR_J_GET_COLUMNS_TEMPLATE
            .replace("'__SCHEMA__'", "'reports'")
            .replace("'__TABLE_PATTERN__'", "'jdbc_records'")
            .replace("'__COLUMN_PATTERN__'", "'%'");
        assert_eq!(
            parse_optional_connector_j_information_schema_query(
                &columns,
                SessionSqlMode::default()
            )
            .unwrap(),
            Some(ConnectorJInformationSchemaQuery::Columns {
                schema: "reports".to_owned(),
                table_pattern: Some("jdbc_records".to_owned()),
                column_pattern: "%".to_owned(),
            })
        );
        let columns_without_table_pattern =
            columns.replace(" AND TABLE_NAME LIKE 'jdbc_records'", "");
        assert_eq!(
            parse_optional_connector_j_information_schema_query(
                &columns_without_table_pattern,
                SessionSqlMode::default()
            )
            .unwrap(),
            Some(ConnectorJInformationSchemaQuery::Columns {
                schema: "reports".to_owned(),
                table_pattern: None,
                column_pattern: "%".to_owned(),
            })
        );
        assert_eq!(
            parse_optional_connector_j_information_schema_query(
                &columns.replace("NUM_PREC_RADIX", "NUM_PREC_BASE"),
                SessionSqlMode::default()
            )
            .unwrap(),
            None
        );
    }

    #[test]
    fn measured_connector_j_key_queries_capture_table() {
        for (template, expected) in [
            (
                CONNECTOR_J_GET_PRIMARY_KEYS_TEMPLATE,
                ConnectorJInformationSchemaQuery::PrimaryKeys {
                    schema: "reports".to_owned(),
                    table: "jdbc_records".to_owned(),
                },
            ),
            (
                CONNECTOR_J_GET_INDEX_INFO_TEMPLATE,
                ConnectorJInformationSchemaQuery::IndexInfo {
                    schema: "reports".to_owned(),
                    table: "jdbc_records".to_owned(),
                },
            ),
            (
                CONNECTOR_J_GET_IMPORTED_KEYS_TEMPLATE,
                ConnectorJInformationSchemaQuery::ImportedKeys {
                    schema: "reports".to_owned(),
                    table: "jdbc_records".to_owned(),
                },
            ),
            (
                CONNECTOR_J_GET_EXPORTED_KEYS_TEMPLATE,
                ConnectorJInformationSchemaQuery::ExportedKeys {
                    schema: "reports".to_owned(),
                    table: "jdbc_records".to_owned(),
                },
            ),
        ] {
            let sql = template
                .replace("'__SCHEMA__'", "'reports'")
                .replace("'__TABLE__'", "'jdbc_records'");
            assert_eq!(
                parse_optional_connector_j_information_schema_query(
                    &sql,
                    SessionSqlMode::default()
                )
                .unwrap(),
                Some(expected)
            );
        }
    }

    #[test]
    fn rendered_foreign_keys_can_be_used_for_connector_j_rows() {
        let keys = parse_connector_j_foreign_keys(
            "CREATE TABLE `jdbc_child` (`id` int NOT NULL, `parent_id` int, CONSTRAINT `fk_jdbc_parent` FOREIGN KEY (`parent_id`) REFERENCES `jdbc_records` (`id`) ON DELETE CASCADE)",
            SessionSqlMode::default(),
        ).unwrap();
        assert_eq!(keys.len(), 1);
        assert_eq!(keys[0].name, "fk_jdbc_parent");
        assert_eq!(keys[0].parent_table, "jdbc_records");
        assert_eq!(keys[0].child_columns, vec!["parent_id"]);
        assert_eq!(keys[0].parent_columns, vec!["id"]);
        assert_eq!(keys[0].on_delete.as_deref(), Some("CASCADE"));
    }

    #[test]
    fn connector_j_information_schema_collation_query_is_exact() {
        let query = "SELECT DEFAULT_COLLATION_NAME FROM INFORMATION_SCHEMA.SCHEMATA WHERE SCHEMA_NAME = 'information_schema'";
        assert!(is_connector_j_information_schema_collation_query(
            query,
            SessionSqlMode::default()
        )
        .unwrap());
        assert!(!is_connector_j_information_schema_collation_query(
            &query.replace("'information_schema'", "'reports'"),
            SessionSqlMode::default(),
        )
        .unwrap());
        assert!(!is_connector_j_information_schema_collation_query(
            "SELECT DEFAULT_COLLATION_NAME FROM INFORMATION_SCHEMA.SCHEMATA",
            SessionSqlMode::default(),
        )
        .unwrap());
        let ordinary_query = "SELECT TABLE_NAME, TABLE_TYPE FROM information_schema.TABLES WHERE TABLE_TYPE = 'BASE TABLE' ORDER BY TABLE_NAME DESC";
        assert!(!is_connector_j_information_schema_collation_query(
            ordinary_query,
            SessionSqlMode::default(),
        )
        .unwrap());
        assert_eq!(
            parse_optional_connector_j_information_schema_query(
                ordinary_query,
                SessionSqlMode::default(),
            )
            .unwrap(),
            None,
        );
    }

    #[test]
    fn connector_j_schemata_listings_and_untyped_tables_are_measured_shapes() {
        let catalogs =
            "SELECT SCHEMA_NAME AS TABLE_CAT FROM INFORMATION_SCHEMA.SCHEMATA ORDER BY TABLE_CAT";
        assert_eq!(
            parse_optional_connector_j_schemata_listing_query(catalogs, SessionSqlMode::default())
                .unwrap(),
            Some(ConnectorJSchemataListingQuery::Catalogs),
        );
        let schemas = "SELECT SCHEMA_NAME AS TABLE_SCHEM, CATALOG_NAME AS TABLE_CATALOG FROM INFORMATION_SCHEMA.SCHEMATA WHERE FALSE ORDER BY TABLE_CATALOG, TABLE_SCHEM";
        assert_eq!(
            parse_optional_connector_j_schemata_listing_query(schemas, SessionSqlMode::default())
                .unwrap(),
            Some(ConnectorJSchemataListingQuery::Schemas),
        );
        let tables = CONNECTOR_J_GET_TABLES_TEMPLATE
            .replace("'__SCHEMA__'", "'reports'")
            .replace("'__TABLE_PATTERN__'", "'%'")
            .replace(
                " HAVING TABLE_TYPE IN (__TYPE1__,__TYPE2__,__TYPE3__,__TYPE4__,__TYPE5__)",
                "",
            );
        assert_eq!(
            parse_optional_connector_j_information_schema_query(&tables, SessionSqlMode::default())
                .unwrap(),
            Some(ConnectorJInformationSchemaQuery::Tables {
                schema: "reports".to_owned(),
                table_pattern: "%".to_owned(),
                types: Vec::new(),
            }),
        );
    }

    #[test]
    fn connector_j_reserved_keywords_query_is_exact() {
        let query = "SELECT WORD FROM INFORMATION_SCHEMA.KEYWORDS WHERE RESERVED = 1 ORDER BY WORD";
        assert!(is_connector_j_reserved_keywords_query(query, SessionSqlMode::default()).unwrap());
        assert!(!is_connector_j_reserved_keywords_query(
            "SELECT WORD FROM INFORMATION_SCHEMA.KEYWORDS ORDER BY WORD",
            SessionSqlMode::default(),
        )
        .unwrap());
        assert!(!is_connector_j_reserved_keywords_query(
            &query.replace("RESERVED = 1", "RESERVED = 0"),
            SessionSqlMode::default(),
        )
        .unwrap());
    }
}

pub(crate) fn contains_information_schema_tables(tokens: &[Token]) -> bool {
    contains_information_schema_object(tokens, "TABLES")
}

pub(crate) fn contains_information_schema_object(tokens: &[Token], expected_object: &str) -> bool {
    let significant = tokens
        .iter()
        .filter(|token| !matches!(token, Token::Whitespace(_)))
        .collect::<Vec<_>>();
    significant.windows(3).any(|window| {
        is_information_schema_identifier_token(window[0], "information_schema")
            && matches!(window[1], Token::Period)
            && is_information_schema_identifier_token(window[2], expected_object)
    })
}

fn is_information_schema_identifier_token(token: &Token, expected: &str) -> bool {
    matches!(
        token,
        Token::Word(word) if word.value.eq_ignore_ascii_case(expected)
    )
}

pub(crate) fn reject_information_schema_query_tokens(tokens: &[Token]) -> Result<(), ParseError> {
    if tokens.iter().any(|token| {
        matches!(
            token,
            Token::Whitespace(
                Whitespace::SingleLineComment { .. } | Whitespace::MultiLineComment(_)
            )
        )
    }) {
        return unsupported("comments in information_schema.TABLES query");
    }

    let semicolon_count = tokens
        .iter()
        .filter(|token| matches!(token, Token::SemiColon))
        .count();
    if semicolon_count > 1 {
        return unsupported("multiple information_schema.TABLES statements");
    }
    if semicolon_count == 1 {
        let last = tokens
            .iter()
            .rposition(|token| !matches!(token, Token::Whitespace(_)));
        if !matches!(
            last.and_then(|index| tokens.get(index)),
            Some(Token::SemiColon)
        ) {
            return unsupported("information_schema.TABLES semicolon position");
        }
    }
    Ok(())
}

pub(crate) fn validate_information_schema_tables_query(
    query: &sqlparser::ast::Query,
) -> Result<Vec<super::MySqlInformationSchemaTablesColumn>, ParseError> {
    if query.with.is_some()
        || query.limit_clause.is_some()
        || query.fetch.is_some()
        || !query.locks.is_empty()
        || query.for_clause.is_some()
        || query.settings.is_some()
        || query.format_clause.is_some()
        || !query.pipe_operators.is_empty()
    {
        return unsupported("information_schema.TABLES query clause");
    }
    let SetExpr::Select(select) = query.body.as_ref() else {
        return unsupported("information_schema.TABLES compound query");
    };
    if !matches!(select.flavor, SelectFlavor::Standard)
        || !select.optimizer_hints.is_empty()
        || select.distinct.is_some()
        || select.select_modifiers.is_some()
        || select.top.is_some()
        || select.top_before_distinct
        || select.exclude.is_some()
        || select.into.is_some()
        || !select.lateral_views.is_empty()
        || select.prewhere.is_some()
        || !select.connect_by.is_empty()
        || !matches!(
            &select.group_by,
            sqlparser::ast::GroupByExpr::Expressions(exprs, _) if exprs.is_empty()
        )
        || !select.cluster_by.is_empty()
        || !select.distribute_by.is_empty()
        || !select.sort_by.is_empty()
        || select.having.is_some()
        || !select.named_window.is_empty()
        || select.qualify.is_some()
        || select.window_before_qualify
        || select.value_table_mode.is_some()
    {
        return unsupported("information_schema.TABLES SELECT feature");
    }

    // Any of the columns this answers, in any order a query names them, which
    // is the order MySQL answers in. A column outside that set is refused
    // rather than answered with a value that would be made up.
    let columns = projected_columns(
        &select.projection,
        super::MySqlInformationSchemaTablesColumn::named,
    )
    .ok_or(ParseError::Unsupported {
        feature: "information_schema.TABLES projection",
    })?;

    let [from] = select.from.as_slice() else {
        return unsupported("information_schema.TABLES table source");
    };
    if !from.joins.is_empty() {
        return unsupported("information_schema.TABLES JOIN");
    }
    let TableFactor::Table {
        name,
        alias,
        args,
        with_hints,
        version,
        with_ordinality,
        partitions,
        json_path,
        sample,
        index_hints,
    } = &from.relation
    else {
        return unsupported("information_schema.TABLES table source");
    };
    if alias.is_some()
        || args.is_some()
        || !with_hints.is_empty()
        || version.is_some()
        || *with_ordinality
        || !partitions.is_empty()
        || json_path.is_some()
        || sample.is_some()
        || !index_hints.is_empty()
    {
        return unsupported("information_schema.TABLES table option");
    }
    let [ObjectNamePart::Identifier(database), ObjectNamePart::Identifier(table)] =
        name.0.as_slice()
    else {
        return unsupported("qualified information_schema.TABLES source");
    };
    if !is_identifier_named(database, "information_schema") || !is_identifier_named(table, "TABLES")
    {
        return unsupported("information_schema.TABLES source");
    }

    let Some(selection) = select.selection.as_ref() else {
        return unsupported("information_schema.TABLES WHERE clause");
    };
    let Expr::BinaryOp { left, op, right } = selection else {
        return unsupported("information_schema.TABLES WHERE clause");
    };
    if !matches!(op, BinaryOperator::Eq)
        || !matches!(left.as_ref(), Expr::Identifier(identifier) if is_identifier_named(identifier, "TABLE_SCHEMA"))
        || !is_database_function(right)
    {
        return unsupported("information_schema.TABLES WHERE clause");
    }

    // The rows come back in table-name order whether or not the query asks
    // for it, so an absent ORDER BY is taken and the one MySQL clients write
    // is taken as well. Any other ordering is refused.
    if let Some(order_by) = query.order_by.as_ref() {
        let sqlparser::ast::OrderByKind::Expressions(expressions) = &order_by.kind else {
            return unsupported("information_schema.TABLES ORDER BY clause");
        };
        let [order] = expressions.as_slice() else {
            return unsupported("information_schema.TABLES ORDER BY clause");
        };
        if order_by.interpolate.is_some()
            || !an_ordering_these_rows_already_have(&order.options)
            || order.with_fill.is_some()
            || !matches!(&order.expr, Expr::Identifier(identifier) if is_identifier_named(identifier, "TABLE_NAME"))
        {
            return unsupported("information_schema.TABLES ORDER BY clause");
        }
    }
    Ok(columns)
}

/// Reads a projection as the columns it names, in the order it names them.
///
/// Every item has to be a plain column of the table being read: a wildcard
/// would name columns this does not answer, and an alias or an expression is
/// a shape the result metadata is not built for.
fn projected_columns<T: PartialEq>(
    projection: &[SelectItem],
    named: impl Fn(&str) -> Option<T>,
) -> Option<Vec<T>> {
    if projection.is_empty() {
        return None;
    }
    let mut columns: Vec<T> = Vec::with_capacity(projection.len());
    for item in projection {
        let SelectItem::UnnamedExpr(Expr::Identifier(identifier)) = item else {
            return None;
        };
        let column = named(&identifier.value)?;
        // MySQL takes the same column named twice and answers it twice. It is
        // refused here so that what a row holds is never wider than what the
        // whole row would hold, which is what the result's size is measured
        // against.
        if columns.contains(&column) {
            return None;
        }
        columns.push(column);
    }
    Some(columns)
}

pub(crate) fn validate_information_schema_schemata_query(
    query: &sqlparser::ast::Query,
) -> Result<(), ParseError> {
    if query.with.is_some()
        || query.limit_clause.is_some()
        || query.fetch.is_some()
        || !query.locks.is_empty()
        || query.for_clause.is_some()
        || query.settings.is_some()
        || query.format_clause.is_some()
        || !query.pipe_operators.is_empty()
        || query.order_by.is_some()
    {
        return unsupported("information_schema.SCHEMATA query clause");
    }
    let SetExpr::Select(select) = query.body.as_ref() else {
        return unsupported("information_schema.SCHEMATA compound query");
    };
    if !matches!(select.flavor, SelectFlavor::Standard)
        || !select.optimizer_hints.is_empty()
        || select.distinct.is_some()
        || select.select_modifiers.is_some()
        || select.top.is_some()
        || select.top_before_distinct
        || select.exclude.is_some()
        || select.into.is_some()
        || !select.lateral_views.is_empty()
        || select.prewhere.is_some()
        || !select.connect_by.is_empty()
        || !matches!(
            &select.group_by,
            sqlparser::ast::GroupByExpr::Expressions(exprs, _) if exprs.is_empty()
        )
        || !select.cluster_by.is_empty()
        || !select.distribute_by.is_empty()
        || !select.sort_by.is_empty()
        || select.having.is_some()
        || !select.named_window.is_empty()
        || select.qualify.is_some()
        || select.window_before_qualify
        || select.value_table_mode.is_some()
    {
        return unsupported("information_schema.SCHEMATA SELECT feature");
    }

    let [SelectItem::UnnamedExpr(Expr::Identifier(schema_name))] = select.projection.as_slice()
    else {
        return unsupported("information_schema.SCHEMATA projection");
    };
    if !is_identifier_named(schema_name, "SCHEMA_NAME") {
        return unsupported("information_schema.SCHEMATA projection");
    }

    let [from] = select.from.as_slice() else {
        return unsupported("information_schema.SCHEMATA table source");
    };
    if !from.joins.is_empty() {
        return unsupported("information_schema.SCHEMATA JOIN");
    }
    let TableFactor::Table {
        name,
        alias,
        args,
        with_hints,
        version,
        with_ordinality,
        partitions,
        json_path,
        sample,
        index_hints,
    } = &from.relation
    else {
        return unsupported("information_schema.SCHEMATA table source");
    };
    if alias.is_some()
        || args.is_some()
        || !with_hints.is_empty()
        || version.is_some()
        || *with_ordinality
        || !partitions.is_empty()
        || json_path.is_some()
        || sample.is_some()
        || !index_hints.is_empty()
    {
        return unsupported("information_schema.SCHEMATA table option");
    }
    let [ObjectNamePart::Identifier(database), ObjectNamePart::Identifier(table)] =
        name.0.as_slice()
    else {
        return unsupported("qualified information_schema.SCHEMATA source");
    };
    if !is_identifier_named(database, "information_schema")
        || !is_identifier_named(table, "SCHEMATA")
    {
        return unsupported("information_schema.SCHEMATA source");
    }
    if select.selection.is_some() {
        return unsupported("information_schema.SCHEMATA WHERE clause");
    }
    Ok(())
}

pub(crate) fn reject_information_schema_schemata_query_tokens(
    tokens: &[Token],
) -> Result<(), ParseError> {
    if tokens.iter().any(|token| {
        matches!(
            token,
            Token::Whitespace(
                Whitespace::SingleLineComment { .. } | Whitespace::MultiLineComment(_)
            )
        )
    }) {
        return unsupported("comments in information_schema.SCHEMATA query");
    }

    let semicolon_count = tokens
        .iter()
        .filter(|token| matches!(token, Token::SemiColon))
        .count();
    if semicolon_count > 1 {
        return unsupported("multiple information_schema.SCHEMATA statements");
    }
    if semicolon_count == 1 {
        let last = tokens
            .iter()
            .rposition(|token| !matches!(token, Token::Whitespace(_)));
        if !matches!(
            last.and_then(|index| tokens.get(index)),
            Some(Token::SemiColon)
        ) {
            return unsupported("information_schema.SCHEMATA semicolon position");
        }
    }
    Ok(())
}

pub(crate) fn reject_information_schema_columns_query_tokens(
    tokens: &[Token],
) -> Result<(), ParseError> {
    if tokens.iter().any(|token| {
        matches!(
            token,
            Token::Whitespace(
                Whitespace::SingleLineComment { .. } | Whitespace::MultiLineComment(_)
            )
        )
    }) {
        return unsupported("comments in information_schema.COLUMNS query");
    }

    let semicolon_count = tokens
        .iter()
        .filter(|token| matches!(token, Token::SemiColon))
        .count();
    if semicolon_count > 1 {
        return unsupported("multiple information_schema.COLUMNS statements");
    }
    if semicolon_count == 1 {
        let last = tokens
            .iter()
            .rposition(|token| !matches!(token, Token::Whitespace(_)));
        if !matches!(
            last.and_then(|index| tokens.get(index)),
            Some(Token::SemiColon)
        ) {
            return unsupported("information_schema.COLUMNS semicolon position");
        }
    }
    Ok(())
}

pub(crate) fn validate_information_schema_columns_query(
    query: &sqlparser::ast::Query,
) -> Result<
    (
        Option<String>,
        MySqlTableName,
        Vec<super::MySqlInformationSchemaColumnsColumn>,
    ),
    ParseError,
> {
    if query.with.is_some()
        || query.limit_clause.is_some()
        || query.fetch.is_some()
        || !query.locks.is_empty()
        || query.for_clause.is_some()
        || query.settings.is_some()
        || query.format_clause.is_some()
        || !query.pipe_operators.is_empty()
    {
        return unsupported("information_schema.COLUMNS query clause");
    }
    let SetExpr::Select(select) = query.body.as_ref() else {
        return unsupported("information_schema.COLUMNS compound query");
    };
    if !matches!(select.flavor, SelectFlavor::Standard)
        || !select.optimizer_hints.is_empty()
        || select.distinct.is_some()
        || select.select_modifiers.is_some()
        || select.top.is_some()
        || select.top_before_distinct
        || select.exclude.is_some()
        || select.into.is_some()
        || !select.lateral_views.is_empty()
        || select.prewhere.is_some()
        || !select.connect_by.is_empty()
        || !matches!(
            &select.group_by,
            sqlparser::ast::GroupByExpr::Expressions(exprs, _) if exprs.is_empty()
        )
        || !select.cluster_by.is_empty()
        || !select.distribute_by.is_empty()
        || !select.sort_by.is_empty()
        || select.having.is_some()
        || !select.named_window.is_empty()
        || select.qualify.is_some()
        || select.window_before_qualify
        || select.value_table_mode.is_some()
    {
        return unsupported("information_schema.COLUMNS SELECT feature");
    }

    let columns = projected_columns(
        &select.projection,
        super::MySqlInformationSchemaColumnsColumn::named,
    )
    .ok_or(ParseError::Unsupported {
        feature: "information_schema.COLUMNS projection",
    })?;

    let [from] = select.from.as_slice() else {
        return unsupported("information_schema.COLUMNS table source");
    };
    if !from.joins.is_empty() {
        return unsupported("information_schema.COLUMNS JOIN");
    }
    let TableFactor::Table {
        name,
        alias,
        args,
        with_hints,
        version,
        with_ordinality,
        partitions,
        json_path,
        sample,
        index_hints,
    } = &from.relation
    else {
        return unsupported("information_schema.COLUMNS table source");
    };
    if alias.is_some()
        || args.is_some()
        || !with_hints.is_empty()
        || version.is_some()
        || *with_ordinality
        || !partitions.is_empty()
        || json_path.is_some()
        || sample.is_some()
        || !index_hints.is_empty()
    {
        return unsupported("information_schema.COLUMNS table option");
    }
    let [ObjectNamePart::Identifier(database), ObjectNamePart::Identifier(table)] =
        name.0.as_slice()
    else {
        return unsupported("qualified information_schema.COLUMNS source");
    };
    if !is_identifier_named(database, "information_schema")
        || !is_identifier_named(table, "COLUMNS")
    {
        return unsupported("information_schema.COLUMNS source");
    }

    let Some(selection) = select.selection.as_ref() else {
        return unsupported("information_schema.COLUMNS WHERE clause");
    };
    let Expr::BinaryOp {
        left: schema_predicate,
        op: BinaryOperator::And,
        right: table_predicate,
    } = selection
    else {
        return unsupported("information_schema.COLUMNS WHERE clause");
    };
    let schema = information_schema_columns_schema(schema_predicate)?;
    let table = information_schema_columns_table_name(table_predicate)?;

    // The rows come back in declaration order whether or not the query asks
    // for it, so an absent ORDER BY is taken and so is the one MySQL clients
    // write.
    if let Some(order_by) = query.order_by.as_ref() {
        let sqlparser::ast::OrderByKind::Expressions(expressions) = &order_by.kind else {
            return unsupported("information_schema.COLUMNS ORDER BY clause");
        };
        let [order] = expressions.as_slice() else {
            return unsupported("information_schema.COLUMNS ORDER BY clause");
        };
        if order_by.interpolate.is_some()
            || !an_ordering_these_rows_already_have(&order.options)
            || order.with_fill.is_some()
            || !matches!(
                &order.expr,
                Expr::Identifier(identifier) if is_identifier_named(identifier, "ORDINAL_POSITION")
            )
        {
            return unsupported("information_schema.COLUMNS ORDER BY clause");
        }
    }
    Ok((schema, table, columns))
}

/// The database one `information_schema.COLUMNS` query asks about.
///
/// `None` says it asked for the selected one with `DATABASE()`; a name says it
/// wrote the database out, which a migration tool that knows which database it
/// is working on does.
fn information_schema_columns_schema(expr: &Expr) -> Result<Option<String>, ParseError> {
    let Expr::BinaryOp {
        left,
        op: BinaryOperator::Eq,
        right,
    } = expr
    else {
        return unsupported("information_schema.COLUMNS WHERE clause");
    };
    if !matches!(left.as_ref(), Expr::Identifier(identifier) if is_identifier_named(identifier, "TABLE_SCHEMA"))
    {
        return unsupported("information_schema.COLUMNS WHERE clause");
    }
    if is_database_function(right) {
        return Ok(None);
    }
    let Expr::Value(value) = right.as_ref() else {
        return unsupported("information_schema.COLUMNS WHERE clause");
    };
    let Value::SingleQuotedString(name) = &value.value else {
        return unsupported("information_schema.COLUMNS WHERE clause");
    };
    Ok(Some(name.clone()))
}

fn information_schema_columns_table_name(expr: &Expr) -> Result<MySqlTableName, ParseError> {
    let Expr::BinaryOp {
        left,
        op: BinaryOperator::Eq,
        right,
    } = expr
    else {
        return unsupported("information_schema.COLUMNS WHERE clause");
    };
    if !matches!(left.as_ref(), Expr::Identifier(identifier) if is_identifier_named(identifier, "TABLE_NAME"))
    {
        return unsupported("information_schema.COLUMNS WHERE clause");
    }
    let Expr::Value(value) = right.as_ref() else {
        return unsupported("information_schema.COLUMNS WHERE clause");
    };
    let Value::SingleQuotedString(name) = &value.value else {
        return unsupported("information_schema.COLUMNS WHERE clause");
    };
    MySqlTableName::parse(name)
}

fn is_identifier_named(identifier: &Ident, expected: &str) -> bool {
    identifier.value.eq_ignore_ascii_case(expected)
}

fn is_database_function(expr: &Expr) -> bool {
    let Expr::Function(function) = expr else {
        return false;
    };
    matches!(
        function.name.0.as_slice(),
        [ObjectNamePart::Identifier(identifier)] if is_identifier_named(identifier, "DATABASE")
    ) && !function.uses_odbc_syntax
        && matches!(function.parameters, FunctionArguments::None)
        && matches!(
            &function.args,
            FunctionArguments::List(arguments)
                if arguments.args.is_empty()
                    && arguments.duplicate_treatment.is_none()
                    && arguments.clauses.is_empty()
        )
        && function.filter.is_none()
        && function.null_treatment.is_none()
        && function.over.is_none()
        && function.within_group.is_empty()
}

/// Reports whether an `ORDER BY` clause asks for the order the rows come back
/// in anyway.
///
/// These rows are answered in one order whether or not the query asks for it,
/// so a clause naming that order says nothing. Measured on MySQL 8.4.11: an
/// explicit `ASC` reads the same rows as no `ASC` at all, and a `DESC` reads
/// them the other way round, which this does not do.
fn an_ordering_these_rows_already_have(options: &sqlparser::ast::OrderByOptions) -> bool {
    options.nulls_first.is_none() && matches!(options.asc, None | Some(true))
}

#[cfg(test)]
mod gorm_prepared_tests {
    use super::*;

    #[test]
    fn accepts_only_the_pinned_gorm_catalog_queries() {
        let mode = SessionSqlMode::default();
        let current = "SELECT SCHEMA_NAME from Information_schema.SCHEMATA where \
            SCHEMA_NAME LIKE ? ORDER BY SCHEMA_NAME=? DESC,SCHEMA_NAME limit 1";
        assert_eq!(
            parse_optional_gorm_information_schema_prepared_query(current, mode).unwrap(),
            Some(GormInformationSchemaPreparedQuery::CurrentDatabase)
        );
        let columns = "SELECT column_name, column_default, is_nullable = 'YES', \
            data_type, character_maximum_length, column_type, column_key, extra, \
            column_comment, numeric_precision, numeric_scale, datetime_precision \
            FROM information_schema.columns WHERE table_schema = ? AND table_name = ? \
            ORDER BY ORDINAL_POSITION";
        assert_eq!(
            parse_optional_gorm_information_schema_prepared_query(columns, mode).unwrap(),
            Some(GormInformationSchemaPreparedQuery::Columns)
        );
        let has_table = "SELECT count(*) FROM information_schema.tables WHERE \
            table_schema = ? AND table_name = ? AND table_type = ?";
        assert_eq!(
            parse_optional_gorm_information_schema_prepared_query(has_table, mode).unwrap(),
            Some(GormInformationSchemaPreparedQuery::HasTable)
        );
        let has_column = "SELECT count(*) FROM INFORMATION_SCHEMA.columns WHERE \
            table_schema = ? AND table_name = ? AND column_name = ?";
        assert_eq!(
            parse_optional_gorm_information_schema_prepared_query(has_column, mode).unwrap(),
            Some(GormInformationSchemaPreparedQuery::HasColumn)
        );
        let has_index = "SELECT count(*) FROM information_schema.statistics WHERE \
            table_schema = ? AND table_name = ? AND index_name = ?";
        assert_eq!(
            parse_optional_gorm_information_schema_prepared_query(has_index, mode).unwrap(),
            Some(GormInformationSchemaPreparedQuery::HasIndex)
        );
        let has_constraint = "SELECT count(*) FROM INFORMATION_SCHEMA.table_constraints WHERE \
            constraint_schema = ? AND table_name = ? AND constraint_name = ?";
        assert_eq!(
            parse_optional_gorm_information_schema_prepared_query(has_constraint, mode).unwrap(),
            Some(GormInformationSchemaPreparedQuery::HasConstraint)
        );
        for sql in [
            columns.replace("table_name = ?", "table_name = 'users'"),
            columns.replace("datetime_precision", "bogus_column"),
            format!("{columns}; DROP TABLE users"),
            format!("{columns} /* comment */"),
            has_table.replace("table_type = ?", "table_type = 'BASE TABLE'"),
            format!("{has_table}; DROP TABLE users"),
            has_column.replace("column_name = ?", "column_name LIKE ?"),
            has_index.replace("index_name = ?", "index_name LIKE ?"),
            has_constraint.replace("constraint_name = ?", "constraint_name LIKE ?"),
        ] {
            assert_eq!(
                parse_optional_gorm_information_schema_prepared_query(&sql, mode).unwrap(),
                None,
                "{sql}"
            );
        }
    }
}
