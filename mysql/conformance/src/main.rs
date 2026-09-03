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
use mysql_async::{Column, Conn, Error as DriverError, Params, QueryResult, Row, Value};
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
const ORACLE_COMPOSE_FILE_ENV: &str = "MYSQL_CONFORMANCE_COMPOSE_FILE";
const REFERENCE_SERVER_VERSION_PREFIX: &str = "8.4.11";

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
        outcome.affected_rows = stream.affected_rows();
        outcome.last_insert_id = stream.last_insert_id().unwrap_or_default();
        outcome.warning_count = stream.get_warnings();
        if !columns.is_empty() {
            outcome.result = Some(ResultSet { columns, rows });
        }
    }

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
}
