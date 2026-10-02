use super::*;

type Adapter = AuthorizedDatabaseCommandAdapter<RecordingAuthorizer>;

struct Sessions {
    _directory: tempfile::TempDir,
    one: Adapter,
    two: Adapter,
    three: Adapter,
}

fn sessions() -> Option<Sessions> {
    if !turso_mysql::databases_open_in_mvcc() {
        return None;
    }
    let authorizer = Arc::new(RecordingAuthorizer::default());
    let (directory, catalog, _factory) = catalog_factory(authorizer.clone());
    catalog.create("shop").unwrap();
    let session = |byte: u8| {
        let mut adapter = AuthorizedDatabaseAdapterFactory::new(
            catalog.clone(),
            binary_context(),
            authorizer.clone(),
        )
        .build(AuthenticatedPrincipal::from_account_id_for_testing(
            AccountId::from_bytes([byte; 32]),
        ))
        .unwrap();
        adapter.authorize_connection().unwrap();
        adapter.execute_init_db("shop").unwrap();
        adapter
    };
    let mut one = session(91);
    let two = session(92);
    let three = session(93);
    for sql in [
        "CREATE TABLE t (id INT NOT NULL PRIMARY KEY, v INT)",
        "CREATE TABLE u (id INT NOT NULL PRIMARY KEY, v INT)",
        "INSERT INTO t (id, v) VALUES (1, 1), (2, 2)",
        "INSERT INTO u (id, v) VALUES (1, 1), (2, 2)",
    ] {
        run(&mut one, sql);
    }
    Some(Sessions {
        _directory: directory,
        one,
        two,
        three,
    })
}

fn run(adapter: &mut Adapter, sql: &str) {
    match adapter.execute_query(sql) {
        Ok(_) => {}
        Err(error) => panic!("{sql}: {error:?} {:?}", adapter.take_error_message()),
    }
}

fn count(adapter: &mut Adapter, table: &str) -> Result<String, FrontendErrorKind> {
    let CommandExecutionResult::ResultSet(result) =
        adapter.execute_query(&format!("SELECT COUNT(*) FROM {table}"))?
    else {
        panic!("a count must return a result set");
    };
    Ok(String::from_utf8(result.rows[0][0].clone().unwrap()).unwrap())
}

fn in_the_background(
    mut adapter: Adapter,
    sql: &'static str,
) -> std::thread::JoinHandle<(Adapter, Result<(), FrontendErrorKind>)> {
    std::thread::spawn(move || {
        let result = adapter.execute_query(sql).map(|_| ());
        (adapter, result)
    })
}

fn still_waiting<T>(waiting: &std::thread::JoinHandle<T>) -> bool {
    std::thread::sleep(std::time::Duration::from_millis(300));
    !waiting.is_finished()
}

fn in_transaction(adapter: &Adapter) -> bool {
    adapter.status_flags() & SERVER_STATUS_IN_TRANS != 0
}

#[test]
fn an_alter_waits_for_a_transaction_that_read_the_table_and_later_reads_wait_behind_it() {
    let Some(Sessions {
        _directory,
        mut one,
        mut two,
        mut three,
    }) = sessions()
    else {
        return;
    };
    run(&mut one, "BEGIN");
    assert_eq!(count(&mut one, "t").unwrap(), "2");
    run(&mut two, "SET SESSION lock_wait_timeout = 2");
    let started = std::time::Instant::now();
    let altering = in_the_background(two, "ALTER TABLE t ADD COLUMN c INT");
    assert!(still_waiting(&altering));
    assert_eq!(count(&mut three, "u").unwrap(), "2");
    let reading = std::thread::spawn(move || {
        let counted = count(&mut three, "t");
        (three, counted)
    });
    assert!(still_waiting(&reading));
    assert_eq!(count(&mut one, "t").unwrap(), "2");

    let (_two, altered) = altering.join().unwrap();
    assert_eq!(altered, Err(FrontendErrorKind::DatabaseBusy));
    assert!(started.elapsed() >= std::time::Duration::from_secs(2));
    let (_three, counted) = reading.join().unwrap();
    assert_eq!(counted.unwrap(), "2");
    run(&mut one, "COMMIT");
}

#[test]
fn a_transaction_that_read_a_table_and_then_writes_it_behind_a_waiting_alter_is_rolled_back_with_1213(
) {
    let Some(Sessions {
        _directory,
        mut one,
        two,
        ..
    }) = sessions()
    else {
        return;
    };
    run(&mut one, "BEGIN");
    run(&mut one, "INSERT INTO u (id, v) VALUES (3, 3)");
    assert_eq!(count(&mut one, "t").unwrap(), "2");
    let altering = in_the_background(two, "ALTER TABLE t ADD COLUMN c INT");
    assert!(still_waiting(&altering));

    assert_eq!(
        one.execute_query("INSERT INTO t (id, v) VALUES (3, 3)")
            .map(|_| ()),
        Err(FrontendErrorKind::SerializationFailure)
    );
    assert!(!in_transaction(&one));
    let (_two, altered) = altering.join().unwrap();
    assert_eq!(altered, Ok(()));
    assert_eq!(count(&mut one, "u").unwrap(), "2");
}

#[test]
fn lock_tables_write_keeps_other_sessions_off_that_table_alone_and_their_commits_go_ahead() {
    let Some(Sessions {
        _directory,
        mut one,
        mut two,
        ..
    }) = sessions()
    else {
        return;
    };
    run(&mut one, "LOCK TABLES t WRITE");
    run(&mut one, "UPDATE t SET v = 10 WHERE id = 1");
    run(&mut two, "SET SESSION lock_wait_timeout = 1");
    run(&mut two, "SET SESSION innodb_lock_wait_timeout = 1");
    let started = std::time::Instant::now();
    assert_eq!(count(&mut two, "t"), Err(FrontendErrorKind::DatabaseBusy));
    assert!(started.elapsed() >= std::time::Duration::from_secs(1));
    assert_eq!(count(&mut two, "u").unwrap(), "2");
    run(&mut two, "INSERT INTO u (id, v) VALUES (3, 3)");
    run(&mut two, "BEGIN");
    run(&mut two, "UPDATE u SET v = 20 WHERE id = 1");
    let started = std::time::Instant::now();
    run(&mut two, "COMMIT");
    assert!(started.elapsed() < std::time::Duration::from_millis(500));

    run(&mut one, "UNLOCK TABLES");
    assert_eq!(count(&mut two, "t").unwrap(), "2");
    let Ok(CommandExecutionResult::ResultSet(result)) =
        two.execute_query("SELECT v FROM t WHERE id = 1")
    else {
        panic!("the row must read back");
    };
    assert_eq!(result.rows, [[Some(b"10".to_vec())]]);
}

#[test]
fn lock_tables_read_lets_other_sessions_read_and_makes_their_writes_wait() {
    let Some(Sessions {
        _directory,
        mut one,
        mut two,
        ..
    }) = sessions()
    else {
        return;
    };
    run(&mut one, "LOCK TABLES t READ");
    run(&mut two, "SET SESSION lock_wait_timeout = 1");
    assert_eq!(count(&mut two, "t").unwrap(), "2");
    assert_eq!(
        two.execute_query("INSERT INTO t (id, v) VALUES (3, 3)")
            .map(|_| ()),
        Err(FrontendErrorKind::DatabaseBusy)
    );
    run(&mut two, "INSERT INTO u (id, v) VALUES (3, 3)");
    run(&mut one, "UNLOCK TABLES");
    run(&mut two, "INSERT INTO t (id, v) VALUES (3, 3)");
}

#[test]
fn lock_tables_read_waits_for_a_transaction_that_wrote_the_table() {
    let Some(Sessions {
        _directory,
        mut one,
        mut two,
        ..
    }) = sessions()
    else {
        return;
    };
    run(&mut one, "BEGIN");
    run(&mut one, "UPDATE t SET v = 5 WHERE id = 1");
    run(&mut two, "SET SESSION lock_wait_timeout = 1");
    assert_eq!(
        two.execute_query("LOCK TABLES t READ").map(|_| ()),
        Err(FrontendErrorKind::DatabaseBusy)
    );
    assert!(!two.session.connection().unwrap().tables_are_locked());
    run(&mut two, "LOCK TABLES u READ");
    run(&mut two, "UNLOCK TABLES");
    run(&mut one, "COMMIT");
}

#[test]
fn definition_changes_of_one_table_go_ahead_beside_a_writer_of_another() {
    let Some(Sessions {
        _directory,
        mut one,
        mut two,
        ..
    }) = sessions()
    else {
        return;
    };
    run(&mut one, "BEGIN");
    run(&mut one, "UPDATE u SET v = 5 WHERE id = 1");
    run(&mut two, "SET SESSION lock_wait_timeout = 1");
    for sql in [
        "LOCK TABLES t WRITE",
        "UNLOCK TABLES",
        "ALTER TABLE t ADD COLUMN c INT",
        "TRUNCATE TABLE t",
        "CREATE INDEX t_v ON t (v)",
        "CREATE TABLE d (id INT NOT NULL PRIMARY KEY)",
        "DROP TABLE t",
    ] {
        run(&mut two, sql);
    }
    run(&mut one, "UPDATE u SET v = 6 WHERE id = 2");
    run(&mut one, "COMMIT");
    let Ok(CommandExecutionResult::ResultSet(result)) =
        two.execute_query("SELECT v FROM u ORDER BY id")
    else {
        panic!("the rows must read back");
    };
    assert_eq!(result.rows, [[Some(b"5".to_vec())], [Some(b"6".to_vec())]]);
}

#[test]
fn every_definition_change_waits_for_a_transaction_that_read_the_table() {
    let Some(Sessions {
        _directory,
        mut one,
        mut two,
        ..
    }) = sessions()
    else {
        return;
    };
    run(&mut one, "BEGIN");
    assert_eq!(count(&mut one, "t").unwrap(), "2");
    run(&mut two, "SET SESSION lock_wait_timeout = 1");
    for sql in [
        "TRUNCATE TABLE t",
        "CREATE INDEX t_v ON t (v)",
        "RENAME TABLE t TO t9",
        "DROP TABLE t",
        "LOCK TABLES t WRITE",
    ] {
        assert_eq!(
            two.execute_query(sql).map(|_| ()),
            Err(FrontendErrorKind::DatabaseBusy),
            "{sql}"
        );
    }
    run(&mut two, "CREATE TABLE d (id INT NOT NULL PRIMARY KEY)");
    run(&mut one, "COMMIT");
    run(&mut two, "TRUNCATE TABLE t");
}

#[test]
fn a_read_view_older_than_another_sessions_alter_answers_1412_and_keeps_the_transaction() {
    let Some(Sessions {
        _directory,
        mut one,
        mut two,
        ..
    }) = sessions()
    else {
        return;
    };
    run(&mut one, "BEGIN");
    assert_eq!(count(&mut one, "u").unwrap(), "2");
    run(&mut two, "ALTER TABLE t ADD COLUMN c INT");
    assert_eq!(
        count(&mut one, "t"),
        Err(FrontendErrorKind::TableDefinitionChanged)
    );
    assert!(in_transaction(&one));
    run(&mut one, "UPDATE u SET v = 99 WHERE id = 1");
    run(&mut one, "COMMIT");
    let Ok(CommandExecutionResult::ResultSet(result)) =
        two.execute_query("SELECT v FROM u WHERE id = 1")
    else {
        panic!("the row must read back");
    };
    assert_eq!(result.rows, [[Some(b"99".to_vec())]]);
}

#[test]
fn a_transaction_without_a_read_view_reads_a_table_another_session_altered() {
    let Some(Sessions {
        _directory,
        mut one,
        mut two,
        ..
    }) = sessions()
    else {
        return;
    };
    run(&mut one, "BEGIN");
    run(&mut two, "ALTER TABLE t ADD COLUMN c INT");
    assert_eq!(count(&mut one, "t").unwrap(), "2");
    run(&mut one, "COMMIT");
}

#[test]
fn a_transaction_that_wrote_reads_and_writes_a_table_another_session_created() {
    let Some(Sessions {
        _directory,
        mut one,
        mut two,
        ..
    }) = sessions()
    else {
        return;
    };
    run(&mut one, "BEGIN");
    run(&mut one, "UPDATE u SET v = 5 WHERE id = 1");
    run(&mut two, "CREATE TABLE d (id INT NOT NULL PRIMARY KEY)");
    assert_eq!(count(&mut one, "d").unwrap(), "0");
    run(&mut one, "INSERT INTO d (id) VALUES (1)");
    run(&mut one, "COMMIT");
    assert_eq!(count(&mut two, "d").unwrap(), "1");
}

#[test]
fn a_metadata_lock_timeout_undoes_the_statement_and_keeps_the_transaction() {
    let Some(Sessions {
        _directory,
        mut one,
        mut two,
        ..
    }) = sessions()
    else {
        return;
    };
    run(&mut one, "LOCK TABLES t WRITE");
    run(&mut two, "SET SESSION lock_wait_timeout = 1");
    run(&mut two, "BEGIN");
    run(&mut two, "INSERT INTO u (id, v) VALUES (3, 3)");
    assert_eq!(count(&mut two, "t"), Err(FrontendErrorKind::DatabaseBusy));
    assert!(in_transaction(&two));
    run(&mut two, "COMMIT");
    run(&mut one, "UNLOCK TABLES");
    assert_eq!(count(&mut one, "u").unwrap(), "3");
}

#[test]
fn a_write_that_waited_behind_an_alter_writes_on_the_new_definition() {
    let Some(Sessions {
        _directory,
        mut one,
        two,
        three,
    }) = sessions()
    else {
        return;
    };
    run(&mut one, "BEGIN");
    assert_eq!(count(&mut one, "t").unwrap(), "2");
    let altering = in_the_background(two, "ALTER TABLE t ADD COLUMN c INT DEFAULT 7");
    assert!(still_waiting(&altering));
    let inserting = in_the_background(three, "INSERT INTO t (id, v) VALUES (3, 3)");
    assert!(still_waiting(&inserting));
    run(&mut one, "COMMIT");

    let (_two, altered) = altering.join().unwrap();
    assert_eq!(altered, Ok(()));
    let (mut three, inserted) = inserting.join().unwrap();
    assert_eq!(inserted, Ok(()), "{:?}", three.take_error_message());
    let Ok(CommandExecutionResult::ResultSet(result)) =
        one.execute_query("SELECT id, c FROM t ORDER BY id")
    else {
        panic!("the rows must read back");
    };
    assert_eq!(
        result.rows,
        [
            [Some(b"1".to_vec()), Some(b"7".to_vec())],
            [Some(b"2".to_vec()), Some(b"7".to_vec())],
            [Some(b"3".to_vec()), Some(b"7".to_vec())],
        ]
    );
}

#[test]
fn a_wait_cycle_through_a_row_lock_and_a_metadata_lock_times_out_with_1205_instead_of_1213() {
    let Some(Sessions {
        _directory,
        mut one,
        mut two,
        three,
    }) = sessions()
    else {
        return;
    };
    run(&mut one, "SET SESSION lock_wait_timeout = 1");
    run(&mut one, "BEGIN");
    run(&mut one, "UPDATE t SET v = 10 WHERE id = 1");
    run(&mut two, "BEGIN");
    assert_eq!(count(&mut two, "u").unwrap(), "2");
    let altering = in_the_background(three, "ALTER TABLE u ADD COLUMN c INT");
    assert!(still_waiting(&altering));
    let updating = in_the_background(two, "UPDATE t SET v = 20 WHERE id = 1");
    assert!(still_waiting(&updating));

    let started = std::time::Instant::now();
    assert_eq!(count(&mut one, "u"), Err(FrontendErrorKind::DatabaseBusy));
    assert!(started.elapsed() >= std::time::Duration::from_secs(1));
    assert!(in_transaction(&one));
    assert!(!updating.is_finished());
    run(&mut one, "COMMIT");

    let (mut two, updated) = updating.join().unwrap();
    assert_eq!(updated, Ok(()));
    assert!(still_waiting(&altering));
    run(&mut two, "COMMIT");
    let (_three, altered) = altering.join().unwrap();
    assert_eq!(altered, Ok(()));
    let Ok(CommandExecutionResult::ResultSet(result)) =
        one.execute_query("SELECT v FROM t WHERE id = 1")
    else {
        panic!("the row must read back");
    };
    assert_eq!(result.rows, [[Some(b"20".to_vec())]]);
}

#[test]
fn a_transaction_whose_read_view_predates_an_alter_inserts_on_the_new_definition_and_answers_1412_for_the_rest(
) {
    let Some(Sessions {
        _directory,
        mut one,
        mut two,
        ..
    }) = sessions()
    else {
        return;
    };
    run(
        &mut one,
        "CREATE TABLE w (id INT NOT NULL PRIMARY KEY, v INT)",
    );
    run(&mut one, "INSERT INTO w (id, v) VALUES (1, 1), (2, 2)");
    run(&mut one, "BEGIN");
    assert_eq!(count(&mut one, "u").unwrap(), "2");
    run(&mut two, "ALTER TABLE t ADD COLUMN c INT DEFAULT 7");
    run(&mut two, "TRUNCATE TABLE w");
    run(&mut two, "INSERT INTO u (id, v) VALUES (3, 3)");

    run(&mut one, "INSERT INTO t (id, v, c) VALUES (3, 3, 30)");
    run(&mut one, "INSERT INTO t (id, v) VALUES (4, 4)");
    run(
        &mut one,
        "INSERT INTO t (id, v) VALUES (6, 6) ON DUPLICATE KEY UPDATE v = 60",
    );
    run(&mut one, "REPLACE INTO t (id, v) VALUES (7, 7)");
    run(&mut one, "INSERT INTO w (id, v) VALUES (1, 3)");
    for refused in [
        "UPDATE t SET v = 0 WHERE id = 1",
        "DELETE FROM t WHERE id = 2",
        "INSERT INTO t (id, v) VALUES (2, 20) ON DUPLICATE KEY UPDATE v = 20",
        "REPLACE INTO t (id, v) VALUES (1, 50)",
        "DELETE FROM w WHERE id = 1",
    ] {
        assert_eq!(
            one.execute_query(refused).map(|_| ()),
            Err(FrontendErrorKind::TableDefinitionChanged),
            "{refused}"
        );
    }
    assert_eq!(
        count(&mut one, "t"),
        Err(FrontendErrorKind::TableDefinitionChanged)
    );
    assert_eq!(count(&mut one, "u").unwrap(), "2");
    assert!(in_transaction(&one));
    run(&mut one, "COMMIT");

    let Ok(CommandExecutionResult::ResultSet(result)) =
        two.execute_query("SELECT id, c FROM t ORDER BY id")
    else {
        panic!("the rows must read back");
    };
    let read = |rows: &[Vec<Option<Vec<u8>>>]| {
        rows.iter()
            .map(|row| {
                row.iter()
                    .map(|value| String::from_utf8(value.clone().unwrap()).unwrap())
                    .collect::<Vec<_>>()
                    .join(",")
            })
            .collect::<Vec<_>>()
    };
    assert_eq!(
        read(&result.rows),
        ["1,7", "2,7", "3,30", "4,7", "6,7", "7,7"]
    );
    let Ok(CommandExecutionResult::ResultSet(result)) =
        two.execute_query("SELECT id, v FROM w ORDER BY id")
    else {
        panic!("the rows must read back");
    };
    assert_eq!(read(&result.rows), ["1,3"]);
}
