use std::collections::HashSet;

use serde::{de::Error as _, Deserialize, Deserializer, Serialize};
use thiserror::Error;

use crate::case::{IsolationLevel, SqlMode, TimeZone, TypedValue};

/// The version of the JSON format written for one observed step.
pub const OBSERVATION_FORMAT_VERSION: u32 = 1;

/// Everything observable after one statement has been executed.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Observation {
    pub version: u32,
    pub step_id: String,
    pub session_id: String,
    #[serde(default)]
    pub result: Option<ResultSet>,
    pub affected_rows: u64,
    pub last_insert_id: u64,
    #[serde(default)]
    pub warnings: WarningSet,
    #[serde(default)]
    pub error: Option<MySqlError>,
    pub session_state: SessionState,
}

/// A result set with protocol-visible column descriptions and typed cells.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ResultSet {
    pub columns: Vec<ColumnMetadata>,
    pub rows: Vec<Vec<TypedValue>>,
}

/// MySQL result metadata needed by clients to decode values correctly.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ColumnMetadata {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub original_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub table: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub original_table: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub database: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub catalog: Option<String>,
    pub column_type: MySqlType,
    /// The numeric collation identifier from the classic protocol column definition.
    pub character_set_id: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub character_set: Option<CharacterSet>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub collation: Option<Collation>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub column_length: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decimals: Option<u8>,
    pub nullable: bool,
    #[serde(default)]
    pub flags: Vec<ColumnFlag>,
}

/// MySQL protocol type codes represented by the oracle model.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum MySqlType {
    Null,
    Tiny,
    Short,
    Long,
    LongLong,
    Int24,
    Float,
    Double,
    Decimal,
    NewDecimal,
    Bit,
    Year,
    Date,
    Time,
    DateTime,
    Timestamp,
    VarChar,
    VarString,
    String,
    Blob,
    TinyBlob,
    MediumBlob,
    LongBlob,
    Json,
    Enum,
    Set,
    Geometry,
    Vector,
    TypedArray,
    Unknown { name: String },
}

pub type ColumnType = MySqlType;

/// Character sets supported by the compatibility target, with an escape hatch for recording
/// reference-server metadata before a feature is admitted to the supported subset.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CharacterSet {
    Utf8mb4,
    Binary,
    Other(String),
}

/// Collations are typed so comparison does not accidentally compare unrelated text rules.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Collation {
    Utf8mb4_0900AiCi,
    Utf8mb4Bin,
    Binary,
    Other(String),
}

/// Flags carried in a MySQL column definition or result packet.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ColumnFlag {
    NotNull,
    PrimaryKey,
    UniqueKey,
    MultipleKey,
    Blob,
    Unsigned,
    ZeroFill,
    Binary,
    AutoIncrement,
    Timestamp,
    Enum,
    Set,
    NoDefaultValue,
    OnUpdate,
    PartKey,
    Numeric,
}

/// The warnings produced by one statement. Details are complete, so count is checked against
/// the vector length instead of being treated as an independently trusted number.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct WarningSet {
    pub warning_count: u32,
    #[serde(default)]
    pub details: Vec<WarningDetail>,
}

impl WarningSet {
    pub fn from_details(details: Vec<WarningDetail>) -> Self {
        Self {
            warning_count: details.len() as u32,
            details,
        }
    }

    pub fn validate(&self) -> Result<(), ObservationValidationError> {
        if self.warning_count as usize != self.details.len() {
            return Err(ObservationValidationError::WarningCountMismatch {
                warning_count: self.warning_count,
                detail_count: self.details.len(),
            });
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WarningDetail {
    pub level: WarningLevel,
    pub code: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sql_state: Option<SqlState>,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WarningLevel {
    Note,
    Warning,
    Error,
    Other(String),
}

/// An error in the form expected by MySQL clients and SQLSTATE-aware test suites.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MySqlError {
    pub number: u32,
    pub sql_state: SqlState,
    pub message: String,
}

/// A five-character SQLSTATE. Validation prevents malformed diagnostics from entering a golden.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
#[serde(transparent)]
pub struct SqlState(String);

impl SqlState {
    pub fn new(value: impl Into<String>) -> Result<Self, SqlStateError> {
        let value = value.into();
        if value.len() != 5 || !value.bytes().all(|byte| byte.is_ascii_alphanumeric()) {
            return Err(SqlStateError(value));
        }
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl<'de> Deserialize<'de> for SqlState {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::new(value).map_err(D::Error::custom)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error("SQLSTATE must contain exactly five ASCII letters or digits: `{0}`")]
pub struct SqlStateError(String);

/// The session values that must be equal after a step, not just its rows.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionState {
    pub current_database: Option<String>,
    pub sql_mode: SqlMode,
    pub time_zone: TimeZone,
    pub isolation: IsolationLevel,
    pub autocommit: bool,
    pub transaction: TransactionState,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TransactionState {
    Idle,
    Active,
}

impl Observation {
    pub fn validate(&self) -> Result<(), ObservationValidationError> {
        if self.version != OBSERVATION_FORMAT_VERSION {
            return Err(ObservationValidationError::UnsupportedVersion(self.version));
        }
        if self.step_id.trim().is_empty() {
            return Err(ObservationValidationError::EmptyStepId);
        }
        if self.session_id.trim().is_empty() {
            return Err(ObservationValidationError::EmptySessionId);
        }
        self.warnings.validate()?;
        if let Some(result) = &self.result {
            result.validate()?;
        }
        Ok(())
    }
}

impl ResultSet {
    pub fn validate(&self) -> Result<(), ObservationValidationError> {
        for (index, column) in self.columns.iter().enumerate() {
            if column.name.trim().is_empty() {
                return Err(ObservationValidationError::EmptyColumnName(index));
            }
            let mut flags = HashSet::with_capacity(column.flags.len());
            for flag in &column.flags {
                if !flags.insert(flag) {
                    return Err(ObservationValidationError::DuplicateColumnFlag {
                        column: index,
                        flag: *flag,
                    });
                }
            }
        }
        for (row, values) in self.rows.iter().enumerate() {
            if values.len() != self.columns.len() {
                return Err(ObservationValidationError::RowWidthMismatch {
                    row,
                    expected: self.columns.len(),
                    actual: values.len(),
                });
            }
        }
        Ok(())
    }
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum ObservationValidationError {
    #[error("unsupported observation format version {0}; expected {OBSERVATION_FORMAT_VERSION}")]
    UnsupportedVersion(u32),
    #[error("observation step id must not be empty")]
    EmptyStepId,
    #[error("observation session id must not be empty")]
    EmptySessionId,
    #[error("warning_count is {warning_count}, but there are {detail_count} warning details")]
    WarningCountMismatch {
        warning_count: u32,
        detail_count: usize,
    },
    #[error("result column {0} has an empty name")]
    EmptyColumnName(usize),
    #[error("result column {column} repeats the `{flag:?}` flag")]
    DuplicateColumnFlag { column: usize, flag: ColumnFlag },
    #[error("result row {row} has {actual} values, expected {expected}")]
    RowWidthMismatch {
        row: usize,
        expected: usize,
        actual: usize,
    },
}

pub type OracleObservation = Observation;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::case::{SqlMode, TimeZone};

    fn state() -> SessionState {
        SessionState {
            current_database: Some("test".to_owned()),
            sql_mode: SqlMode::default(),
            time_zone: TimeZone::Utc,
            isolation: IsolationLevel::RepeatableRead,
            autocommit: true,
            transaction: TransactionState::Idle,
        }
    }

    fn observation(warnings: WarningSet) -> Observation {
        Observation {
            version: OBSERVATION_FORMAT_VERSION,
            step_id: "q1".to_owned(),
            session_id: "s1".to_owned(),
            result: Some(ResultSet {
                columns: vec![ColumnMetadata {
                    name: "n".to_owned(),
                    original_name: None,
                    table: None,
                    original_table: None,
                    database: None,
                    catalog: None,
                    column_type: MySqlType::LongLong,
                    character_set_id: None,
                    character_set: None,
                    collation: None,
                    column_length: Some(20),
                    decimals: Some(0),
                    nullable: false,
                    flags: vec![ColumnFlag::Numeric],
                }],
                rows: vec![vec![TypedValue::SignedInt { value: 1 }]],
            }),
            affected_rows: 0,
            last_insert_id: 0,
            warnings,
            error: None,
            session_state: state(),
        }
    }

    #[test]
    fn warning_count_must_match_details() {
        let mut value = observation(WarningSet::default());
        value.warnings.warning_count = 1;
        assert_eq!(
            value.validate(),
            Err(ObservationValidationError::WarningCountMismatch {
                warning_count: 1,
                detail_count: 0,
            })
        );
    }

    #[test]
    fn result_rows_must_match_column_count() {
        let mut value = observation(WarningSet::default());
        value.result.as_mut().expect("result").rows = vec![vec![]];
        assert_eq!(
            value.validate(),
            Err(ObservationValidationError::RowWidthMismatch {
                row: 0,
                expected: 1,
                actual: 0,
            })
        );
    }

    #[test]
    fn sql_state_is_validated_when_deserializing() {
        assert!(SqlState::new("42S02").is_ok());
        assert!(SqlState::new("bad").is_err());
        assert!(serde_json::from_str::<SqlState>(r#""42S0!""#).is_err());
    }

    #[test]
    fn typed_observation_round_trips() {
        let value = observation(WarningSet::default());
        let json = serde_json::to_string(&value).expect("serialize observation");
        let decoded: Observation = serde_json::from_str(&json).expect("deserialize observation");
        assert_eq!(decoded, value);
    }
}
