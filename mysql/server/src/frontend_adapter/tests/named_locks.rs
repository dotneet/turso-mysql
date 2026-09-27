//! MySQL's named locks, shared by every session of one server.
//!
//! Every expectation here was measured on MySQL 8.4.11.

use super::*;

fn two_sessions() -> (
    tempfile::TempDir,
    AuthorizedDatabaseCommandAdapter<RecordingAuthorizer>,
    AuthorizedDatabaseCommandAdapter<RecordingAuthorizer>,
) {
    let authorizer = Arc::new(RecordingAuthorizer::default());
    let (directory, catalog, factory) = catalog_factory(authorizer.clone());
    let second_factory =
        AuthorizedDatabaseAdapterFactory::new(catalog, binary_context(), authorizer);
    let mut one = factory
        .build(AuthenticatedPrincipal::from_account_id_for_testing(
            AccountId::from_bytes([91; 32]),
        ))
        .unwrap();
    one.authorize_connection().unwrap();
    let mut two = second_factory
        .build(AuthenticatedPrincipal::from_account_id_for_testing(
            AccountId::from_bytes([92; 32]),
        ))
        .unwrap();
    two.authorize_connection().unwrap();
    (directory, one, two)
}

fn row(
    adapter: &mut AuthorizedDatabaseCommandAdapter<RecordingAuthorizer>,
    sql: &str,
) -> Vec<Option<String>> {
    let Ok(CommandExecutionResult::ResultSet(result)) = adapter.execute_query(sql) else {
        panic!("{sql} must return a result set");
    };
    result.rows[0]
        .iter()
        .map(|value| {
            value
                .as_ref()
                .map(|value| String::from_utf8(value.clone()).unwrap())
        })
        .collect()
}

fn one(value: &str) -> Vec<Option<String>> {
    vec![Some(value.to_owned())]
}

/// Rails takes its migration lock with a timeout of 0 and needs 1 back;
/// another process trying at the same time gets 0 and stops.
#[test]
fn one_session_holds_a_named_lock_until_it_lets_go() {
    let (_directory, mut one_session, mut two_session) = two_sessions();
    assert_eq!(
        row(
            &mut one_session,
            "SELECT GET_LOCK('7458657555131878620', 0)"
        ),
        one("1")
    );
    // Names are matched whatever their case, and one session may take a lock
    // it holds again.
    assert_eq!(
        row(
            &mut one_session,
            "SELECT GET_LOCK('7458657555131878620', 0)"
        ),
        one("1")
    );
    assert_eq!(
        row(
            &mut two_session,
            "SELECT GET_LOCK('7458657555131878620', 0), IS_FREE_LOCK('7458657555131878620'), IS_FREE_LOCK('nothing')"
        ),
        [Some("0".to_owned()), Some("0".to_owned()), Some("1".to_owned())]
    );
    assert_eq!(
        row(
            &mut two_session,
            "SELECT RELEASE_LOCK('7458657555131878620'), RELEASE_LOCK('nothing')"
        ),
        [Some("0".to_owned()), None]
    );

    // A lock is not a transaction's, so a rollback leaves it held.
    one_session.execute_init_db("REPORTS").unwrap();
    one_session.execute_query("START TRANSACTION").unwrap();
    one_session.execute_query("ROLLBACK").unwrap();
    assert_eq!(
        row(
            &mut two_session,
            "SELECT IS_FREE_LOCK('7458657555131878620')"
        ),
        one("0")
    );

    // Taken twice, so let go of twice.
    assert_eq!(
        row(
            &mut one_session,
            "SELECT RELEASE_LOCK('7458657555131878620')"
        ),
        one("1")
    );
    assert_eq!(
        row(
            &mut two_session,
            "SELECT GET_LOCK('7458657555131878620', 0)"
        ),
        one("0")
    );
    assert_eq!(
        row(
            &mut one_session,
            "SELECT RELEASE_LOCK('7458657555131878620')"
        ),
        one("1")
    );
    assert_eq!(
        row(
            &mut two_session,
            "SELECT GET_LOCK('7458657555131878620', 0) AS got"
        ),
        one("1")
    );
}

/// Prisma never lets go of its lock: it closes the connection. A reset lets
/// go of every lock too, and so does the session ending.
#[test]
fn a_session_lets_go_of_its_named_locks_when_it_resets_or_ends() {
    let (_directory, mut one_session, mut two_session) = two_sessions();
    assert_eq!(
        row(&mut one_session, "SELECT GET_LOCK('prisma_migrate', 10)"),
        one("1")
    );
    one_session.execute_reset_connection().unwrap();
    assert_eq!(
        row(&mut two_session, "SELECT GET_LOCK('prisma_migrate', 0)"),
        one("1")
    );
    drop(two_session);
    assert_eq!(
        row(&mut one_session, "SELECT GET_LOCK('prisma_migrate', 0)"),
        one("1")
    );
    assert_eq!(
        row(
            &mut one_session,
            "SELECT GET_LOCK('a', 0), GET_LOCK('a', 0), RELEASE_ALL_LOCKS()"
        ),
        [
            Some("1".to_owned()),
            Some("1".to_owned()),
            Some("3".to_owned())
        ]
    );
}

#[test]
fn a_named_lock_answers_mysqls_columns_and_errors() {
    let (_directory, mut session, _other) = two_sessions();
    let Ok(CommandExecutionResult::ResultSet(result)) =
        session.execute_query("SELECT GET_LOCK('m', 0), RELEASE_ALL_LOCKS()")
    else {
        panic!("the calls must return a result set");
    };
    assert_eq!(result.columns[0].name, "GET_LOCK('m', 0)");
    assert_eq!(result.columns[0].column_type, MYSQL_TYPE_LONGLONG);
    assert_eq!(result.columns[0].column_length, 1);
    assert_eq!(result.columns[0].flags, MYSQL_BINARY_FLAG | MYSQL_NUM_FLAG);
    assert_eq!(result.columns[1].column_length, 21);
    assert_eq!(
        result.columns[1].flags,
        MYSQL_NOT_NULL_FLAG | MYSQL_UNSIGNED_FLAG | MYSQL_BINARY_FLAG | MYSQL_NUM_FLAG
    );

    assert_eq!(
        row(
            &mut session,
            &format!("SELECT GET_LOCK('{}', 0)", "x".repeat(64))
        ),
        one("1")
    );
    for (sql, error) in [
        (
            format!("SELECT GET_LOCK('{}', 0)", "x".repeat(65)),
            FrontendErrorKind::UserLockNameTooLong,
        ),
        (
            "SELECT GET_LOCK('', 0)".to_owned(),
            FrontendErrorKind::IncorrectUserLockName,
        ),
        (
            "SELECT RELEASE_LOCK(NULL)".to_owned(),
            FrontendErrorKind::IncorrectUserLockName,
        ),
        (
            "SELECT IS_FREE_LOCK(NULL)".to_owned(),
            FrontendErrorKind::IncorrectUserLockName,
        ),
    ] {
        assert_eq!(session.execute_query(&sql), Err(error), "{sql}");
    }
}

/// Measured on MySQL 8.4.11: a session waits for the timeout it gave, and a
/// timeout of NULL waits no time at all.
#[test]
fn a_session_waits_for_a_named_lock_as_long_as_it_asked() {
    let (_directory, mut one_session, mut two_session) = two_sessions();
    assert_eq!(row(&mut one_session, "SELECT GET_LOCK('w', 0)"), one("1"));
    assert_eq!(
        row(&mut two_session, "SELECT GET_LOCK('w', NULL)"),
        one("0")
    );
    let waited = std::time::Instant::now();
    assert_eq!(row(&mut two_session, "SELECT GET_LOCK('w', 1)"), one("0"));
    assert!(waited.elapsed() >= Duration::from_secs(1));
}
