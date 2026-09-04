//! Strict decoding for the parameter portion of `COM_STMT_EXECUTE`.

use std::{error::Error, fmt, str};

/// MySQL's `MYSQL_TYPE_TINY` parameter type code.
pub const MYSQL_TYPE_TINY: u8 = 0x01;
/// MySQL's `MYSQL_TYPE_SHORT` parameter type code.
pub const MYSQL_TYPE_SHORT: u8 = 0x02;
/// MySQL's `MYSQL_TYPE_LONG` parameter type code.
pub const MYSQL_TYPE_LONG: u8 = 0x03;
/// MySQL's `MYSQL_TYPE_FLOAT` parameter type code.
pub const MYSQL_TYPE_FLOAT: u8 = 0x04;
/// MySQL's `MYSQL_TYPE_DOUBLE` parameter type code.
pub const MYSQL_TYPE_DOUBLE: u8 = 0x05;
/// MySQL's `MYSQL_TYPE_NULL` parameter type code.
pub const MYSQL_TYPE_NULL: u8 = 0x06;
/// MySQL's `MYSQL_TYPE_LONGLONG` parameter type code.
pub const MYSQL_TYPE_LONGLONG: u8 = 0x08;
/// MySQL's `MYSQL_TYPE_VARCHAR` parameter type code.
pub const MYSQL_TYPE_VARCHAR: u8 = 0x0f;
/// MySQL's `MYSQL_TYPE_TINY_BLOB` parameter type code.
pub const MYSQL_TYPE_TINY_BLOB: u8 = 0xf9;
/// MySQL's `MYSQL_TYPE_MEDIUM_BLOB` parameter type code.
pub const MYSQL_TYPE_MEDIUM_BLOB: u8 = 0xfa;
/// MySQL's `MYSQL_TYPE_LONG_BLOB` parameter type code.
pub const MYSQL_TYPE_LONG_BLOB: u8 = 0xfb;
/// MySQL's `MYSQL_TYPE_BLOB` parameter type code.
pub const MYSQL_TYPE_BLOB: u8 = 0xfc;
/// MySQL's `MYSQL_TYPE_VAR_STRING` parameter type code.
pub const MYSQL_TYPE_VAR_STRING: u8 = 0xfd;
/// MySQL's `MYSQL_TYPE_STRING` parameter type code.
pub const MYSQL_TYPE_STRING: u8 = 0xfe;

/// The type metadata carried by `COM_STMT_EXECUTE` or cached for later executions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StatementParameterType {
    /// The MySQL binary-protocol type code.
    pub type_code: u8,
    /// Whether integer values use their unsigned wire representation.
    pub unsigned: bool,
}

/// An owned parameter value decoded from the MySQL binary protocol.
#[derive(Debug, Clone, PartialEq)]
pub enum StatementParameterValue {
    /// A SQL NULL value.
    Null,
    /// A signed integer value.
    Integer(i64),
    /// A single-precision floating point value.
    Float(f32),
    /// A double-precision floating point value.
    Double(f64),
    /// A valid UTF-8 string parameter.
    String(String),
    /// An opaque binary parameter.
    Bytes(Vec<u8>),
}

/// Decoded parameters and the type vector to cache for the next execution.
#[derive(Debug, Clone, PartialEq)]
pub struct StatementExecuteParameters {
    /// Decoded values in parameter order.
    pub values: Vec<StatementParameterValue>,
    /// The type metadata used to decode the values.
    pub types: Vec<StatementParameterType>,
}

/// Decodes the parameter portion of a `COM_STMT_EXECUTE` payload.
///
/// `payload` begins at the null bitmap, after the statement id, flags, and iteration count.
/// Callers should retain the returned type vector and pass it as `cached_types` when a later
/// execution sets `new_params_bound_flag` to zero.
pub fn decode_statement_execute_parameters(
    payload: &[u8],
    parameter_count: usize,
    cached_types: Option<&[StatementParameterType]>,
) -> Result<StatementExecuteParameters, StatementExecuteDecodeError> {
    if parameter_count == 0 {
        if payload.is_empty() {
            return Ok(StatementExecuteParameters {
                values: Vec::new(),
                types: Vec::new(),
            });
        }
        return Err(StatementExecuteDecodeError::TrailingBytes {
            remaining: payload.len(),
        });
    }

    let null_bitmap_len = parameter_count
        .checked_add(7)
        .ok_or(StatementExecuteDecodeError::ParameterCountTooLarge { parameter_count })?
        / 8;
    let mut reader = Reader::new(payload);
    let null_bitmap = reader.read_exact(null_bitmap_len, "null bitmap")?;
    let new_params_bound_flag = reader.read_u8("new parameters bound flag")?;
    let types = match new_params_bound_flag {
        0 => cached_types
            .ok_or(StatementExecuteDecodeError::MissingCachedTypes)?
            .to_vec(),
        1 => read_types(&mut reader, parameter_count)?,
        flag => return Err(StatementExecuteDecodeError::InvalidNewParamsBoundFlag { flag }),
    };

    if types.len() != parameter_count {
        return Err(StatementExecuteDecodeError::CachedTypeCountMismatch {
            expected: parameter_count,
            actual: types.len(),
        });
    }
    for (index, parameter_type) in types.iter().copied().enumerate() {
        validate_type(index, parameter_type)?;
    }

    let mut values = Vec::with_capacity(parameter_count);
    for (index, parameter_type) in types.iter().copied().enumerate() {
        if null_bitmap[index / 8] & (1 << (index % 8)) != 0
            || parameter_type.type_code == MYSQL_TYPE_NULL
        {
            values.push(StatementParameterValue::Null);
            continue;
        }
        values.push(read_value(&mut reader, index, parameter_type)?);
    }
    reader.finish()?;

    Ok(StatementExecuteParameters { values, types })
}

fn read_types(
    reader: &mut Reader<'_>,
    parameter_count: usize,
) -> Result<Vec<StatementParameterType>, StatementExecuteDecodeError> {
    let mut types = Vec::with_capacity(parameter_count);
    for index in 0..parameter_count {
        let type_code = reader.read_u8("parameter type")?;
        let unsigned_flag = reader.read_u8("parameter unsigned flag")?;
        let unsigned = match unsigned_flag {
            0 => false,
            0x80 => true,
            flag => {
                return Err(StatementExecuteDecodeError::InvalidUnsignedFlag { index, flag });
            }
        };
        let parameter_type = StatementParameterType {
            type_code,
            unsigned,
        };
        validate_type(index, parameter_type)?;
        types.push(parameter_type);
    }
    Ok(types)
}

fn validate_type(
    index: usize,
    parameter_type: StatementParameterType,
) -> Result<(), StatementExecuteDecodeError> {
    match parameter_type.type_code {
        MYSQL_TYPE_NULL
        | MYSQL_TYPE_TINY
        | MYSQL_TYPE_SHORT
        | MYSQL_TYPE_LONG
        | MYSQL_TYPE_LONGLONG
        | MYSQL_TYPE_FLOAT
        | MYSQL_TYPE_DOUBLE
        | MYSQL_TYPE_VARCHAR
        | MYSQL_TYPE_VAR_STRING
        | MYSQL_TYPE_STRING
        | MYSQL_TYPE_TINY_BLOB
        | MYSQL_TYPE_MEDIUM_BLOB
        | MYSQL_TYPE_LONG_BLOB
        | MYSQL_TYPE_BLOB => Ok(()),
        type_code => Err(StatementExecuteDecodeError::UnsupportedType { index, type_code }),
    }
}

fn read_value(
    reader: &mut Reader<'_>,
    index: usize,
    parameter_type: StatementParameterType,
) -> Result<StatementParameterValue, StatementExecuteDecodeError> {
    let value = match parameter_type.type_code {
        MYSQL_TYPE_TINY => read_integer(reader, index, parameter_type.unsigned, 1)?,
        MYSQL_TYPE_SHORT => read_integer(reader, index, parameter_type.unsigned, 2)?,
        MYSQL_TYPE_LONG => read_integer(reader, index, parameter_type.unsigned, 4)?,
        MYSQL_TYPE_LONGLONG => read_integer(reader, index, parameter_type.unsigned, 8)?,
        MYSQL_TYPE_FLOAT => {
            let bytes = reader.read_exact(4, "float parameter")?;
            StatementParameterValue::Float(f32::from_le_bytes([
                bytes[0], bytes[1], bytes[2], bytes[3],
            ]))
        }
        MYSQL_TYPE_DOUBLE => {
            let bytes = reader.read_exact(8, "double parameter")?;
            StatementParameterValue::Double(f64::from_le_bytes([
                bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7],
            ]))
        }
        MYSQL_TYPE_VARCHAR | MYSQL_TYPE_VAR_STRING | MYSQL_TYPE_STRING => {
            let bytes = reader.read_lenenc_bytes(index)?;
            let value = str::from_utf8(bytes)
                .map_err(|_| StatementExecuteDecodeError::InvalidUtf8 { index })?;
            StatementParameterValue::String(value.to_owned())
        }
        MYSQL_TYPE_TINY_BLOB | MYSQL_TYPE_MEDIUM_BLOB | MYSQL_TYPE_LONG_BLOB | MYSQL_TYPE_BLOB => {
            StatementParameterValue::Bytes(reader.read_lenenc_bytes(index)?.to_vec())
        }
        MYSQL_TYPE_NULL => StatementParameterValue::Null,
        type_code => return Err(StatementExecuteDecodeError::UnsupportedType { index, type_code }),
    };
    Ok(value)
}

fn read_integer(
    reader: &mut Reader<'_>,
    index: usize,
    unsigned: bool,
    width: usize,
) -> Result<StatementParameterValue, StatementExecuteDecodeError> {
    let bytes = reader.read_exact(width, "integer parameter")?;
    let value = match (width, unsigned) {
        (1, false) => i64::from(i8::from_le_bytes([bytes[0]])),
        (1, true) => i64::from(bytes[0]),
        (2, false) => i64::from(i16::from_le_bytes([bytes[0], bytes[1]])),
        (2, true) => i64::from(u16::from_le_bytes([bytes[0], bytes[1]])),
        (4, false) => i64::from(i32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]])),
        (4, true) => i64::from(u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]])),
        (8, false) => i64::from_le_bytes([
            bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7],
        ]),
        (8, true) => {
            let unsigned_value = u64::from_le_bytes([
                bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7],
            ]);
            i64::try_from(unsigned_value).map_err(|_| {
                StatementExecuteDecodeError::UnsignedValueOutOfRange {
                    index,
                    value: unsigned_value,
                }
            })?
        }
        _ => unreachable!("only fixed MySQL integer widths are passed to read_integer"),
    };
    Ok(StatementParameterValue::Integer(value))
}

struct Reader<'a> {
    bytes: &'a [u8],
    offset: usize,
}

impl<'a> Reader<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, offset: 0 }
    }

    fn read_exact(
        &mut self,
        length: usize,
        field: &'static str,
    ) -> Result<&'a [u8], StatementExecuteDecodeError> {
        let remaining = self.bytes.len().saturating_sub(self.offset);
        if remaining < length {
            return Err(StatementExecuteDecodeError::Truncated {
                field,
                needed: length,
                remaining,
            });
        }
        let start = self.offset;
        self.offset += length;
        Ok(&self.bytes[start..self.offset])
    }

    fn read_u8(&mut self, field: &'static str) -> Result<u8, StatementExecuteDecodeError> {
        Ok(self.read_exact(1, field)?[0])
    }

    fn read_lenenc_bytes(&mut self, index: usize) -> Result<&'a [u8], StatementExecuteDecodeError> {
        let length = self.read_lenenc_integer(index)?;
        let length = usize::try_from(length)
            .map_err(|_| StatementExecuteDecodeError::LengthTooLarge { index, length })?;
        self.read_exact(length, "length-encoded parameter")
    }

    fn read_lenenc_integer(&mut self, index: usize) -> Result<u64, StatementExecuteDecodeError> {
        let marker = self.read_u8("length-encoded parameter length")?;
        match marker {
            0..=0xfa => Ok(u64::from(marker)),
            0xfb | 0xff => {
                Err(StatementExecuteDecodeError::InvalidLengthEncodedInteger { index, marker })
            }
            0xfc => {
                let bytes = self.read_exact(2, "length-encoded parameter length")?;
                let value = u64::from(u16::from_le_bytes([bytes[0], bytes[1]]));
                if value < 0xfb {
                    return Err(
                        StatementExecuteDecodeError::NonCanonicalLengthEncodedInteger {
                            index,
                            value,
                        },
                    );
                }
                Ok(value)
            }
            0xfd => {
                let bytes = self.read_exact(3, "length-encoded parameter length")?;
                let value =
                    u64::from(bytes[0]) | (u64::from(bytes[1]) << 8) | (u64::from(bytes[2]) << 16);
                if value <= 0xffff {
                    return Err(
                        StatementExecuteDecodeError::NonCanonicalLengthEncodedInteger {
                            index,
                            value,
                        },
                    );
                }
                Ok(value)
            }
            0xfe => {
                let bytes = self.read_exact(8, "length-encoded parameter length")?;
                let value = u64::from_le_bytes([
                    bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7],
                ]);
                if value <= 0xff_ffff {
                    return Err(
                        StatementExecuteDecodeError::NonCanonicalLengthEncodedInteger {
                            index,
                            value,
                        },
                    );
                }
                Ok(value)
            }
        }
    }

    fn finish(&self) -> Result<(), StatementExecuteDecodeError> {
        if self.offset != self.bytes.len() {
            return Err(StatementExecuteDecodeError::TrailingBytes {
                remaining: self.bytes.len() - self.offset,
            });
        }
        Ok(())
    }
}

/// Errors returned while decoding `COM_STMT_EXECUTE` parameter data.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StatementExecuteDecodeError {
    /// The supplied parameter count cannot be converted to a bitmap length.
    ParameterCountTooLarge { parameter_count: usize },
    /// A fixed-size protocol field ended before all of its bytes were present.
    Truncated {
        /// Name of the field that ended early.
        field: &'static str,
        /// Number of bytes required to complete the field.
        needed: usize,
        /// Number of bytes still available.
        remaining: usize,
    },
    /// The type-cache selector is neither zero nor one.
    InvalidNewParamsBoundFlag { flag: u8 },
    /// The payload asked to reuse types without providing a cached type vector.
    MissingCachedTypes,
    /// The cached type vector has a different count from the prepared statement.
    CachedTypeCountMismatch { expected: usize, actual: usize },
    /// A type entry's unsigned flag has a value other than zero or `0x80`.
    InvalidUnsignedFlag { index: usize, flag: u8 },
    /// A parameter type is outside this decoder's supported binary encodings.
    UnsupportedType { index: usize, type_code: u8 },
    /// An unsigned integer cannot fit in the decoder's signed neutral representation.
    UnsignedValueOutOfRange { index: usize, value: u64 },
    /// A string parameter is not valid UTF-8.
    InvalidUtf8 { index: usize },
    /// A length-encoded parameter length used an invalid marker.
    InvalidLengthEncodedInteger { index: usize, marker: u8 },
    /// A length-encoded parameter length used a wider-than-needed representation.
    NonCanonicalLengthEncodedInteger { index: usize, value: u64 },
    /// A length-encoded parameter length cannot fit in `usize`.
    LengthTooLarge { index: usize, length: u64 },
    /// Bytes remain after the last parameter value.
    TrailingBytes { remaining: usize },
}

impl fmt::Display for StatementExecuteDecodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ParameterCountTooLarge { parameter_count } => {
                write!(f, "parameter count {parameter_count} is too large")
            }
            Self::Truncated {
                field,
                needed,
                remaining,
            } => write!(
                f,
                "{field} is truncated: need {needed} bytes, got {remaining}"
            ),
            Self::InvalidNewParamsBoundFlag { flag } => {
                write!(
                    f,
                    "new parameters bound flag must be 0 or 1, got 0x{flag:02x}"
                )
            }
            Self::MissingCachedTypes => f.write_str("execution has no cached parameter types"),
            Self::CachedTypeCountMismatch { expected, actual } => write!(
                f,
                "cached parameter type count is {actual}, expected {expected}"
            ),
            Self::InvalidUnsignedFlag { index, flag } => write!(
                f,
                "parameter {index} unsigned flag must be 0 or 0x80, got 0x{flag:02x}"
            ),
            Self::UnsupportedType { index, type_code } => {
                write!(
                    f,
                    "parameter {index} has unsupported type 0x{type_code:02x}"
                )
            }
            Self::UnsignedValueOutOfRange { index, value } => write!(
                f,
                "parameter {index} unsigned value {value} exceeds i64::MAX"
            ),
            Self::InvalidUtf8 { index } => write!(f, "parameter {index} is not valid UTF-8"),
            Self::InvalidLengthEncodedInteger { index, marker } => write!(
                f,
                "parameter {index} has invalid length-encoded integer marker 0x{marker:02x}"
            ),
            Self::NonCanonicalLengthEncodedInteger { index, value } => {
                write!(
                    f,
                    "parameter {index} encodes length {value} non-canonically"
                )
            }
            Self::LengthTooLarge { index, length } => {
                write!(f, "parameter {index} length {length} does not fit in usize")
            }
            Self::TrailingBytes { remaining } => {
                write!(
                    f,
                    "statement execute payload has {remaining} trailing bytes"
                )
            }
        }
    }
}

impl Error for StatementExecuteDecodeError {}

#[cfg(test)]
mod tests {
    use super::*;

    fn decode(
        payload: &[u8],
        parameter_count: usize,
    ) -> Result<StatementExecuteParameters, StatementExecuteDecodeError> {
        decode_statement_execute_parameters(payload, parameter_count, None)
    }

    #[test]
    fn decodes_all_supported_value_families() {
        let mut payload = vec![0, 1];
        payload.extend_from_slice(&[
            MYSQL_TYPE_TINY,
            0,
            MYSQL_TYPE_SHORT,
            0x80,
            MYSQL_TYPE_LONG,
            0,
            MYSQL_TYPE_LONGLONG,
            0x80,
            MYSQL_TYPE_FLOAT,
            0,
            MYSQL_TYPE_DOUBLE,
            0,
            MYSQL_TYPE_VARCHAR,
            0,
            MYSQL_TYPE_BLOB,
            0,
        ]);
        payload.extend_from_slice(&[0xfe]);
        payload.extend_from_slice(&500u16.to_le_bytes());
        payload.extend_from_slice(&(-1i32).to_le_bytes());
        payload.extend_from_slice(&i64::MAX.to_le_bytes());
        payload.extend_from_slice(&1.5f32.to_le_bytes());
        payload.extend_from_slice(&(-2.5f64).to_le_bytes());
        payload.extend_from_slice(&[2, b'h', b'i']);
        payload.extend_from_slice(&[3, 0, 0xff, 1]);

        let decoded = decode(&payload, 8).unwrap();
        assert_eq!(
            decoded.values,
            vec![
                StatementParameterValue::Integer(-2),
                StatementParameterValue::Integer(500),
                StatementParameterValue::Integer(-1),
                StatementParameterValue::Integer(i64::MAX),
                StatementParameterValue::Float(1.5),
                StatementParameterValue::Double(-2.5),
                StatementParameterValue::String("hi".into()),
                StatementParameterValue::Bytes(vec![0, 0xff, 1]),
            ]
        );
    }

    #[test]
    fn decodes_nulls_and_type_null_without_consuming_values() {
        let payload = [0b0000_0001, 1, MYSQL_TYPE_LONG, 0, MYSQL_TYPE_NULL, 0];
        let decoded = decode(&payload, 2).unwrap();
        assert_eq!(
            decoded.values,
            vec![StatementParameterValue::Null, StatementParameterValue::Null]
        );
    }

    #[test]
    fn reuses_validated_cached_types() {
        let types = [StatementParameterType {
            type_code: MYSQL_TYPE_STRING,
            unsigned: false,
        }];
        let decoded =
            decode_statement_execute_parameters(&[0, 0, 3, b'a', b'b', b'c'], 1, Some(&types))
                .unwrap();
        assert_eq!(
            decoded.values,
            vec![StatementParameterValue::String("abc".into())]
        );
        assert_eq!(decoded.types, types);
    }

    #[test]
    fn rejects_missing_or_wrong_cached_types() {
        assert_eq!(
            decode(&[0, 0], 1),
            Err(StatementExecuteDecodeError::MissingCachedTypes)
        );
        let types = [];
        assert_eq!(
            decode_statement_execute_parameters(&[0, 0], 1, Some(&types)),
            Err(StatementExecuteDecodeError::CachedTypeCountMismatch {
                expected: 1,
                actual: 0,
            })
        );
    }

    #[test]
    fn rejects_invalid_metadata() {
        assert_eq!(
            decode(&[0, 2], 1),
            Err(StatementExecuteDecodeError::InvalidNewParamsBoundFlag { flag: 2 })
        );
        assert_eq!(
            decode(&[0, 1, MYSQL_TYPE_TINY, 1], 1),
            Err(StatementExecuteDecodeError::InvalidUnsignedFlag { index: 0, flag: 1 })
        );
        assert_eq!(
            decode(&[0, 1, 0xff, 0], 1),
            Err(StatementExecuteDecodeError::UnsupportedType {
                index: 0,
                type_code: 0xff,
            })
        );
    }

    #[test]
    fn rejects_unsigned_longlong_that_does_not_fit_i64() {
        let mut payload = vec![0, 1, MYSQL_TYPE_LONGLONG, 0x80];
        payload.extend_from_slice(&(i64::MAX as u64 + 1).to_le_bytes());
        assert_eq!(
            decode(&payload, 1),
            Err(StatementExecuteDecodeError::UnsignedValueOutOfRange {
                index: 0,
                value: i64::MAX as u64 + 1,
            })
        );
    }

    #[test]
    fn strings_require_utf8_but_blobs_do_not() {
        assert_eq!(
            decode(&[0, 1, MYSQL_TYPE_VAR_STRING, 0, 1, 0xff], 1),
            Err(StatementExecuteDecodeError::InvalidUtf8 { index: 0 })
        );
        assert_eq!(
            decode(&[0, 1, MYSQL_TYPE_TINY_BLOB, 0, 1, 0xff], 1)
                .unwrap()
                .values,
            vec![StatementParameterValue::Bytes(vec![0xff])]
        );
    }

    #[test]
    fn decodes_each_string_and_blob_type_code() {
        for type_code in [MYSQL_TYPE_VARCHAR, MYSQL_TYPE_VAR_STRING, MYSQL_TYPE_STRING] {
            let payload = [0, 1, type_code, 0, 1, b'x'];
            assert_eq!(
                decode(&payload, 1).unwrap().values,
                vec![StatementParameterValue::String("x".into())]
            );
        }
        for type_code in [
            MYSQL_TYPE_TINY_BLOB,
            MYSQL_TYPE_MEDIUM_BLOB,
            MYSQL_TYPE_LONG_BLOB,
            MYSQL_TYPE_BLOB,
        ] {
            let payload = [0, 1, type_code, 0, 1, 0xff];
            assert_eq!(
                decode(&payload, 1).unwrap().values,
                vec![StatementParameterValue::Bytes(vec![0xff])]
            );
        }
    }

    #[test]
    fn rejects_invalid_noncanonical_and_truncated_length_encodings() {
        assert_eq!(
            decode(&[0, 1, MYSQL_TYPE_BLOB, 0, 0xfb], 1),
            Err(StatementExecuteDecodeError::InvalidLengthEncodedInteger {
                index: 0,
                marker: 0xfb,
            })
        );
        assert_eq!(
            decode(&[0, 1, MYSQL_TYPE_BLOB, 0, 0xfc, 250, 0], 1),
            Err(
                StatementExecuteDecodeError::NonCanonicalLengthEncodedInteger {
                    index: 0,
                    value: 250,
                }
            )
        );
        assert_eq!(
            decode(&[0, 1, MYSQL_TYPE_BLOB, 0, 3, 1], 1),
            Err(StatementExecuteDecodeError::Truncated {
                field: "length-encoded parameter",
                needed: 3,
                remaining: 1,
            })
        );
    }

    #[test]
    fn rejects_truncated_fixed_width_fields() {
        assert_eq!(
            decode(&[0, 1, MYSQL_TYPE_DOUBLE, 0, 0, 0, 0], 1),
            Err(StatementExecuteDecodeError::Truncated {
                field: "double parameter",
                needed: 8,
                remaining: 3,
            })
        );
        assert_eq!(
            decode(&[0, 1, MYSQL_TYPE_LONG], 1),
            Err(StatementExecuteDecodeError::Truncated {
                field: "parameter unsigned flag",
                needed: 1,
                remaining: 0,
            })
        );
    }

    #[test]
    fn rejects_trailing_bytes_and_accepts_no_parameter_payload() {
        assert_eq!(
            decode(&[0, 1, MYSQL_TYPE_TINY, 0, 1, 2], 1),
            Err(StatementExecuteDecodeError::TrailingBytes { remaining: 1 })
        );
        assert_eq!(
            decode(&[], 0).unwrap(),
            StatementExecuteParameters {
                values: Vec::new(),
                types: Vec::new(),
            }
        );
    }
}
