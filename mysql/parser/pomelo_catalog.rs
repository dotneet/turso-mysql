//! The `information_schema` reads Entity Framework Core's `dbcontext
//! scaffold` makes through Pomelo 9 (`MySqlDatabaseModelFactory`), sent as
//! text with every name written in.
//!
//! They are recognized rather than translated. Each reaches past what the
//! checked `SELECT` takes — a `LEFT JOIN` of `COLLATION_CHARACTER_SET_APPLICABILITY`,
//! `GROUP_CONCAT` over `CAST`, `IFNULL` and `CONCAT_WS` of catalog columns, a
//! correlated subquery — so the server works each answer out from plain reads
//! of the catalog.

use super::*;
use information_schema::connector_j_template_captures;

/// One of the scaffold's catalog reads, with the database and table it names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PomeloInformationSchemaQuery {
    /// The base tables and views of the selected database, with each one's
    /// comment, collation and the collation's character set.
    Tables,
    /// The columns of a table's primary key, joined by commas.
    PrimaryKey { schema: String, table: String },
    /// Every index of a table beside its primary key, one row each.
    Indexes { schema: String, table: String },
    /// Every foreign key of a table, one row each, with its `ON DELETE` rule.
    ForeignKeys { schema: String, table: String },
}

const TABLES: &str = "SELECT `t`.`TABLE_NAME`, `t`.`TABLE_TYPE`, \
    IF(`t`.`TABLE_COMMENT` = 'VIEW' AND `t`.`TABLE_TYPE` = 'VIEW', '', `t`.`TABLE_COMMENT`) AS `TABLE_COMMENT`, \
    `ccsa`.`CHARACTER_SET_NAME` as `TABLE_CHARACTER_SET`, `t`.`TABLE_COLLATION` \
    FROM `INFORMATION_SCHEMA`.`TABLES` as `t` \
    LEFT JOIN `INFORMATION_SCHEMA`.`COLLATION_CHARACTER_SET_APPLICABILITY` as `ccsa` \
    ON `ccsa`.`COLLATION_NAME` = `t`.`TABLE_COLLATION` \
    WHERE `TABLE_SCHEMA` = SCHEMA() AND `TABLE_TYPE` IN ('BASE TABLE', 'VIEW')";

const PRIMARY_KEY: &str = "SELECT `INDEX_NAME`, \
    GROUP_CONCAT(`COLUMN_NAME` ORDER BY `SEQ_IN_INDEX` SEPARATOR ',') AS `COLUMNS`, \
    GROUP_CONCAT(CAST(IFNULL(`SUB_PART`, 0) AS CHAR) ORDER BY `SEQ_IN_INDEX` SEPARATOR ',') AS `SUB_PARTS` \
    FROM `INFORMATION_SCHEMA`.`STATISTICS` \
    WHERE `TABLE_SCHEMA` = '__SCHEMA__' AND `TABLE_NAME` = '__TABLE__' AND `INDEX_NAME` = 'PRIMARY' \
    GROUP BY `INDEX_NAME`";

const INDEXES: &str = "SELECT `INDEX_NAME`, `NON_UNIQUE`, \
    GROUP_CONCAT(`COLUMN_NAME` ORDER BY `SEQ_IN_INDEX` SEPARATOR ',') AS `COLUMNS`, \
    GROUP_CONCAT(CAST(IFNULL(`SUB_PART`, 0) AS CHAR) ORDER BY `SEQ_IN_INDEX` SEPARATOR ',') AS `SUB_PARTS`, \
    GROUP_CONCAT(IFNULL(`COLLATION`, 'A') ORDER BY `SEQ_IN_INDEX` SEPARATOR ',') AS `COLLATION`, \
    `INDEX_TYPE` \
    FROM `INFORMATION_SCHEMA`.`STATISTICS` \
    WHERE `TABLE_SCHEMA` = '__SCHEMA__' AND `TABLE_NAME` = '__TABLE__' AND `INDEX_NAME` <> 'PRIMARY' \
    GROUP BY `INDEX_NAME`, `NON_UNIQUE`, `INDEX_TYPE`";

const FOREIGN_KEYS: &str = "SELECT `CONSTRAINT_NAME`, `TABLE_NAME`, `REFERENCED_TABLE_NAME`, \
    GROUP_CONCAT(CONCAT_WS('|', `COLUMN_NAME`, `REFERENCED_COLUMN_NAME`) ORDER BY `ORDINAL_POSITION` SEPARATOR ',') AS PAIRED_COLUMNS, \
    (SELECT `DELETE_RULE` FROM `INFORMATION_SCHEMA`.`REFERENTIAL_CONSTRAINTS` \
    WHERE `REFERENTIAL_CONSTRAINTS`.`CONSTRAINT_NAME` = `KEY_COLUMN_USAGE`.`CONSTRAINT_NAME` \
    AND `REFERENTIAL_CONSTRAINTS`.`CONSTRAINT_SCHEMA` = `KEY_COLUMN_USAGE`.`CONSTRAINT_SCHEMA`) AS `DELETE_RULE` \
    FROM `INFORMATION_SCHEMA`.`KEY_COLUMN_USAGE` \
    WHERE `TABLE_SCHEMA` = '__SCHEMA__' AND `TABLE_NAME` = '__TABLE__' AND `CONSTRAINT_NAME` <> 'PRIMARY' \
    AND `REFERENCED_TABLE_NAME` IS NOT NULL \
    GROUP BY `CONSTRAINT_SCHEMA`, `CONSTRAINT_NAME`, `TABLE_NAME`, `REFERENCED_TABLE_NAME`";

/// Recognizes one of the scaffold's catalog reads, whatever its spacing and
/// with or without the `;` Pomelo ends each with, or `None` for any other
/// statement.
pub fn parse_optional_pomelo_information_schema_query(
    sql: &str,
    mode: SessionSqlMode,
) -> Result<Option<PomeloInformationSchemaQuery>, ParseError> {
    if !mentions_ignoring_case(sql, "COLLATION_CHARACTER_SET_APPLICABILITY")
        && !mentions_ignoring_case(sql, "GROUP_CONCAT")
    {
        return Ok(None);
    }
    let mut tokens = std::rc::Rc::unwrap_or_clone(tokenize_information_schema_query(sql, mode)?);
    while matches!(tokens.last(), Some(Token::Whitespace(_))) {
        tokens.pop();
    }
    if matches!(tokens.last(), Some(Token::SemiColon)) {
        tokens.pop();
    }
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
    if connector_j_template_captures(&tokens, TABLES, mode)?.is_some() {
        return Ok(Some(PomeloInformationSchemaQuery::Tables));
    }
    for (template, read) in [(PRIMARY_KEY, 0), (INDEXES, 1), (FOREIGN_KEYS, 2)] {
        let Some(captures) = connector_j_template_captures(&tokens, template, mode)? else {
            continue;
        };
        let [Some(schema), Some(table)] = captures.as_slice() else {
            return Ok(None);
        };
        let (schema, table) = (schema.clone(), table.clone());
        return Ok(Some(match read {
            0 => PomeloInformationSchemaQuery::PrimaryKey { schema, table },
            1 => PomeloInformationSchemaQuery::Indexes { schema, table },
            _ => PomeloInformationSchemaQuery::ForeignKeys { schema, table },
        }));
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The reads as the framework harness captured them against MySQL.
    #[test]
    fn the_scaffolds_reads_are_recognized_with_their_database_and_table() {
        let mode = SessionSqlMode::default();
        let read = |sql: &str| parse_optional_pomelo_information_schema_query(sql, mode).unwrap();
        assert_eq!(
            read("SELECT\n    `t`.`TABLE_NAME`,\n    `t`.`TABLE_TYPE`,\n    IF(`t`.`TABLE_COMMENT` = 'VIEW' AND `t`.`TABLE_TYPE` = 'VIEW', '', `t`.`TABLE_COMMENT`) AS `TABLE_COMMENT`,\n    `ccsa`.`CHARACTER_SET_NAME` as `TABLE_CHARACTER_SET`,\n    `t`.`TABLE_COLLATION`\nFROM\n    `INFORMATION_SCHEMA`.`TABLES` as `t`\nLEFT JOIN\n\t`INFORMATION_SCHEMA`.`COLLATION_CHARACTER_SET_APPLICABILITY` as `ccsa` ON `ccsa`.`COLLATION_NAME` = `t`.`TABLE_COLLATION`\nWHERE\n    `TABLE_SCHEMA` = SCHEMA()\nAND\n    `TABLE_TYPE` IN ('BASE TABLE', 'VIEW');"),
            Some(PomeloInformationSchemaQuery::Tables)
        );
        assert_eq!(
            read("SELECT `INDEX_NAME`,\n     GROUP_CONCAT(`COLUMN_NAME` ORDER BY `SEQ_IN_INDEX` SEPARATOR ',') AS `COLUMNS`,\n     GROUP_CONCAT(CAST(IFNULL(`SUB_PART`, 0) AS CHAR) ORDER BY `SEQ_IN_INDEX` SEPARATOR ',') AS `SUB_PARTS`\n     FROM `INFORMATION_SCHEMA`.`STATISTICS`\n     WHERE `TABLE_SCHEMA` = 'efcore'\n     AND `TABLE_NAME` = 'PostTags'\n     AND `INDEX_NAME` = 'PRIMARY'\n     GROUP BY `INDEX_NAME`;"),
            Some(PomeloInformationSchemaQuery::PrimaryKey {
                schema: "efcore".to_owned(),
                table: "PostTags".to_owned()
            })
        );
        assert_eq!(
            read("SELECT `INDEX_NAME`,\n     `NON_UNIQUE`,\n     GROUP_CONCAT(`COLUMN_NAME` ORDER BY `SEQ_IN_INDEX` SEPARATOR ',') AS `COLUMNS`,\n     GROUP_CONCAT(CAST(IFNULL(`SUB_PART`, 0) AS CHAR) ORDER BY `SEQ_IN_INDEX` SEPARATOR ',') AS `SUB_PARTS`,\n     GROUP_CONCAT(IFNULL(`COLLATION`, 'A') ORDER BY `SEQ_IN_INDEX` SEPARATOR ',') AS `COLLATION`,\n     `INDEX_TYPE`\n     FROM `INFORMATION_SCHEMA`.`STATISTICS`\n     WHERE `TABLE_SCHEMA` = 'efcore'\n     AND `TABLE_NAME` = 'Posts'\n     AND `INDEX_NAME` <> 'PRIMARY'\n     GROUP BY `INDEX_NAME`, `NON_UNIQUE`, `INDEX_TYPE`;"),
            Some(PomeloInformationSchemaQuery::Indexes {
                schema: "efcore".to_owned(),
                table: "Posts".to_owned()
            })
        );
        assert_eq!(
            read("SELECT\n \t`CONSTRAINT_NAME`,\n \t`TABLE_NAME`,\n \t`REFERENCED_TABLE_NAME`,\n \tGROUP_CONCAT(CONCAT_WS('|', `COLUMN_NAME`, `REFERENCED_COLUMN_NAME`) ORDER BY `ORDINAL_POSITION` SEPARATOR ',') AS PAIRED_COLUMNS,\n \t(SELECT `DELETE_RULE` FROM `INFORMATION_SCHEMA`.`REFERENTIAL_CONSTRAINTS` WHERE `REFERENTIAL_CONSTRAINTS`.`CONSTRAINT_NAME` = `KEY_COLUMN_USAGE`.`CONSTRAINT_NAME` AND `REFERENTIAL_CONSTRAINTS`.`CONSTRAINT_SCHEMA` = `KEY_COLUMN_USAGE`.`CONSTRAINT_SCHEMA`) AS `DELETE_RULE`\n FROM `INFORMATION_SCHEMA`.`KEY_COLUMN_USAGE`\n WHERE `TABLE_SCHEMA` = 'efcore'\n \t\tAND `TABLE_NAME` = 'Users'\n \t\tAND `CONSTRAINT_NAME` <> 'PRIMARY'\n        AND `REFERENCED_TABLE_NAME` IS NOT NULL\n        GROUP BY `CONSTRAINT_SCHEMA`,\n        `CONSTRAINT_NAME`,\n        `TABLE_NAME`,\n        `REFERENCED_TABLE_NAME`;"),
            Some(PomeloInformationSchemaQuery::ForeignKeys {
                schema: "efcore".to_owned(),
                table: "Users".to_owned()
            })
        );
        // Any other read over the same tables is left to the checked path.
        assert_eq!(
            read("SELECT `INDEX_NAME`, GROUP_CONCAT(`COLUMN_NAME` ORDER BY `SEQ_IN_INDEX` SEPARATOR ';') AS `COLUMNS` FROM `INFORMATION_SCHEMA`.`STATISTICS` WHERE `TABLE_SCHEMA` = 'efcore' AND `TABLE_NAME` = 'Posts' AND `INDEX_NAME` = 'PRIMARY' GROUP BY `INDEX_NAME`"),
            None
        );
        assert_eq!(
            read("SELECT `t`.`TABLE_NAME`, `t`.`TABLE_TYPE`, IF(`t`.`TABLE_COMMENT` = 'VIEW' AND `t`.`TABLE_TYPE` = 'VIEW', '', `t`.`TABLE_COMMENT`) AS `TABLE_COMMENT`, `ccsa`.`CHARACTER_SET_NAME` as `TABLE_CHARACTER_SET`, `t`.`TABLE_COLLATION` FROM `INFORMATION_SCHEMA`.`TABLES` as `t` LEFT JOIN `INFORMATION_SCHEMA`.`COLLATION_CHARACTER_SET_APPLICABILITY` as `ccsa` ON `ccsa`.`COLLATION_NAME` = `t`.`TABLE_COLLATION` WHERE `TABLE_SCHEMA` = 'efcore' AND `TABLE_TYPE` IN ('BASE TABLE', 'VIEW')"),
            None
        );
    }
}
