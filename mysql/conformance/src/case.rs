use std::collections::{HashMap, HashSet};

use base64::{engine::general_purpose::STANDARD, Engine as _};
use serde::{de::Error as _, Deserialize, Deserializer, Serialize};
use thiserror::Error;

/// The version of the JSON format written by the conformance runner.
pub const CASE_FORMAT_VERSION: u32 = 1;

/// A reproducible group of multi-session SQL steps.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Case {
    pub version: u32,
    pub id: String,
    #[serde(default)]
    pub tags: Vec<String>,
    pub sessions: Vec<SessionSpec>,
    pub steps: Vec<Step>,
    #[serde(default)]
    pub parallel_assertions: Vec<ParallelAllocationAssertion>,
}

/// The descriptive name and connection defaults for one oracle session.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionSpec {
    pub id: String,
    pub sql_mode: SqlMode,
    pub time_zone: TimeZone,
    pub isolation: IsolationLevel,
    pub autocommit: bool,
}

/// One statement sent to a session.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Step {
    pub id: String,
    pub session_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub probe: Option<ModeProbe>,
    pub sql: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub params: Option<Vec<TypedValue>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parallel: Option<ParallelBarrier>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub schedule_dependent: Option<String>,
}

/// Releases all members of a named group before they execute their statements concurrently.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ParallelBarrier {
    pub group: String,
}

/// Checks allocation facts that may vary by concurrent execution order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ParallelAllocationAssertion {
    pub group: String,
    pub participants: Vec<ParallelAllocationParticipant>,
    pub evidence_step_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub post_rollback_label: Option<String>,
}

/// Connects one concurrent insert with that session's later `LAST_INSERT_ID()` read.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ParallelAllocationParticipant {
    pub insert_step_id: String,
    pub last_insert_id_step_id: String,
    pub affected_rows: u64,
    pub labels: Vec<String>,
    #[serde(default)]
    pub rolled_back: bool,
}

/// A two-phase case whose connection boundary is an oracle restart.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LifecycleCase {
    pub version: u32,
    pub id: String,
    #[serde(default)]
    pub tags: Vec<String>,
    pub sessions: Vec<SessionSpec>,
    pub before_restart: Vec<Step>,
    pub after_restart: Vec<Step>,
}

/// Identifies variants of the same read-only SQL used to isolate a session-mode effect.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ModeProbe {
    pub group: String,
    pub variant: String,
}

/// SQL modes that affect the supported MySQL behavior.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SqlModeFlag {
    OnlyFullGroupBy,
    StrictTransTables,
    StrictAllTables,
    NoZeroInDate,
    NoZeroDate,
    ErrorForDivisionByZero,
    NoEngineSubstitution,
    AnsiQuotes,
    NoBackslashEscapes,
    PipesAsConcat,
    PadCharToFullLength,
    HighNotPrecedence,
    RealAsFloat,
    IgnoreSpace,
    NoAutoValueOnZero,
    NoUnsignedSubtraction,
    Ansi,
    Traditional,
    AllowInvalidDates,
    NoDirInCreate,
    TimeTruncateFractional,
}

/// An ordered, duplicate-free set of SQL modes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SqlMode {
    #[serde(default)]
    pub flags: Vec<SqlModeFlag>,
}

impl Default for SqlMode {
    fn default() -> Self {
        Self {
            flags: vec![
                SqlModeFlag::OnlyFullGroupBy,
                SqlModeFlag::StrictTransTables,
                SqlModeFlag::NoZeroInDate,
                SqlModeFlag::NoZeroDate,
                SqlModeFlag::ErrorForDivisionByZero,
                SqlModeFlag::NoEngineSubstitution,
            ],
        }
    }
}

impl SqlMode {
    /// Creates a canonical mode list so JSON comparisons do not depend on input order.
    pub fn new(mut flags: Vec<SqlModeFlag>) -> Result<Self, SqlModeError> {
        flags.sort_unstable();
        for pair in flags.windows(2) {
            if pair[0] == pair[1] {
                return Err(SqlModeError::Duplicate(pair[0]));
            }
        }
        Ok(Self { flags })
    }

    pub fn contains(&self, flag: SqlModeFlag) -> bool {
        self.flags.contains(&flag)
    }

    pub fn validate(&self) -> Result<(), SqlModeError> {
        Self::new(self.flags.clone()).map(|_| ())
    }
}

impl<'de> Deserialize<'de> for SqlMode {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct Fields {
            #[serde(default)]
            flags: Vec<SqlModeFlag>,
        }

        let fields = Fields::deserialize(deserializer)?;
        Self::new(fields.flags).map_err(D::Error::custom)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum SqlModeError {
    #[error("SQL mode contains duplicate flag `{0:?}`")]
    Duplicate(SqlModeFlag),
}

/// A session time zone accepted by the MySQL compatibility target.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TimeZone {
    Utc,
    FixedOffset { seconds: i32 },
    Iana { name: String },
}

impl Default for TimeZone {
    fn default() -> Self {
        Self::Utc
    }
}

impl TimeZone {
    pub fn fixed_offset(seconds: i32) -> Result<Self, TimeZoneError> {
        if seconds == 0 {
            return Ok(Self::Utc);
        }
        let time_zone = Self::FixedOffset { seconds };
        time_zone.validate()?;
        Ok(time_zone)
    }

    pub fn iana(name: impl Into<String>) -> Result<Self, TimeZoneError> {
        let time_zone = Self::Iana { name: name.into() };
        time_zone.validate()?;
        Ok(time_zone)
    }

    pub fn validate(&self) -> Result<(), TimeZoneError> {
        match self {
            Self::Utc => Ok(()),
            Self::FixedOffset { seconds: 0 } => Err(TimeZoneError::ZeroOffsetMustUseUtc),
            Self::FixedOffset { seconds }
                if (-13 * 60 * 60 - 59 * 60..=14 * 60 * 60).contains(seconds) =>
            {
                Ok(())
            }
            Self::FixedOffset { seconds } => Err(TimeZoneError::OffsetOutOfRange(*seconds)),
            Self::Iana { name } if !name.trim().is_empty() => Ok(()),
            Self::Iana { .. } => Err(TimeZoneError::EmptyIanaName),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum TimeZoneError {
    #[error("zero time-zone offset must use the UTC representation")]
    ZeroOffsetMustUseUtc,
    #[error("fixed time-zone offset {0} seconds is outside -13:59..+14:00")]
    OffsetOutOfRange(i32),
    #[error("IANA time-zone name must not be empty")]
    EmptyIanaName,
}

/// Isolation levels that can occur in an input case, including levels rejected by Turso.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IsolationLevel {
    ReadUncommitted,
    ReadCommitted,
    RepeatableRead,
    Serializable,
}

impl Default for IsolationLevel {
    fn default() -> Self {
        Self::RepeatableRead
    }
}

impl IsolationLevel {
    pub fn is_supported(self) -> bool {
        matches!(self, Self::ReadCommitted | Self::RepeatableRead)
    }
}

/// Bytes encoded as standard base64 in JSON instead of an implementation-specific byte array.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
#[serde(transparent)]
pub struct Base64Bytes(String);

impl Base64Bytes {
    pub fn from_bytes(bytes: &[u8]) -> Self {
        Self(STANDARD.encode(bytes))
    }

    pub fn from_base64(value: impl Into<String>) -> Result<Self, base64::DecodeError> {
        let value = value.into();
        STANDARD.decode(&value)?;
        Ok(Self(value))
    }

    pub fn decode(&self) -> Result<Vec<u8>, base64::DecodeError> {
        STANDARD.decode(&self.0)
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl<'de> Deserialize<'de> for Base64Bytes {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        STANDARD
            .decode(&value)
            .map(|_| Self(value))
            .map_err(D::Error::custom)
    }
}

/// A value with enough type information to reproduce MySQL parameter and result semantics.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum TypedValue {
    Null,
    Bool { value: bool },
    SignedInt { value: i64 },
    UnsignedInt { value: u64 },
    Float { value: f64 },
    Decimal { value: String },
    Text { value: String },
    Bytes { base64: Base64Bytes },
    Date { value: String },
    Time { value: String },
    DateTime { value: String },
    Timestamp { value: String },
    Json { value: serde_json::Value },
}

pub type SqlParam = TypedValue;

impl Case {
    pub fn validate(&self) -> Result<(), CaseValidationError> {
        if self.version != CASE_FORMAT_VERSION {
            return Err(CaseValidationError::UnsupportedVersion(self.version));
        }
        if self.id.trim().is_empty() {
            return Err(CaseValidationError::EmptyCaseId);
        }
        validate_unique_nonempty(&self.tags, "tag")?;
        if self.sessions.is_empty() {
            return Err(CaseValidationError::NoSessions);
        }
        if self.steps.is_empty() {
            return Err(CaseValidationError::NoSteps);
        }

        let mut session_ids = HashSet::with_capacity(self.sessions.len());
        let mut sessions_by_id = HashMap::with_capacity(self.sessions.len());
        for session in &self.sessions {
            if session.id.trim().is_empty() {
                return Err(CaseValidationError::EmptySessionId);
            }
            if !session_ids.insert(&session.id) {
                return Err(CaseValidationError::DuplicateSession(session.id.clone()));
            }
            sessions_by_id.insert(session.id.as_str(), session);
            session
                .validate()
                .map_err(|source| CaseValidationError::InvalidSession {
                    session_id: session.id.clone(),
                    source,
                })?;
        }

        let mut step_ids = HashSet::with_capacity(self.steps.len());
        let mut probe_variants = HashSet::new();
        let mut parallel_groups = HashMap::<&str, Vec<usize>>::new();
        for step in &self.steps {
            if step.id.trim().is_empty() {
                return Err(CaseValidationError::EmptyStepId);
            }
            if !step_ids.insert(&step.id) {
                return Err(CaseValidationError::DuplicateStep(step.id.clone()));
            }
            if !session_ids.contains(&step.session_id) {
                return Err(CaseValidationError::UnknownSession {
                    step_id: step.id.clone(),
                    session_id: step.session_id.clone(),
                });
            }
            if step.sql.trim().is_empty() {
                return Err(CaseValidationError::EmptySql(step.id.clone()));
            }
            if let Some(parallel) = &step.parallel {
                if parallel.group.trim().is_empty() {
                    return Err(CaseValidationError::EmptyParallelGroup(step.id.clone()));
                }
                if step.probe.is_some() {
                    return Err(CaseValidationError::ParallelProbe(step.id.clone()));
                }
                parallel_groups
                    .entry(parallel.group.as_str())
                    .or_default()
                    .push(step_ids.len() - 1);
            }
            if let Some(group) = &step.schedule_dependent {
                if group.trim().is_empty() {
                    return Err(CaseValidationError::EmptyScheduleGroup(step.id.clone()));
                }
            }
            if let Some(probe) = &step.probe {
                if probe.group.trim().is_empty() {
                    return Err(CaseValidationError::EmptyProbeGroup(step.id.clone()));
                }
                if probe.variant.trim().is_empty() {
                    return Err(CaseValidationError::EmptyProbeVariant(step.id.clone()));
                }
                if !probe_variants.insert((&probe.group, &probe.variant)) {
                    return Err(CaseValidationError::DuplicateProbeVariant {
                        group: probe.group.clone(),
                        variant: probe.variant.clone(),
                    });
                }
            }
        }

        for (group, indexes) in &parallel_groups {
            if indexes.len() < 2 {
                return Err(CaseValidationError::ParallelNeedsParticipants {
                    group: (*group).to_owned(),
                });
            }
            if indexes.windows(2).any(|pair| pair[1] != pair[0] + 1) {
                return Err(CaseValidationError::ParallelMustBeContiguous {
                    group: (*group).to_owned(),
                });
            }
        }

        let mut asserted_parallel_groups = HashSet::new();
        for assertion in &self.parallel_assertions {
            if assertion.group.trim().is_empty() {
                return Err(CaseValidationError::EmptyParallelAssertionGroup);
            }
            if !asserted_parallel_groups.insert(&assertion.group) {
                return Err(CaseValidationError::DuplicateParallelAssertion(
                    assertion.group.clone(),
                ));
            }
            let Some(indexes) = parallel_groups.get(assertion.group.as_str()) else {
                return Err(CaseValidationError::UnknownParallelAssertionGroup(
                    assertion.group.clone(),
                ));
            };
            if assertion.participants.len() != indexes.len() {
                return Err(CaseValidationError::ParallelAssertionParticipantCount {
                    group: assertion.group.clone(),
                    expected: indexes.len(),
                    actual: assertion.participants.len(),
                });
            }
            let evidence = self
                .steps
                .iter()
                .find(|step| step.id == assertion.evidence_step_id)
                .ok_or_else(|| CaseValidationError::UnknownParallelEvidenceStep {
                    group: assertion.group.clone(),
                    step_id: assertion.evidence_step_id.clone(),
                })?;
            if evidence.schedule_dependent.is_some() {
                return Err(CaseValidationError::ParallelEvidenceMustBeStable {
                    group: assertion.group.clone(),
                    step_id: assertion.evidence_step_id.clone(),
                });
            }
            let mut insert_steps = HashSet::new();
            let mut last_insert_steps = HashSet::new();
            let mut labels = HashSet::new();
            for participant in &assertion.participants {
                let insert = self
                    .steps
                    .iter()
                    .find(|step| step.id == participant.insert_step_id)
                    .ok_or_else(|| CaseValidationError::UnknownParallelParticipantStep {
                        group: assertion.group.clone(),
                        step_id: participant.insert_step_id.clone(),
                    })?;
                if insert.parallel.as_ref().map(|item| item.group.as_str())
                    != Some(assertion.group.as_str())
                {
                    return Err(CaseValidationError::ParallelParticipantNotInGroup {
                        group: assertion.group.clone(),
                        step_id: participant.insert_step_id.clone(),
                    });
                }
                let last_insert = self
                    .steps
                    .iter()
                    .find(|step| step.id == participant.last_insert_id_step_id)
                    .ok_or_else(|| CaseValidationError::UnknownParallelParticipantStep {
                        group: assertion.group.clone(),
                        step_id: participant.last_insert_id_step_id.clone(),
                    })?;
                if last_insert.session_id != insert.session_id
                    || last_insert.schedule_dependent.as_deref() != Some(assertion.group.as_str())
                {
                    return Err(CaseValidationError::ParallelLastInsertIdStepMismatch {
                        group: assertion.group.clone(),
                        step_id: participant.last_insert_id_step_id.clone(),
                    });
                }
                if !insert_steps.insert(&participant.insert_step_id)
                    || !last_insert_steps.insert(&participant.last_insert_id_step_id)
                {
                    return Err(CaseValidationError::DuplicateParallelParticipantStep {
                        group: assertion.group.clone(),
                    });
                }
                if participant.affected_rows == 0
                    || participant.labels.len() != participant.affected_rows as usize
                {
                    return Err(CaseValidationError::ParallelParticipantLabelsMismatch {
                        group: assertion.group.clone(),
                        step_id: participant.insert_step_id.clone(),
                        expected: participant.affected_rows,
                        actual: participant.labels.len(),
                    });
                }
                for label in &participant.labels {
                    if label.trim().is_empty() || !labels.insert(label) {
                        return Err(CaseValidationError::DuplicateParallelLabel {
                            group: assertion.group.clone(),
                            label: label.clone(),
                        });
                    }
                }
            }
            if let Some(label) = &assertion.post_rollback_label {
                if label.trim().is_empty() || !labels.insert(label) {
                    return Err(CaseValidationError::DuplicateParallelLabel {
                        group: assertion.group.clone(),
                        label: label.clone(),
                    });
                }
            }
        }

        for step in self.steps.iter().filter(|step| step.probe.is_some()) {
            let probe = step.probe.as_ref().expect("filtered probe steps");
            let group = self
                .steps
                .iter()
                .filter(|candidate| {
                    candidate.probe.as_ref().map(|item| &item.group) == Some(&probe.group)
                })
                .collect::<Vec<_>>();
            if group.len() < 2 {
                return Err(CaseValidationError::ProbeNeedsVariants(probe.group.clone()));
            }
            let first = group[0];
            let first_session = sessions_by_id[first.session_id.as_str()];
            let mut has_mode_difference = false;
            for candidate in group.iter().skip(1) {
                if candidate.sql != first.sql || candidate.params != first.params {
                    return Err(CaseValidationError::ProbeStatementMismatch {
                        group: probe.group.clone(),
                    });
                }
                let candidate_session = sessions_by_id[candidate.session_id.as_str()];
                has_mode_difference |= candidate_session.sql_mode != first_session.sql_mode;
                if candidate_session.time_zone != first_session.time_zone
                    || candidate_session.isolation != first_session.isolation
                    || candidate_session.autocommit != first_session.autocommit
                {
                    return Err(CaseValidationError::ProbeEnvironmentMismatch {
                        group: probe.group.clone(),
                    });
                }
            }
            if !has_mode_difference {
                return Err(CaseValidationError::ProbeModeNotDifferent {
                    group: probe.group.clone(),
                });
            }
        }
        Ok(())
    }
}

impl SessionSpec {
    pub fn validate(&self) -> Result<(), SessionValidationError> {
        self.sql_mode.validate()?;
        self.time_zone.validate()?;
        Ok(())
    }
}

impl LifecycleCase {
    pub fn validate(&self) -> Result<(), CaseValidationError> {
        Case {
            version: self.version,
            id: self.id.clone(),
            tags: self.tags.clone(),
            sessions: self.sessions.clone(),
            steps: self
                .before_restart
                .iter()
                .chain(&self.after_restart)
                .cloned()
                .collect(),
            parallel_assertions: Vec::new(),
        }
        .validate()
    }
}

fn validate_unique_nonempty(values: &[String], label: &str) -> Result<(), CaseValidationError> {
    let mut seen = HashSet::with_capacity(values.len());
    for value in values {
        if value.trim().is_empty() {
            return Err(CaseValidationError::EmptyTag {
                label: label.to_owned(),
            });
        }
        if !seen.insert(value) {
            return Err(CaseValidationError::DuplicateTag {
                label: label.to_owned(),
                value: value.clone(),
            });
        }
    }
    Ok(())
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum SessionValidationError {
    #[error(transparent)]
    SqlMode(#[from] SqlModeError),
    #[error(transparent)]
    TimeZone(#[from] TimeZoneError),
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum CaseValidationError {
    #[error("unsupported case format version {0}; expected {CASE_FORMAT_VERSION}")]
    UnsupportedVersion(u32),
    #[error("case id must not be empty")]
    EmptyCaseId,
    #[error("{label} must not be empty")]
    EmptyTag { label: String },
    #[error("duplicate {label} `{value}`")]
    DuplicateTag { label: String, value: String },
    #[error("session id must not be empty")]
    EmptySessionId,
    #[error("case must define at least one session")]
    NoSessions,
    #[error("duplicate session id `{0}`")]
    DuplicateSession(String),
    #[error("invalid session `{session_id}`: {source}")]
    InvalidSession {
        session_id: String,
        source: SessionValidationError,
    },
    #[error("step id must not be empty")]
    EmptyStepId,
    #[error("case must define at least one step")]
    NoSteps,
    #[error("duplicate step id `{0}`")]
    DuplicateStep(String),
    #[error("step `{step_id}` refers to unknown session `{session_id}`")]
    UnknownSession { step_id: String, session_id: String },
    #[error("step `{0}` SQL must not be empty")]
    EmptySql(String),
    #[error("parallel group for step `{0}` must not be empty")]
    EmptyParallelGroup(String),
    #[error("schedule-dependent group for step `{0}` must not be empty")]
    EmptyScheduleGroup(String),
    #[error("parallel step `{0}` cannot be a mode probe")]
    ParallelProbe(String),
    #[error("parallel group `{group}` must contain at least two steps")]
    ParallelNeedsParticipants { group: String },
    #[error("parallel group `{group}` must be contiguous")]
    ParallelMustBeContiguous { group: String },
    #[error("parallel assertion group must not be empty")]
    EmptyParallelAssertionGroup,
    #[error("duplicate parallel assertion for group `{0}`")]
    DuplicateParallelAssertion(String),
    #[error("parallel assertion refers to unknown group `{0}`")]
    UnknownParallelAssertionGroup(String),
    #[error("parallel assertion `{group}` has {actual} participants, expected {expected}")]
    ParallelAssertionParticipantCount {
        group: String,
        expected: usize,
        actual: usize,
    },
    #[error("parallel assertion `{group}` refers to unknown step `{step_id}")]
    UnknownParallelParticipantStep { group: String, step_id: String },
    #[error("parallel assertion `{group}` participant `{step_id}` is not in that group")]
    ParallelParticipantNotInGroup { group: String, step_id: String },
    #[error("parallel assertion `{group}` has an incompatible LAST_INSERT_ID step `{step_id}")]
    ParallelLastInsertIdStepMismatch { group: String, step_id: String },
    #[error("parallel assertion `{group}` repeats a participant step")]
    DuplicateParallelParticipantStep { group: String },
    #[error("parallel assertion `{group}` refers to unknown final evidence step `{step_id}")]
    UnknownParallelEvidenceStep { group: String, step_id: String },
    #[error(
        "parallel assertion `{group}` final evidence step `{step_id}` must be schedule-independent"
    )]
    ParallelEvidenceMustBeStable { group: String, step_id: String },
    #[error("parallel assertion `{group}` participant `{step_id}` has {actual} labels, expected {expected}")]
    ParallelParticipantLabelsMismatch {
        group: String,
        step_id: String,
        expected: u64,
        actual: usize,
    },
    #[error("parallel assertion `{group}` repeats or leaves empty label `{label}")]
    DuplicateParallelLabel { group: String, label: String },
    #[error("step `{0}` probe group must not be empty")]
    EmptyProbeGroup(String),
    #[error("step `{0}` probe variant must not be empty")]
    EmptyProbeVariant(String),
    #[error("duplicate probe variant `{variant}` in group `{group}`")]
    DuplicateProbeVariant { group: String, variant: String },
    #[error("probe group `{0}` must contain at least two variants")]
    ProbeNeedsVariants(String),
    #[error("probe group `{group}` must use identical SQL and parameters")]
    ProbeStatementMismatch { group: String },
    #[error("probe group `{group}` may differ only by SQL mode")]
    ProbeEnvironmentMismatch { group: String },
    #[error("probe group `{group}` must contain at least two different SQL modes")]
    ProbeModeNotDifferent { group: String },
}

pub type OracleCase = Case;

#[cfg(test)]
mod tests {
    use super::*;

    fn session() -> SessionSpec {
        SessionSpec {
            id: "s1".to_owned(),
            sql_mode: SqlMode::default(),
            time_zone: TimeZone::Utc,
            isolation: IsolationLevel::RepeatableRead,
            autocommit: true,
        }
    }

    #[test]
    fn default_mode_is_the_mysql_8_4_default() {
        let mode = SqlMode::default();
        assert!(mode.contains(SqlModeFlag::OnlyFullGroupBy));
        assert!(mode.contains(SqlModeFlag::StrictTransTables));
        assert!(mode.contains(SqlModeFlag::NoEngineSubstitution));
        assert_eq!(mode.flags.len(), 6);
    }

    #[test]
    fn sql_mode_constructor_canonicalizes_and_rejects_duplicates() {
        let mode = SqlMode::new(vec![SqlModeFlag::AnsiQuotes, SqlModeFlag::OnlyFullGroupBy])
            .expect("unique flags are valid");
        assert_eq!(
            mode.flags,
            vec![SqlModeFlag::OnlyFullGroupBy, SqlModeFlag::AnsiQuotes]
        );
        assert_eq!(
            SqlMode::new(vec![SqlModeFlag::AnsiQuotes, SqlModeFlag::AnsiQuotes]),
            Err(SqlModeError::Duplicate(SqlModeFlag::AnsiQuotes))
        );
    }

    #[test]
    fn base64_bytes_round_trip_as_json_string() {
        let value = TypedValue::Bytes {
            base64: Base64Bytes::from_bytes(b"a\0b"),
        };
        let json = serde_json::to_string(&value).expect("serialize value");
        assert_eq!(json, r#"{"type":"bytes","base64":"YQBi"}"#);
        let decoded: TypedValue = serde_json::from_str(&json).expect("deserialize value");
        assert_eq!(decoded, value);
    }

    #[test]
    fn case_validation_checks_step_session_references() {
        let case = Case {
            version: CASE_FORMAT_VERSION,
            id: "smoke".to_owned(),
            tags: vec!["p0".to_owned()],
            sessions: vec![session()],
            parallel_assertions: vec![],
            steps: vec![Step {
                id: "q1".to_owned(),
                session_id: "missing".to_owned(),
                probe: None,
                sql: "SELECT 1".to_owned(),
                params: None,
                parallel: None,
                schedule_dependent: None,
            }],
        };
        assert_eq!(
            case.validate(),
            Err(CaseValidationError::UnknownSession {
                step_id: "q1".to_owned(),
                session_id: "missing".to_owned(),
            })
        );
    }

    #[test]
    fn fixed_offset_and_iana_time_zones_validate() {
        assert!(TimeZone::fixed_offset(9 * 60 * 60).is_ok());
        assert!(TimeZone::fixed_offset(15 * 60 * 60).is_err());
        assert!(TimeZone::iana("Asia/Tokyo").is_ok());
        assert!(TimeZone::iana(" ").is_err());
    }

    #[test]
    fn mode_probe_requires_identical_statements_and_distinct_modes() {
        let default = session();
        let ansi = SessionSpec {
            id: "ansi".to_owned(),
            sql_mode: SqlMode::new(vec![SqlModeFlag::AnsiQuotes]).unwrap(),
            ..default.clone()
        };
        let mut case = Case {
            version: CASE_FORMAT_VERSION,
            id: "probe".to_owned(),
            tags: vec![],
            sessions: vec![default, ansi],
            parallel_assertions: vec![],
            steps: vec![
                Step {
                    id: "default".to_owned(),
                    session_id: "s1".to_owned(),
                    probe: Some(ModeProbe {
                        group: "quotes".to_owned(),
                        variant: "default".to_owned(),
                    }),
                    sql: "SELECT \"id\"".to_owned(),
                    params: None,
                    parallel: None,
                    schedule_dependent: None,
                },
                Step {
                    id: "ansi".to_owned(),
                    session_id: "ansi".to_owned(),
                    probe: Some(ModeProbe {
                        group: "quotes".to_owned(),
                        variant: "ansi".to_owned(),
                    }),
                    sql: "SELECT \"id\"".to_owned(),
                    params: None,
                    parallel: None,
                    schedule_dependent: None,
                },
            ],
        };
        assert!(case.validate().is_ok());

        case.steps[1].sql = "SELECT 1".to_owned();
        assert_eq!(
            case.validate(),
            Err(CaseValidationError::ProbeStatementMismatch {
                group: "quotes".to_owned()
            })
        );
    }

    #[test]
    fn numeric_coercion_fixture_is_valid() {
        validate_fixture(include_str!("../cases/p0/numeric-coercion.json"));
    }

    #[test]
    fn select_integer_equality_fixture_is_valid() {
        validate_fixture(include_str!("../cases/p0/select-integer-equality.json"));
    }

    #[test]
    fn utf8mb4_0900_ai_ci_collation_fixture_is_valid() {
        validate_fixture(include_str!(
            "../cases/p0/collation-utf8mb4-0900-ai-ci.json"
        ));
    }

    #[test]
    fn auto_increment_fixture_is_valid() {
        validate_fixture(include_str!("../cases/p0/auto-increment.json"));
    }

    #[test]
    fn parallel_auto_increment_fixture_is_valid() {
        validate_fixture(include_str!("../cases/p0/auto-increment-parallel.json"));
    }

    #[test]
    fn restart_auto_increment_fixture_is_valid() {
        let case: LifecycleCase =
            serde_json::from_str(include_str!("../cases/p0/auto-increment-restart.json"))
                .expect("fixture must deserialize");
        case.validate()
            .expect("fixture must satisfy the case contract");
        for step in case.before_restart.iter().chain(&case.after_restart) {
            let session = case
                .sessions
                .iter()
                .find(|session| session.id == step.session_id)
                .expect("case validation confirmed the step session");
            let dialect =
                crate::session_dialect::SessionMySqlDialect::from_sql_mode(&session.sql_mode);
            let statements = sqlparser::parser::Parser::parse_sql(&dialect, &step.sql)
                .expect("fixture SQL must parse with the pinned MySQL dialect");
            assert_eq!(statements.len(), 1, "step `{}`", step.id);
        }
    }

    fn validate_fixture(json: &str) {
        let case: Case = serde_json::from_str(json).expect("fixture must deserialize");
        case.validate()
            .expect("fixture must satisfy the case contract");

        for step in &case.steps {
            let session = case
                .sessions
                .iter()
                .find(|session| session.id == step.session_id)
                .expect("case validation confirmed the step session");
            let dialect =
                crate::session_dialect::SessionMySqlDialect::from_sql_mode(&session.sql_mode);
            let statements = sqlparser::parser::Parser::parse_sql(&dialect, &step.sql)
                .expect("fixture SQL must parse with the pinned MySQL dialect");
            assert_eq!(statements.len(), 1, "step `{}`", step.id);
        }
    }
}
