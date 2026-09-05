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

pub(crate) fn static_result_column_metadata(
    metadata: &StaticSelectMetadata,
) -> StaticResultColumnMetadata {
    match metadata {
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
        StaticSelectMetadata::Null => StaticResultColumnMetadata {
            column_type: MYSQL_TYPE_NULL,
            character_set: MYSQL_BINARY_COLLATION,
            column_length: 0,
            flags: MYSQL_BINARY_FLAG,
            decimals: 0,
        },
    }
}

pub(crate) fn static_column_definition(
    name: String,
    metadata: &StaticSelectMetadata,
) -> ColumnDefinitionConfig {
    let metadata = static_result_column_metadata(metadata);
    let mut definition = ColumnDefinitionConfig::new(name, metadata.column_type);
    definition.character_set = metadata.character_set;
    definition.column_length = metadata.column_length;
    definition.flags = metadata.flags;
    definition.decimals = metadata.decimals;
    definition
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
        let definition = static_column_definition("value".to_owned(), &metadata);
        assert_eq!(definition.column_type, MYSQL_TYPE_LONGLONG);
        assert_eq!(definition.character_set, MYSQL_BINARY_COLLATION);
        assert_eq!(definition.column_length, 5);
        assert_eq!(definition.decimals, 0);
        assert_eq!(definition.flags, MYSQL_NOT_NULL_FLAG | MYSQL_BINARY_FLAG);
    }

    #[test]
    fn null_and_boolean_use_static_wire_metadata() {
        let null = static_result_column_metadata(&StaticSelectMetadata::Null);
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
        let boolean = static_result_column_metadata(&StaticSelectMetadata::Boolean(true));
        assert_eq!(boolean.column_type, MYSQL_TYPE_LONGLONG);
        assert_eq!(boolean.column_length, 1);
        assert_eq!(boolean.flags, MYSQL_NOT_NULL_FLAG | MYSQL_BINARY_FLAG);
    }
}
