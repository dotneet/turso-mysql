//! A word written with the `_utf8mb4` introducer, and `BINARY col = 'x'`,
//! which compares a column by its bytes.
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
            AccountId::from_bytes([157; 32]),
        ))
        .unwrap();
    adapter.authorize_connection().unwrap();
    adapter.execute_init_db("REPORTS").unwrap();
    for sql in [
        "CREATE TABLE logins (id INT PRIMARY KEY, name VARCHAR(20), n INT)",
        "INSERT INTO logins VALUES (1, 'alpha', 1), (2, 'Alpha', 2), (3, 'ALPHA', 3), (4, 'alpha ', 4), (5, NULL, 5), (6, 'beta', 6)",
    ] {
        adapter
            .execute_query(sql)
            .unwrap_or_else(|error| panic!("{sql}: {error:?}"));
    }
    (directory, adapter)
}

fn ids(
    adapter: &mut AuthorizedDatabaseCommandAdapter<RecordingAuthorizer>,
    sql: &str,
) -> Vec<String> {
    let Ok(CommandExecutionResult::ResultSet(result)) = adapter.execute_query(sql) else {
        panic!("{sql} must return a result set");
    };
    let prepared = adapter
        .execute_stmt_prepare(sql)
        .unwrap_or_else(|error| panic!("prepare {sql}: {error:?}"));
    adapter.execute_stmt_close(prepared.statement_id);
    result
        .rows
        .into_iter()
        .map(|row| String::from_utf8(row[0].clone().unwrap()).unwrap())
        .collect()
}

/// Measured: an introducer names the character set the word is written in,
/// and the word keeps the collation a plain one has, so it is compared under
/// the column's.
#[test]
fn a_word_written_with_the_utf8mb4_introducer_is_the_word() {
    let (_directory, mut adapter) = adapter();
    for sql in [
        "SELECT id FROM logins WHERE name = _utf8mb4'alpha' ORDER BY id",
        "SELECT id FROM logins WHERE name = _utf8mb4 'ALPHA' ORDER BY id",
        "SELECT id FROM logins WHERE _utf8mb4'ALPHA' = name ORDER BY id",
        "SELECT id FROM logins WHERE name IN (_utf8mb4'ALPHA', 'gamma') ORDER BY id",
    ] {
        assert_eq!(ids(&mut adapter, sql), ["1", "2", "3"], "{sql}");
    }
    assert_eq!(
        ids(
            &mut adapter,
            "SELECT id FROM logins WHERE name LIKE _utf8mb4'ALPH%' ORDER BY id"
        ),
        ["1", "2", "3", "4"]
    );
    let Ok(CommandExecutionResult::ResultSet(result)) =
        adapter.execute_query("SELECT _utf8mb4'abc'")
    else {
        panic!("a result set");
    };
    assert_eq!(result.rows, [[Some(b"abc".to_vec())]]);
    // Another character set, and the binary one, change how the word is
    // compared.
    for sql in [
        "SELECT id FROM logins WHERE name = _latin1'alpha'",
        "SELECT id FROM logins WHERE name = _binary'alpha'",
        // Only a SELECT reads a word without its introducer.
        "UPDATE logins SET n = 0 WHERE name = _utf8mb4'alpha'",
        "DELETE FROM logins WHERE name = _utf8mb4'alpha'",
    ] {
        assert!(adapter.execute_query(sql).is_err(), "{sql}");
    }
}

/// Measured: `BINARY` casts the column alone, so the comparison is by bytes,
/// and a binary string keeps its trailing spaces where `utf8mb4_bin` pads
/// them away.
#[test]
fn binary_compares_the_column_by_its_bytes() {
    let (_directory, mut adapter) = adapter();
    for (sql, expected) in [
        (
            "SELECT id FROM logins WHERE BINARY name = 'alpha' ORDER BY id",
            &["1"][..],
        ),
        (
            "SELECT id FROM logins WHERE BINARY name = 'alpha ' ORDER BY id",
            &["4"],
        ),
        (
            "SELECT id FROM logins WHERE BINARY name <> 'alpha' ORDER BY id",
            &["2", "3", "4", "6"],
        ),
        (
            "SELECT id FROM logins WHERE BINARY name < 'alpha' ORDER BY id",
            &["2", "3"],
        ),
        (
            "SELECT id FROM logins WHERE BINARY name = 'alpha' AND n > 0 ORDER BY id",
            &["1"],
        ),
        (
            "SELECT id FROM logins WHERE name = 'alpha' ORDER BY id",
            &["1", "2", "3"],
        ),
    ] {
        assert_eq!(ids(&mut adapter, sql), expected, "{sql}");
    }
    // `CAST(name = 'ALPHA' AS BINARY)` casts the comparison's answer, which
    // MySQL reads as the case-ignoring comparison; a bound value and a number
    // are not words written out; and the `BINARY` on the right of a
    // comparison is a shape sqlparser reads differently.
    for sql in [
        "SELECT id FROM logins WHERE CAST(name = 'ALPHA' AS BINARY)",
        "SELECT id FROM logins WHERE BINARY name = ?",
        "SELECT id FROM logins WHERE BINARY n = 1",
        "SELECT id FROM logins WHERE 'alpha' = BINARY name",
    ] {
        assert!(adapter.execute_query(sql).is_err(), "{sql}");
    }
}
