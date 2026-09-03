use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;

use crate::case::{Case, ParallelAllocationAssertion, TypedValue};
use crate::observe::{Observation, ObservationValidationError};

/// A comparison result whose paths can be used directly to locate a failing field in JSON.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ComparisonReport {
    pub equal: bool,
    pub mismatches: Vec<Mismatch>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Mismatch {
    pub path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub actual: Option<Value>,
}

#[derive(Debug, Error)]
pub enum ComparisonError {
    #[error("expected observation is invalid: {0}")]
    InvalidExpected(ObservationValidationError),
    #[error("actual observation is invalid: {0}")]
    InvalidActual(ObservationValidationError),
    #[error("failed to encode an observation for comparison: {0}")]
    Serialization(#[from] serde_json::Error),
}

/// Validates both observations and compares every serialized field recursively.
pub fn compare_observations(
    expected: &Observation,
    actual: &Observation,
) -> Result<ComparisonReport, ComparisonError> {
    expected
        .validate()
        .map_err(ComparisonError::InvalidExpected)?;
    actual.validate().map_err(ComparisonError::InvalidActual)?;

    let expected = serde_json::to_value(expected)?;
    let actual = serde_json::to_value(actual)?;
    Ok(compare_json_values(&expected, &actual))
}

/// Compares a complete multi-session run while preserving step-indexed mismatch paths.
pub fn compare_runs(
    expected: &[Observation],
    actual: &[Observation],
) -> Result<ComparisonReport, ComparisonError> {
    for observation in expected {
        observation
            .validate()
            .map_err(ComparisonError::InvalidExpected)?;
    }
    for observation in actual {
        observation
            .validate()
            .map_err(ComparisonError::InvalidActual)?;
    }

    let expected = serde_json::to_value(expected)?;
    let actual = serde_json::to_value(actual)?;
    Ok(compare_json_values(&expected, &actual))
}

/// Compares a case while checking concurrent allocation relationships instead of one schedule.
pub fn compare_case_runs(
    case: &Case,
    expected: &[Observation],
    actual: &[Observation],
) -> Result<ComparisonReport, ComparisonError> {
    for observation in expected {
        observation
            .validate()
            .map_err(ComparisonError::InvalidExpected)?;
    }
    for observation in actual {
        observation
            .validate()
            .map_err(ComparisonError::InvalidActual)?;
    }

    let mut expected_json = serde_json::to_value(expected)?;
    let actual_json = serde_json::to_value(actual)?;
    let evidence_steps = case
        .parallel_assertions
        .iter()
        .map(|assertion| assertion.evidence_step_id.as_str())
        .collect::<HashSet<_>>();
    for (index, step) in case.steps.iter().enumerate() {
        let Some(expected_step) = expected_json.get_mut(index) else {
            continue;
        };
        let Some(actual_step) = actual_json.get(index) else {
            continue;
        };
        if step.parallel.is_some() {
            expected_step["last_insert_id"] = actual_step["last_insert_id"].clone();
        }
        if step.schedule_dependent.is_some() || evidence_steps.contains(step.id.as_str()) {
            expected_step["result"]["rows"] = actual_step["result"]["rows"].clone();
        }
    }

    let mut report = compare_json_values(&expected_json, &actual_json);
    report.mismatches.extend(validate_parallel_allocations(
        &case.parallel_assertions,
        actual,
    ));
    report.equal = report.mismatches.is_empty();
    Ok(report)
}

fn validate_parallel_allocations(
    assertions: &[ParallelAllocationAssertion],
    observations: &[Observation],
) -> Vec<Mismatch> {
    let by_step = observations
        .iter()
        .map(|observation| (observation.step_id.as_str(), observation))
        .collect::<std::collections::HashMap<_, _>>();
    let mut mismatches = Vec::new();
    let mut allocated_ids = BTreeMap::<u64, String>::new();
    let mut expected_visible_labels = HashMap::<String, u64>::new();
    let mut rollback_labels = HashSet::new();
    let mut post_rollback_ids = Vec::new();
    let mut evidence = None;

    for assertion in assertions {
        let evidence_path = format!(
            "$.parallel[{}].{}",
            assertion.group, assertion.evidence_step_id
        );
        let Some(observation) = by_step.get(assertion.evidence_step_id.as_str()) else {
            mismatches.push(missing_parallel_observation(&evidence_path));
            continue;
        };
        match label_ids(observation) {
            Ok(label_ids) => match &evidence {
                Some(existing) if existing != &label_ids => mismatches.push(parallel_mismatch(
                    format!("{evidence_path}.result.rows"),
                    serde_json::to_value(existing).unwrap_or(Value::Null),
                    serde_json::to_value(&label_ids).unwrap_or(Value::Null),
                )),
                None => evidence = Some(label_ids),
                _ => {}
            },
            Err(actual) => mismatches.push(parallel_mismatch(
                format!("{evidence_path}.result.rows"),
                Value::String("rows of unsigned id and text label".to_owned()),
                actual,
            )),
        }
        for participant in &assertion.participants {
            let insert_path = format!(
                "$.parallel[{}].{}",
                assertion.group, participant.insert_step_id
            );
            let Some(insert) = by_step.get(participant.insert_step_id.as_str()) else {
                mismatches.push(missing_parallel_observation(&insert_path));
                continue;
            };
            if insert.error.is_some() {
                mismatches.push(parallel_mismatch(
                    format!("{insert_path}.error"),
                    Value::Null,
                    serde_json::to_value(&insert.error).expect("error serializes"),
                ));
            }
            if insert.affected_rows != participant.affected_rows {
                mismatches.push(parallel_mismatch(
                    format!("{insert_path}.affected_rows"),
                    Value::from(participant.affected_rows),
                    Value::from(insert.affected_rows),
                ));
            }
            if insert.last_insert_id == 0 {
                mismatches.push(parallel_mismatch(
                    format!("{insert_path}.last_insert_id"),
                    Value::String("nonzero generated ID".to_owned()),
                    Value::from(insert.last_insert_id),
                ));
            }

            let end = insert
                .last_insert_id
                .checked_add(participant.affected_rows.saturating_sub(1));
            let Some(end) = end else {
                mismatches.push(parallel_mismatch(
                    format!("{insert_path}.last_insert_id"),
                    Value::String("range that fits unsigned 64-bit ID".to_owned()),
                    Value::from(insert.last_insert_id),
                ));
                continue;
            };
            for id in insert.last_insert_id..=end {
                if let Some(other) = allocated_ids.insert(id, participant.insert_step_id.clone()) {
                    mismatches.push(parallel_mismatch(
                        format!("{insert_path}.allocated_ids"),
                        Value::String("non-overlapping allocation range".to_owned()),
                        Value::String(format!("{id} also allocated by {other}")),
                    ));
                }
            }
            for (offset, label) in participant.labels.iter().enumerate() {
                let id = insert.last_insert_id + offset as u64;
                if participant.rolled_back {
                    rollback_labels.insert(label.clone());
                } else if expected_visible_labels.insert(label.clone(), id).is_some() {
                    mismatches.push(parallel_mismatch(
                        format!("{insert_path}.labels"),
                        Value::String("unique visible label".to_owned()),
                        Value::String(label.clone()),
                    ));
                }
            }

            let last_path = format!(
                "$.parallel[{}].{}",
                assertion.group, participant.last_insert_id_step_id
            );
            let Some(last_insert) = by_step.get(participant.last_insert_id_step_id.as_str()) else {
                mismatches.push(missing_parallel_observation(&last_path));
                continue;
            };
            let actual_last_insert_id = last_insert_id_value(last_insert);
            if actual_last_insert_id != Some(insert.last_insert_id) {
                mismatches.push(parallel_mismatch(
                    format!("{last_path}.result.rows[0][0]"),
                    Value::from(insert.last_insert_id),
                    actual_last_insert_id
                        .map(Value::from)
                        .unwrap_or(Value::Null),
                ));
            }
        }
        if let Some(label) = &assertion.post_rollback_label {
            post_rollback_ids.push(label.clone());
        }
    }

    if let Some(evidence) = evidence {
        for (label, id) in &expected_visible_labels {
            if evidence.get(label) != Some(id) {
                mismatches.push(parallel_mismatch(
                    format!("$.parallel.final_rows.{label}"),
                    Value::from(*id),
                    evidence
                        .get(label)
                        .copied()
                        .map(Value::from)
                        .unwrap_or(Value::Null),
                ));
            }
        }
        for label in rollback_labels {
            if let Some(id) = evidence.get(&label) {
                mismatches.push(parallel_mismatch(
                    format!("$.parallel.final_rows.{label}"),
                    Value::Null,
                    Value::from(*id),
                ));
            }
        }
        if let Some(max_allocated) = allocated_ids.last_key_value().map(|(id, _)| *id) {
            let min_allocated = allocated_ids
                .first_key_value()
                .map(|(id, _)| *id)
                .unwrap_or(0);
            if max_allocated - min_allocated + 1 != allocated_ids.len() as u64 {
                mismatches.push(parallel_mismatch(
                    "$.parallel.allocated_ids".to_owned(),
                    Value::String("one continuous allocation span".to_owned()),
                    Value::String("an unexplained allocation gap".to_owned()),
                ));
            }
            for label in post_rollback_ids {
                let expected = max_allocated.saturating_add(1);
                expected_visible_labels.insert(label.clone(), expected);
                if evidence.get(&label) != Some(&expected) {
                    mismatches.push(parallel_mismatch(
                        format!("$.parallel.post_rollback.{label}"),
                        Value::from(expected),
                        evidence
                            .get(&label)
                            .copied()
                            .map(Value::from)
                            .unwrap_or(Value::Null),
                    ));
                }
            }
        }
        let expected_labels = expected_visible_labels
            .keys()
            .cloned()
            .collect::<BTreeSet<_>>();
        let actual_labels = evidence.keys().cloned().collect::<BTreeSet<_>>();
        if actual_labels != expected_labels {
            mismatches.push(parallel_mismatch(
                "$.parallel.final_rows.labels".to_owned(),
                serde_json::to_value(expected_labels).unwrap_or(Value::Null),
                serde_json::to_value(actual_labels).unwrap_or(Value::Null),
            ));
        }
    }
    mismatches
}

fn label_ids(observation: &Observation) -> Result<BTreeMap<String, u64>, Value> {
    let Some(result) = &observation.result else {
        return Err(Value::Null);
    };
    let mut labels = BTreeMap::new();
    for row in &result.rows {
        let [id, label] = row.as_slice() else {
            return Err(serde_json::to_value(row).unwrap_or(Value::Null));
        };
        let Some(id) = typed_u64(id) else {
            return Err(serde_json::to_value(id).unwrap_or(Value::Null));
        };
        let TypedValue::Text { value: label } = label else {
            return Err(serde_json::to_value(label).unwrap_or(Value::Null));
        };
        if labels.insert(label.clone(), id).is_some() {
            return Err(Value::String(format!("duplicate label {label}")));
        }
    }
    Ok(labels)
}

fn last_insert_id_value(observation: &Observation) -> Option<u64> {
    let value = observation.result.as_ref()?.rows.first()?.first()?;
    typed_u64(value)
}

fn typed_u64(value: &TypedValue) -> Option<u64> {
    match value {
        TypedValue::SignedInt { value } => u64::try_from(*value).ok(),
        TypedValue::UnsignedInt { value } => Some(*value),
        _ => None,
    }
}

fn missing_parallel_observation(path: &str) -> Mismatch {
    Mismatch {
        path: path.to_owned(),
        expected: Some(Value::String("observation".to_owned())),
        actual: None,
    }
}

fn parallel_mismatch(path: String, expected: Value, actual: Value) -> Mismatch {
    Mismatch {
        path,
        expected: Some(expected),
        actual: Some(actual),
    }
}

/// Recursively compares two already-canonical JSON values.
pub fn compare_json_values(expected: &Value, actual: &Value) -> ComparisonReport {
    let mut mismatches = Vec::new();
    collect_mismatches("$", Some(expected), Some(actual), &mut mismatches);
    ComparisonReport {
        equal: mismatches.is_empty(),
        mismatches,
    }
}

impl ComparisonReport {
    pub fn is_equal(&self) -> bool {
        self.equal
    }

    pub fn into_result(self) -> Result<(), Self> {
        if self.equal {
            Ok(())
        } else {
            Err(self)
        }
    }
}

fn collect_mismatches(
    path: &str,
    expected: Option<&Value>,
    actual: Option<&Value>,
    mismatches: &mut Vec<Mismatch>,
) {
    match (expected, actual) {
        (None, None) => {}
        (Some(expected), Some(actual)) if expected == actual => {}
        (Some(Value::Object(expected)), Some(Value::Object(actual))) => {
            let keys: BTreeSet<&str> = expected
                .keys()
                .map(String::as_str)
                .chain(actual.keys().map(String::as_str))
                .collect();
            for key in keys {
                let child_path = format!("{path}.{key}");
                collect_mismatches(&child_path, expected.get(key), actual.get(key), mismatches);
            }
        }
        (Some(Value::Array(expected)), Some(Value::Array(actual))) => {
            let length = expected.len().max(actual.len());
            for index in 0..length {
                let child_path = format!("{path}[{index}]");
                collect_mismatches(
                    &child_path,
                    expected.get(index),
                    actual.get(index),
                    mismatches,
                );
            }
        }
        (expected, actual) => mismatches.push(Mismatch {
            path: path.to_owned(),
            expected: expected.cloned(),
            actual: actual.cloned(),
        }),
    }
}

pub type ObservationComparison = ComparisonReport;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::case::{
        IsolationLevel, ParallelAllocationAssertion, ParallelAllocationParticipant,
        ParallelBarrier, SessionSpec, SqlMode, Step, TimeZone, CASE_FORMAT_VERSION,
    };
    use crate::observe::{
        ColumnFlag, ColumnMetadata, MySqlType, ResultSet, SessionState, TransactionState,
        WarningSet,
    };

    fn observation(value: i64) -> Observation {
        Observation {
            version: 1,
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
                    column_length: None,
                    decimals: None,
                    nullable: false,
                    flags: vec![ColumnFlag::Numeric],
                }],
                rows: vec![vec![TypedValue::SignedInt { value }]],
            }),
            affected_rows: 0,
            last_insert_id: 0,
            warnings: WarningSet::default(),
            error: None,
            session_state: SessionState {
                current_database: None,
                sql_mode: SqlMode::default(),
                time_zone: TimeZone::Utc,
                isolation: IsolationLevel::RepeatableRead,
                autocommit: true,
                transaction: TransactionState::Idle,
            },
        }
    }

    fn label_observation(rows: &[(u64, &str)]) -> Observation {
        let mut observation = observation(0);
        observation.result = Some(ResultSet {
            columns: vec![
                ColumnMetadata {
                    name: "id".to_owned(),
                    original_name: None,
                    table: None,
                    original_table: None,
                    database: None,
                    catalog: None,
                    column_type: MySqlType::Long,
                    character_set_id: None,
                    character_set: None,
                    collation: None,
                    column_length: None,
                    decimals: None,
                    nullable: false,
                    flags: vec![ColumnFlag::Numeric],
                },
                ColumnMetadata {
                    name: "label".to_owned(),
                    original_name: None,
                    table: None,
                    original_table: None,
                    database: None,
                    catalog: None,
                    column_type: MySqlType::VarString,
                    character_set_id: None,
                    character_set: None,
                    collation: None,
                    column_length: None,
                    decimals: None,
                    nullable: false,
                    flags: vec![],
                },
            ],
            rows: rows
                .iter()
                .map(|(id, label)| {
                    vec![
                        TypedValue::UnsignedInt { value: *id },
                        TypedValue::Text {
                            value: (*label).to_owned(),
                        },
                    ]
                })
                .collect(),
        });
        observation
    }

    fn insert_observation(step_id: &str, id: u64, affected_rows: u64) -> Observation {
        let mut observation = observation(0);
        observation.step_id = step_id.to_owned();
        observation.result = None;
        observation.affected_rows = affected_rows;
        observation.last_insert_id = id;
        observation
    }

    fn last_insert_observation(step_id: &str, id: u64) -> Observation {
        let mut observation = observation(id as i64);
        observation.step_id = step_id.to_owned();
        observation
    }

    fn allocation_assertion(
        participants: Vec<ParallelAllocationParticipant>,
        post_rollback_label: Option<&str>,
    ) -> ParallelAllocationAssertion {
        ParallelAllocationAssertion {
            group: "writers".to_owned(),
            participants,
            evidence_step_id: "final".to_owned(),
            post_rollback_label: post_rollback_label.map(str::to_owned),
        }
    }

    #[test]
    fn reports_nested_paths_for_value_mismatch() {
        let report = compare_observations(&observation(1), &observation(2)).expect("valid input");
        assert!(!report.is_equal());
        assert_eq!(report.mismatches.len(), 1);
        assert_eq!(report.mismatches[0].path, "$.result.rows[0][0].value");
        assert_eq!(report.mismatches[0].expected, Some(serde_json::json!(1)));
        assert_eq!(report.mismatches[0].actual, Some(serde_json::json!(2)));
    }

    #[test]
    fn reports_missing_and_extra_rows_with_index_paths() {
        let expected = vec![observation(1)];
        let actual = vec![observation(1), observation(2)];
        let report = compare_runs(&expected, &actual).expect("valid input");
        assert_eq!(report.mismatches.len(), 1);
        assert_eq!(report.mismatches[0].path, "$[1]");
        assert_eq!(report.mismatches[0].expected, None);
        assert!(report.mismatches[0].actual.is_some());
    }

    #[test]
    fn validates_warning_count_before_comparing() {
        let mut actual = observation(1);
        actual.warnings.warning_count = 1;
        assert!(matches!(
            compare_observations(&observation(1), &actual),
            Err(ComparisonError::InvalidActual(
                ObservationValidationError::WarningCountMismatch { .. }
            ))
        ));
    }

    #[test]
    fn equal_observations_have_no_mismatches() {
        let report = compare_observations(&observation(1), &observation(1)).expect("valid input");
        assert_eq!(
            report,
            ComparisonReport {
                equal: true,
                mismatches: Vec::new(),
            }
        );
        assert_eq!(report.into_result(), Ok(()));
    }

    #[test]
    fn parallel_allocation_comparison_accepts_a_different_session_schedule() {
        let session = |id: &str| SessionSpec {
            id: id.to_owned(),
            sql_mode: SqlMode::default(),
            time_zone: TimeZone::Utc,
            isolation: IsolationLevel::RepeatableRead,
            autocommit: true,
        };
        let step = |id: &str,
                    session_id: &str,
                    parallel: Option<&str>,
                    schedule_dependent: Option<&str>| Step {
            id: id.to_owned(),
            session_id: session_id.to_owned(),
            probe: None,
            sql: "SELECT 1".to_owned(),
            params: None,
            parallel: parallel.map(|group| ParallelBarrier {
                group: group.to_owned(),
            }),
            schedule_dependent: schedule_dependent.map(str::to_owned),
        };
        let case = Case {
            version: CASE_FORMAT_VERSION,
            id: "parallel".to_owned(),
            tags: vec![],
            sessions: vec![session("a"), session("b")],
            steps: vec![
                step("insert_a", "a", Some("writers"), None),
                step("insert_b", "b", Some("writers"), None),
                step("last_a", "a", None, Some("writers")),
                step("last_b", "b", None, Some("writers")),
                step("final", "a", None, None),
            ],
            parallel_assertions: vec![ParallelAllocationAssertion {
                group: "writers".to_owned(),
                evidence_step_id: "final".to_owned(),
                post_rollback_label: None,
                participants: vec![
                    ParallelAllocationParticipant {
                        insert_step_id: "insert_a".to_owned(),
                        last_insert_id_step_id: "last_a".to_owned(),
                        affected_rows: 1,
                        labels: vec!["a".to_owned()],
                        rolled_back: false,
                    },
                    ParallelAllocationParticipant {
                        insert_step_id: "insert_b".to_owned(),
                        last_insert_id_step_id: "last_b".to_owned(),
                        affected_rows: 1,
                        labels: vec!["b".to_owned()],
                        rolled_back: false,
                    },
                ],
            }],
        };
        let mut expected = vec![
            observation(1),
            observation(2),
            observation(1),
            observation(2),
            label_observation(&[(1, "a"), (2, "b")]),
        ];
        let mut actual = vec![
            observation(2),
            observation(1),
            observation(2),
            observation(1),
            label_observation(&[(1, "b"), (2, "a")]),
        ];
        for (index, observation) in expected.iter_mut().enumerate() {
            observation.step_id = case.steps[index].id.clone();
            observation.session_id = case.steps[index].session_id.clone();
            if index < 2 {
                observation.result = None;
                observation.affected_rows = 1;
                observation.last_insert_id = (index + 1) as u64;
            }
        }
        for (index, observation) in actual.iter_mut().enumerate() {
            observation.step_id = case.steps[index].id.clone();
            observation.session_id = case.steps[index].session_id.clone();
            if index < 2 {
                observation.result = None;
                observation.affected_rows = 1;
                observation.last_insert_id = (2 - index) as u64;
            }
        }

        assert!(compare_case_runs(&case, &expected, &actual).unwrap().equal);
    }

    #[test]
    fn parallel_allocation_rejects_duplicate_successful_ids() {
        let assertion = allocation_assertion(
            vec![
                ParallelAllocationParticipant {
                    insert_step_id: "a".to_owned(),
                    last_insert_id_step_id: "last_a".to_owned(),
                    affected_rows: 1,
                    labels: vec!["a".to_owned()],
                    rolled_back: false,
                },
                ParallelAllocationParticipant {
                    insert_step_id: "b".to_owned(),
                    last_insert_id_step_id: "last_b".to_owned(),
                    affected_rows: 1,
                    labels: vec!["b".to_owned()],
                    rolled_back: false,
                },
            ],
            None,
        );
        let mut evidence = label_observation(&[(1, "a"), (1, "b")]);
        evidence.step_id = "final".to_owned();
        let observations = vec![
            insert_observation("a", 1, 1),
            insert_observation("b", 1, 1),
            last_insert_observation("last_a", 1),
            last_insert_observation("last_b", 1),
            evidence,
        ];
        assert!(!validate_parallel_allocations(&[assertion], &observations).is_empty());
    }

    #[test]
    fn parallel_allocation_rejects_interleaved_multi_row_range() {
        let assertion = allocation_assertion(
            vec![
                ParallelAllocationParticipant {
                    insert_step_id: "a".to_owned(),
                    last_insert_id_step_id: "last_a".to_owned(),
                    affected_rows: 2,
                    labels: vec!["a1".to_owned(), "a2".to_owned()],
                    rolled_back: false,
                },
                ParallelAllocationParticipant {
                    insert_step_id: "b".to_owned(),
                    last_insert_id_step_id: "last_b".to_owned(),
                    affected_rows: 2,
                    labels: vec!["b1".to_owned(), "b2".to_owned()],
                    rolled_back: false,
                },
            ],
            None,
        );
        let mut evidence = label_observation(&[(1, "a1"), (2, "b1"), (3, "a2"), (4, "b2")]);
        evidence.step_id = "final".to_owned();
        let observations = vec![
            insert_observation("a", 1, 2),
            insert_observation("b", 3, 2),
            last_insert_observation("last_a", 1),
            last_insert_observation("last_b", 3),
            evidence,
        ];
        assert!(!validate_parallel_allocations(&[assertion], &observations).is_empty());
    }

    #[test]
    fn parallel_allocation_rejects_unrelated_gap_after_rollback() {
        let assertion = allocation_assertion(
            vec![
                ParallelAllocationParticipant {
                    insert_step_id: "committed".to_owned(),
                    last_insert_id_step_id: "last_committed".to_owned(),
                    affected_rows: 1,
                    labels: vec!["committed".to_owned()],
                    rolled_back: false,
                },
                ParallelAllocationParticipant {
                    insert_step_id: "rolled_back".to_owned(),
                    last_insert_id_step_id: "last_rolled_back".to_owned(),
                    affected_rows: 1,
                    labels: vec!["rolled_back".to_owned()],
                    rolled_back: true,
                },
            ],
            Some("after_rollback"),
        );
        let mut evidence = label_observation(&[(1, "committed"), (4, "after_rollback")]);
        evidence.step_id = "final".to_owned();
        let observations = vec![
            insert_observation("committed", 1, 1),
            insert_observation("rolled_back", 3, 1),
            last_insert_observation("last_committed", 1),
            last_insert_observation("last_rolled_back", 3),
            evidence,
        ];
        assert!(!validate_parallel_allocations(&[assertion], &observations).is_empty());
    }

    #[test]
    fn parallel_allocation_rejects_phantom_final_label() {
        let assertion = allocation_assertion(
            vec![
                ParallelAllocationParticipant {
                    insert_step_id: "a".to_owned(),
                    last_insert_id_step_id: "last_a".to_owned(),
                    affected_rows: 1,
                    labels: vec!["a".to_owned()],
                    rolled_back: false,
                },
                ParallelAllocationParticipant {
                    insert_step_id: "b".to_owned(),
                    last_insert_id_step_id: "last_b".to_owned(),
                    affected_rows: 1,
                    labels: vec!["b".to_owned()],
                    rolled_back: false,
                },
            ],
            None,
        );
        let mut evidence = label_observation(&[(1, "a"), (2, "b"), (3, "phantom")]);
        evidence.step_id = "final".to_owned();
        let observations = vec![
            insert_observation("a", 1, 1),
            insert_observation("b", 2, 1),
            last_insert_observation("last_a", 1),
            last_insert_observation("last_b", 2),
            evidence,
        ];
        assert!(!validate_parallel_allocations(&[assertion], &observations).is_empty());
    }
}
