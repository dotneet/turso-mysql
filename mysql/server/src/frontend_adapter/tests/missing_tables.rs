//! A statement naming a table or a column that is not there.
//!
//! Every expectation here was measured on MySQL 8.4.11, which reports
//! `lower_case_table_names=1` the way this server does.

use super::*;

type Adapter = AuthorizedDatabaseCommandAdapter<RecordingAuthorizer>;

fn adapter() -> (tempfile::TempDir, Adapter) {
    adapter_with(RecordingAuthorizer::with_schema_creator("root"))
}

fn adapter_with(authorizer: RecordingAuthorizer) -> (tempfile::TempDir, Adapter) {
    let (directory, catalog, factory) = catalog_factory(Arc::new(authorizer));
    catalog.create("probe").unwrap();
    let mut seed = catalog.new_session(binary_context());
    seed.select_database("probe").unwrap();
    seed.connection()
        .unwrap()
        .execute("CREATE TABLE posts (id INT PRIMARY KEY, title VARCHAR(20))")
        .unwrap();
    drop(seed);
    let mut adapter = factory
        .build(AuthenticatedPrincipal::from_account_id_for_testing(
            AccountId::from_bytes([153; 32]),
        ))
        .unwrap();
    adapter.authorize_connection().unwrap();
    adapter.execute_init_db("probe").unwrap();
    (directory, adapter)
}

/// The code, SQLSTATE and message of the error `sql` answers, as a client
/// reads it, sent as a query and prepared.
fn answers(adapter: &mut Adapter, sql: &str) -> [(u16, String, String); 2] {
    [COM_QUERY, crate::COM_STMT_PREPARE].map(|command| {
        let mut connection = ready_connection();
        let mut payload = vec![command];
        payload.extend_from_slice(sql.as_bytes());
        let codec = PacketCodec::new(4096).unwrap();
        let frame = codec.encode(COMMAND_SEQUENCE_ID, &payload).unwrap();
        let frames = dispatch_command_frame(&mut connection, adapter, &frame).unwrap();
        let error = crate::ErrPacket::decode(
            codec,
            &frames[0],
            REQUIRED_CLIENT_HANDSHAKE_RESPONSE_CAPABILITIES,
        )
        .unwrap_or_else(|_| panic!("{sql} must answer an error"));
        (
            error.error_code,
            String::from_utf8(error.sql_state.unwrap().to_vec()).unwrap(),
            String::from_utf8(error.message).unwrap(),
        )
    })
}

#[test]
fn a_statement_naming_a_table_that_is_not_there_answers_1146() {
    let (_directory, mut adapter) = adapter();
    for (sql, table) in [
        ("SELECT id FROM no_such_table", "no_such_table"),
        ("SELECT * FROM Missing", "missing"),
        ("SELECT COUNT(*) FROM missing WHERE id = 1", "missing"),
        ("INSERT INTO missing VALUES (1)", "missing"),
        ("INSERT INTO missing (a) VALUES (1)", "missing"),
        ("UPDATE missing SET a = 1", "missing"),
        ("DELETE FROM missing WHERE a = 1", "missing"),
        ("DESCRIBE missing", "missing"),
        ("SHOW COLUMNS FROM missing", "missing"),
        ("SHOW INDEX FROM missing", "missing"),
        ("SHOW CREATE TABLE missing", "missing"),
        (
            "SELECT p.id FROM posts p JOIN missing m ON m.id = p.id",
            "missing",
        ),
        (
            "SELECT id FROM posts WHERE id IN (SELECT id FROM missing)",
            "missing",
        ),
        ("INSERT INTO posts SELECT * FROM missing", "missing"),
        ("SELECT id FROM probe.missing", "missing"),
    ] {
        let [query, prepared] = answers(&mut adapter, sql);
        let expected = (
            1146,
            "42S02".to_owned(),
            format!("Table 'probe.{table}' doesn't exist"),
        );
        assert_eq!(query, expected, "{sql}");
        if !sql.starts_with("SHOW") && !sql.starts_with("DESCRIBE") {
            assert_eq!(prepared, expected, "prepared {sql}");
        }
    }
}

#[test]
fn a_statement_naming_a_column_its_table_has_not_got_answers_1054() {
    let (_directory, mut adapter) = adapter();
    for (sql, message) in [
        (
            "SELECT nope FROM posts",
            "Unknown column 'nope' in 'field list'",
        ),
        (
            "SELECT id FROM posts WHERE nope = 1",
            "Unknown column 'nope' in 'where clause'",
        ),
        (
            "SELECT id FROM posts p WHERE p.nope = 1",
            "Unknown column 'p.nope' in 'where clause'",
        ),
        (
            "UPDATE posts SET nope = 1 WHERE nope2 = 1",
            "Unknown column 'nope2' in 'where clause'",
        ),
        (
            "UPDATE posts SET title = nope3 WHERE id = 1",
            "Unknown column 'nope3' in 'field list'",
        ),
        (
            "INSERT INTO posts (id, nope) VALUES (1, 2)",
            "Unknown column 'nope' in 'field list'",
        ),
        (
            "DELETE FROM posts WHERE p.id = 1",
            "Unknown column 'p.id' in 'where clause'",
        ),
        (
            "SELECT nope.id FROM posts",
            "Unknown column 'nope.id' in 'field list'",
        ),
    ] {
        for answer in answers(&mut adapter, sql) {
            assert_eq!(
                answer,
                (1054, "42S22".to_owned(), message.to_owned()),
                "{sql}"
            );
        }
    }
}

/// A temporary table and a view are tables a statement may name, so a
/// statement over one refused for another reason keeps its own answer.
#[test]
fn a_temporary_table_and_a_view_are_not_missing() {
    let (_directory, mut adapter) = adapter();
    adapter
        .execute_query("CREATE TEMPORARY TABLE scratch (id INT, note VARCHAR(10))")
        .unwrap();
    adapter
        .execute_query("CREATE VIEW titles AS SELECT id, title FROM posts")
        .unwrap();
    let [(code, ..), _] = answers(&mut adapter, "SELECT id FROM scratch WHERE nope = 1");
    assert_ne!(code, 1146);
    assert_eq!(
        answers(&mut adapter, "SELECT id FROM titles WHERE nope = 1")[0],
        (
            1054,
            "42S22".to_owned(),
            "Unknown column 'nope' in 'where clause'".to_owned()
        )
    );
}

/// Looking a table up to answer 1146 changes nothing the session holds: a
/// statement prepared before or after runs as it would have.
#[test]
fn a_statement_prepared_around_a_1146_still_runs() {
    let (_directory, mut adapter) = adapter();
    adapter
        .execute_query("INSERT INTO posts VALUES (1, 'first')")
        .unwrap();
    assert_eq!(answers(&mut adapter, "SELECT id FROM missing")[0].0, 1146);
    let prepared = adapter
        .execute_stmt_prepare("SELECT title FROM posts WHERE id = ?")
        .unwrap();
    let mut one = vec![0, 1, MYSQL_TYPE_LONGLONG, 0];
    one.extend_from_slice(&1i64.to_le_bytes());
    for _ in 0..2 {
        assert_eq!(
            prepared_result_set(
                adapter
                    .execute_stmt_execute(prepared.statement_id, &one)
                    .unwrap()
            )
            .rows,
            [vec![BinaryResultValue::Text("first".to_owned())]]
        );
        assert_eq!(answers(&mut adapter, "SELECT id FROM missing")[0].0, 1146);
    }
}

/// A statement refused before it is authorized — here one outside ASCII from
/// a session that named latin1 — is answered 1146 only when the session may
/// query the database, so a session that may not learns nothing of which
/// tables are there.
#[test]
fn only_a_session_that_may_query_the_database_is_told_a_table_is_not_there() {
    let sql = "SELECT 'é' FROM missing";
    let (_directory, mut allowed) = adapter();
    allowed.take_client_collation(8).unwrap();
    assert_eq!(answers(&mut allowed, sql)[0].0, 1146);

    let (_directory, mut denied) = adapter_with(RecordingAuthorizer::with_decisions(
        [Ok(()), Ok(())]
            .into_iter()
            .chain(std::iter::repeat_n(Err(AuthorizationError::Denied), 8)),
    ));
    denied.take_client_collation(8).unwrap();
    assert_eq!(
        denied.execute_query(sql),
        Err(FrontendErrorKind::Unsupported)
    );
    assert_eq!(denied.take_error_message(), None);
}
