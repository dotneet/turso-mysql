use turso_mysql_parser::StaticSelectMetadata;

use crate::ColumnDefinitionConfig;

const MYSQL_TYPE_NULL: u8 = 0x06;
const MYSQL_TYPE_LONGLONG: u8 = 0x08;
const MYSQL_NOT_NULL_FLAG: u16 = 1;
const MYSQL_BINARY_FLAG: u16 = 128;
const MYSQL_BINARY_COLLATION: u16 = 63;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct StaticResultColumnMetadata {
    pub(crate) column_type: u8,
    pub(crate) character_set: u16,
    pub(crate) column_length: u32,
    pub(crate) flags: u16,
    pub(crate) decimals: u8,
}

/// Returns the result metadata a static projection fixes on its own.
///
/// A `MIN` or `MAX` answers None: its type is the named column's, which lives
/// in the table rather than in the statement, so the caller finishes it.
pub(crate) fn static_result_column_metadata(
    metadata: &StaticSelectMetadata,
) -> Option<StaticResultColumnMetadata> {
    Some(match metadata {
        StaticSelectMetadata::Integer { digit_count, .. } => StaticResultColumnMetadata {
            column_type: MYSQL_TYPE_LONGLONG,
            character_set: MYSQL_BINARY_COLLATION,
            column_length: digit_count
                .checked_add(1)
                .expect("checked MySQL integer digit count fits u32"),
            flags: MYSQL_NOT_NULL_FLAG | MYSQL_BINARY_FLAG,
            decimals: 0,
        },
        StaticSelectMetadata::Boolean(_) => StaticResultColumnMetadata {
            column_type: MYSQL_TYPE_LONGLONG,
            character_set: MYSQL_BINARY_COLLATION,
            column_length: 1,
            flags: MYSQL_NOT_NULL_FLAG | MYSQL_BINARY_FLAG,
            decimals: 0,
        },
        // Measured on MySQL 8.4.11: a COUNT answers a non-null LONGLONG of
        // length 21 whatever it counts, and 0 rather than NULL on an empty
        // table, so none of this depends on the argument.
        StaticSelectMetadata::Count => StaticResultColumnMetadata {
            column_type: MYSQL_TYPE_LONGLONG,
            character_set: MYSQL_BINARY_COLLATION,
            column_length: 21,
            flags: MYSQL_NOT_NULL_FLAG | MYSQL_BINARY_FLAG,
            decimals: 0,
        },
        StaticSelectMetadata::Null => StaticResultColumnMetadata {
            column_type: MYSQL_TYPE_NULL,
            character_set: MYSQL_BINARY_COLLATION,
            column_length: 0,
            flags: MYSQL_BINARY_FLAG,
            decimals: 0,
        },
        StaticSelectMetadata::ColumnAggregate { .. } => return None,
    })
}

pub(crate) fn static_column_definition(
    name: String,
    metadata: &StaticSelectMetadata,
) -> Option<ColumnDefinitionConfig> {
    let metadata = static_result_column_metadata(metadata)?;
    let mut definition = ColumnDefinitionConfig::new(name, metadata.column_type);
    definition.character_set = metadata.character_set;
    definition.column_length = metadata.column_length;
    definition.flags = metadata.flags;
    definition.decimals = metadata.decimals;
    Some(definition)
}

#[cfg(test)]
mod tests {
    use super::*;
    use turso_mysql_parser::StaticIntegerSign;

    #[test]
    fn integer_width_preserves_source_digits() {
        let metadata = StaticSelectMetadata::Integer {
            digit_count: 4,
            sign: StaticIntegerSign::Negative,
        };
        let definition = static_column_definition("value".to_owned(), &metadata).unwrap();
        assert_eq!(definition.column_type, MYSQL_TYPE_LONGLONG);
        assert_eq!(definition.character_set, MYSQL_BINARY_COLLATION);
        assert_eq!(definition.column_length, 5);
        assert_eq!(definition.decimals, 0);
        assert_eq!(definition.flags, MYSQL_NOT_NULL_FLAG | MYSQL_BINARY_FLAG);
    }

    #[test]
    fn null_and_boolean_use_static_wire_metadata() {
        let null = static_result_column_metadata(&StaticSelectMetadata::Null).unwrap();
        assert_eq!(
            null,
            StaticResultColumnMetadata {
                column_type: MYSQL_TYPE_NULL,
                character_set: MYSQL_BINARY_COLLATION,
                column_length: 0,
                flags: MYSQL_BINARY_FLAG,
                decimals: 0,
            }
        );
        let boolean = static_result_column_metadata(&StaticSelectMetadata::Boolean(true)).unwrap();
        assert_eq!(boolean.column_type, MYSQL_TYPE_LONGLONG);
        assert_eq!(boolean.column_length, 1);
        assert_eq!(boolean.flags, MYSQL_NOT_NULL_FLAG | MYSQL_BINARY_FLAG);
    }

    #[test]
    fn an_aggregate_leaves_its_metadata_to_the_caller() {
        assert!(
            static_result_column_metadata(&StaticSelectMetadata::ColumnAggregate {
                column_name: "id".to_owned(),
                kind: turso_mysql_parser::ColumnAggregateKind::MinMax,
            })
            .is_none()
        );
    }
}
