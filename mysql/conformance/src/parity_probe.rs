use std::{env, fs, net::IpAddr, path::PathBuf};

use anyhow::{anyhow, bail, Context, Result};
use futures::TryStreamExt;
use mysql_async::{prelude::Queryable, Conn, Error as DriverError, Opts, OptsBuilder, Row, Value};
use serde::Serialize;

const CREATE: &str = "CREATE TABLE mysql_parity_probe (id INT NOT NULL PRIMARY KEY, amount DECIMAL(20,6) NOT NULL, note VARCHAR(20) NULL) ENGINE=InnoDB";
const INSERT: &str = "INSERT INTO mysql_parity_probe (id, amount, note) VALUES (1, 1234567890.123456, NULL), (2, 2.000001, 'two')";
const SELECT: &str = "SELECT id, amount, note FROM mysql_parity_probe ORDER BY id";
const UPDATE: &str = "UPDATE mysql_parity_probe SET note = 'updated' WHERE id = 1";
const DUPLICATE: &str =
    "INSERT INTO mysql_parity_probe (id, amount, note) VALUES (1, 1.000000, 'duplicate')";
const DELETE: &str = "DELETE FROM mysql_parity_probe WHERE id = 2";
const DROP: &str = "DROP TABLE mysql_parity_probe";

#[derive(Serialize)]
struct Column {
    name: String,
    column_type: u8,
    column_length: u32,
    decimals: u8,
    null_and_primary_flags: u16,
}

#[derive(Serialize)]
struct ServerError {
    number: u16,
    sql_state: String,
}

#[derive(Serialize)]
struct Step {
    name: &'static str,
    columns: Option<Vec<Column>>,
    rows: Option<Vec<Vec<Option<String>>>>,
    affected_rows: Option<u64>,
    error: Option<ServerError>,
}

#[tokio::main]
async fn main() -> Result<()> {
    let mut args = env::args_os().skip(1);
    let dsn_env = args
        .next()
        .context("expected DSN environment variable name")?;
    let output = PathBuf::from(args.next().context("expected output JSON path")?);
    if args.next().is_some() {
        bail!("expected exactly a DSN environment variable name and output path");
    }
    let dsn_env = dsn_env
        .to_str()
        .context("DSN environment variable name must be UTF-8")?;
    if dsn_env != "MYSQL_ORACLE_DSN" && dsn_env != "TURSO_DSN" {
        bail!("DSN environment variable must be MYSQL_ORACLE_DSN or TURSO_DSN");
    }
    let dsn = env::var(dsn_env).with_context(|| format!("{dsn_env} is required"))?;
    let options = Opts::from_url(&dsn).map_err(|_| anyhow!("{dsn_env} is not a MySQL DSN"))?;
    let host = options.ip_or_hostname();
    let loopback = host
        .parse::<IpAddr>()
        .is_ok_and(|address| address.is_loopback());
    if options.socket().is_none() && !loopback {
        bail!("{dsn_env} must use a numeric loopback address or an explicit Unix socket");
    }
    let mut conn = Conn::new(OptsBuilder::from_opts(options).prefer_socket(false))
        .await
        .map_err(|_| anyhow!("failed to connect to {dsn_env}"))?;
    if dsn_env == "MYSQL_ORACLE_DSN" {
        let version: String = conn
            .query_first("SELECT VERSION()")
            .await?
            .context("Oracle returned no version")?;
        if !version.starts_with("8.4.11") {
            bail!("Oracle version is not MySQL 8.4.11");
        }
    }

    let mut steps = Vec::new();
    for (name, sql) in [
        ("create", CREATE),
        ("insert", INSERT),
        ("read", SELECT),
        ("update", UPDATE),
        ("read_after_update", SELECT),
        ("duplicate", DUPLICATE),
        ("delete", DELETE),
        ("read_after_delete", SELECT),
        ("drop", DROP),
    ] {
        steps.push(execute(&mut conn, name, sql).await?);
    }
    conn.disconnect().await?;
    fs::write(&output, serde_json::to_vec_pretty(&steps)?)?;
    println!("MySQL parity probe: {} observations written", steps.len());
    Ok(())
}

async fn execute(conn: &mut Conn, name: &'static str, sql: &str) -> Result<Step> {
    let mut result = match conn.query_iter(sql).await {
        Ok(result) => result,
        Err(DriverError::Server(error)) => {
            return Ok(Step {
                name,
                columns: None,
                rows: None,
                affected_rows: None,
                error: Some(ServerError {
                    number: error.code,
                    sql_state: error.state,
                }),
            });
        }
        Err(error) => return Err(error.into()),
    };
    let mut columns = None;
    let mut rows = None;
    let mut result_set_count = 0;
    while let Some(mut stream) = result.stream::<Row>().await? {
        result_set_count += 1;
        if result_set_count > 1 {
            bail!("probe statement returned multiple result sets");
        }
        let observed_columns: Vec<Column> = stream
            .columns_ref()
            .iter()
            .map(|column| Column {
                name: column.name_str().to_string(),
                column_type: column.column_type() as u8,
                column_length: column.column_length(),
                decimals: column.decimals(),
                null_and_primary_flags: column.flags().bits() & 0x0003,
            })
            .collect();
        let mut observed_rows = Vec::new();
        while let Some(row) = stream.try_next().await? {
            let mut observed_row = Vec::new();
            for value in row.unwrap() {
                observed_row.push(match value {
                    Value::NULL => None,
                    Value::Bytes(bytes) => Some(String::from_utf8(bytes)?),
                    Value::Int(value) => Some(value.to_string()),
                    Value::UInt(value) => Some(value.to_string()),
                    value => bail!("probe returned an unexpected value type: {value:?}"),
                });
            }
            observed_rows.push(observed_row);
        }
        if !observed_columns.is_empty() {
            columns = Some(observed_columns);
            rows = Some(observed_rows);
        }
    }
    let affected_rows = if columns.is_some() {
        None
    } else {
        Some(result.affected_rows())
    };
    Ok(Step {
        name,
        columns,
        rows,
        affected_rows,
        error: None,
    })
}
