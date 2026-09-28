//! What database GUIs — DBeaver, MySQL Workbench, TablePlus — send when a
//! user browses a schema, modelled on their general-log output.
//!
//! Every expectation here was measured on MySQL 8.4.11.

use super::*;

type Adapter = AuthorizedDatabaseCommandAdapter<RecordingAuthorizer>;

fn adapter() -> (tempfile::TempDir, Adapter) {
    let authorizer = Arc::new(RecordingAuthorizer::with_schema_creator("root"));
    let (directory, catalog, factory) = catalog_factory(authorizer);
    catalog.create("dbtools").unwrap();
    let mut adapter = factory
        .build(AuthenticatedPrincipal::from_account_id_for_testing(
            AccountId::from_bytes([156; 32]),
        ))
        .unwrap();
    adapter.authorize_connection().unwrap();
    adapter.execute_init_db("dbtools").unwrap();
    for sql in [
        "CREATE TABLE users (id BIGINT NOT NULL AUTO_INCREMENT PRIMARY KEY, email VARCHAR(191) NOT NULL, is_active TINYINT(1) NOT NULL DEFAULT 1)",
        "CREATE VIEW active_users AS SELECT id, email FROM users WHERE is_active = 1",
        "INSERT INTO users (email) VALUES ('alice@example.com'), ('bob@example.com')",
    ] {
        adapter
            .execute_query(sql)
            .unwrap_or_else(|error| panic!("{sql}: {error:?}"));
    }
    (directory, adapter)
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

/// TablePlus reads one table's status by name. Measured: the name is
/// compared by its bytes, so `'USERS'` finds nothing, where `LIKE` would.
#[test]
fn tableplus_reads_one_table_s_status_by_its_name() {
    let (_directory, mut adapter) = adapter();
    let named = rows(
        &mut adapter,
        "SHOW TABLE STATUS FROM `dbtools` WHERE Name = 'users'",
    );
    assert_eq!(
        named,
        rows(
            &mut adapter,
            "SHOW TABLE STATUS FROM `dbtools` LIKE 'users'"
        )
    );
    assert_eq!(named.len(), 1);
    assert!(rows(
        &mut adapter,
        "SHOW TABLE STATUS FROM `dbtools` WHERE Name = 'USERS'"
    )
    .is_empty());
    assert!(adapter
        .execute_query("SHOW TABLE STATUS WHERE Engine = 'InnoDB'")
        .is_err());
}

/// Measured: a view's status is its name and the comment `VIEW`, every other
/// figure NULL. It read as an InnoDB table holding no rows.
#[test]
fn a_view_s_status_is_its_name_and_view() {
    let (_directory, mut adapter) = adapter();
    let mut expected = vec![None; 18];
    expected[0] = Some("active_users".to_owned());
    expected[17] = Some("VIEW".to_owned());
    assert_eq!(
        rows(&mut adapter, "SHOW TABLE STATUS LIKE 'active_users'"),
        [expected]
    );
}

/// DBeaver reads a database's triggers whole. It answered 1064, the table
/// being unknown. Measured: one row for each trigger, holding what `SHOW
/// TRIGGERS` shows for it beside MySQL's constants — the first trigger of
/// its table, event and timing, fired for each `ROW`, reading `OLD` and
/// `NEW` — over both protocols.
#[test]
fn dbeaver_reads_the_triggers_of_a_database() {
    let (_directory, mut adapter) = adapter();
    for sql in [
        "CREATE TABLE audit (id INT NOT NULL PRIMARY KEY, what VARCHAR(20))",
        "CREATE TRIGGER users_ai AFTER INSERT ON users FOR EACH ROW INSERT INTO audit (id, what) VALUES (NEW.id, 'added')",
    ] {
        adapter
            .execute_query(sql)
            .unwrap_or_else(|error| panic!("{sql}: {error:?}"));
    }
    let shown = rows(&mut adapter, "SHOW TRIGGERS");
    let [shown] = shown.as_slice() else {
        panic!("one trigger: {shown:?}");
    };
    let text = |value: &str| Some(value.to_owned());
    let expected = vec![
        text("def"),
        text("dbtools"),
        text("users_ai"),
        text("INSERT"),
        text("def"),
        text("dbtools"),
        text("users"),
        text("1"),
        None,
        text("INSERT INTO audit (id, what) VALUES (NEW.id, 'added')"),
        text("ROW"),
        text("AFTER"),
        None,
        None,
        text("OLD"),
        text("NEW"),
        shown[5].clone(),
        shown[6].clone(),
        text("root@%"),
        shown[8].clone(),
        shown[9].clone(),
        shown[10].clone(),
    ];
    assert_eq!(
        rows(
            &mut adapter,
            "SELECT * FROM information_schema.TRIGGERS WHERE TRIGGER_SCHEMA='dbtools'"
        ),
        [expected]
    );

    let statement = adapter
        .execute_stmt_prepare("SELECT * FROM information_schema.TRIGGERS WHERE TRIGGER_SCHEMA=?")
        .unwrap();
    assert_eq!(statement.columns.len(), 22);
    assert_eq!(
        (
            statement.columns[16].column_type,
            statement.columns[16].column_length,
            statement.columns[16].decimals
        ),
        (MYSQL_TYPE_TIMESTAMP, 22, 2)
    );
    let mut dbtools = vec![0, 1, MYSQL_TYPE_VAR_STRING, 0, 7];
    dbtools.extend(b"dbtools");
    let PreparedStatementExecutionResult::ResultSet(read) = adapter
        .execute_stmt_execute(statement.statement_id, &dbtools)
        .unwrap()
    else {
        panic!("the triggers must read back");
    };
    assert_eq!(read.rows.len(), 1);
    assert_eq!(read.rows[0][7], BinaryResultValue::Integer(1));
}

/// Connector/J asks for results in each column's own character set, and a
/// catalog column MySQL works out rather than reads — `VIEWS`'s
/// `VIEW_DEFINITION`, `EVENTS`'s `EVENT_BODY` — names its table and no
/// database. The server looked for a user table of that name to read the
/// column's collation from, and answered 1235: DBeaver could not list a
/// database's views.
#[test]
fn a_jdbc_session_reads_the_views_and_events_of_a_database() {
    let (_directory, mut adapter) = adapter();
    adapter
        .execute_query("SET character_set_results = NULL")
        .unwrap();
    let views = rows(
        &mut adapter,
        "SELECT * FROM information_schema.VIEWS WHERE TABLE_SCHEMA='dbtools'",
    );
    assert_eq!(views.len(), 1);
    assert_eq!(views[0][2].as_deref(), Some("active_users"));
    assert!(rows(
        &mut adapter,
        "SELECT * FROM information_schema.EVENTS WHERE EVENT_SCHEMA='dbtools'"
    )
    .is_empty());
}

/// MySQL lists a trigger only to a session holding the `TRIGGER` privilege on
/// its table, which a session here holds only through the whole database: one
/// granted the table alone sees no trigger.
#[test]
fn a_session_granted_tables_alone_sees_no_trigger() {
    let authorizer = Arc::new(RecordingAuthorizer::with_schema_creator("root"));
    let (_directory, catalog, factory) = catalog_factory(authorizer);
    let mut owner = factory
        .build(AuthenticatedPrincipal::from_account_id_for_testing(
            AccountId::from_bytes([157; 32]),
        ))
        .unwrap();
    owner.authorize_connection().unwrap();
    owner.execute_init_db("reports").unwrap();
    for sql in [
        "CREATE TABLE audit (id INT NOT NULL PRIMARY KEY)",
        "CREATE TRIGGER records_ai AFTER INSERT ON records FOR EACH ROW INSERT INTO audit (id) VALUES (NEW.id)",
    ] {
        owner
            .execute_query(sql)
            .unwrap_or_else(|error| panic!("{sql}: {error:?}"));
    }
    assert_eq!(
        rows(
            &mut owner,
            "SELECT TRIGGER_NAME FROM information_schema.TRIGGERS"
        )
        .len(),
        1
    );
    drop(owner);

    let granted = Arc::new(RecordingAuthorizer::with_decisions_and_table_decisions(
        [Ok(()), Ok(()), Err(AuthorizationError::Denied)],
        [Ok(()), Ok(())],
    ));
    let mut reader = AuthorizedDatabaseAdapterFactory::new(catalog, binary_context(), granted)
        .build(AuthenticatedPrincipal::from_account_id_for_testing(
            AccountId::from_bytes([158; 32]),
        ))
        .unwrap();
    reader.authorize_connection().unwrap();
    reader.execute_init_db("reports").unwrap();
    assert!(rows(
        &mut reader,
        "SELECT TRIGGER_NAME FROM information_schema.TRIGGERS"
    )
    .is_empty());
}
