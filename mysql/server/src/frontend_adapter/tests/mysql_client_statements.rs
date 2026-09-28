//! What the `mysql` command-line client 8.4 sends besides the statements it is
//! given: each comment line of a script as a statement of its own, since it
//! keeps comments by default, and `select $$` as it connects, to learn whether
//! the server reads `$tag$` as a quote.
//!
//! Every expectation here was measured on MySQL 8.4.11 by sending the same text
//! as one `COM_QUERY`.

use super::*;

type Adapter = AuthorizedDatabaseCommandAdapter<RecordingAuthorizer>;

fn adapter() -> (tempfile::TempDir, Adapter) {
    let authorizer = Arc::new(RecordingAuthorizer::default());
    let (directory, _catalog, factory) = catalog_factory(authorizer);
    let mut adapter = factory
        .build(AuthenticatedPrincipal::from_account_id_for_testing(
            AccountId::from_bytes([83; 32]),
        ))
        .unwrap();
    adapter.authorize_connection().unwrap();
    (directory, adapter)
}

fn ok(adapter: &mut Adapter, sql: &str) -> CommandOkResult {
    match adapter.execute_query(sql) {
        Ok(CommandExecutionResult::Ok(result)) => result,
        other => panic!("{sql:?}: {other:?}"),
    }
}

fn rows(adapter: &mut Adapter, sql: &str) -> Vec<Vec<Option<String>>> {
    let Ok(CommandExecutionResult::ResultSet(result)) = adapter.execute_query(sql) else {
        panic!("{sql} must return a result set");
    };
    result
        .rows
        .into_iter()
        .map(|row| {
            row.into_iter()
                .map(|value| value.map(|value| String::from_utf8(value).unwrap()))
                .collect()
        })
        .collect()
}

/// A comment line answers an OK with nothing affected and no warning, with or
/// without a database selected, and inside a transaction it leaves the
/// transaction open. It clears the warnings the statement before it left, and
/// `ROW_COUNT()` reads 0 after it.
#[test]
fn a_comment_line_answers_ok_and_changes_nothing() {
    let (_directory, mut adapter) = adapter();
    for sql in [
        "-- MySQL dump 10.13  Distrib 8.4.11, for Linux (aarch64)",
        "--",
        "-- Server version\t8.4.11",
        "# a note",
        "/* a note */",
        "/*!99999 SET @a = 1 */",
        "-- a\n;",
    ] {
        let answer = ok(&mut adapter, sql);
        assert_eq!(
            (answer.affected_rows, answer.last_insert_id, answer.warnings),
            (0, 0, 0),
            "{sql:?}"
        );
        assert_eq!(answer.status_flags, SERVER_STATUS_AUTOCOMMIT, "{sql:?}");
    }
    assert_eq!(rows(&mut adapter, "SELECT @a"), [[None]]);

    ok(&mut adapter, "CREATE DATABASE scripts");
    ok(&mut adapter, "USE scripts");
    ok(&mut adapter, "CREATE TABLE t (id INT NOT NULL PRIMARY KEY)");
    assert_eq!(ok(&mut adapter, "DROP TABLE IF EXISTS missing").warnings, 1);
    ok(&mut adapter, "-- c");
    assert!(rows(&mut adapter, "SHOW WARNINGS").is_empty());
    assert_eq!(
        ok(&mut adapter, "INSERT INTO t VALUES (1), (2)").affected_rows,
        2
    );
    ok(&mut adapter, "-- c");
    assert_eq!(
        rows(&mut adapter, "SELECT ROW_COUNT()"),
        [[Some("0".to_owned())]]
    );

    ok(&mut adapter, "BEGIN");
    ok(&mut adapter, "INSERT INTO t VALUES (3)");
    let inside = ok(&mut adapter, "-- in a transaction");
    assert_ne!(inside.status_flags & SERVER_STATUS_IN_TRANS, 0);
    ok(&mut adapter, "ROLLBACK");
    assert_eq!(
        rows(&mut adapter, "SELECT COUNT(*) FROM t"),
        [[Some("2".to_owned())]]
    );
}

/// Text holding no comment and no statement is 1065, and `ROW_COUNT()` reads
/// -1 after it, as after any error.
#[test]
fn text_with_nothing_in_it_is_an_empty_query() {
    let (_directory, mut adapter) = adapter();
    for sql in ["", "   ", "\n", ";", " ; "] {
        assert_eq!(
            adapter.execute_query(sql),
            Err(FrontendErrorKind::EmptyQuery),
            "{sql:?}"
        );
    }
    assert_eq!(
        rows(&mut adapter, "SELECT ROW_COUNT()"),
        [[Some("-1".to_owned())]]
    );
}

/// A comment after a semicolon, which MySQL answers 1064, is not taken for a
/// comment line.
#[test]
fn a_comment_after_a_semicolon_is_not_a_comment_line() {
    let (_directory, mut adapter) = adapter();
    for sql in ["; -- a", ";/* c */"] {
        assert!(adapter.execute_query(sql).is_err(), "{sql}");
    }
}

/// The client sends a comment line with whatever bytes the script holds. A
/// comment is not read, so MySQL answers it OK under latin1 too, where a
/// statement outside ASCII is refused here.
#[test]
fn a_comment_line_is_answered_whatever_the_client_character_set() {
    let (_directory, mut adapter) = adapter();
    adapter.take_client_collation(8).unwrap();
    ok(&mut adapter, "-- Café");
    assert_eq!(
        adapter.execute_query("SELECT 'Café'"),
        Err(FrontendErrorKind::Unsupported)
    );
}

/// The client asks `select $$` as it connects and reads 1064 as the server
/// taking `$tag$` for a quote, which MySQL 8.4 does: from then on it keeps a
/// `;` between two `$$` inside the statement it is reading.
#[test]
fn select_dollar_dollar_is_a_syntax_error() {
    let (_directory, mut adapter) = adapter();
    assert_eq!(
        adapter.execute_query("select $$"),
        Err(FrontendErrorKind::Syntax)
    );
    ok(&mut adapter, "CREATE DATABASE scripts");
    ok(&mut adapter, "USE scripts");
    for sql in ["select $$", "select $a$", "select 1 as $$"] {
        assert_eq!(
            adapter.execute_query(sql),
            Err(FrontendErrorKind::Syntax),
            "{sql}"
        );
    }
    assert_eq!(rows(&mut adapter, "select '$$'"), [[Some("$$".to_owned())]]);
}
