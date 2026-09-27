use turso_mysql_parser::{StaticSelectMetadata, WrittenValue};

use crate::{ColumnDefinitionConfig, DEFAULT_UTF8MB4_COLLATION};

const MYSQL_TYPE_DOUBLE: u8 = 0x05;
const MYSQL_TYPE_NULL: u8 = 0x06;
const MYSQL_TYPE_LONGLONG: u8 = 0x08;
const MYSQL_TYPE_DATE: u8 = 0x0a;
const MYSQL_TYPE_TIME: u8 = 0x0b;
const MYSQL_TYPE_DATETIME: u8 = 0x0c;
const MYSQL_TYPE_JSON: u8 = 0xf5;
const MYSQL_TYPE_NEWDECIMAL: u8 = 0xf6;
const MYSQL_TYPE_VAR_STRING: u8 = 0xfd;
const MYSQL_NOT_NULL_FLAG: u16 = 1;
const MYSQL_UNSIGNED_FLAG: u16 = 32;
const MYSQL_BINARY_FLAG: u16 = 128;
/// The decimals a column reports when its answer has no fixed count of them.
const NOT_FIXED_DECIMALS: u8 = 31;
/// The widest a JSON document can be, which every JSON answer reports.
const MYSQL_JSON_LENGTH: u32 = u32::MAX - 3;
const UTF8MB4_MAX_BYTES_PER_CHARACTER: u32 = 4;
/// Some numeric expressions carry this flag; a plain integer literal does not.
const MYSQL_NUM_FLAG: u16 = 32_768;
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
        StaticSelectMetadata::Boolean(_) | StaticSelectMetadata::Exists => {
            StaticResultColumnMetadata {
                column_type: MYSQL_TYPE_LONGLONG,
                character_set: MYSQL_BINARY_COLLATION,
                column_length: 1,
                flags: MYSQL_NOT_NULL_FLAG | MYSQL_BINARY_FLAG | MYSQL_NUM_FLAG,
                decimals: 0,
            }
        }
        // Measured on MySQL 8.4.11: a COUNT answers a non-null LONGLONG of
        // length 21 whatever it counts, and 0 rather than NULL on an empty
        // table, so none of this depends on the argument.
        StaticSelectMetadata::Count => StaticResultColumnMetadata {
            column_type: MYSQL_TYPE_LONGLONG,
            character_set: MYSQL_BINARY_COLLATION,
            column_length: 21,
            flags: MYSQL_NOT_NULL_FLAG | MYSQL_BINARY_FLAG | MYSQL_NUM_FLAG,
            decimals: 0,
        },
        StaticSelectMetadata::Null => StaticResultColumnMetadata {
            column_type: MYSQL_TYPE_NULL,
            character_set: MYSQL_BINARY_COLLATION,
            column_length: 0,
            flags: MYSQL_BINARY_FLAG | MYSQL_NUM_FLAG,
            decimals: 0,
        },
        StaticSelectMetadata::WrittenValue(written) => written_value_metadata(*written),
        StaticSelectMetadata::ColumnAggregate { .. }
        | StaticSelectMetadata::WindowAggregate { .. }
        | StaticSelectMetadata::WindowCount
        | StaticSelectMetadata::ScalarSubquery(_)
        | StaticSelectMetadata::DefaultedAggregate(_)
        | StaticSelectMetadata::Arithmetic(_)
        | StaticSelectMetadata::Branches { .. }
        | StaticSelectMetadata::AggregateOverBranches { .. }
        | StaticSelectMetadata::ScalarCall { .. }
        | StaticSelectMetadata::RoundedAggregate { .. }
        | StaticSelectMetadata::CountedColumn { .. }
        | StaticSelectMetadata::RolledUpKey { .. }
        | StaticSelectMetadata::FromARollup(_)
        | StaticSelectMetadata::FromTheGroupingTable { .. } => return None,
    })
}

/// Measured on MySQL 8.4.11, what a value written out in full reports.
fn written_value_metadata(written: WrittenValue) -> StaticResultColumnMetadata {
    let text_collation = u16::from(DEFAULT_UTF8MB4_COLLATION);
    match written {
        WrittenValue::Word { characters } => StaticResultColumnMetadata {
            column_type: MYSQL_TYPE_VAR_STRING,
            character_set: text_collation,
            column_length: characters.saturating_mul(UTF8MB4_MAX_BYTES_PER_CHARACTER),
            flags: MYSQL_NOT_NULL_FLAG,
            decimals: NOT_FIXED_DECIMALS,
        },
        // A sign and a point beside the digits: `1.50` reports 5 and
        // `CAST(1 AS DECIMAL(10,2))` 12, and a whole number no point.
        WrittenValue::Decimal {
            precision,
            scale,
            not_null,
        } => StaticResultColumnMetadata {
            column_type: MYSQL_TYPE_NEWDECIMAL,
            character_set: MYSQL_BINARY_COLLATION,
            column_length: precision + u32::from(scale > 0) + 1,
            flags: MYSQL_BINARY_FLAG
                | MYSQL_NUM_FLAG
                | if not_null { MYSQL_NOT_NULL_FLAG } else { 0 },
            decimals: u8::try_from(scale).expect("a DECIMAL holds at most 30 places"),
        },
        WrittenValue::Double { length } => StaticResultColumnMetadata {
            column_type: MYSQL_TYPE_DOUBLE,
            character_set: MYSQL_BINARY_COLLATION,
            column_length: length,
            flags: MYSQL_NOT_NULL_FLAG | MYSQL_BINARY_FLAG | MYSQL_NUM_FLAG,
            decimals: NOT_FIXED_DECIMALS,
        },
        WrittenValue::Bytes { length, unsigned } => StaticResultColumnMetadata {
            column_type: MYSQL_TYPE_VAR_STRING,
            character_set: MYSQL_BINARY_COLLATION,
            column_length: length,
            flags: MYSQL_NOT_NULL_FLAG
                | MYSQL_BINARY_FLAG
                | if unsigned { MYSQL_UNSIGNED_FLAG } else { 0 },
            decimals: 0,
        },
        // A cast to a day, a moment or a document is nullable whatever it
        // was given, a word naming none answering NULL.
        WrittenValue::Day { not_null } => StaticResultColumnMetadata {
            column_type: MYSQL_TYPE_DATE,
            character_set: MYSQL_BINARY_COLLATION,
            column_length: 10,
            flags: MYSQL_BINARY_FLAG | if not_null { MYSQL_NOT_NULL_FLAG } else { 0 },
            decimals: 0,
        },
        WrittenValue::Time => StaticResultColumnMetadata {
            column_type: MYSQL_TYPE_TIME,
            character_set: MYSQL_BINARY_COLLATION,
            column_length: 10,
            flags: MYSQL_BINARY_FLAG,
            decimals: 0,
        },
        WrittenValue::Moment => StaticResultColumnMetadata {
            column_type: MYSQL_TYPE_DATETIME,
            character_set: MYSQL_BINARY_COLLATION,
            column_length: 19,
            flags: MYSQL_BINARY_FLAG,
            decimals: 0,
        },
        WrittenValue::Json => StaticResultColumnMetadata {
            column_type: MYSQL_TYPE_JSON,
            character_set: text_collation,
            column_length: MYSQL_JSON_LENGTH,
            flags: MYSQL_BINARY_FLAG,
            decimals: NOT_FIXED_DECIMALS,
        },
        WrittenValue::Text { characters } => StaticResultColumnMetadata {
            column_type: MYSQL_TYPE_VAR_STRING,
            character_set: text_collation,
            column_length: characters.saturating_mul(UTF8MB4_MAX_BYTES_PER_CHARACTER),
            flags: 0,
            decimals: NOT_FIXED_DECIMALS,
        },
        WrittenValue::BinaryWord { length } => StaticResultColumnMetadata {
            column_type: MYSQL_TYPE_VAR_STRING,
            character_set: MYSQL_BINARY_COLLATION,
            column_length: length,
            flags: MYSQL_BINARY_FLAG,
            decimals: NOT_FIXED_DECIMALS,
        },
        WrittenValue::WholeNumber { length } => StaticResultColumnMetadata {
            column_type: MYSQL_TYPE_LONGLONG,
            character_set: MYSQL_BINARY_COLLATION,
            column_length: length,
            flags: MYSQL_NOT_NULL_FLAG | MYSQL_BINARY_FLAG | MYSQL_NUM_FLAG,
            decimals: 0,
        },
    }
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
                flags: MYSQL_BINARY_FLAG | MYSQL_NUM_FLAG,
                decimals: 0,
            }
        );
        let boolean = static_result_column_metadata(&StaticSelectMetadata::Boolean(true)).unwrap();
        assert_eq!(boolean.column_type, MYSQL_TYPE_LONGLONG);
        assert_eq!(boolean.column_length, 1);
        assert_eq!(
            boolean.flags,
            MYSQL_NOT_NULL_FLAG | MYSQL_BINARY_FLAG | MYSQL_NUM_FLAG
        );
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
