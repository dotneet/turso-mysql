use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use sqlparser::{
    ast::Statement,
    dialect::{Dialect, MySqlDialect},
    parser::Parser,
};
use thiserror::Error;

use crate::case::{Case, SqlMode};
use crate::compare::{compare_json_values, Mismatch};
use crate::observe::{MySqlError, Observation};
use crate::session_dialect::SessionMySqlDialect;

pub const PARSER_REPORT_FORMAT_VERSION: u32 = 4;
pub const SQLPARSER_VERSION: &str = "0.62.0";

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ParserReport {
    pub version: u32,
    pub parser: ParserIdentity,
    pub baseline_parser: ParserIdentity,
    pub case_id: String,
    pub summary: ParserSummary,
    pub steps: Vec<ParserStepReport>,
    pub mode_semantic_collisions: Vec<ModeSemanticCollision>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ParserIdentity {
    pub crate_name: String,
    pub version: String,
    pub dialect: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ParserSummary {
    pub both_accept: usize,
    pub mysql_only: usize,
    pub sqlparser_only: usize,
    pub both_reject: usize,
    pub changed_round_trips: usize,
    pub steps_changed_by_session_dialect: usize,
    pub mode_semantic_collisions: usize,
    pub collisions_distinguished_by_session_dialect: usize,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ParserStepReport {
    pub step_id: String,
    pub session_id: String,
    pub sql_mode_before: SqlMode,
    pub sql_mode_after: SqlMode,
    pub sql: String,
    pub mysql: MySqlSyntaxOutcome,
    pub baseline_sqlparser: SqlparserOutcome,
    pub sqlparser: SqlparserOutcome,
    pub acceptance: AcceptanceComparison,
    pub changed_by_session_dialect: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModeSemanticCollision {
    pub group: String,
    pub earlier_step_id: String,
    pub later_step_id: String,
    pub sql: String,
    pub earlier_sql_mode: SqlMode,
    pub later_sql_mode: SqlMode,
    pub session_dialect_distinguishes: bool,
    pub semantic_differences: Vec<Mismatch>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum MySqlSyntaxOutcome {
    Accepted {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        execution_error: Option<MySqlError>,
    },
    Rejected {
        error: MySqlError,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum SqlparserOutcome {
    Accepted {
        statement_count: usize,
        normalized_sql: String,
        debug_fingerprint: String,
        round_trip: AstRoundTrip,
    },
    Rejected {
        error: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum AstRoundTrip {
    Equal,
    Changed {
        original_ast: String,
        reparsed_ast: String,
    },
    Rejected {
        error: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AcceptanceComparison {
    BothAccept,
    MySqlOnly,
    SqlparserOnly,
    BothReject,
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum ParserReportError {
    #[error("case contains {case_steps} steps but the reference has {reference_steps}")]
    StepCount {
        case_steps: usize,
        reference_steps: usize,
    },
    #[error(
        "reference step {index} is `{actual_step}` for session `{actual_session}`, expected `{expected_step}` for session `{expected_session}`"
    )]
    StepIdentity {
        index: usize,
        expected_step: String,
        expected_session: String,
        actual_step: String,
        actual_session: String,
    },
    #[error("step `{step_id}` refers to missing session `{session_id}`")]
    MissingSession { step_id: String, session_id: String },
    #[error("mode probe step `{step_id}` must be one non-locking query")]
    ProbeMustBeNonLockingQuery { step_id: String },
}

pub fn build_parser_report(
    case: &Case,
    reference: &[Observation],
) -> Result<ParserReport, ParserReportError> {
    if case.steps.len() != reference.len() {
        return Err(ParserReportError::StepCount {
            case_steps: case.steps.len(),
            reference_steps: reference.len(),
        });
    }

    let sessions = case
        .sessions
        .iter()
        .map(|session| (session.id.as_str(), session))
        .collect::<HashMap<_, _>>();
    let mut effective_modes = case
        .sessions
        .iter()
        .map(|session| (session.id.as_str(), session.sql_mode.clone()))
        .collect::<HashMap<_, _>>();
    let mut summary = ParserSummary::default();
    let mut steps = Vec::with_capacity(case.steps.len());

    for (index, (step, observation)) in case.steps.iter().zip(reference).enumerate() {
        if step.id != observation.step_id || step.session_id != observation.session_id {
            return Err(ParserReportError::StepIdentity {
                index,
                expected_step: step.id.clone(),
                expected_session: step.session_id.clone(),
                actual_step: observation.step_id.clone(),
                actual_session: observation.session_id.clone(),
            });
        }
        sessions.get(step.session_id.as_str()).ok_or_else(|| {
            ParserReportError::MissingSession {
                step_id: step.id.clone(),
                session_id: step.session_id.clone(),
            }
        })?;
        let sql_mode_before = effective_modes
            .get(step.session_id.as_str())
            .expect("session map is initialized from the validated case")
            .clone();
        let sql_mode_after = observation.session_state.sql_mode.clone();
        let mysql = mysql_syntax_outcome(observation);
        let baseline_sqlparser = sqlparser_outcome(&MySqlDialect {}, &step.sql);
        let session_dialect = SessionMySqlDialect::from_sql_mode(&sql_mode_before);
        if step.probe.is_some() && !is_nonlocking_query(&session_dialect, &step.sql) {
            return Err(ParserReportError::ProbeMustBeNonLockingQuery {
                step_id: step.id.clone(),
            });
        }
        let sqlparser = sqlparser_outcome(&session_dialect, &step.sql);
        let acceptance = compare_acceptance(&mysql, &sqlparser);
        update_summary(&mut summary, acceptance, &sqlparser);
        let changed_by_session_dialect =
            !parser_results_equal(&MySqlDialect {}, &session_dialect, &step.sql);
        if changed_by_session_dialect {
            summary.steps_changed_by_session_dialect += 1;
        }
        steps.push(ParserStepReport {
            step_id: step.id.clone(),
            session_id: step.session_id.clone(),
            sql_mode_before,
            sql_mode_after: sql_mode_after.clone(),
            sql: step.sql.clone(),
            mysql,
            baseline_sqlparser,
            sqlparser,
            acceptance,
            changed_by_session_dialect,
        });
        effective_modes.insert(step.session_id.as_str(), sql_mode_after);
    }

    let mode_semantic_collisions = find_mode_semantic_collisions(case, reference, &steps);
    summary.mode_semantic_collisions = mode_semantic_collisions.len();
    summary.collisions_distinguished_by_session_dialect = mode_semantic_collisions
        .iter()
        .filter(|collision| collision.session_dialect_distinguishes)
        .count();

    Ok(ParserReport {
        version: PARSER_REPORT_FORMAT_VERSION,
        parser: ParserIdentity {
            crate_name: "sqlparser".to_owned(),
            version: SQLPARSER_VERSION.to_owned(),
            dialect: "SessionMySqlDialect".to_owned(),
        },
        baseline_parser: ParserIdentity {
            crate_name: "sqlparser".to_owned(),
            version: SQLPARSER_VERSION.to_owned(),
            dialect: "MySqlDialect".to_owned(),
        },
        case_id: case.id.clone(),
        summary,
        steps,
        mode_semantic_collisions,
    })
}

fn is_nonlocking_query(session_dialect: &SessionMySqlDialect, sql: &str) -> bool {
    [&MySqlDialect {} as &dyn Dialect, session_dialect]
        .into_iter()
        .filter_map(|dialect| Parser::parse_sql(dialect, sql).ok())
        .any(|statements| {
            matches!(statements.as_slice(), [Statement::Query(query)] if query.locks.is_empty())
        })
}

fn find_mode_semantic_collisions(
    case: &Case,
    reference: &[Observation],
    steps: &[ParserStepReport],
) -> Vec<ModeSemanticCollision> {
    let mut collisions = Vec::new();
    for earlier in 0..case.steps.len() {
        for later in (earlier + 1)..case.steps.len() {
            let earlier_case = &case.steps[earlier];
            let later_case = &case.steps[later];
            let earlier_report = &steps[earlier];
            let later_report = &steps[later];
            let Some(earlier_probe) = &earlier_case.probe else {
                continue;
            };
            let Some(later_probe) = &later_case.probe else {
                continue;
            };
            if earlier_probe.group != later_probe.group
                || earlier_report.sql_mode_before == later_report.sql_mode_before
                || statement_outcomes_equal(&reference[earlier], &reference[later])
            {
                continue;
            }
            let earlier_dialect =
                SessionMySqlDialect::from_sql_mode(&earlier_report.sql_mode_before);
            let later_dialect = SessionMySqlDialect::from_sql_mode(&later_report.sql_mode_before);
            let semantic_differences = compare_json_values(
                &statement_semantics(&reference[earlier]),
                &statement_semantics(&reference[later]),
            )
            .mismatches;
            collisions.push(ModeSemanticCollision {
                group: earlier_probe.group.clone(),
                earlier_step_id: earlier_case.id.clone(),
                later_step_id: later_case.id.clone(),
                sql: earlier_case.sql.clone(),
                earlier_sql_mode: earlier_report.sql_mode_before.clone(),
                later_sql_mode: later_report.sql_mode_before.clone(),
                session_dialect_distinguishes: !parser_results_equal(
                    &earlier_dialect,
                    &later_dialect,
                    &earlier_case.sql,
                ),
                semantic_differences,
            });
        }
    }
    collisions
}

fn statement_outcomes_equal(left: &Observation, right: &Observation) -> bool {
    statement_semantics(left) == statement_semantics(right)
}

fn statement_semantics(observation: &Observation) -> serde_json::Value {
    let warnings = observation
        .warnings
        .details
        .iter()
        .map(|warning| {
            serde_json::json!({
                "level": warning.level,
                "code": warning.code,
                "sql_state": warning.sql_state,
            })
        })
        .collect::<Vec<_>>();
    let error = observation.error.as_ref().map(|error| {
        serde_json::json!({
            "number": error.number,
            "sql_state": error.sql_state,
        })
    });
    serde_json::json!({
        "result": observation.result,
        "affected_rows": observation.affected_rows,
        "last_insert_id": observation.last_insert_id,
        "warnings": warnings,
        "error": error,
        "session_state": {
            "current_database": observation.session_state.current_database,
            "time_zone": observation.session_state.time_zone,
            "isolation": observation.session_state.isolation,
            "autocommit": observation.session_state.autocommit,
            "transaction": observation.session_state.transaction,
        }
    })
}

fn mysql_syntax_outcome(observation: &Observation) -> MySqlSyntaxOutcome {
    match &observation.error {
        Some(error) if matches!(error.number, 1064 | 1149) => MySqlSyntaxOutcome::Rejected {
            error: error.clone(),
        },
        error => MySqlSyntaxOutcome::Accepted {
            execution_error: error.clone(),
        },
    }
}

fn sqlparser_outcome(dialect: &dyn Dialect, sql: &str) -> SqlparserOutcome {
    let statements = match Parser::parse_sql(dialect, sql) {
        Ok(statements) if statements.is_empty() => {
            return SqlparserOutcome::Rejected {
                error: "parser returned no statements".to_owned(),
            };
        }
        Ok(statements) => statements,
        Err(error) => {
            return SqlparserOutcome::Rejected {
                error: error.to_string(),
            };
        }
    };
    let normalized_sql = statements
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("; ");
    let debug_fingerprint = fingerprint_ast_debug(&statements);
    let round_trip = match Parser::parse_sql(dialect, &normalized_sql) {
        Ok(reparsed) if reparsed == statements => AstRoundTrip::Equal,
        Ok(reparsed) => AstRoundTrip::Changed {
            original_ast: format!("{statements:#?}"),
            reparsed_ast: format!("{reparsed:#?}"),
        },
        Err(error) => AstRoundTrip::Rejected {
            error: error.to_string(),
        },
    };
    SqlparserOutcome::Accepted {
        statement_count: statements.len(),
        normalized_sql,
        debug_fingerprint,
        round_trip,
    }
}

fn parser_results_equal(left: &dyn Dialect, right: &dyn Dialect, sql: &str) -> bool {
    match (Parser::parse_sql(left, sql), Parser::parse_sql(right, sql)) {
        (Ok(left), Ok(right)) => left == right,
        (Err(left), Err(right)) => left.to_string() == right.to_string(),
        _ => false,
    }
}

fn fingerprint_ast_debug(statements: &[sqlparser::ast::Statement]) -> String {
    // This compact digest is diagnostic only. Structural comparisons above use
    // Statement::eq directly; Debug includes source spans and is not canonical.
    let mut hash = 0xcbf29ce484222325_u64;
    for byte in format!("{statements:#?}").bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    format!("fnv1a64:{hash:016x}")
}

fn compare_acceptance(
    mysql: &MySqlSyntaxOutcome,
    sqlparser: &SqlparserOutcome,
) -> AcceptanceComparison {
    match (mysql, sqlparser) {
        (MySqlSyntaxOutcome::Accepted { .. }, SqlparserOutcome::Accepted { .. }) => {
            AcceptanceComparison::BothAccept
        }
        (MySqlSyntaxOutcome::Accepted { .. }, SqlparserOutcome::Rejected { .. }) => {
            AcceptanceComparison::MySqlOnly
        }
        (MySqlSyntaxOutcome::Rejected { .. }, SqlparserOutcome::Accepted { .. }) => {
            AcceptanceComparison::SqlparserOnly
        }
        (MySqlSyntaxOutcome::Rejected { .. }, SqlparserOutcome::Rejected { .. }) => {
            AcceptanceComparison::BothReject
        }
    }
}

fn update_summary(
    summary: &mut ParserSummary,
    acceptance: AcceptanceComparison,
    sqlparser: &SqlparserOutcome,
) {
    match acceptance {
        AcceptanceComparison::BothAccept => summary.both_accept += 1,
        AcceptanceComparison::MySqlOnly => summary.mysql_only += 1,
        AcceptanceComparison::SqlparserOnly => summary.sqlparser_only += 1,
        AcceptanceComparison::BothReject => summary.both_reject += 1,
    }
    if matches!(
        sqlparser,
        SqlparserOutcome::Accepted {
            round_trip: AstRoundTrip::Changed { .. } | AstRoundTrip::Rejected { .. },
            ..
        }
    ) {
        summary.changed_round_trips += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::case::{
        IsolationLevel, ModeProbe, SessionSpec, SqlModeFlag, Step, TimeZone, CASE_FORMAT_VERSION,
    };
    use crate::observe::{
        SessionState, SqlState, TransactionState, WarningSet, OBSERVATION_FORMAT_VERSION,
    };

    fn case(sql_mode: SqlMode, sql: &str) -> Case {
        Case {
            version: CASE_FORMAT_VERSION,
            id: "parser".to_owned(),
            tags: vec![],
            sessions: vec![SessionSpec {
                id: "s1".to_owned(),
                sql_mode,
                time_zone: TimeZone::Utc,
                isolation: IsolationLevel::RepeatableRead,
                autocommit: true,
            }],
            parallel_assertions: vec![],
            steps: vec![Step {
                id: "q1".to_owned(),
                session_id: "s1".to_owned(),
                probe: None,
                sql: sql.to_owned(),
                params: None,
                parallel: None,
                schedule_dependent: None,
            }],
        }
    }

    fn observation_for(step_id: &str, session_id: &str, error: Option<MySqlError>) -> Observation {
        Observation {
            version: OBSERVATION_FORMAT_VERSION,
            step_id: step_id.to_owned(),
            session_id: session_id.to_owned(),
            result: None,
            affected_rows: 0,
            last_insert_id: 0,
            warnings: WarningSet::default(),
            error,
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

    fn observation(error: Option<MySqlError>) -> Observation {
        observation_for("q1", "s1", error)
    }

    #[test]
    fn semantic_mysql_error_still_proves_syntax_acceptance() {
        let duplicate = MySqlError {
            number: 1062,
            sql_state: SqlState::new("23000").unwrap(),
            message: "duplicate".to_owned(),
        };
        let report = build_parser_report(
            &case(SqlMode::default(), "SELECT 1"),
            &[observation(Some(duplicate))],
        )
        .unwrap();
        assert_eq!(report.summary.both_accept, 1);
        assert_eq!(report.steps[0].acceptance, AcceptanceComparison::BothAccept);
    }

    #[test]
    fn syntax_error_is_compared_with_sqlparser_rejection() {
        let syntax = MySqlError {
            number: 1064,
            sql_state: SqlState::new("42000").unwrap(),
            message: "syntax".to_owned(),
        };
        let report = build_parser_report(
            &case(SqlMode::default(), "SELECT FROM"),
            &[observation(Some(syntax))],
        )
        .unwrap();
        assert_eq!(report.summary.both_reject, 1);
    }

    #[test]
    fn session_dialect_changes_ansi_quotes_parse() {
        let sql_mode = SqlMode::new(vec![SqlModeFlag::AnsiQuotes]).unwrap();
        let report =
            build_parser_report(&case(sql_mode, "SELECT \"name\""), &[observation(None)]).unwrap();
        assert_eq!(report.summary.steps_changed_by_session_dialect, 1);
        assert!(report.steps[0].changed_by_session_dialect);
    }

    #[test]
    fn mode_probe_rejects_state_changing_statements() {
        assert!(!is_nonlocking_query(
            &SessionMySqlDialect::default(),
            "INSERT INTO t VALUES (1)"
        ));
        assert!(!is_nonlocking_query(
            &SessionMySqlDialect::default(),
            "SELECT * FROM t FOR UPDATE"
        ));
    }

    #[test]
    fn observed_session_mode_is_used_by_the_next_step() {
        let ansi_mode = SqlMode::new(vec![SqlModeFlag::AnsiQuotes]).unwrap();
        let mut case = case(SqlMode::default(), "SET SESSION sql_mode = 'ANSI_QUOTES'");
        case.steps.push(Step {
            id: "q2".to_owned(),
            session_id: "s1".to_owned(),
            probe: None,
            sql: "SELECT \"name\"".to_owned(),
            params: None,
            parallel: None,
            schedule_dependent: None,
        });
        let mut set_observation = observation(None);
        set_observation.session_state.sql_mode = ansi_mode.clone();
        let mut select_observation = observation_for("q2", "s1", None);
        select_observation.session_state.sql_mode = ansi_mode.clone();

        let report = build_parser_report(&case, &[set_observation, select_observation]).unwrap();

        assert_eq!(report.steps[0].sql_mode_before, SqlMode::default());
        assert_eq!(report.steps[0].sql_mode_after, ansi_mode);
        assert!(report.steps[1].changed_by_session_dialect);
    }

    #[test]
    fn detects_static_ast_collision_resolved_by_session_dialect() {
        let default = SessionSpec {
            id: "default".to_owned(),
            sql_mode: SqlMode::default(),
            time_zone: TimeZone::Utc,
            isolation: IsolationLevel::RepeatableRead,
            autocommit: true,
        };
        let no_escapes = SessionSpec {
            id: "no_escapes".to_owned(),
            sql_mode: SqlMode::new(vec![SqlModeFlag::NoBackslashEscapes]).unwrap(),
            ..default.clone()
        };
        let sql = r"SELECT 'it\'s'";
        let case = Case {
            version: CASE_FORMAT_VERSION,
            id: "mode-collision".to_owned(),
            tags: vec![],
            sessions: vec![default, no_escapes],
            parallel_assertions: vec![],
            steps: vec![
                Step {
                    id: "default".to_owned(),
                    session_id: "default".to_owned(),
                    probe: Some(ModeProbe {
                        group: "escaped-quote".to_owned(),
                        variant: "default".to_owned(),
                    }),
                    sql: sql.to_owned(),
                    params: None,
                    parallel: None,
                    schedule_dependent: None,
                },
                Step {
                    id: "no-escapes".to_owned(),
                    session_id: "no_escapes".to_owned(),
                    probe: Some(ModeProbe {
                        group: "escaped-quote".to_owned(),
                        variant: "no-escapes".to_owned(),
                    }),
                    sql: sql.to_owned(),
                    params: None,
                    parallel: None,
                    schedule_dependent: None,
                },
            ],
        };
        let syntax = MySqlError {
            number: 1064,
            sql_state: SqlState::new("42000").unwrap(),
            message: "syntax".to_owned(),
        };
        let reference = [
            observation_for("default", "default", None),
            observation_for("no-escapes", "no_escapes", Some(syntax)),
        ];

        let report = build_parser_report(&case, &reference).unwrap();

        assert_eq!(report.summary.mode_semantic_collisions, 1);
        assert_eq!(
            report.summary.collisions_distinguished_by_session_dialect,
            1
        );
        assert!(report.mode_semantic_collisions[0].session_dialect_distinguishes);
        assert_eq!(
            report.mode_semantic_collisions[0].semantic_differences[0].path,
            "$.error"
        );
    }
}
