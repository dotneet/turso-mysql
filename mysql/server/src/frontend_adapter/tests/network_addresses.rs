//! `INET_ATON`, `INET_NTOA` and `IS_IPV4`, which store and check an IPv4
//! address as a number.
//!
//! Every expectation here was measured on MySQL 8.4.11.

use super::*;

fn adapter() -> (
    tempfile::TempDir,
    AuthorizedDatabaseCommandAdapter<RecordingAuthorizer>,
) {
    let authorizer = Arc::new(RecordingAuthorizer::default());
    let (directory, _catalog, factory) = catalog_factory(authorizer);
    let mut adapter = factory
        .build(AuthenticatedPrincipal::from_account_id_for_testing(
            AccountId::from_bytes([159; 32]),
        ))
        .unwrap();
    adapter.authorize_connection().unwrap();
    adapter.execute_init_db("REPORTS").unwrap();
    for sql in [
        "CREATE TABLE ips (id INT PRIMARY KEY, ip VARCHAR(45), ipn VARCHAR(45) NOT NULL, num INT UNSIGNED, bnum BIGINT, t TEXT)",
        "INSERT INTO ips VALUES (1, '10.0.0.1', '192.168.1.255', 167772161, 4294967295, '1.2.3.4'), (2, NULL, 'nope', NULL, 4294967296, NULL), (3, '10.0.1', '::1', 0, -1, '255.255.255.255'), (4, '10.0.0.256', ' 10.0.0.1', 1, 16909060, '1.2.3.4.5')",
    ] {
        adapter
            .execute_query(sql)
            .unwrap_or_else(|error| panic!("{sql}: {error:?}"));
    }
    (directory, adapter)
}

fn result(
    adapter: &mut AuthorizedDatabaseCommandAdapter<RecordingAuthorizer>,
    sql: &str,
) -> TextResultSet {
    match adapter.execute_query(sql) {
        Ok(CommandExecutionResult::ResultSet(result)) => result,
        other => panic!("{sql} must return a result set, answered {other:?}"),
    }
}

fn rows(
    adapter: &mut AuthorizedDatabaseCommandAdapter<RecordingAuthorizer>,
    sql: &str,
) -> Vec<Vec<String>> {
    result(adapter, sql)
        .rows
        .into_iter()
        .map(|row| {
            row.into_iter()
                .map(|value| match value {
                    Some(value) => String::from_utf8(value).unwrap(),
                    None => "NULL".to_owned(),
                })
                .collect()
        })
        .collect()
}

fn shapes(
    adapter: &mut AuthorizedDatabaseCommandAdapter<RecordingAuthorizer>,
    sql: &str,
) -> Vec<(u8, u32, u16)> {
    let text = result(adapter, sql);
    let prepared = adapter.execute_stmt_prepare(sql).unwrap();
    assert_eq!(prepared.columns, text.columns, "{sql}");
    adapter.execute_stmt_close(prepared.statement_id);
    text.columns
        .iter()
        .map(|column| (column.column_type, column.column_length, column.flags))
        .collect()
}

/// Measured: the short forms fill the last byte from the last group, and a
/// word that is no address answers NULL.
#[test]
fn inet_aton_reads_an_address_as_a_number() {
    let (_directory, mut adapter) = adapter();
    let sql = "SELECT INET_ATON(ip), INET_ATON(ipn), INET_ATON(t), INET_ATON('10.0.0.1') FROM ips ORDER BY id";
    assert_eq!(
        rows(&mut adapter, sql),
        [
            ["167772161", "3232236031", "16909060", "167772161"],
            ["NULL", "NULL", "NULL", "167772161"],
            ["167772161", "NULL", "4294967295", "167772161"],
            ["NULL", "NULL", "NULL", "167772161"],
        ]
    );
    let unsigned = MYSQL_UNSIGNED_FLAG | MYSQL_BINARY_FLAG | MYSQL_NUM_FLAG;
    assert_eq!(
        shapes(&mut adapter, sql),
        [(MYSQL_TYPE_LONGLONG, 21, unsigned); 4]
    );
    assert_eq!(
        rows(
            &mut adapter,
            "SELECT INET_ATON('127.1'), INET_ATON('1.2.3'), INET_ATON('1..2'), INET_ATON('.1'), INET_ATON('0001.2.3.4')"
        ),
        [["2130706433", "16908291", "16777218", "1", "16909060"]]
    );
}

#[test]
fn inet_ntoa_writes_a_number_out_as_an_address() {
    let (_directory, mut adapter) = adapter();
    let sql = "SELECT INET_NTOA(num), INET_NTOA(bnum), INET_NTOA(167772161) FROM ips ORDER BY id";
    assert_eq!(
        rows(&mut adapter, sql),
        [
            ["10.0.0.1", "255.255.255.255", "10.0.0.1"],
            ["NULL", "NULL", "10.0.0.1"],
            ["0.0.0.0", "NULL", "10.0.0.1"],
            ["0.0.0.1", "1.2.3.4", "10.0.0.1"],
        ]
    );
    assert_eq!(
        shapes(&mut adapter, sql),
        [(MYSQL_TYPE_VAR_STRING, 124, 0); 3]
    );
}

#[test]
fn is_ipv4_takes_four_groups_of_at_most_three_digits() {
    let (_directory, mut adapter) = adapter();
    let sql = "SELECT IS_IPV4(ip), IS_IPV4(ipn), IS_IPV4('10.0.0.1'), IS_IPV4(t), IS_IPV4('010.0.0.1'), IS_IPV4('1.2.3.0004') FROM ips ORDER BY id";
    assert_eq!(
        rows(&mut adapter, sql),
        [
            ["1", "1", "1", "1", "1", "0"],
            ["NULL", "0", "1", "NULL", "1", "0"],
            ["0", "0", "1", "1", "1", "0"],
            ["0", "0", "1", "0", "1", "0"],
        ]
    );
    let number = MYSQL_BINARY_FLAG | MYSQL_NUM_FLAG;
    let not_null = number | MYSQL_NOT_NULL_FLAG;
    assert_eq!(
        shapes(&mut adapter, sql),
        [
            (MYSQL_TYPE_LONGLONG, 1, number),
            (MYSQL_TYPE_LONGLONG, 1, not_null),
            (MYSQL_TYPE_LONGLONG, 1, not_null),
            (MYSQL_TYPE_LONGLONG, 1, number),
            (MYSQL_TYPE_LONGLONG, 1, not_null),
            (MYSQL_TYPE_LONGLONG, 1, not_null),
        ]
    );
}

/// Measured: a written value that is no address answers NULL with warning
/// 1411, which is not raised here; and MySQL writes a number out before it
/// reads an address from it.
#[test]
fn a_written_value_that_warns_or_a_number_read_as_an_address_is_refused() {
    let (_directory, mut adapter) = adapter();
    for sql in [
        "SELECT INET_ATON('10.0.0.256')",
        "SELECT INET_NTOA(-1)",
        "SELECT INET_ATON(num) FROM ips",
        "SELECT INET_NTOA(ip) FROM ips",
    ] {
        assert!(adapter.execute_query(sql).is_err(), "{sql}");
    }
}
