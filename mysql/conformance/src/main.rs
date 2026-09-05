pub mod case;
pub mod compare;
pub mod observe;
pub mod parser_probe;
pub mod session_dialect;

use std::collections::HashMap;
use std::env;
use std::fs;
use std::net::{IpAddr, SocketAddr};
use std::path::{Path, PathBuf};
use std::process::Command as ProcessCommand;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use clap::{Parser, Subcommand};
use futures::{future::try_join_all, TryStreamExt};
use mysql_async::consts::{
    ColumnFlags as DriverColumnFlags, ColumnType as DriverColumnType, StatusFlags,
};
use mysql_async::prelude::{Protocol, Queryable};
use mysql_async::{Column, Conn, Error as DriverError, Opts, Params, QueryResult, Row, Value};
use serde::Serialize;
use tokio::sync::Barrier;

use crate::case::{
    Case, IsolationLevel, LifecycleCase, SessionSpec, SqlMode, SqlModeFlag, Step, TimeZone,
    TypedValue,
};
use crate::compare::{compare_case_runs, compare_runs};
use crate::observe::{
    CharacterSet, Collation, ColumnFlag, ColumnMetadata, MySqlError, MySqlType, Observation,
    ResultSet, SessionState, SqlState, TransactionState, WarningDetail, WarningLevel, WarningSet,
    OBSERVATION_FORMAT_VERSION,
};
use crate::parser_probe::build_parser_report;

const ORACLE_DSN_ENV: &str = "MYSQL_ORACLE_DSN";
const TURSO_DSN_ENV: &str = "TURSO_DSN";
const ORACLE_COMPOSE_FILE_ENV: &str = "MYSQL_CONFORMANCE_COMPOSE_FILE";
const REFERENCE_SERVER_VERSION_PREFIX: &str = "8.4.11";
const INITIAL_TURSO_CASE_ID: &str = "p0.transaction.observer";
const INITIAL_TURSO_PROFILE_NAME: &str = "transaction-observer-wire-v1";
const TURSO_PROFILE_NOT_MEASURED: &[&str] = &[
    "session_state.current_database",
    "session_state.sql_mode",
    "session_state.time_zone",
    "session_state.isolation",
    "warnings.details",
    "result.columns.collation",
];
const TURSO_PROFILE_NOT_COMPARED: &[&str] = &["error.message"];

#[derive(Debug, Parser)]
#[command(about = "Record and verify observable MySQL 8.4 behavior")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Run a case and write its observations to an explicit path.
    Record {
        #[arg(long)]
        case: PathBuf,
        #[arg(long)]
        output: PathBuf,
    },
    /// Run a case and compare its observations with a recorded golden.
    Verify {
        #[arg(long)]
        case: PathBuf,
        #[arg(long)]
        golden: PathBuf,
    },
    /// Compare the bounded transaction observer profile with a Turso endpoint.
    CompareTurso {
        #[arg(long)]
        case: PathBuf,
        #[arg(long)]
        golden: PathBuf,
        /// Acknowledge that the DSN database is disposable for this run.
        #[arg(long = "acknowledge-disposable-db", value_name = "DATABASE")]
        acknowledge_disposable_db: String,
    },
    /// Record a case that restarts the pinned oracle between two step lists.
    RecordLifecycle {
        #[arg(long)]
        case: PathBuf,
        #[arg(long)]
        output: PathBuf,
    },
    /// Verify a case that restarts the pinned oracle between two step lists.
    VerifyLifecycle {
        #[arg(long)]
        case: PathBuf,
        #[arg(long)]
        golden: PathBuf,
    },
    /// Compare MySQL syntax acceptance with the pinned bootstrap parser.
    ParserReport {
        #[arg(long)]
        case: PathBuf,
        #[arg(long)]
        golden: PathBuf,
        #[arg(long)]
        output: PathBuf,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Command::Record { case, output } => {
            let dsn = oracle_dsn()?;
            let observations = run_case(&case, &dsn).await?;
            write_json(&output, &observations)?;
            println!(
                "recorded {} steps in {}",
                observations.len(),
                output.display()
            );
        }
        Command::Verify { case, golden } => {
            let dsn = oracle_dsn()?;
            let case_definition = read_case(&case)?;
            let expected = read_observations(&golden)?;
            let actual = run_case_definition(&case_definition, &dsn).await?;
            let report = compare_case_runs(&case_definition, &expected, &actual)?;
            if !report.equal {
                eprintln!("{}", serde_json::to_string_pretty(&report)?);
                bail!("MySQL observations differ from {}", golden.display());
            }
            println!(
                "verified {} steps against {}",
                actual.len(),
                golden.display()
            );
        }
        Command::CompareTurso {
            case,
            golden,
            acknowledge_disposable_db,
        } => {
            let dsn = turso_dsn()?;
            let case_definition = read_case(&case)?;
            let expected = read_observations(&golden)?;
            let report = compare_turso_case(
                &case_definition,
                &expected,
                &dsn,
                &acknowledge_disposable_db,
                &case,
                &golden,
            )
            .await?;
            println!("{}", serde_json::to_string_pretty(&report)?);
            match report.status {
                TursoComparisonStatus::ScopedPass => {}
                TursoComparisonStatus::Fail => {
                    bail!("Turso observations differ within the bounded profile")
                }
                TursoComparisonStatus::Inconclusive => {
                    bail!("Turso comparison is inconclusive within the bounded profile")
                }
            }
        }
        Command::RecordLifecycle { case, output } => {
            let dsn = oracle_dsn()?;
            let case = read_lifecycle_case(&case)?;
            let observations = run_lifecycle_case(&case, &dsn).await?;
            write_json(&output, &observations)?;
            println!(
                "recorded lifecycle case `{}` in {}",
                case.id,
                output.display()
            );
        }
        Command::VerifyLifecycle { case, golden } => {
            let dsn = oracle_dsn()?;
            let case = read_lifecycle_case(&case)?;
            let expected: LifecycleObservations = read_json(&golden)?;
            let actual = run_lifecycle_case(&case, &dsn).await?;
            let before = compare_runs(&expected.before_restart, &actual.before_restart)?;
            let after = compare_runs(&expected.after_restart, &actual.after_restart)?;
            if !before.equal || !after.equal {
                eprintln!(
                    "{}",
                    serde_json::to_string_pretty(&LifecycleComparison { before, after })?
                );
                bail!(
                    "MySQL lifecycle observations differ from {}",
                    golden.display()
                );
            }
            println!("verified lifecycle case `{}`", case.id);
        }
        Command::ParserReport {
            case,
            golden,
            output,
        } => {
            let case = read_case(&case)?;
            let reference = read_observations(&golden)?;
            let report = build_parser_report(&case, &reference)?;
            write_json(&output, &report)?;
            println!(
                "analyzed {} steps in {}",
                report.steps.len(),
                output.display()
            );
        }
    }

    Ok(())
}

fn oracle_dsn() -> Result<String> {
    env::var(ORACLE_DSN_ENV)
        .map_err(|_| anyhow!("{ORACLE_DSN_ENV} must contain the reference MySQL DSN"))
}

fn turso_dsn() -> Result<String> {
    env::var(TURSO_DSN_ENV).map_err(|_| anyhow!("{TURSO_DSN_ENV} must contain the Turso MySQL DSN"))
}

async fn run_case(path: &Path, dsn: &str) -> Result<Vec<Observation>> {
    let case = read_case(path)?;
    run_case_definition(&case, dsn).await
}

async fn run_case_definition(case: &Case, dsn: &str) -> Result<Vec<Observation>> {
    let collations = load_collations(dsn).await?;
    let mut sessions = connect_sessions(case, dsn).await?;
    let mut observations = Vec::with_capacity(case.steps.len());

    let mut index = 0;
    while index < case.steps.len() {
        let step = &case.steps[index];
        if let Some(parallel) = &step.parallel {
            let mut group_end = index + 1;
            while case
                .steps
                .get(group_end)
                .and_then(|candidate| candidate.parallel.as_ref())
                .map(|candidate| candidate.group.as_str())
                == Some(parallel.group.as_str())
            {
                group_end += 1;
            }
            let group = &case.steps[index..group_end];
            observations.extend(
                execute_parallel_group(&mut sessions, group, &collations)
                    .await
                    .with_context(|| format!("parallel group `{}` failed", parallel.group))?,
            );
            index = group_end;
            continue;
        }
        let conn = sessions
            .get_mut(&step.session_id)
            .ok_or_else(|| anyhow!("case validation missed session `{}`", step.session_id))?;
        observations.push(
            execute_step(conn, step, &collations)
                .await
                .with_context(|| format!("step `{}` failed", step.id))?,
        );
        index += 1;
    }

    for (_, conn) in sessions {
        conn.disconnect().await?;
    }
    Ok(observations)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
enum TursoComparisonStatus {
    ScopedPass,
    Fail,
    Inconclusive,
}

#[derive(Debug, serde::Serialize)]
struct TursoComparisonReport {
    case_path: String,
    golden_path: String,
    profile: &'static str,
    status: TursoComparisonStatus,
    mismatches: Vec<String>,
    inconclusive_reasons: Vec<String>,
    measured: &'static [&'static str],
    not_measured: &'static [&'static str],
    not_compared: &'static [&'static str],
    observed: Vec<serde_json::Value>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
struct TursoStatus {
    autocommit: bool,
    transaction_active: bool,
}

#[derive(Debug, Clone, serde::Serialize)]
struct TursoObservation {
    step_id: String,
    session_id: String,
    result: Option<ResultSet>,
    affected_rows: u64,
    last_insert_id: u64,
    warning_count: Option<u16>,
    error: Option<MySqlError>,
    status: Option<TursoStatus>,
}

const INITIAL_TURSO_PROFILE: &[(&str, &str)] = &[
    ("disable_notes_for_cleanup", "SET SESSION sql_notes = 0"),
    ("drop_probe", "DROP TABLE IF EXISTS transaction_probe"),
    ("restore_notes_after_cleanup", "SET SESSION sql_notes = 1"),
    (
        "create_probe",
        "CREATE TABLE transaction_probe (id INT PRIMARY KEY) ENGINE=InnoDB",
    ),
    ("commit_before_reads", "COMMIT"),
    ("constant_read", "SELECT 1 AS value"),
    ("table_read", "SELECT id FROM transaction_probe"),
    ("rollback_read", "ROLLBACK"),
    ("cleanup_probe", "DROP TABLE transaction_probe"),
];

const TURSO_PROFILE_MEASURED: &[&str] = &["result.columns.database"];

fn validate_initial_turso_case(case: &Case, expected: &[Observation]) -> Result<()> {
    if case.id != INITIAL_TURSO_CASE_ID {
        bail!(
            "bounded Turso comparison only accepts case `{INITIAL_TURSO_CASE_ID}`, got `{}`",
            case.id
        );
    }
    if case.steps.len() != INITIAL_TURSO_PROFILE.len() {
        bail!(
            "case `{INITIAL_TURSO_CASE_ID}` must contain exactly {} steps",
            INITIAL_TURSO_PROFILE.len()
        );
    }
    if expected.len() != INITIAL_TURSO_PROFILE.len() {
        bail!(
            "golden for `{INITIAL_TURSO_CASE_ID}` must contain exactly {} observations",
            INITIAL_TURSO_PROFILE.len()
        );
    }
    let [session] = case.sessions.as_slice() else {
        bail!("case `{INITIAL_TURSO_CASE_ID}` must contain exactly one session");
    };
    if session.id != "autocommit_off"
        || session.sql_mode != SqlMode::default()
        || session.time_zone != TimeZone::Utc
        || session.isolation != IsolationLevel::RepeatableRead
        || session.autocommit
    {
        bail!("case `{INITIAL_TURSO_CASE_ID}` does not match the bounded fixed session profile");
    }

    for (index, ((step_id, sql), step)) in INITIAL_TURSO_PROFILE.iter().zip(&case.steps).enumerate()
    {
        if step.id != *step_id
            || step.sql != *sql
            || step.session_id != session.id
            || step.params.is_some()
            || step.parallel.is_some()
            || step.probe.is_some()
            || step.schedule_dependent.is_some()
        {
            bail!("step {index} does not match the bounded profile for `{INITIAL_TURSO_CASE_ID}`");
        }
        if expected[index].step_id != *step_id || expected[index].session_id != session.id {
            bail!(
                "golden observation {index} does not match the bounded profile for `{INITIAL_TURSO_CASE_ID}`"
            );
        }
        if expected[index].error.is_some() {
            bail!(
                "golden observation {index} for `{INITIAL_TURSO_CASE_ID}` must not expect an error"
            );
        }
    }
    Ok(())
}

async fn compare_turso_case(
    case: &Case,
    expected: &[Observation],
    dsn: &str,
    acknowledge_disposable_db: &str,
    case_path: &Path,
    golden_path: &Path,
) -> Result<TursoComparisonReport> {
    validate_initial_turso_case(case, expected)?;
    preflight_turso_disposable_database(dsn, acknowledge_disposable_db).await?;
    let actual = run_turso_case_definition(case, dsn).await?;
    let observed = actual
        .iter()
        .map(turso_report_observation)
        .collect::<Result<Vec<_>>>()?;
    let mut mismatches = Vec::new();
    let mut inconclusive_reasons = Vec::new();

    if actual.len() != expected.len() {
        mismatches.push(format!(
            "step count differs: expected {}, actual {}",
            expected.len(),
            actual.len()
        ));
    }
    for (index, (expected, actual)) in expected.iter().zip(&actual).enumerate() {
        compare_turso_observation(
            index,
            expected,
            actual,
            &mut mismatches,
            &mut inconclusive_reasons,
        );
    }

    let status = turso_comparison_status(&mismatches, &inconclusive_reasons);
    Ok(TursoComparisonReport {
        case_path: case_path.display().to_string(),
        golden_path: golden_path.display().to_string(),
        profile: INITIAL_TURSO_PROFILE_NAME,
        status,
        mismatches,
        inconclusive_reasons,
        measured: TURSO_PROFILE_MEASURED,
        not_measured: TURSO_PROFILE_NOT_MEASURED,
        not_compared: TURSO_PROFILE_NOT_COMPARED,
        observed,
    })
}

fn turso_report_observation(observation: &TursoObservation) -> Result<serde_json::Value> {
    let mut value = serde_json::to_value(observation)?;
    if let Some(columns) = value
        .pointer_mut("/result/columns")
        .and_then(serde_json::Value::as_array_mut)
    {
        for column in columns {
            if let Some(column) = column.as_object_mut() {
                column.remove("collation");
            }
        }
    }
    Ok(value)
}

fn turso_comparison_status(
    mismatches: &[String],
    inconclusive_reasons: &[String],
) -> TursoComparisonStatus {
    if !mismatches.is_empty() {
        TursoComparisonStatus::Fail
    } else if !inconclusive_reasons.is_empty() {
        TursoComparisonStatus::Inconclusive
    } else {
        TursoComparisonStatus::ScopedPass
    }
}

fn compare_turso_observation(
    index: usize,
    expected: &Observation,
    actual: &TursoObservation,
    mismatches: &mut Vec<String>,
    inconclusive_reasons: &mut Vec<String>,
) {
    let path = format!("steps[{index}]");
    if actual.step_id != expected.step_id {
        mismatches.push(format!(
            "{path}.step_id differs: expected `{}`, actual `{}`",
            expected.step_id, actual.step_id
        ));
    }
    if actual.session_id != expected.session_id {
        mismatches.push(format!(
            "{path}.session_id differs: expected `{}`, actual `{}`",
            expected.session_id, actual.session_id
        ));
    }

    match (&expected.error, &actual.error) {
        (Some(expected), Some(actual)) => {
            if expected.number != actual.number {
                mismatches.push(format!(
                    "{path}.error.number differs: expected {}, actual {}",
                    expected.number, actual.number
                ));
            }
            if expected.sql_state != actual.sql_state {
                mismatches.push(format!(
                    "{path}.error.sql_state differs: expected {}, actual {}",
                    expected.sql_state.as_str(),
                    actual.sql_state.as_str()
                ));
            }
            return;
        }
        (None, Some(actual)) => {
            mismatches.push(format!(
                "{path}.error differs: Turso returned {} / {}",
                actual.number,
                actual.sql_state.as_str()
            ));
            return;
        }
        (Some(expected), None) => {
            mismatches.push(format!(
                "{path}.error differs: expected {} / {}, but Turso returned no error",
                expected.number,
                expected.sql_state.as_str()
            ));
            return;
        }
        (None, None) => {}
    }

    compare_turso_result(
        &path,
        expected.result.as_ref(),
        actual.result.as_ref(),
        mismatches,
    );
    if expected.affected_rows != actual.affected_rows {
        mismatches.push(format!(
            "{path}.affected_rows differs: expected {}, actual {}",
            expected.affected_rows, actual.affected_rows
        ));
    }
    if expected.last_insert_id != actual.last_insert_id {
        mismatches.push(format!(
            "{path}.last_insert_id differs: expected {}, actual {}",
            expected.last_insert_id, actual.last_insert_id
        ));
    }

    match actual.warning_count {
        Some(actual_count) => {
            if expected.warnings.warning_count != u32::from(actual_count) {
                mismatches.push(format!(
                    "{path}.warnings.warning_count differs: expected {}, actual {}",
                    expected.warnings.warning_count, actual_count
                ));
            }
            if actual_count != 0 || !expected.warnings.details.is_empty() {
                inconclusive_reasons.push(format!(
                    "{path}.warnings.details are not available from the bounded Turso profile"
                ));
            }
        }
        None => inconclusive_reasons.push(format!(
            "{path}.warnings.warning_count was not available from a Turso OK packet"
        )),
    }

    match actual.status {
        Some(status) => {
            if expected.session_state.autocommit != status.autocommit {
                mismatches.push(format!(
                    "{path}.session_state.autocommit differs: expected {}, actual {}",
                    expected.session_state.autocommit, status.autocommit
                ));
            }
            let expected_transaction_active =
                expected.session_state.transaction == TransactionState::Active;
            if expected_transaction_active != status.transaction_active {
                mismatches.push(format!(
                    "{path}.session_state.transaction differs: expected {}, actual {}",
                    expected_transaction_active, status.transaction_active
                ));
            }
        }
        None => inconclusive_reasons.push(format!(
            "{path}.session_state.autocommit and transaction were not available from a Turso OK packet"
        )),
    }
}

fn compare_turso_result(
    path: &str,
    expected: Option<&ResultSet>,
    actual: Option<&ResultSet>,
    mismatches: &mut Vec<String>,
) {
    match (expected, actual) {
        (None, None) => {}
        (None, Some(_)) => mismatches.push(format!(
            "{path}.result differs: expected no result set, but Turso returned one"
        )),
        (Some(_), None) => mismatches.push(format!(
            "{path}.result differs: expected a result set, but Turso returned none"
        )),
        (Some(expected), Some(actual)) => {
            if expected.columns.len() != actual.columns.len() {
                mismatches.push(format!(
                    "{path}.result.columns length differs: expected {}, actual {}",
                    expected.columns.len(),
                    actual.columns.len()
                ));
            }
            for (column_index, (expected, actual)) in
                expected.columns.iter().zip(&actual.columns).enumerate()
            {
                let column_path = format!("{path}.result.columns[{column_index}]");
                compare_turso_column(&column_path, expected, actual, mismatches);
            }
            if expected.rows != actual.rows {
                mismatches.push(format!(
                    "{path}.result.rows differs: expected {:?}, actual {:?}",
                    expected.rows, actual.rows
                ));
            }
        }
    }
}

fn compare_turso_column(
    path: &str,
    expected: &ColumnMetadata,
    actual: &ColumnMetadata,
    mismatches: &mut Vec<String>,
) {
    macro_rules! compare_column_field {
        ($field:ident) => {
            if expected.$field != actual.$field {
                mismatches.push(format!(
                    "{path}.{} differs: expected {:?}, actual {:?}",
                    stringify!($field),
                    expected.$field,
                    actual.$field
                ));
            }
        };
    }

    compare_column_field!(name);
    compare_column_field!(original_name);
    compare_column_field!(table);
    compare_column_field!(original_table);
    compare_column_field!(database);
    compare_column_field!(catalog);
    compare_column_field!(column_type);
    compare_column_field!(character_set_id);
    compare_column_field!(character_set);
    compare_column_field!(column_length);
    compare_column_field!(decimals);
    compare_column_field!(nullable);
    compare_column_field!(flags);
}

async fn run_turso_case_definition(case: &Case, dsn: &str) -> Result<Vec<TursoObservation>> {
    let mut sessions = HashMap::with_capacity(case.sessions.len());
    for session in &case.sessions {
        let mut conn = connect_turso(dsn).await?;
        configure_turso_session(&mut conn, session).await?;
        sessions.insert(session.id.clone(), conn);
    }
    let collations = initial_turso_collations();
    let mut observations = Vec::with_capacity(case.steps.len());
    for step in &case.steps {
        if step.parallel.is_some() {
            bail!("bounded Turso comparison does not support parallel steps");
        }
        let conn = sessions
            .get_mut(&step.session_id)
            .ok_or_else(|| anyhow!("case validation missed session `{}`", step.session_id))?;
        observations.push(
            execute_turso_step(conn, step, &collations)
                .await
                .with_context(|| format!("step `{}` failed", step.id))?,
        );
    }
    for (_, conn) in sessions {
        conn.disconnect().await?;
    }
    Ok(observations)
}

fn initial_turso_collations() -> HashMap<u16, CollationDefinition> {
    HashMap::from([(
        63,
        CollationDefinition {
            character_set: CharacterSet::Binary,
            collation: Collation::Binary,
        },
    )])
}

async fn connect_turso(dsn: &str) -> Result<Conn> {
    let options = parse_turso_options(dsn)?;
    connect_turso_options(options).await
}

fn parse_turso_options(dsn: &str) -> Result<Opts> {
    let options =
        Opts::from_url(dsn).map_err(|_| anyhow!("{TURSO_DSN_ENV} is not a valid MySQL DSN"))?;
    validate_turso_endpoint(&options)?;
    Ok(options)
}

async fn connect_turso_options(options: Opts) -> Result<Conn> {
    let options = mysql_async::OptsBuilder::from_opts(options).prefer_socket(false);
    Conn::new(options)
        .await
        .map_err(|_| anyhow!("failed to connect to the Turso MySQL server"))
}

fn validate_disposable_database(options: &Opts, acknowledged_database: &str) -> Result<()> {
    if acknowledged_database.is_empty() {
        bail!("--acknowledge-disposable-db must not be empty");
    }
    match options.db_name() {
        Some(database) if database == acknowledged_database => Ok(()),
        Some(database) => bail!(
            "--acknowledge-disposable-db must exactly match the {TURSO_DSN_ENV} database `{database}`"
        ),
        None => bail!(
            "{TURSO_DSN_ENV} must include a database name matching --acknowledge-disposable-db"
        ),
    }
}

async fn preflight_turso_disposable_database(dsn: &str, acknowledged_database: &str) -> Result<()> {
    let options = parse_turso_options(dsn)?;
    validate_disposable_database(&options, acknowledged_database)?;
    let mut conn = connect_turso_options(options).await?;
    let table_names: Vec<String> = conn
        .query("SHOW TABLES")
        .await
        .map_err(|_| anyhow!("failed to inspect the disposable Turso database with SHOW TABLES"))?;
    conn.disconnect().await.map_err(|_| {
        anyhow!("failed to close the Turso disposable-database preflight connection")
    })?;
    if table_names
        .iter()
        .any(|table| table.eq_ignore_ascii_case("transaction_probe"))
    {
        bail!(
            "the disposable Turso database already contains transaction_probe; refusing to mutate it"
        );
    }
    Ok(())
}

fn validate_turso_endpoint(options: &Opts) -> Result<()> {
    if options.socket().is_some_and(|socket| !socket.is_empty())
        || is_numeric_loopback(options.ip_or_hostname())
    {
        return Ok(());
    }
    bail!("{TURSO_DSN_ENV} must use an explicit Unix socket or a numeric loopback TCP address")
}

async fn configure_turso_session(conn: &mut Conn, session: &SessionSpec) -> Result<()> {
    if session.sql_mode != SqlMode::default()
        || session.time_zone != TimeZone::Utc
        || session.isolation != IsolationLevel::RepeatableRead
    {
        bail!(
            "bounded Turso comparison only supports the default SQL mode, UTC, and REPEATABLE READ profile"
        );
    }
    conn.query_drop(if session.autocommit {
        "SET SESSION autocommit = 1"
    } else {
        "SET SESSION autocommit = 0"
    })
    .await?;
    Ok(())
}

async fn execute_turso_step(
    conn: &mut Conn,
    step: &Step,
    collations: &HashMap<u16, CollationDefinition>,
) -> Result<TursoObservation> {
    let outcome = match &step.params {
        Some(params) => {
            let params = Params::Positional(
                params
                    .iter()
                    .map(parameter_value)
                    .collect::<Result<Vec<_>>>()?,
            );
            match conn.exec_iter(&step.sql, params).await {
                Ok(result) => consume_result(result, collations).await?,
                Err(error) => StatementOutcome::from_error(error)?,
            }
        }
        None => match conn.query_iter(&step.sql).await {
            Ok(result) => consume_result(result, collations).await?,
            Err(error) => StatementOutcome::from_error(error)?,
        },
    };
    // `ResultSetStream::ok_packet()` is captured before rows and the result
    // terminator are consumed. Read the connection's public latest OK packet
    // after `consume_result` returns so result status is not stale.
    let status = if outcome.error.is_none() {
        conn.last_ok_packet()
            .map(|packet| packet.status_flags())
            .map(turso_status)
    } else {
        None
    };
    let warning_count = status.as_ref().map(|_| outcome.warning_count);
    Ok(TursoObservation {
        step_id: step.id.clone(),
        session_id: step.session_id.clone(),
        result: outcome.result,
        affected_rows: outcome.affected_rows,
        last_insert_id: outcome.last_insert_id,
        warning_count,
        error: outcome.error,
        status,
    })
}

fn turso_status(flags: StatusFlags) -> TursoStatus {
    TursoStatus {
        autocommit: flags.contains(StatusFlags::SERVER_STATUS_AUTOCOMMIT),
        transaction_active: flags.contains(StatusFlags::SERVER_STATUS_IN_TRANS),
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, serde::Deserialize)]
struct LifecycleObservations {
    before_restart: Vec<Observation>,
    after_restart: Vec<Observation>,
}

#[derive(Serialize)]
struct LifecycleComparison {
    before: crate::compare::ComparisonReport,
    after: crate::compare::ComparisonReport,
}

async fn run_lifecycle_case(case: &LifecycleCase, dsn: &str) -> Result<LifecycleObservations> {
    let before = Case {
        version: case.version,
        id: format!("{}.before_restart", case.id),
        tags: case.tags.clone(),
        sessions: case.sessions.clone(),
        steps: case.before_restart.clone(),
        parallel_assertions: Vec::new(),
    };
    let after = Case {
        version: case.version,
        id: format!("{}.after_restart", case.id),
        tags: case.tags.clone(),
        sessions: case.sessions.clone(),
        steps: case.after_restart.clone(),
        parallel_assertions: Vec::new(),
    };
    let compose_file = oracle_compose_file()?;
    let before_service = oracle_service(&compose_file, dsn)?;
    let before_restart = run_case_definition(&before, dsn).await?;
    restart_oracle(&compose_file)?;
    wait_for_oracle(dsn).await?;
    let after_service = oracle_service(&compose_file, dsn)?;
    verify_restart(&before_service, &after_service)?;
    let after_restart = run_case_definition(&after, dsn).await?;
    Ok(LifecycleObservations {
        before_restart,
        after_restart,
    })
}

fn oracle_compose_file() -> Result<String> {
    env::var(ORACLE_COMPOSE_FILE_ENV)
        .map_err(|_| anyhow!("{ORACLE_COMPOSE_FILE_ENV} must name the oracle Compose file"))
}

fn restart_oracle(compose_file: &str) -> Result<()> {
    let status = ProcessCommand::new("docker")
        .args(["compose", "-f", compose_file, "restart", "mysql"])
        .status()
        .context("failed to restart the oracle Compose service")?;
    if !status.success() {
        bail!("oracle Compose restart failed with {status}");
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct OracleService {
    endpoint: SocketAddr,
    container_id: String,
    started_at: String,
    data_mount: DataMount,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct DataMount {
    name: Option<String>,
    source: String,
    destination: String,
}

fn oracle_service(compose_file: &str, dsn: &str) -> Result<OracleService> {
    let endpoint = compose_endpoint(compose_file)?;
    let options = mysql_async::Opts::from_url(dsn)
        .map_err(|_| anyhow!("{ORACLE_DSN_ENV} is not a valid MySQL DSN"))?;
    let dsn_host = options.ip_or_hostname();
    if !is_numeric_loopback(dsn_host)
        || endpoint.ip().is_unspecified()
        || !endpoint.ip().is_loopback()
        || dsn_host != endpoint.ip().to_string()
        || options.tcp_port() != endpoint.port()
    {
        bail!("{ORACLE_DSN_ENV} must point at the Compose mysql service endpoint {endpoint}");
    }
    let container_id = compose_service_id(compose_file)?;
    let inspect = docker_output(["inspect", container_id.as_str()])?;
    let value: serde_json::Value = serde_json::from_str(&inspect)
        .context("docker inspect returned invalid JSON for the oracle service")?;
    let container = value
        .as_array()
        .and_then(|items| items.first())
        .ok_or_else(|| anyhow!("docker inspect returned no oracle service container"))?;
    let inspected_id = container
        .get("Id")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| anyhow!("docker inspect omitted the oracle container ID"))?;
    if inspected_id != container_id {
        bail!("docker inspect returned a different oracle service container");
    }
    let started_at = container
        .pointer("/State/StartedAt")
        .and_then(serde_json::Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| anyhow!("docker inspect omitted the oracle service start time"))?;
    let mounts = container
        .get("Mounts")
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| anyhow!("docker inspect omitted oracle service mounts"))?;
    let data_mounts = mounts
        .iter()
        .filter(|mount| {
            mount.get("Destination").and_then(serde_json::Value::as_str) == Some("/var/lib/mysql")
        })
        .collect::<Vec<_>>();
    let [data_mount] = data_mounts.as_slice() else {
        bail!("oracle service must have exactly one persistent /var/lib/mysql mount");
    };
    let source = data_mount
        .get("Source")
        .and_then(serde_json::Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| anyhow!("oracle data mount has no source"))?;
    Ok(OracleService {
        endpoint,
        container_id,
        started_at: started_at.to_owned(),
        data_mount: DataMount {
            name: data_mount
                .get("Name")
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned),
            source: source.to_owned(),
            destination: "/var/lib/mysql".to_owned(),
        },
    })
}

fn compose_endpoint(compose_file: &str) -> Result<SocketAddr> {
    let endpoint = docker_output(["compose", "-f", compose_file, "port", "mysql", "3306"])?;
    endpoint
        .trim()
        .parse::<SocketAddr>()
        .context("oracle Compose service did not publish a numeric socket endpoint")
}

fn compose_service_id(compose_file: &str) -> Result<String> {
    let id = docker_output(["compose", "-f", compose_file, "ps", "-q", "mysql"])?;
    let id = id.trim();
    if id.is_empty() || id.lines().count() != 1 {
        bail!("oracle Compose service must resolve to exactly one running container");
    }
    Ok(id.to_owned())
}

fn docker_output<const N: usize>(args: [&str; N]) -> Result<String> {
    let output = ProcessCommand::new("docker")
        .args(args)
        .output()
        .context("failed to inspect the oracle Compose service")?;
    if !output.status.success() {
        bail!("oracle Compose inspection failed with {}", output.status);
    }
    String::from_utf8(output.stdout).context("oracle Compose inspection returned non-UTF-8 output")
}

fn verify_restart(before: &OracleService, after: &OracleService) -> Result<()> {
    if before.endpoint != after.endpoint
        || before.container_id != after.container_id
        || before.started_at == after.started_at
        || before.data_mount != after.data_mount
    {
        bail!("oracle restart did not preserve the same service endpoint, container, and data mount with a new start time");
    }
    Ok(())
}

async fn wait_for_oracle(dsn: &str) -> Result<()> {
    let mut last_error = None;
    for _ in 0..60 {
        match connect(dsn).await {
            Ok(conn) => {
                conn.disconnect().await?;
                return Ok(());
            }
            Err(error) => last_error = Some(error),
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    Err(last_error.unwrap_or_else(|| anyhow!("oracle did not become ready after restart")))
}

async fn execute_parallel_group(
    sessions: &mut HashMap<String, Conn>,
    steps: &[Step],
    collations: &HashMap<u16, CollationDefinition>,
) -> Result<Vec<Observation>> {
    let barrier = Arc::new(Barrier::new(steps.len()));
    let mut tasks = Vec::with_capacity(steps.len());
    for step in steps {
        let session_id = step.session_id.clone();
        let mut conn = sessions
            .remove(&session_id)
            .ok_or_else(|| anyhow!("case validation missed session `{session_id}`"))?;
        let step = step.clone();
        let barrier = Arc::clone(&barrier);
        tasks.push(async move {
            barrier.wait().await;
            let observation = execute_step(&mut conn, &step, collations)
                .await
                .with_context(|| format!("step `{}` failed", step.id))?;
            Ok::<_, anyhow::Error>((session_id, conn, observation))
        });
    }

    let results = try_join_all(tasks).await?;
    let mut observations = Vec::with_capacity(results.len());
    for (session_id, conn, observation) in results {
        if sessions.insert(session_id.clone(), conn).is_some() {
            bail!("parallel group returned duplicate session `{session_id}`");
        }
        observations.push(observation);
    }
    Ok(observations)
}

fn read_case(path: &Path) -> Result<Case> {
    let bytes =
        fs::read(path).with_context(|| format!("failed to read case {}", path.display()))?;
    let case: Case = serde_json::from_slice(&bytes)
        .with_context(|| format!("failed to decode case {}", path.display()))?;
    case.validate()
        .with_context(|| format!("invalid case {}", path.display()))?;
    Ok(case)
}

fn read_lifecycle_case(path: &Path) -> Result<LifecycleCase> {
    let bytes =
        fs::read(path).with_context(|| format!("failed to read case {}", path.display()))?;
    let case: LifecycleCase = serde_json::from_slice(&bytes)
        .with_context(|| format!("failed to decode case {}", path.display()))?;
    case.validate()
        .with_context(|| format!("invalid lifecycle case {}", path.display()))?;
    Ok(case)
}

fn read_json<T>(path: &Path) -> Result<T>
where
    T: serde::de::DeserializeOwned,
{
    let bytes =
        fs::read(path).with_context(|| format!("failed to read golden {}", path.display()))?;
    serde_json::from_slice(&bytes)
        .with_context(|| format!("failed to decode golden {}", path.display()))
}

fn read_observations(path: &Path) -> Result<Vec<Observation>> {
    let bytes =
        fs::read(path).with_context(|| format!("failed to read golden {}", path.display()))?;
    let observations: Vec<Observation> = serde_json::from_slice(&bytes)
        .with_context(|| format!("failed to decode golden {}", path.display()))?;
    for observation in &observations {
        observation
            .validate()
            .with_context(|| format!("invalid golden {}", path.display()))?;
    }
    Ok(observations)
}

fn write_json<T>(path: &Path, value: &T) -> Result<()>
where
    T: Serialize + ?Sized,
{
    let mut json = serde_json::to_vec_pretty(value)?;
    json.push(b'\n');
    fs::write(path, json)
        .with_context(|| format!("failed to write observations to {}", path.display()))
}

#[derive(Debug, Clone)]
struct CollationDefinition {
    character_set: CharacterSet,
    collation: Collation,
}

async fn load_collations(dsn: &str) -> Result<HashMap<u16, CollationDefinition>> {
    let mut conn = connect(dsn).await?;
    let rows: Vec<(String, String, u16)> = conn
        .query(
            "SELECT CHARACTER_SET_NAME, COLLATION_NAME, ID \
             FROM information_schema.COLLATIONS",
        )
        .await?;
    conn.disconnect().await?;

    index_collations(rows)
}

fn index_collations(
    rows: impl IntoIterator<Item = (String, String, u16)>,
) -> Result<HashMap<u16, CollationDefinition>> {
    let mut collations = HashMap::new();
    for (character_set, collation, id) in rows {
        let definition = CollationDefinition {
            character_set: parse_character_set(character_set),
            collation: parse_collation(collation),
        };
        if collations.insert(id, definition).is_some() {
            bail!("information_schema.COLLATIONS repeats collation ID {id}");
        }
    }
    Ok(collations)
}

async fn connect_sessions(case: &Case, dsn: &str) -> Result<HashMap<String, Conn>> {
    let mut sessions = HashMap::with_capacity(case.sessions.len());
    for session in &case.sessions {
        let mut conn = connect(dsn).await?;
        configure_session(&mut conn, session).await?;
        sessions.insert(session.id.clone(), conn);
    }
    Ok(sessions)
}

async fn connect(dsn: &str) -> Result<Conn> {
    let options = mysql_async::Opts::from_url(dsn)
        .map_err(|_| anyhow!("{ORACLE_DSN_ENV} is not a valid MySQL DSN"))?;
    let host = options.ip_or_hostname();
    if !is_numeric_loopback(host) {
        bail!("{ORACLE_DSN_ENV} must use a numeric loopback address");
    }
    let options = mysql_async::OptsBuilder::from_opts(options).prefer_socket(false);
    let mut conn = Conn::new(options)
        .await
        .map_err(|error| anyhow!("failed to connect to the reference MySQL server: {error}"))?;
    let version: String = conn
        .query_first("SELECT VERSION()")
        .await?
        .ok_or_else(|| anyhow!("reference MySQL returned no server version"))?;
    if !version.starts_with(REFERENCE_SERVER_VERSION_PREFIX) {
        bail!(
            "reference server version `{version}` does not match pinned MySQL {REFERENCE_SERVER_VERSION_PREFIX}"
        );
    }
    Ok(conn)
}

fn is_numeric_loopback(host: &str) -> bool {
    host.parse::<IpAddr>()
        .is_ok_and(|address| address.is_loopback())
}

async fn configure_session(conn: &mut Conn, session: &SessionSpec) -> Result<()> {
    conn.exec_drop(
        "SET SESSION sql_mode = ?",
        (format_sql_mode(&session.sql_mode),),
    )
    .await?;
    conn.exec_drop(
        "SET SESSION time_zone = ?",
        (format_time_zone(&session.time_zone),),
    )
    .await?;
    conn.query_drop(format!(
        "SET SESSION TRANSACTION ISOLATION LEVEL {}",
        format_isolation(session.isolation)
    ))
    .await?;
    conn.query_drop(if session.autocommit {
        "SET SESSION autocommit = 1"
    } else {
        "SET SESSION autocommit = 0"
    })
    .await?;
    Ok(())
}

async fn execute_step(
    conn: &mut Conn,
    step: &Step,
    collations: &HashMap<u16, CollationDefinition>,
) -> Result<Observation> {
    let outcome = match &step.params {
        Some(params) => {
            let params = Params::Positional(
                params
                    .iter()
                    .map(parameter_value)
                    .collect::<Result<Vec<_>>>()?,
            );
            match conn.exec_iter(&step.sql, params).await {
                Ok(result) => consume_result(result, collations).await?,
                Err(error) => StatementOutcome::from_error(error)?,
            }
        }
        None => match conn.query_iter(&step.sql).await {
            Ok(result) => consume_result(result, collations).await?,
            Err(error) => StatementOutcome::from_error(error)?,
        },
    };

    let warnings = read_warnings(conn, outcome.warning_count).await?;
    let session_state = read_session_state(conn).await?;
    let observation = Observation {
        version: OBSERVATION_FORMAT_VERSION,
        step_id: step.id.clone(),
        session_id: step.session_id.clone(),
        result: outcome.result,
        affected_rows: outcome.affected_rows,
        last_insert_id: outcome.last_insert_id,
        warnings,
        error: outcome.error,
        session_state,
    };
    observation.validate()?;
    Ok(observation)
}

struct StatementOutcome {
    result: Option<ResultSet>,
    affected_rows: u64,
    last_insert_id: u64,
    warning_count: u16,
    error: Option<MySqlError>,
}

impl StatementOutcome {
    fn from_error(error: DriverError) -> Result<Self> {
        match error {
            DriverError::Server(error) => Ok(Self {
                result: None,
                affected_rows: 0,
                last_insert_id: 0,
                warning_count: 0,
                error: Some(MySqlError {
                    number: u32::from(error.code),
                    sql_state: SqlState::new(error.state)?,
                    message: error.message,
                }),
            }),
            error => Err(error.into()),
        }
    }
}

async fn consume_result<P>(
    mut query_result: QueryResult<'_, '_, P>,
    collations: &HashMap<u16, CollationDefinition>,
) -> Result<StatementOutcome>
where
    P: Protocol + Unpin,
{
    let mut outcome = StatementOutcome {
        result: None,
        affected_rows: 0,
        last_insert_id: 0,
        warning_count: 0,
        error: None,
    };
    let mut result_set_count = 0;

    while let Some(mut stream) = query_result.stream::<Row>().await? {
        result_set_count += 1;
        if result_set_count > 1 {
            bail!("a case step must contain exactly one MySQL statement");
        }
        let driver_columns = stream.columns_ref().to_vec();
        let columns = driver_columns
            .iter()
            .map(|column| column_metadata(column, collations))
            .collect::<Result<Vec<_>>>()?;
        let mut rows = Vec::new();
        while let Some(row) = stream.try_next().await? {
            rows.push(row_values(row, &driver_columns)?);
        }
        if !columns.is_empty() {
            outcome.result = Some(ResultSet { columns, rows });
        }
    }

    // ResultSetStream keeps the OK packet captured when the stream is created.
    // The connection-backed QueryResult values are updated by the result
    // terminator, so read all packet-derived fields after the stream is done.
    outcome.affected_rows = query_result.affected_rows();
    outcome.last_insert_id = query_result.last_insert_id().unwrap_or_default();
    outcome.warning_count = query_result.warnings();

    Ok(outcome)
}

fn row_values(row: Row, columns: &[Column]) -> Result<Vec<TypedValue>> {
    if row.len() != columns.len() {
        bail!(
            "MySQL returned {} values for {} result columns",
            row.len(),
            columns.len()
        );
    }
    columns
        .iter()
        .enumerate()
        .map(|(index, column)| {
            let value = row
                .as_ref(index)
                .cloned()
                .ok_or_else(|| anyhow!("MySQL result column {index} was unexpectedly missing"))?;
            typed_result_value(value, column)
        })
        .collect()
}

fn typed_result_value(value: Value, column: &Column) -> Result<TypedValue> {
    match value {
        Value::NULL => Ok(TypedValue::Null),
        Value::Int(value) => Ok(TypedValue::SignedInt { value }),
        Value::UInt(value) => Ok(TypedValue::UnsignedInt { value }),
        Value::Float(value) => Ok(TypedValue::Float {
            value: f64::from(value),
        }),
        Value::Double(value) => Ok(TypedValue::Float { value }),
        Value::Date(year, month, day, hour, minute, second, micros) => {
            let value = format_date_time(year, month, day, hour, minute, second, micros);
            match column.column_type() {
                DriverColumnType::MYSQL_TYPE_DATE | DriverColumnType::MYSQL_TYPE_NEWDATE => {
                    Ok(TypedValue::Date { value })
                }
                DriverColumnType::MYSQL_TYPE_TIMESTAMP => Ok(TypedValue::Timestamp { value }),
                _ => Ok(TypedValue::DateTime { value }),
            }
        }
        Value::Time(negative, days, hours, minutes, seconds, micros) => Ok(TypedValue::Time {
            value: format_time(negative, days, hours, minutes, seconds, micros),
        }),
        Value::Bytes(value) => typed_bytes(value, column),
    }
}

fn typed_bytes(value: Vec<u8>, column: &Column) -> Result<TypedValue> {
    use DriverColumnType::*;

    match column.column_type() {
        MYSQL_TYPE_TINY | MYSQL_TYPE_SHORT | MYSQL_TYPE_LONG | MYSQL_TYPE_LONGLONG
        | MYSQL_TYPE_INT24 | MYSQL_TYPE_YEAR => {
            let value = String::from_utf8(value).context("MySQL returned a non-UTF-8 integer")?;
            if column.flags().contains(DriverColumnFlags::UNSIGNED_FLAG) {
                Ok(TypedValue::UnsignedInt {
                    value: value.parse()?,
                })
            } else {
                Ok(TypedValue::SignedInt {
                    value: value.parse()?,
                })
            }
        }
        MYSQL_TYPE_FLOAT | MYSQL_TYPE_DOUBLE => {
            let value = String::from_utf8(value).context("MySQL returned a non-UTF-8 float")?;
            Ok(TypedValue::Float {
                value: value.parse()?,
            })
        }
        MYSQL_TYPE_DECIMAL | MYSQL_TYPE_NEWDECIMAL => Ok(TypedValue::Decimal {
            value: String::from_utf8(value).context("MySQL returned a non-UTF-8 decimal")?,
        }),
        MYSQL_TYPE_DATE | MYSQL_TYPE_NEWDATE => Ok(TypedValue::Date {
            value: String::from_utf8(value).context("MySQL returned a non-UTF-8 date")?,
        }),
        MYSQL_TYPE_TIME | MYSQL_TYPE_TIME2 => Ok(TypedValue::Time {
            value: String::from_utf8(value).context("MySQL returned a non-UTF-8 time")?,
        }),
        MYSQL_TYPE_DATETIME | MYSQL_TYPE_DATETIME2 => Ok(TypedValue::DateTime {
            value: String::from_utf8(value).context("MySQL returned a non-UTF-8 datetime")?,
        }),
        MYSQL_TYPE_TIMESTAMP | MYSQL_TYPE_TIMESTAMP2 => Ok(TypedValue::Timestamp {
            value: String::from_utf8(value).context("MySQL returned a non-UTF-8 timestamp")?,
        }),
        MYSQL_TYPE_JSON => {
            let value = serde_json::from_slice(&value).context("MySQL returned invalid JSON")?;
            Ok(TypedValue::Json { value })
        }
        MYSQL_TYPE_BIT | MYSQL_TYPE_GEOMETRY | MYSQL_TYPE_VECTOR | MYSQL_TYPE_TYPED_ARRAY => {
            Ok(TypedValue::Bytes {
                base64: case::Base64Bytes::from_bytes(&value),
            })
        }
        _ if column.character_set() == 63
            || column.flags().contains(DriverColumnFlags::BINARY_FLAG) =>
        {
            Ok(TypedValue::Bytes {
                base64: case::Base64Bytes::from_bytes(&value),
            })
        }
        _ => match String::from_utf8(value) {
            Ok(value) => Ok(TypedValue::Text { value }),
            Err(error) => Ok(TypedValue::Bytes {
                base64: case::Base64Bytes::from_bytes(error.as_bytes()),
            }),
        },
    }
}

fn column_metadata(
    column: &Column,
    collations: &HashMap<u16, CollationDefinition>,
) -> Result<ColumnMetadata> {
    let character_set_id = column.character_set();
    let (character_set, collation) = collations
        .get(&character_set_id)
        .map(|definition| {
            (
                definition.character_set.clone(),
                definition.collation.clone(),
            )
        })
        .unwrap_or_else(|| {
            (
                CharacterSet::Other(format!("id:{character_set_id}")),
                Collation::Other(format!("id:{character_set_id}")),
            )
        });
    let flags = column_flags(column.flags());

    Ok(ColumnMetadata {
        name: metadata_string(column.name_ref(), "column name")?,
        original_name: optional_metadata_string(column.org_name_ref(), "original column name")?,
        table: optional_metadata_string(column.table_ref(), "table name")?,
        original_table: optional_metadata_string(column.org_table_ref(), "original table name")?,
        database: optional_metadata_string(column.schema_ref(), "database name")?,
        catalog: Some("def".to_owned()),
        column_type: mysql_type(column.column_type()),
        character_set_id: Some(character_set_id),
        character_set: Some(character_set),
        collation: Some(collation),
        column_length: Some(u64::from(column.column_length())),
        decimals: Some(column.decimals()),
        nullable: !column.flags().contains(DriverColumnFlags::NOT_NULL_FLAG),
        flags,
    })
}

fn mysql_type(column_type: DriverColumnType) -> MySqlType {
    use DriverColumnType::*;

    match column_type {
        MYSQL_TYPE_NULL => MySqlType::Null,
        MYSQL_TYPE_TINY => MySqlType::Tiny,
        MYSQL_TYPE_SHORT => MySqlType::Short,
        MYSQL_TYPE_LONG => MySqlType::Long,
        MYSQL_TYPE_LONGLONG => MySqlType::LongLong,
        MYSQL_TYPE_INT24 => MySqlType::Int24,
        MYSQL_TYPE_FLOAT => MySqlType::Float,
        MYSQL_TYPE_DOUBLE => MySqlType::Double,
        MYSQL_TYPE_DECIMAL => MySqlType::Decimal,
        MYSQL_TYPE_NEWDECIMAL => MySqlType::NewDecimal,
        MYSQL_TYPE_BIT => MySqlType::Bit,
        MYSQL_TYPE_YEAR => MySqlType::Year,
        MYSQL_TYPE_DATE | MYSQL_TYPE_NEWDATE => MySqlType::Date,
        MYSQL_TYPE_TIME | MYSQL_TYPE_TIME2 => MySqlType::Time,
        MYSQL_TYPE_DATETIME | MYSQL_TYPE_DATETIME2 => MySqlType::DateTime,
        MYSQL_TYPE_TIMESTAMP | MYSQL_TYPE_TIMESTAMP2 => MySqlType::Timestamp,
        MYSQL_TYPE_VARCHAR => MySqlType::VarChar,
        MYSQL_TYPE_VAR_STRING => MySqlType::VarString,
        MYSQL_TYPE_STRING => MySqlType::String,
        MYSQL_TYPE_BLOB => MySqlType::Blob,
        MYSQL_TYPE_TINY_BLOB => MySqlType::TinyBlob,
        MYSQL_TYPE_MEDIUM_BLOB => MySqlType::MediumBlob,
        MYSQL_TYPE_LONG_BLOB => MySqlType::LongBlob,
        MYSQL_TYPE_JSON => MySqlType::Json,
        MYSQL_TYPE_ENUM => MySqlType::Enum,
        MYSQL_TYPE_SET => MySqlType::Set,
        MYSQL_TYPE_GEOMETRY => MySqlType::Geometry,
        MYSQL_TYPE_VECTOR => MySqlType::Vector,
        MYSQL_TYPE_TYPED_ARRAY => MySqlType::TypedArray,
        other => MySqlType::Unknown {
            name: format!("{other:?}"),
        },
    }
}

fn column_flags(flags: DriverColumnFlags) -> Vec<ColumnFlag> {
    let mappings = [
        (DriverColumnFlags::NOT_NULL_FLAG, ColumnFlag::NotNull),
        (DriverColumnFlags::PRI_KEY_FLAG, ColumnFlag::PrimaryKey),
        (DriverColumnFlags::UNIQUE_KEY_FLAG, ColumnFlag::UniqueKey),
        (
            DriverColumnFlags::MULTIPLE_KEY_FLAG,
            ColumnFlag::MultipleKey,
        ),
        (DriverColumnFlags::BLOB_FLAG, ColumnFlag::Blob),
        (DriverColumnFlags::UNSIGNED_FLAG, ColumnFlag::Unsigned),
        (DriverColumnFlags::ZEROFILL_FLAG, ColumnFlag::ZeroFill),
        (DriverColumnFlags::BINARY_FLAG, ColumnFlag::Binary),
        (
            DriverColumnFlags::AUTO_INCREMENT_FLAG,
            ColumnFlag::AutoIncrement,
        ),
        (DriverColumnFlags::TIMESTAMP_FLAG, ColumnFlag::Timestamp),
        (DriverColumnFlags::ENUM_FLAG, ColumnFlag::Enum),
        (DriverColumnFlags::SET_FLAG, ColumnFlag::Set),
        (
            DriverColumnFlags::NO_DEFAULT_VALUE_FLAG,
            ColumnFlag::NoDefaultValue,
        ),
        (DriverColumnFlags::ON_UPDATE_NOW_FLAG, ColumnFlag::OnUpdate),
        (DriverColumnFlags::PART_KEY_FLAG, ColumnFlag::PartKey),
        (DriverColumnFlags::NUM_FLAG, ColumnFlag::Numeric),
    ];
    mappings
        .into_iter()
        .filter_map(|(driver, recorded)| flags.contains(driver).then_some(recorded))
        .collect()
}

async fn read_warnings(conn: &mut Conn, expected_count: u16) -> Result<WarningSet> {
    if expected_count == 0 {
        return Ok(WarningSet::default());
    }
    let rows: Vec<(String, u32, String)> = conn.query("SHOW WARNINGS").await?;
    let details = rows
        .into_iter()
        .map(|(level, code, message)| WarningDetail {
            level: match level.as_str() {
                "Note" => WarningLevel::Note,
                "Warning" => WarningLevel::Warning,
                "Error" => WarningLevel::Error,
                _ => WarningLevel::Other(level),
            },
            code,
            sql_state: None,
            message,
        })
        .collect::<Vec<_>>();
    if details.len() != usize::from(expected_count) {
        bail!(
            "MySQL reported {expected_count} warnings but SHOW WARNINGS returned {} details",
            details.len()
        );
    }
    Ok(WarningSet::from_details(details))
}

async fn read_session_state(conn: &mut Conn) -> Result<SessionState> {
    let row: (Option<String>, String, String, String, u8) = conn
        .query_first(
            "SELECT DATABASE(), @@session.sql_mode, @@session.time_zone, \
             @@session.transaction_isolation, @@session.autocommit",
        )
        .await?
        .ok_or_else(|| anyhow!("MySQL returned no session-state row"))?;
    let status = read_server_status(conn).await?;
    Ok(SessionState {
        current_database: row.0,
        sql_mode: parse_sql_mode(&row.1)?,
        time_zone: parse_time_zone(&row.2)?,
        isolation: parse_isolation(&row.3)?,
        autocommit: row.4 != 0,
        transaction: if status.contains(StatusFlags::SERVER_STATUS_IN_TRANS) {
            TransactionState::Active
        } else {
            TransactionState::Idle
        },
    })
}

async fn read_server_status(conn: &mut Conn) -> Result<StatusFlags> {
    let mut result = conn.query_iter("DO 0").await?;
    let stream = result
        .stream::<Row>()
        .await?
        .ok_or_else(|| anyhow!("MySQL returned no status result"))?;
    Ok(stream
        .ok_packet()
        .ok_or_else(|| anyhow!("MySQL returned no status packet"))?
        .status_flags())
}

fn format_sql_mode(sql_mode: &SqlMode) -> String {
    sql_mode
        .flags
        .iter()
        .map(|mode| sql_mode_name(*mode))
        .collect::<Vec<_>>()
        .join(",")
}

fn parse_sql_mode(value: &str) -> Result<SqlMode> {
    let flags = if value.is_empty() {
        Vec::new()
    } else {
        value
            .split(',')
            .map(parse_sql_mode_name)
            .collect::<Result<Vec<_>>>()?
    };
    Ok(SqlMode::new(flags)?)
}

fn sql_mode_name(mode: SqlModeFlag) -> &'static str {
    match mode {
        SqlModeFlag::OnlyFullGroupBy => "ONLY_FULL_GROUP_BY",
        SqlModeFlag::StrictTransTables => "STRICT_TRANS_TABLES",
        SqlModeFlag::StrictAllTables => "STRICT_ALL_TABLES",
        SqlModeFlag::NoZeroInDate => "NO_ZERO_IN_DATE",
        SqlModeFlag::NoZeroDate => "NO_ZERO_DATE",
        SqlModeFlag::ErrorForDivisionByZero => "ERROR_FOR_DIVISION_BY_ZERO",
        SqlModeFlag::NoEngineSubstitution => "NO_ENGINE_SUBSTITUTION",
        SqlModeFlag::AnsiQuotes => "ANSI_QUOTES",
        SqlModeFlag::NoBackslashEscapes => "NO_BACKSLASH_ESCAPES",
        SqlModeFlag::PipesAsConcat => "PIPES_AS_CONCAT",
        SqlModeFlag::PadCharToFullLength => "PAD_CHAR_TO_FULL_LENGTH",
        SqlModeFlag::HighNotPrecedence => "HIGH_NOT_PRECEDENCE",
        SqlModeFlag::RealAsFloat => "REAL_AS_FLOAT",
        SqlModeFlag::IgnoreSpace => "IGNORE_SPACE",
        SqlModeFlag::NoAutoValueOnZero => "NO_AUTO_VALUE_ON_ZERO",
        SqlModeFlag::NoUnsignedSubtraction => "NO_UNSIGNED_SUBTRACTION",
        SqlModeFlag::Ansi => "ANSI",
        SqlModeFlag::Traditional => "TRADITIONAL",
        SqlModeFlag::AllowInvalidDates => "ALLOW_INVALID_DATES",
        SqlModeFlag::NoDirInCreate => "NO_DIR_IN_CREATE",
        SqlModeFlag::TimeTruncateFractional => "TIME_TRUNCATE_FRACTIONAL",
    }
}

fn parse_sql_mode_name(value: &str) -> Result<SqlModeFlag> {
    let mode = match value {
        "ONLY_FULL_GROUP_BY" => SqlModeFlag::OnlyFullGroupBy,
        "STRICT_TRANS_TABLES" => SqlModeFlag::StrictTransTables,
        "STRICT_ALL_TABLES" => SqlModeFlag::StrictAllTables,
        "NO_ZERO_IN_DATE" => SqlModeFlag::NoZeroInDate,
        "NO_ZERO_DATE" => SqlModeFlag::NoZeroDate,
        "ERROR_FOR_DIVISION_BY_ZERO" => SqlModeFlag::ErrorForDivisionByZero,
        "NO_ENGINE_SUBSTITUTION" => SqlModeFlag::NoEngineSubstitution,
        "ANSI_QUOTES" => SqlModeFlag::AnsiQuotes,
        "NO_BACKSLASH_ESCAPES" => SqlModeFlag::NoBackslashEscapes,
        "PIPES_AS_CONCAT" => SqlModeFlag::PipesAsConcat,
        "PAD_CHAR_TO_FULL_LENGTH" => SqlModeFlag::PadCharToFullLength,
        "HIGH_NOT_PRECEDENCE" => SqlModeFlag::HighNotPrecedence,
        "REAL_AS_FLOAT" => SqlModeFlag::RealAsFloat,
        "IGNORE_SPACE" => SqlModeFlag::IgnoreSpace,
        "NO_AUTO_VALUE_ON_ZERO" => SqlModeFlag::NoAutoValueOnZero,
        "NO_UNSIGNED_SUBTRACTION" => SqlModeFlag::NoUnsignedSubtraction,
        "ANSI" => SqlModeFlag::Ansi,
        "TRADITIONAL" => SqlModeFlag::Traditional,
        "ALLOW_INVALID_DATES" => SqlModeFlag::AllowInvalidDates,
        "NO_DIR_IN_CREATE" => SqlModeFlag::NoDirInCreate,
        "TIME_TRUNCATE_FRACTIONAL" => SqlModeFlag::TimeTruncateFractional,
        other => bail!("MySQL returned unsupported SQL mode `{other}`"),
    };
    Ok(mode)
}

fn format_time_zone(time_zone: &TimeZone) -> String {
    match time_zone {
        TimeZone::Utc => "+00:00".to_owned(),
        TimeZone::FixedOffset { seconds } => {
            let sign = if *seconds < 0 { '-' } else { '+' };
            let seconds = seconds.abs();
            format!("{sign}{:02}:{:02}", seconds / 3600, seconds % 3600 / 60)
        }
        TimeZone::Iana { name } => name.clone(),
    }
}

fn parse_time_zone(value: &str) -> Result<TimeZone> {
    if value == "+00:00" {
        return Ok(TimeZone::Utc);
    }
    if let Some(seconds) = parse_fixed_offset(value) {
        return Ok(TimeZone::fixed_offset(seconds)?);
    }
    Ok(TimeZone::iana(value)?)
}

fn parse_fixed_offset(value: &str) -> Option<i32> {
    let sign = match value.as_bytes().first() {
        Some(b'+') => 1,
        Some(b'-') => -1,
        _ => return None,
    };
    let (hours, minutes) = value.get(1..)?.split_once(':')?;
    if hours.len() != 2 || minutes.len() != 2 {
        return None;
    }
    let hours: i32 = hours.parse().ok()?;
    let minutes: i32 = minutes.parse().ok()?;
    if minutes >= 60 {
        return None;
    }
    Some(sign * (hours * 3600 + minutes * 60))
}

fn format_isolation(isolation: IsolationLevel) -> &'static str {
    match isolation {
        IsolationLevel::ReadUncommitted => "READ UNCOMMITTED",
        IsolationLevel::ReadCommitted => "READ COMMITTED",
        IsolationLevel::RepeatableRead => "REPEATABLE READ",
        IsolationLevel::Serializable => "SERIALIZABLE",
    }
}

fn parse_isolation(value: &str) -> Result<IsolationLevel> {
    match value {
        "READ-UNCOMMITTED" => Ok(IsolationLevel::ReadUncommitted),
        "READ-COMMITTED" => Ok(IsolationLevel::ReadCommitted),
        "REPEATABLE-READ" => Ok(IsolationLevel::RepeatableRead),
        "SERIALIZABLE" => Ok(IsolationLevel::Serializable),
        other => bail!("MySQL returned unsupported isolation level `{other}`"),
    }
}

fn parameter_value(value: &TypedValue) -> Result<Value> {
    Ok(match value {
        TypedValue::Null => Value::NULL,
        TypedValue::Bool { value } => Value::Int(i64::from(*value)),
        TypedValue::SignedInt { value } => Value::Int(*value),
        TypedValue::UnsignedInt { value } => Value::UInt(*value),
        TypedValue::Float { value } => Value::Double(*value),
        TypedValue::Decimal { value } | TypedValue::Text { value } => {
            Value::Bytes(value.as_bytes().to_vec())
        }
        TypedValue::Date { value } => parse_date_parameter(value)?,
        TypedValue::Time { value } => parse_time_parameter(value)?,
        TypedValue::DateTime { value } | TypedValue::Timestamp { value } => {
            parse_date_time_parameter(value)?
        }
        TypedValue::Bytes { base64 } => Value::Bytes(base64.decode()?),
        TypedValue::Json { value } => Value::Bytes(serde_json::to_vec(value)?),
    })
}

fn parse_date_parameter(value: &str) -> Result<Value> {
    let (year, month, day) = parse_date_parts(value)?;
    Ok(Value::Date(year, month, day, 0, 0, 0, 0))
}

fn parse_date_time_parameter(value: &str) -> Result<Value> {
    let (date, time) = value
        .split_once(' ')
        .or_else(|| value.split_once('T'))
        .ok_or_else(|| anyhow!("invalid date-time parameter `{value}`"))?;
    let (year, month, day) = parse_date_parts(date)?;
    let (negative, days, hour, minute, second, micros) = parse_time_parts(time)?;
    if negative || days != 0 {
        bail!("invalid date-time parameter `{value}`");
    }
    Ok(Value::Date(year, month, day, hour, minute, second, micros))
}

fn parse_time_parameter(value: &str) -> Result<Value> {
    let (negative, days, hour, minute, second, micros) = parse_time_parts(value)?;
    Ok(Value::Time(negative, days, hour, minute, second, micros))
}

fn parse_date_parts(value: &str) -> Result<(u16, u8, u8)> {
    let mut parts = value.split('-');
    let year = parts.next().and_then(|part| part.parse().ok());
    let month = parts.next().and_then(|part| part.parse().ok());
    let day = parts.next().and_then(|part| part.parse().ok());
    if parts.next().is_some() {
        bail!("invalid date parameter `{value}`");
    }
    match (year, month, day) {
        (Some(year), Some(month @ 0..=12), Some(day @ 0..=31)) => Ok((year, month, day)),
        _ => bail!("invalid date parameter `{value}`"),
    }
}

fn parse_time_parts(value: &str) -> Result<(bool, u32, u8, u8, u8, u32)> {
    let (negative, value) = match value.strip_prefix('-') {
        Some(value) => (true, value),
        None => (false, value),
    };
    let (clock, fraction) = value.split_once('.').unwrap_or((value, ""));
    let mut parts = clock.split(':');
    let hours: u32 = parts
        .next()
        .and_then(|part| part.parse().ok())
        .ok_or_else(|| anyhow!("invalid time parameter `{value}`"))?;
    let minutes: u8 = parts
        .next()
        .and_then(|part| part.parse().ok())
        .ok_or_else(|| anyhow!("invalid time parameter `{value}`"))?;
    let seconds: u8 = parts
        .next()
        .and_then(|part| part.parse().ok())
        .ok_or_else(|| anyhow!("invalid time parameter `{value}`"))?;
    if parts.next().is_some() || hours > 838 || minutes > 59 || seconds > 59 {
        bail!("invalid time parameter `{value}`");
    }
    let micros = parse_micros(fraction, value)?;
    Ok((
        negative,
        hours / 24,
        (hours % 24) as u8,
        minutes,
        seconds,
        micros,
    ))
}

fn parse_micros(fraction: &str, original: &str) -> Result<u32> {
    if fraction.is_empty() {
        return Ok(0);
    }
    if fraction.len() > 6 || !fraction.bytes().all(|byte| byte.is_ascii_digit()) {
        bail!("invalid fractional seconds in `{original}`");
    }
    let micros: u32 = fraction.parse()?;
    Ok(micros * 10_u32.pow(6 - fraction.len() as u32))
}

fn parse_character_set(value: String) -> CharacterSet {
    match value.as_str() {
        "utf8mb4" => CharacterSet::Utf8mb4,
        "binary" => CharacterSet::Binary,
        _ => CharacterSet::Other(value),
    }
}

fn parse_collation(value: String) -> Collation {
    match value.as_str() {
        "utf8mb4_0900_ai_ci" => Collation::Utf8mb4_0900AiCi,
        "utf8mb4_bin" => Collation::Utf8mb4Bin,
        "binary" => Collation::Binary,
        _ => Collation::Other(value),
    }
}

fn metadata_string(value: &[u8], field: &str) -> Result<String> {
    String::from_utf8(value.to_vec()).with_context(|| format!("MySQL returned invalid {field}"))
}

fn optional_metadata_string(value: &[u8], field: &str) -> Result<Option<String>> {
    if value.is_empty() {
        Ok(None)
    } else {
        metadata_string(value, field).map(Some)
    }
}

fn format_date_time(
    year: u16,
    month: u8,
    day: u8,
    hour: u8,
    minute: u8,
    second: u8,
    micros: u32,
) -> String {
    let mut value = format!("{year:04}-{month:02}-{day:02}");
    if hour != 0 || minute != 0 || second != 0 || micros != 0 {
        value.push_str(&format!(" {hour:02}:{minute:02}:{second:02}"));
        if micros != 0 {
            value.push_str(&format!(".{micros:06}"));
        }
    }
    value
}

fn format_time(
    negative: bool,
    days: u32,
    hours: u8,
    minutes: u8,
    seconds: u8,
    micros: u32,
) -> String {
    let sign = if negative { "-" } else { "" };
    let hours = days * 24 + u32::from(hours);
    let mut value = format!("{sign}{hours:02}:{minutes:02}:{seconds:02}");
    if micros != 0 {
        value.push_str(&format!(".{micros:06}"));
    }
    value
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::thread;

    fn expected_observation(step_id: &str, result: Option<ResultSet>) -> Observation {
        Observation {
            version: OBSERVATION_FORMAT_VERSION,
            step_id: step_id.to_owned(),
            session_id: "autocommit_off".to_owned(),
            result,
            affected_rows: 0,
            last_insert_id: 0,
            warnings: WarningSet::default(),
            error: None,
            session_state: SessionState {
                current_database: Some("turso_oracle".to_owned()),
                sql_mode: SqlMode::default(),
                time_zone: TimeZone::Utc,
                isolation: IsolationLevel::RepeatableRead,
                autocommit: false,
                transaction: TransactionState::Idle,
            },
        }
    }

    fn profile_column(column_length: u64) -> ColumnMetadata {
        ColumnMetadata {
            name: "value".to_owned(),
            original_name: None,
            table: None,
            original_table: None,
            database: None,
            catalog: Some("def".to_owned()),
            column_type: MySqlType::LongLong,
            character_set_id: Some(63),
            character_set: Some(CharacterSet::Binary),
            collation: Some(Collation::Binary),
            column_length: Some(column_length),
            decimals: Some(0),
            nullable: false,
            flags: vec![ColumnFlag::NotNull, ColumnFlag::Binary],
        }
    }

    #[test]
    fn bounded_turso_profile_matches_the_frozen_transaction_case() {
        let case: Case =
            serde_json::from_str(include_str!("../cases/p0/transaction-observer.json")).unwrap();
        let expected: Vec<Observation> = serde_json::from_str(include_str!(
            "../goldens/mysql-8.4/sha256-b3b90af2a6552ae30c266fdb7d5dd55f3afb72404bb78d37fe8a23eb857fd3fb/transaction-observer.json"
        ))
        .unwrap();

        validate_initial_turso_case(&case, &expected).unwrap();
    }

    #[test]
    fn bounded_turso_profile_rejects_golden_expected_errors() {
        let case: Case =
            serde_json::from_str(include_str!("../cases/p0/transaction-observer.json")).unwrap();
        let mut expected: Vec<Observation> = serde_json::from_str(include_str!(
            "../goldens/mysql-8.4/sha256-b3b90af2a6552ae30c266fdb7d5dd55f3afb72404bb78d37fe8a23eb857fd3fb/transaction-observer.json"
        ))
        .unwrap();
        expected[0].error = Some(MySqlError {
            number: 1064,
            sql_state: SqlState::new("42000").unwrap(),
            message: "unexpected expected error".to_owned(),
        });

        let error = validate_initial_turso_case(&case, &expected)
            .unwrap_err()
            .to_string();
        assert!(error.contains("must not expect an error"));
    }

    #[test]
    fn bounded_turso_profile_marks_unobserved_fields_instead_of_passing_them() {
        let expected = expected_observation("commit_before_reads", None);
        let actual = TursoObservation {
            step_id: "commit_before_reads".to_owned(),
            session_id: "autocommit_off".to_owned(),
            result: None,
            affected_rows: 0,
            last_insert_id: 0,
            warning_count: None,
            error: None,
            status: None,
        };
        let mut mismatches = Vec::new();
        let mut inconclusive_reasons = Vec::new();
        compare_turso_observation(
            0,
            &expected,
            &actual,
            &mut mismatches,
            &mut inconclusive_reasons,
        );

        assert!(mismatches.is_empty());
        assert_eq!(
            turso_comparison_status(&mismatches, &inconclusive_reasons),
            TursoComparisonStatus::Inconclusive
        );
        assert!(inconclusive_reasons
            .iter()
            .any(|reason| reason.contains("warnings.warning_count")));
        assert!(inconclusive_reasons
            .iter()
            .any(|reason| reason.contains("session_state.autocommit")));
    }

    #[test]
    fn bounded_turso_profile_reports_unexpected_cleanup_errors() {
        let expected = expected_observation("disable_notes_for_cleanup", None);
        let actual = TursoObservation {
            step_id: expected.step_id.clone(),
            session_id: expected.session_id.clone(),
            result: None,
            affected_rows: 0,
            last_insert_id: 0,
            warning_count: None,
            error: Some(MySqlError {
                number: 1064,
                sql_state: SqlState::new("42000").unwrap(),
                message: "wrong error".to_owned(),
            }),
            status: None,
        };
        let mut mismatches = Vec::new();
        let mut inconclusive_reasons = Vec::new();
        compare_turso_observation(
            0,
            &expected,
            &actual,
            &mut mismatches,
            &mut inconclusive_reasons,
        );
        assert_eq!(mismatches.len(), 1);
        assert!(inconclusive_reasons.is_empty());
    }

    #[test]
    fn bounded_turso_profile_compares_error_identity_without_comparing_messages() {
        let mut expected = expected_observation("disable_notes_for_cleanup", None);
        expected.error = Some(MySqlError {
            number: 1235,
            sql_state: SqlState::new("42000").unwrap(),
            message: "reference wording".to_owned(),
        });
        let actual = TursoObservation {
            step_id: expected.step_id.clone(),
            session_id: expected.session_id.clone(),
            result: None,
            affected_rows: 0,
            last_insert_id: 0,
            warning_count: None,
            error: Some(MySqlError {
                number: 1235,
                sql_state: SqlState::new("42000").unwrap(),
                message: "target wording".to_owned(),
            }),
            status: None,
        };
        let mut mismatches = Vec::new();
        let mut inconclusive_reasons = Vec::new();
        compare_turso_observation(
            0,
            &expected,
            &actual,
            &mut mismatches,
            &mut inconclusive_reasons,
        );
        assert!(mismatches.is_empty());
        assert!(inconclusive_reasons.is_empty());
    }

    #[test]
    fn bounded_turso_mismatches_fail_the_comparison() {
        assert_eq!(
            turso_comparison_status(&["difference".to_owned()], &[]),
            TursoComparisonStatus::Fail
        );
        assert_eq!(
            turso_comparison_status(&[], &["not measured".to_owned()]),
            TursoComparisonStatus::Inconclusive
        );
    }

    #[test]
    fn turso_connection_options_keep_explicit_unix_sockets_without_socket_fallback() {
        let options = Opts::from_url(
            "mysql://user:password@127.0.0.1/database?socket=%2Ftmp%2Fturso-mysql.sock",
        )
        .unwrap();
        let options: Opts = mysql_async::OptsBuilder::from_opts(options)
            .prefer_socket(false)
            .into();

        assert_eq!(options.socket(), Some("/tmp/turso-mysql.sock"));
        assert!(!options.prefer_socket());
    }

    #[test]
    fn turso_endpoint_requires_an_explicit_socket_or_numeric_loopback() {
        let unix_socket = Opts::from_url(
            "mysql://user:password@example.invalid/database?socket=%2Ftmp%2Fturso-mysql.sock",
        )
        .unwrap();
        assert!(validate_turso_endpoint(&unix_socket).is_ok());

        let loopback = Opts::from_url("mysql://user:password@127.0.0.1/database").unwrap();
        assert!(validate_turso_endpoint(&loopback).is_ok());

        for dsn in [
            "mysql://user:password@localhost/database",
            "mysql://user:password@192.0.2.1/database",
        ] {
            let options = Opts::from_url(dsn).unwrap();
            assert!(validate_turso_endpoint(&options).is_err());
        }
    }

    #[test]
    fn bounded_turso_profile_compares_protocol_column_metadata() {
        let expected_result = ResultSet {
            columns: vec![profile_column(2)],
            rows: vec![vec![TypedValue::SignedInt { value: 1 }]],
        };
        let actual_result = ResultSet {
            columns: vec![profile_column(3)],
            rows: vec![vec![TypedValue::SignedInt { value: 1 }]],
        };
        let expected = expected_observation("constant_read", Some(expected_result));
        let actual = TursoObservation {
            step_id: "constant_read".to_owned(),
            session_id: "autocommit_off".to_owned(),
            result: Some(actual_result),
            affected_rows: 0,
            last_insert_id: 0,
            warning_count: Some(0),
            error: None,
            status: Some(TursoStatus {
                autocommit: false,
                transaction_active: false,
            }),
        };
        let mut mismatches = Vec::new();
        let mut inconclusive_reasons = Vec::new();
        compare_turso_observation(
            0,
            &expected,
            &actual,
            &mut mismatches,
            &mut inconclusive_reasons,
        );

        assert!(mismatches
            .iter()
            .any(|mismatch| mismatch.contains("column_length")));
        assert!(inconclusive_reasons.is_empty());
    }

    #[test]
    fn bounded_turso_profile_compares_result_database_metadata() {
        let mut expected_column = profile_column(2);
        expected_column.database = Some("expected_db".to_owned());
        let mut actual_column = profile_column(2);
        actual_column.database = Some("actual_db".to_owned());
        let expected = expected_observation(
            "constant_read",
            Some(ResultSet {
                columns: vec![expected_column],
                rows: vec![vec![TypedValue::SignedInt { value: 1 }]],
            }),
        );
        let actual = TursoObservation {
            step_id: "constant_read".to_owned(),
            session_id: "autocommit_off".to_owned(),
            result: Some(ResultSet {
                columns: vec![actual_column],
                rows: vec![vec![TypedValue::SignedInt { value: 1 }]],
            }),
            affected_rows: 0,
            last_insert_id: 0,
            warning_count: Some(0),
            error: None,
            status: Some(TursoStatus {
                autocommit: false,
                transaction_active: false,
            }),
        };
        let mut mismatches = Vec::new();
        let mut inconclusive_reasons = Vec::new();
        compare_turso_observation(
            0,
            &expected,
            &actual,
            &mut mismatches,
            &mut inconclusive_reasons,
        );

        assert!(mismatches
            .iter()
            .any(|mismatch| mismatch.contains("database")));
        assert!(inconclusive_reasons.is_empty());
        assert!(TURSO_PROFILE_MEASURED.contains(&"result.columns.database"));
        assert!(!TURSO_PROFILE_NOT_MEASURED.contains(&"result.columns.database"));
    }

    #[test]
    fn disposable_database_acknowledgement_matches_the_dsn_database() {
        let options = Opts::from_url("mysql://user:password@127.0.0.1/mysql_compare_tmp").unwrap();
        assert!(validate_disposable_database(&options, "mysql_compare_tmp").is_ok());
        assert!(validate_disposable_database(&options, "another_database")
            .unwrap_err()
            .to_string()
            .contains("must exactly match"));

        let without_database = Opts::from_url("mysql://user:password@127.0.0.1").unwrap();
        assert!(
            validate_disposable_database(&without_database, "mysql_compare_tmp")
                .unwrap_err()
                .to_string()
                .contains("must include a database name")
        );
        assert!(validate_disposable_database(&options, "")
            .unwrap_err()
            .to_string()
            .contains("must not be empty"));
    }

    #[test]
    fn default_sql_mode_round_trips_through_mysql_names() {
        let sql_mode = SqlMode::default();
        assert_eq!(
            parse_sql_mode(&format_sql_mode(&sql_mode)).unwrap(),
            sql_mode
        );
    }

    #[test]
    fn oracle_connections_are_confined_to_numeric_loopback_hosts() {
        assert!(is_numeric_loopback("127.0.0.1"));
        assert!(is_numeric_loopback("::1"));
        assert!(!is_numeric_loopback("localhost"));
        assert!(!is_numeric_loopback("192.0.2.1"));
    }

    #[test]
    fn fixed_time_zones_round_trip_at_supported_boundaries() {
        for seconds in [
            -13 * 60 * 60 - 59 * 60,
            -90 * 60,
            0,
            5 * 60 * 60 + 30 * 60,
            14 * 60 * 60,
        ] {
            let time_zone = TimeZone::fixed_offset(seconds).unwrap();
            assert_eq!(
                parse_time_zone(&format_time_zone(&time_zone)).unwrap(),
                time_zone
            );
        }
    }

    #[test]
    fn binary_and_unsigned_text_results_keep_their_types() {
        let unsigned = Column::new(DriverColumnType::MYSQL_TYPE_LONGLONG)
            .with_flags(DriverColumnFlags::UNSIGNED_FLAG);
        assert_eq!(
            typed_bytes(b"18446744073709551615".to_vec(), &unsigned).unwrap(),
            TypedValue::UnsignedInt { value: u64::MAX }
        );

        let binary = Column::new(DriverColumnType::MYSQL_TYPE_BLOB).with_character_set(63);
        assert_eq!(
            typed_bytes(vec![0, 255], &binary).unwrap(),
            TypedValue::Bytes {
                base64: case::Base64Bytes::from_bytes(&[0, 255])
            }
        );
    }

    #[test]
    fn column_metadata_keeps_the_protocol_collation_id() {
        let column = Column::new(DriverColumnType::MYSQL_TYPE_VAR_STRING).with_character_set(45);
        let collations = HashMap::from([(
            45,
            CollationDefinition {
                character_set: CharacterSet::Utf8mb4,
                collation: Collation::Other("utf8mb4_general_ci".to_owned()),
            },
        )]);

        let metadata = column_metadata(&column, &collations).unwrap();

        assert_eq!(metadata.character_set_id, Some(45));
        assert_eq!(metadata.character_set, Some(CharacterSet::Utf8mb4));
        assert_eq!(
            metadata.collation,
            Some(Collation::Other("utf8mb4_general_ci".to_owned()))
        );
    }

    #[test]
    fn repeated_protocol_collation_id_is_rejected() {
        let error = index_collations([
            ("utf8mb4".to_owned(), "utf8mb4_general_ci".to_owned(), 45),
            ("utf8mb4".to_owned(), "utf8mb4_0900_ai_ci".to_owned(), 45),
        ])
        .unwrap_err();

        assert!(error
            .to_string()
            .contains("information_schema.COLLATIONS repeats collation ID 45"));
    }

    #[test]
    fn temporal_parameters_use_binary_protocol_types() {
        assert_eq!(
            parameter_value(&TypedValue::Date {
                value: "2024-01-02".to_owned()
            })
            .unwrap(),
            Value::Date(2024, 1, 2, 0, 0, 0, 0)
        );
        assert_eq!(
            parameter_value(&TypedValue::Timestamp {
                value: "2024-01-02 03:04:05.1234".to_owned()
            })
            .unwrap(),
            Value::Date(2024, 1, 2, 3, 4, 5, 123_400)
        );
        assert_eq!(
            parameter_value(&TypedValue::Time {
                value: "-25:06:07.000001".to_owned()
            })
            .unwrap(),
            Value::Time(true, 1, 1, 6, 7, 1)
        );
    }

    #[tokio::test]
    async fn turso_status_uses_result_terminator_after_a_previous_ok_packet() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = thread::spawn(move || run_status_probe_server(listener));
        let options = mysql_async::OptsBuilder::default()
            .ip_or_hostname("127.0.0.1")
            .tcp_port(port)
            .user(Some("root"))
            .pass(Some(""))
            .db_name(Some("test"))
            .max_allowed_packet(Some(4 * 1024 * 1024))
            .wait_timeout(Some(28_800))
            .prefer_socket(false);
        let mut conn = Conn::new(options).await.unwrap();
        conn.query_drop("SET SESSION autocommit = 0").await.unwrap();

        let update = execute_turso_step(
            &mut conn,
            &Step {
                id: "mutation".to_owned(),
                session_id: "autocommit_off".to_owned(),
                probe: None,
                sql: "UPDATE transaction_probe SET id = 1".to_owned(),
                params: None,
                parallel: None,
                schedule_dependent: None,
            },
            &initial_turso_collations(),
        )
        .await
        .unwrap();
        assert_eq!(update.affected_rows, 5);
        assert_eq!(update.last_insert_id, 7);
        assert_eq!(update.warning_count, Some(3));

        let actual = execute_turso_step(
            &mut conn,
            &Step {
                id: "table_read".to_owned(),
                session_id: "autocommit_off".to_owned(),
                probe: None,
                sql: "SELECT id FROM transaction_probe".to_owned(),
                params: None,
                parallel: None,
                schedule_dependent: None,
            },
            &initial_turso_collations(),
        )
        .await
        .unwrap_or_else(|error| panic!("status probe failed: {error:#}"));

        assert_eq!(
            actual.status,
            Some(TursoStatus {
                autocommit: false,
                transaction_active: true,
            })
        );
        assert_eq!(actual.affected_rows, 0);
        assert_eq!(actual.last_insert_id, 0);
        assert_eq!(actual.warning_count, Some(9));
        conn.disconnect().await.unwrap();
        server.join().unwrap();
    }

    fn run_status_probe_server(listener: TcpListener) {
        let (mut stream, _) = listener.accept().unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        stream
            .set_write_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        write_mysql_packet(&mut stream, 0, &status_probe_handshake());
        let _handshake_response = read_mysql_packet(&mut stream);
        write_mysql_packet(&mut stream, 2, &[0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00]);

        loop {
            let packet = read_mysql_packet(&mut stream);
            match packet.first().copied() {
                Some(0x01) => break,
                Some(0x03) => {
                    let sql = String::from_utf8_lossy(&packet[1..]);
                    if sql.starts_with("SET SESSION autocommit") {
                        write_mysql_packet(
                            &mut stream,
                            1,
                            &[0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00],
                        );
                    } else if sql == "UPDATE transaction_probe SET id = 1" {
                        write_mysql_packet(
                            &mut stream,
                            1,
                            &[0x00, 0x05, 0x07, 0x00, 0x00, 0x03, 0x00],
                        );
                    } else if sql == "SELECT id FROM transaction_probe" {
                        write_mysql_packet(&mut stream, 1, &[0x01]);
                        write_mysql_packet(&mut stream, 2, &status_probe_column());
                        write_mysql_packet(
                            &mut stream,
                            3,
                            &[0xfe, 0x00, 0x00, 0x01, 0x00, 0x09, 0x00],
                        );
                    } else {
                        panic!("unexpected SQL in status probe: {sql}");
                    }
                }
                command => panic!("unexpected MySQL command in status probe: {command:?}"),
            }
        }
    }

    fn status_probe_handshake() -> Vec<u8> {
        let mut payload = Vec::new();
        payload.push(0x0a);
        payload.extend_from_slice(b"8.4.11\0");
        payload.extend_from_slice(&1_u32.to_le_bytes());
        payload.extend_from_slice(&[0x11; 8]);
        payload.push(0x00);
        payload.extend_from_slice(&0x8208_u16.to_le_bytes());
        payload.push(0x21);
        payload.extend_from_slice(&0_u16.to_le_bytes());
        payload.extend_from_slice(&0x0900_u16.to_le_bytes());
        payload.push(21);
        payload.extend_from_slice(&[0; 10]);
        payload.extend_from_slice(&[0x22; 12]);
        payload.push(0x00);
        payload.extend_from_slice(b"mysql_native_password\0");
        payload
    }

    fn status_probe_column() -> Vec<u8> {
        let mut payload = Vec::new();
        for value in [
            b"def".as_slice(),
            b"".as_slice(),
            b"transaction_probe".as_slice(),
            b"transaction_probe".as_slice(),
            b"id".as_slice(),
            b"id".as_slice(),
        ] {
            payload.push(value.len() as u8);
            payload.extend_from_slice(value);
        }
        payload.push(0x0c);
        payload.extend_from_slice(&63_u16.to_le_bytes());
        payload.extend_from_slice(&11_u32.to_le_bytes());
        payload.push(0x03);
        payload.extend_from_slice(&0_u16.to_le_bytes());
        payload.push(0x00);
        payload.extend_from_slice(&[0x00, 0x00]);
        payload
    }

    fn read_mysql_packet(stream: &mut TcpStream) -> Vec<u8> {
        let mut header = [0; 4];
        stream.read_exact(&mut header).unwrap();
        let length =
            usize::from(header[0]) | (usize::from(header[1]) << 8) | (usize::from(header[2]) << 16);
        let mut payload = vec![0; length];
        stream.read_exact(&mut payload).unwrap();
        payload
    }

    fn write_mysql_packet(stream: &mut TcpStream, sequence: u8, payload: &[u8]) {
        assert!(payload.len() <= 0x00ff_ffff);
        let length = payload.len();
        let header = [
            (length & 0xff) as u8,
            ((length >> 8) & 0xff) as u8,
            ((length >> 16) & 0xff) as u8,
            sequence,
        ];
        stream.write_all(&header).unwrap();
        stream.write_all(payload).unwrap();
    }
}
