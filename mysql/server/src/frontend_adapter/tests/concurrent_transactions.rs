//! Two pooled sessions each inserting inside a transaction of its own, the
//! way Prisma's `Promise.all([tag.create(...), tag.create(...)])` does.
//!
//! Every expectation here was measured on MySQL 8.4.11 with two sessions
//! taking the steps in the order written.

use super::*;

type Adapter = AuthorizedDatabaseCommandAdapter<RecordingAuthorizer>;

const PRISMA_TAGS: &str = "CREATE TABLE `tags` (`id` BIGINT NOT NULL AUTO_INCREMENT, `name` VARCHAR(100) NOT NULL, UNIQUE INDEX `tags_name_key`(`name`), PRIMARY KEY (`id`)) DEFAULT CHARACTER SET utf8mb4 COLLATE utf8mb4_unicode_ci";
const INSERT_TAG: &str = "INSERT INTO `prisma`.`tags` (`id`,`name`) VALUES (?,?)";
const READ_TAG: &str = "SELECT `prisma`.`tags`.`id`, `prisma`.`tags`.`name` FROM `prisma`.`tags` WHERE `prisma`.`tags`.`id` = ? LIMIT ? OFFSET ?";

struct TwoSessions {
    _directory: tempfile::TempDir,
    one: Adapter,
    two: Adapter,
}

fn two_sessions() -> TwoSessions {
    let authorizer = Arc::new(RecordingAuthorizer::default());
    let (directory, catalog, factory) = catalog_factory(authorizer.clone());
    catalog.create("prisma").unwrap();
    let second_factory =
        AuthorizedDatabaseAdapterFactory::new(catalog, binary_context(), authorizer);
    let mut one = factory
        .build(AuthenticatedPrincipal::from_account_id_for_testing(
            AccountId::from_bytes([181; 32]),
        ))
        .unwrap();
    one.authorize_connection().unwrap();
    one.execute_init_db("prisma").unwrap();
    run(&mut one, PRISMA_TAGS);
    let mut two = second_factory
        .build(AuthenticatedPrincipal::from_account_id_for_testing(
            AccountId::from_bytes([182; 32]),
        ))
        .unwrap();
    two.authorize_connection().unwrap();
    two.execute_init_db("prisma").unwrap();
    TwoSessions {
        _directory: directory,
        one,
        two,
    }
}

fn run(adapter: &mut Adapter, sql: &str) {
    match adapter.execute_query(sql) {
        Ok(_) => {}
        Err(error) => panic!("{sql}: {error:?} {:?}", adapter.take_error_message()),
    }
}

fn names(adapter: &mut Adapter) -> Vec<String> {
    let Ok(CommandExecutionResult::ResultSet(result)) =
        adapter.execute_query("SELECT name FROM tags ORDER BY name")
    else {
        panic!("the names must read back");
    };
    result
        .rows
        .into_iter()
        .map(|row| String::from_utf8(row[0].clone().unwrap()).unwrap())
        .collect()
}

#[derive(Clone, Copy)]
enum Bound<'a> {
    Word(&'a str),
    Whole(i64),
    Null,
}

/// The null bitmap, the new-parameters flag, the types and the values, as
/// `mysql_async` binds them.
fn payload(values: &[Bound<'_>]) -> Vec<u8> {
    let mut bitmap = vec![0; values.len().div_ceil(8)];
    for (index, value) in values.iter().enumerate() {
        if matches!(value, Bound::Null) {
            bitmap[index / 8] |= 1 << (index % 8);
        }
    }
    let mut payload = bitmap;
    payload.push(1);
    for value in values {
        let code = match value {
            Bound::Word(_) => MYSQL_TYPE_STRING,
            Bound::Whole(_) => MYSQL_TYPE_LONGLONG,
            Bound::Null => MYSQL_TYPE_NULL,
        };
        payload.extend_from_slice(&[code, 0]);
    }
    for value in values {
        match value {
            Bound::Word(word) => {
                payload.push(u8::try_from(word.len()).unwrap());
                payload.extend_from_slice(word.as_bytes());
            }
            Bound::Whole(number) => payload.extend_from_slice(&number.to_le_bytes()),
            Bound::Null => {}
        }
    }
    payload
}

fn prepare(adapter: &mut Adapter, sql: &str) -> u32 {
    adapter
        .execute_stmt_prepare(sql)
        .unwrap_or_else(|error| panic!("prepare {sql}: {error:?}"))
        .statement_id
}

fn execute(
    adapter: &mut Adapter,
    statement_id: u32,
    values: &[Bound<'_>],
) -> Result<PreparedStatementExecutionResult, FrontendErrorKind> {
    adapter.execute_stmt_execute(statement_id, &payload(values))
}

/// The order Prisma's two pooled connections took in the framework run: the
/// second begins and prepares its insert while the first holds the write
/// lock, and executes it after the first committed. MySQL takes both rows.
#[test]
fn an_insert_prepared_while_another_session_writes_runs_after_that_session_commits() {
    for id in [Bound::Null, Bound::Whole(2)] {
        let TwoSessions {
            _directory,
            mut one,
            mut two,
        } = two_sessions();
        let first_id = match id {
            Bound::Null => Bound::Null,
            _ => Bound::Whole(1),
        };

        run(&mut one, "BEGIN");
        let one_insert = prepare(&mut one, INSERT_TAG);
        run(&mut two, "BEGIN");
        execute(&mut one, one_insert, &[first_id, Bound::Word("news")]).unwrap();
        let two_insert = prepare(&mut two, INSERT_TAG);
        let one_read = prepare(&mut one, READ_TAG);
        execute(
            &mut one,
            one_read,
            &[Bound::Whole(1), Bound::Whole(1), Bound::Whole(0)],
        )
        .unwrap();
        run(&mut one, "COMMIT");
        execute(&mut two, two_insert, &[id, Bound::Word("rust")])
            .unwrap_or_else(|error| panic!("the second insert: {error:?}"));
        run(&mut two, "COMMIT");

        assert_eq!(names(&mut one), ["news", "rust"]);
    }
}

/// A transaction's snapshot is taken by its first read, which executing a
/// prepared `SELECT` is and preparing an `INSERT` is not.
#[test]
fn a_transaction_that_prepared_an_insert_reads_what_was_committed_before_its_first_read() {
    let TwoSessions {
        _directory,
        mut one,
        mut two,
    } = two_sessions();

    run(&mut two, "BEGIN");
    prepare(&mut two, INSERT_TAG);
    let count = prepare(&mut two, "SELECT COUNT(*) FROM tags");
    run(
        &mut one,
        "INSERT INTO tags (id, name) VALUES (NULL, 'late')",
    );
    assert_eq!(counted(&mut two, count), 1);
    run(
        &mut one,
        "INSERT INTO tags (id, name) VALUES (NULL, 'later')",
    );
    assert_eq!(counted(&mut two, count), 1);
    run(&mut two, "COMMIT");
    assert_eq!(counted(&mut two, count), 2);
}

/// The second session's insert comes while the first still holds its write.
/// In WAL mode it waits for the first to commit, since the first holds the
/// database's one write lock. MySQL takes both rows without waiting, since
/// they are different rows, and so do row locks in MVCC mode.
#[test]
fn an_insert_that_waited_for_another_transaction_writes_once_that_one_commits() {
    let TwoSessions {
        _directory,
        mut one,
        two,
    } = two_sessions();
    run(&mut one, "BEGIN");
    run(
        &mut one,
        "INSERT INTO tags (id, name) VALUES (NULL, 'news')",
    );
    let waiting = insert_in_a_transaction_of_its_own(two, "rust");
    waits_only_without_row_locks(&waiting);
    run(&mut one, "COMMIT");

    let (mut two, result) = waiting.join().unwrap();
    assert_eq!(result, Ok(()));
    assert_ne!(two.status_flags() & SERVER_STATUS_IN_TRANS, 0);
    run(&mut two, "COMMIT");
    assert_eq!(names(&mut one), ["news", "rust"]);
}

/// Measured on MySQL 8.4.11: an insert waiting on another session's row with
/// the same key answers 1062 once that session commits, and only the
/// statement is rolled back, not the transaction.
#[test]
fn an_insert_that_waited_for_a_row_with_its_key_answers_1062_and_keeps_the_transaction() {
    let TwoSessions {
        _directory,
        mut one,
        two,
    } = two_sessions();
    run(&mut one, "BEGIN");
    run(
        &mut one,
        "INSERT INTO tags (id, name) VALUES (NULL, 'same')",
    );
    let waiting = insert_in_a_transaction_of_its_own(two, "same");
    std::thread::sleep(std::time::Duration::from_millis(200));
    assert!(!waiting.is_finished());
    run(&mut one, "COMMIT");

    let (mut two, result) = waiting.join().unwrap();
    assert_eq!(result, Err(FrontendErrorKind::ConstraintViolation));
    assert_ne!(two.status_flags() & SERVER_STATUS_IN_TRANS, 0);
    run(
        &mut two,
        "INSERT INTO tags (id, name) VALUES (NULL, 'other')",
    );
    run(&mut two, "COMMIT");
    assert_eq!(names(&mut one), ["other", "same"]);
}

/// The same wait for a statement that opens no transaction beforehand: an
/// insert with autocommit on, which commits alone, and the first statement
/// with it off, which begins the transaction. The counted insert takes a
/// savepoint, and so its snapshot, before it writes. Measured on MySQL 8.4.11
/// both write without waiting, as they do with row locks in MVCC mode.
#[test]
fn a_statement_outside_a_transaction_that_waited_for_another_writes_once_that_one_commits() {
    for (autocommit, still_in_a_transaction) in [("1", false), ("0", true)] {
        let TwoSessions {
            _directory,
            mut one,
            mut two,
        } = two_sessions();
        run(&mut one, "BEGIN");
        run(
            &mut one,
            "INSERT INTO tags (id, name) VALUES (NULL, 'news')",
        );
        run(&mut two, &format!("SET autocommit = {autocommit}"));
        let waiting = std::thread::spawn(move || {
            let result = two
                .execute_query("INSERT INTO tags (id, name) VALUES (7, 'rust')")
                .map(|_| ());
            (two, result)
        });
        waits_only_without_row_locks(&waiting);
        run(&mut one, "COMMIT");

        let (mut two, result) = waiting.join().unwrap();
        assert_eq!(result, Ok(()), "autocommit = {autocommit}");
        assert_eq!(
            two.status_flags() & SERVER_STATUS_IN_TRANS != 0,
            still_in_a_transaction
        );
        run(&mut two, "COMMIT");
        assert_eq!(names(&mut one), ["news", "rust"]);
    }
}

fn waits_only_without_row_locks<T>(waiting: &std::thread::JoinHandle<T>) {
    let row_locks = turso_mysql::experimental_mvcc_is_on();
    let started = std::time::Instant::now();
    while !waiting.is_finished() && started.elapsed() < std::time::Duration::from_secs(5) {
        if !row_locks && started.elapsed() >= std::time::Duration::from_millis(200) {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    assert_eq!(waiting.is_finished(), row_locks);
}

fn insert_in_a_transaction_of_its_own(
    mut adapter: Adapter,
    name: &str,
) -> std::thread::JoinHandle<(Adapter, Result<(), FrontendErrorKind>)> {
    run(&mut adapter, "BEGIN");
    let sql = format!("INSERT INTO tags (id, name) VALUES (NULL, '{name}')");
    std::thread::spawn(move || {
        let result = adapter.execute_query(&sql).map(|_| ());
        (adapter, result)
    })
}

fn counted(adapter: &mut Adapter, statement_id: u32) -> i64 {
    let result = adapter.execute_stmt_execute(statement_id, &[]);
    let Ok(PreparedStatementExecutionResult::ResultSet(result)) = result else {
        panic!("the prepared count must return rows: {result:?}");
    };
    let [row] = result.rows.as_slice() else {
        panic!("the prepared count must return one row");
    };
    let BinaryResultValue::Integer(count) = row[0] else {
        panic!("a count is a whole number, not {:?}", row[0]);
    };
    count
}

/// Prisma's `Promise.all([tag.create(...), tag.create(...)])` over two pooled
/// connections, each in a transaction of its own, many times over. Each
/// insert takes its id from the table's counter, which lets one session in at
/// a time; the other waits its turn, as MySQL's inserts wait at a table's
/// AUTO-INC lock, rather than answering 1205 at once.
#[test]
fn two_sessions_inserting_counted_rows_at_once_both_write_every_row() {
    const ROUNDS: usize = 200;
    let TwoSessions {
        _directory,
        one,
        two,
    } = two_sessions();
    let inserter = |mut adapter: Adapter, prefix: &'static str| {
        std::thread::spawn(move || {
            let insert = prepare(&mut adapter, INSERT_TAG);
            for round in 0..ROUNDS {
                run(&mut adapter, "BEGIN");
                let name = format!("{prefix}{round}");
                execute(&mut adapter, insert, &[Bound::Null, Bound::Word(&name)])
                    .unwrap_or_else(|error| panic!("{name}: {error:?}"));
                run(&mut adapter, "COMMIT");
            }
            adapter
        })
    };
    let first = inserter(one, "a");
    let second = inserter(two, "b");
    let mut one = first.join().unwrap();
    second.join().unwrap();
    assert_eq!(names(&mut one).len(), 2 * ROUNDS);
}

/// Four sessions inserting counted rows at once, in transactions of their
/// own and outside any, one row and several at a time. Measured on MySQL
/// 8.4.11: every insert succeeds, the id each one reports belongs to the row
/// it wrote, and the ids run from 1 to the number of rows with none spent.
#[test]
fn several_sessions_inserting_counted_rows_at_once_write_every_row_with_its_own_id() {
    const SESSIONS: usize = 4;
    const ROUNDS: usize = 60;
    let (_directory, sessions) = sessions(SESSIONS);
    let inserters: Vec<_> = sessions
        .into_iter()
        .enumerate()
        .map(|(session, mut adapter)| {
            std::thread::spawn(move || {
                let insert = prepare(&mut adapter, INSERT_TAG);
                let mut reported = Vec::new();
                for round in 0..ROUNDS {
                    let name = format!("s{session}r{round}");
                    let result = match round % 3 {
                        0 => {
                            run(&mut adapter, "BEGIN");
                            let result =
                                execute(&mut adapter, insert, &[Bound::Null, Bound::Word(&name)]);
                            run(&mut adapter, "COMMIT");
                            result.map(|result| match result {
                                PreparedStatementExecutionResult::Ok(result) => result,
                                other => panic!("{name}: {other:?}"),
                            })
                        }
                        1 => adapter
                            .execute_query(&format!("INSERT INTO tags (name) VALUES ('{name}')"))
                            .map(|result| match result {
                                CommandExecutionResult::Ok(result) => result,
                                other => panic!("{name}: {other:?}"),
                            }),
                        _ => adapter
                            .execute_query(&format!(
                                "INSERT INTO tags (name) VALUES ('{name}'), ('{name}-second')"
                            ))
                            .map(|result| match result {
                                CommandExecutionResult::Ok(result) => result,
                                other => panic!("{name}: {other:?}"),
                            }),
                    };
                    let result = result.unwrap_or_else(|error| {
                        panic!("{name}: {error:?} {:?}", adapter.take_error_message())
                    });
                    reported.push((result.last_insert_id, name));
                }
                (adapter, reported)
            })
        })
        .collect();
    let mut reported = Vec::new();
    let mut last = None;
    for inserter in inserters {
        let (adapter, rows) = inserter.join().unwrap();
        reported.extend(rows);
        last = Some(adapter);
    }
    let mut adapter = last.unwrap();
    let rows = SESSIONS * (ROUNDS + ROUNDS / 3);
    let Ok(CommandExecutionResult::ResultSet(result)) =
        adapter.execute_query("SELECT id, name FROM tags ORDER BY id")
    else {
        panic!("the rows must read back");
    };
    let written: Vec<(u64, String)> = result
        .rows
        .into_iter()
        .map(|row| {
            let id = String::from_utf8(row[0].clone().unwrap()).unwrap();
            let name = String::from_utf8(row[1].clone().unwrap()).unwrap();
            (id.parse().unwrap(), name)
        })
        .collect();
    assert_eq!(
        written.iter().map(|(id, _)| *id).collect::<Vec<_>>(),
        (1..=rows as u64).collect::<Vec<_>>()
    );
    for (id, name) in reported {
        assert_eq!(written[id as usize - 1], (id, name));
    }
}

/// Measured on MySQL 8.4.11: a counted insert waiting for a key another
/// transaction holds, given up as the victim of a deadlock, answers 1213 and
/// its transaction is rolled back whole; nothing it wrote stays, even once
/// the key is free again.
#[test]
fn a_counted_insert_given_up_for_a_deadlock_writes_nothing() {
    if !turso_mysql::experimental_mvcc_is_on() {
        return;
    }
    let (_directory, sessions) = sessions(3);
    let [mut one, mut two, mut three] = <[Adapter; 3]>::try_from(sessions).ok().unwrap();
    run(
        &mut one,
        "CREATE TABLE accounts (id INT NOT NULL PRIMARY KEY, balance INT)",
    );
    run(
        &mut one,
        "INSERT INTO accounts (id, balance) VALUES (1, 10), (2, 20), (3, 30)",
    );
    run(&mut one, "BEGIN");
    run(&mut one, "UPDATE accounts SET balance = 11 WHERE id = 1");
    run(&mut two, "BEGIN");
    run(&mut two, "INSERT INTO tags (name) VALUES ('held')");
    run(&mut two, "UPDATE accounts SET balance = 21 WHERE id = 2");
    run(&mut two, "UPDATE accounts SET balance = 31 WHERE id = 3");

    let given_up = in_the_background(one, "INSERT INTO tags (name) VALUES ('held')");
    assert!(still_waiting(&given_up));
    run(&mut three, "INSERT INTO tags (name) VALUES ('third')");
    run(&mut two, "UPDATE accounts SET balance = 12 WHERE id = 1");
    run(&mut two, "ROLLBACK");

    let (mut one, result) = given_up.join().unwrap();
    assert_eq!(result, Err(FrontendErrorKind::SerializationFailure));
    assert_eq!(one.status_flags() & SERVER_STATUS_IN_TRANS, 0);
    assert_eq!(names(&mut one), ["third"]);
    assert_eq!(balances(&mut one), ["10", "20", "30"]);
}

/// Measured on MySQL 8.4.11: a counted insert waiting for a key another
/// transaction holds takes its ids before it waits, so a third session
/// inserting meanwhile takes the ids after them. The waiting insert keeps
/// those ids once the other transaction rolls back, and spends them when it
/// commits and the insert answers 1062.
#[test]
fn a_counted_insert_takes_its_ids_before_it_waits_for_a_key() {
    if !turso_mysql::experimental_mvcc_is_on() {
        return;
    }
    struct Case {
        ending: &'static str,
        waiting_rows: &'static str,
        third_id: u64,
        waiting_id: Result<u64, FrontendErrorKind>,
        written: &'static [(u64, &'static str)],
    }
    let cases = [
        Case {
            ending: "ROLLBACK",
            waiting_rows: "('held')",
            third_id: 3,
            waiting_id: Ok(2),
            written: &[(2, "held"), (3, "third")],
        },
        Case {
            ending: "ROLLBACK",
            waiting_rows: "('held'), ('second')",
            third_id: 4,
            waiting_id: Ok(2),
            written: &[(2, "held"), (3, "second"), (4, "third")],
        },
        Case {
            ending: "COMMIT",
            waiting_rows: "('held')",
            third_id: 3,
            waiting_id: Err(FrontendErrorKind::ConstraintViolation),
            written: &[(1, "held"), (3, "third")],
        },
    ];
    for Case {
        ending,
        waiting_rows,
        third_id,
        waiting_id,
        written,
    } in cases
    {
        let (_directory, sessions) = sessions(3);
        let [mut one, mut two, mut three] = <[Adapter; 3]>::try_from(sessions).ok().unwrap();
        run(&mut one, "BEGIN");
        run(&mut one, "INSERT INTO tags (name) VALUES ('held')");
        run(&mut two, "BEGIN");
        let waiting = std::thread::spawn(move || {
            let id = inserted_id(
                &mut two,
                &format!("INSERT INTO tags (name) VALUES {waiting_rows}"),
            );
            (two, id)
        });
        assert!(still_waiting(&waiting));
        assert_eq!(
            inserted_id(&mut three, "INSERT INTO tags (name) VALUES ('third')"),
            Ok(third_id)
        );
        run(&mut one, ending);

        let (mut two, id) = waiting.join().unwrap();
        assert_eq!(id, waiting_id, "{ending} {waiting_rows}");
        run(&mut two, "COMMIT");
        assert_eq!(
            ids_and_names(&mut one),
            written
                .iter()
                .map(|(id, name)| (*id, name.to_string()))
                .collect::<Vec<_>>(),
            "{ending} {waiting_rows}"
        );
    }
}

fn inserted_id(adapter: &mut Adapter, sql: &str) -> Result<u64, FrontendErrorKind> {
    adapter.execute_query(sql).map(|result| match result {
        CommandExecutionResult::Ok(result) => result.last_insert_id,
        other => panic!("{sql}: {other:?}"),
    })
}

fn ids_and_names(adapter: &mut Adapter) -> Vec<(u64, String)> {
    let Ok(CommandExecutionResult::ResultSet(result)) =
        adapter.execute_query("SELECT id, name FROM tags ORDER BY id")
    else {
        panic!("the rows must read back");
    };
    result
        .rows
        .into_iter()
        .map(|row| {
            let id = String::from_utf8(row[0].clone().unwrap()).unwrap();
            let name = String::from_utf8(row[1].clone().unwrap()).unwrap();
            (id.parse().unwrap(), name)
        })
        .collect()
}

fn sessions(count: usize) -> (tempfile::TempDir, Vec<Adapter>) {
    let authorizer = Arc::new(RecordingAuthorizer::default());
    let (directory, catalog, factory) = catalog_factory(authorizer.clone());
    catalog.create("prisma").unwrap();
    let mut first = session_of(factory, 190);
    run(&mut first, PRISMA_TAGS);
    let mut sessions = vec![first];
    for session in 1..count {
        let factory = AuthorizedDatabaseAdapterFactory::new(
            catalog.clone(),
            binary_context(),
            authorizer.clone(),
        );
        sessions.push(session_of(factory, 190 + session as u8));
    }
    (directory, sessions)
}

fn session_of(
    factory: AuthorizedDatabaseAdapterFactory<RecordingAuthorizer>,
    account: u8,
) -> Adapter {
    let mut adapter = factory
        .build(AuthenticatedPrincipal::from_account_id_for_testing(
            AccountId::from_bytes([account; 32]),
        ))
        .unwrap();
    adapter.authorize_connection().unwrap();
    adapter.execute_init_db("prisma").unwrap();
    adapter
}

/// Measured on MySQL 8.4.11 with `innodb_lock_wait_timeout = 1`: an update
/// of a row another open transaction updated waits for that transaction,
/// answers 1205 after about a second, and once the transaction commits the
/// same update goes through.
#[test]
fn an_update_of_a_row_another_transaction_holds_waits_and_gives_up_with_1205() {
    let TwoSessions {
        _directory,
        mut one,
        mut two,
    } = two_sessions();
    run(&mut one, "INSERT INTO tags (id, name) VALUES (1, 'news')");
    run(&mut one, "BEGIN");
    run(&mut one, "UPDATE tags SET name = 'held' WHERE id = 1");
    run(&mut two, "SET SESSION innodb_lock_wait_timeout = 1");

    let started = std::time::Instant::now();
    assert_eq!(
        two.execute_query("UPDATE tags SET name = 'waited' WHERE id = 1")
            .err(),
        Some(FrontendErrorKind::DatabaseBusy)
    );
    let waited = started.elapsed();
    assert!(
        waited >= std::time::Duration::from_millis(900)
            && waited < std::time::Duration::from_secs(10),
        "{waited:?}"
    );

    let waiting = std::thread::spawn(move || {
        let result = two
            .execute_query("UPDATE tags SET name = 'waited' WHERE id = 1")
            .map(|_| ());
        (two, result)
    });
    std::thread::sleep(std::time::Duration::from_millis(200));
    run(&mut one, "COMMIT");
    let (mut two, result) = waiting.join().unwrap();
    assert_eq!(result, Ok(()));
    assert_eq!(names(&mut two), ["waited"]);
}

/// A `SERIALIZABLE` transaction that wrote holds the database's one
/// exclusive write under WAL, so a write of another transaction waits for it
/// to end rather than failing when it commits, and gives up with 1205 after
/// `innodb_lock_wait_timeout`. Measured on MySQL 8.4.11, a write to a row the
/// `SERIALIZABLE` transaction did not touch goes ahead at once, and so it does
/// with row locks in MVCC mode.
#[test]
fn a_write_beside_a_serializable_writer_waits_for_it_only_without_row_locks() {
    let TwoSessions {
        _directory,
        mut one,
        mut two,
    } = two_sessions();
    run(
        &mut one,
        "INSERT INTO tags (id, name) VALUES (1, 'news'), (2, 'rust')",
    );
    run(
        &mut one,
        "SET SESSION TRANSACTION ISOLATION LEVEL SERIALIZABLE",
    );
    run(&mut one, "BEGIN");
    run(&mut one, "UPDATE tags SET name = 'held' WHERE id = 1");
    run(&mut two, "SET SESSION innodb_lock_wait_timeout = 1");
    run(&mut two, "BEGIN");
    if turso_mysql::experimental_mvcc_is_on() {
        run(&mut two, "UPDATE tags SET name = 'beside' WHERE id = 2");
        run(&mut two, "COMMIT");
        run(&mut one, "COMMIT");
        assert_eq!(names(&mut two), ["beside", "held"]);
        return;
    }
    assert_eq!(
        two.execute_query("UPDATE tags SET name = 'gave up' WHERE id = 2")
            .err(),
        Some(FrontendErrorKind::DatabaseBusy)
    );

    let waiting = std::thread::spawn(move || {
        let result = two
            .execute_query("UPDATE tags SET name = 'waited' WHERE id = 2")
            .map(|_| ());
        (two, result)
    });
    std::thread::sleep(std::time::Duration::from_millis(200));
    assert!(!waiting.is_finished());
    run(&mut one, "COMMIT");
    let (mut two, result) = waiting.join().unwrap();
    assert_eq!(result, Ok(()));
    run(&mut two, "COMMIT");
    assert_eq!(names(&mut two), ["held", "waited"]);
}

/// Measured on MySQL 8.4.11: an update of a row another transaction changed
/// waits for that transaction, then changes the row it committed, though the
/// waiting transaction's own reads keep the snapshot they took.
#[test]
fn an_update_waits_for_the_row_another_transaction_changed_and_writes_on_what_it_committed() {
    let Some(TwoSessions {
        _directory,
        mut one,
        mut two,
    }) = row_lock_sessions()
    else {
        return;
    };
    run(&mut two, "BEGIN");
    assert_eq!(balances(&mut two), ["10", "20", "30"]);
    run(&mut one, "BEGIN");
    run(
        &mut one,
        "UPDATE accounts SET balance = balance + 100 WHERE id = 1",
    );
    run(
        &mut one,
        "UPDATE accounts SET balance = balance + 100 WHERE id = 2",
    );

    let waiting = in_the_background(
        two,
        "UPDATE accounts SET balance = balance + 1 WHERE id = 1",
    );
    assert!(still_waiting(&waiting));
    run(&mut one, "COMMIT");
    let (mut two, result) = waiting.join().unwrap();
    assert_eq!(result, Ok(()));
    assert_eq!(balances(&mut two), ["111", "20", "30"]);
    run(&mut two, "COMMIT");
    assert_eq!(balances(&mut one), ["111", "120", "30"]);
}

/// Measured on MySQL 8.4.11: an update of a row another transaction changed
/// and committed after this one's snapshot changes the newer row rather than
/// failing.
#[test]
fn an_update_of_a_row_committed_after_the_snapshot_writes_on_the_newer_row() {
    let Some(TwoSessions {
        _directory,
        mut one,
        mut two,
    }) = row_lock_sessions()
    else {
        return;
    };
    run(&mut two, "BEGIN");
    assert_eq!(balances(&mut two), ["10", "20", "30"]);
    run(&mut one, "UPDATE accounts SET balance = 50 WHERE id = 1");
    run(
        &mut two,
        "UPDATE accounts SET balance = balance + 1 WHERE id = 1",
    );
    assert_eq!(balances(&mut two), ["51", "20", "30"]);
    run(&mut two, "COMMIT");
}

/// Measured on MySQL 8.4.11 with `innodb_lock_wait_timeout = 1`: the update
/// waits a second, answers 1205, and only that statement is undone; the
/// transaction and what it wrote before stay.
#[test]
fn a_lock_wait_timeout_answers_1205_and_keeps_the_transaction() {
    let Some(TwoSessions {
        _directory,
        mut one,
        mut two,
    }) = row_lock_sessions()
    else {
        return;
    };
    run(&mut one, "BEGIN");
    run(&mut one, "UPDATE accounts SET balance = 0 WHERE id = 1");
    run(&mut two, "SET SESSION innodb_lock_wait_timeout = 1");
    run(&mut two, "BEGIN");
    run(&mut two, "UPDATE accounts SET balance = 99 WHERE id = 2");
    let waited = std::time::Instant::now();
    assert_eq!(
        two.execute_query("UPDATE accounts SET balance = 99 WHERE id = 1")
            .map(|_| ()),
        Err(FrontendErrorKind::DatabaseBusy)
    );
    assert!(waited.elapsed() >= std::time::Duration::from_secs(1));
    assert_ne!(two.status_flags() & SERVER_STATUS_IN_TRANS, 0);
    run(&mut two, "COMMIT");
    run(&mut one, "COMMIT");
    assert_eq!(balances(&mut one), ["0", "99", "30"]);
}

/// Measured on MySQL 8.4.11: two transactions that each wrote one row and
/// then wait for each other's row end in 1213 for the one whose wait closes
/// the cycle, which is rolled back whole, while the other's wait goes on.
#[test]
fn a_deadlock_answers_1213_to_the_session_that_closes_it_and_rolls_it_back() {
    let Some(TwoSessions {
        _directory,
        mut one,
        mut two,
    }) = row_lock_sessions()
    else {
        return;
    };
    run(&mut one, "BEGIN");
    run(&mut two, "BEGIN");
    run(
        &mut one,
        "UPDATE accounts SET balance = balance + 1 WHERE id = 1",
    );
    run(
        &mut two,
        "UPDATE accounts SET balance = balance + 10 WHERE id = 2",
    );
    let waiting = in_the_background(
        one,
        "UPDATE accounts SET balance = balance + 1 WHERE id = 2",
    );
    assert!(still_waiting(&waiting));
    assert_eq!(
        two.execute_query("UPDATE accounts SET balance = balance + 10 WHERE id = 1")
            .map(|_| ()),
        Err(FrontendErrorKind::SerializationFailure)
    );
    assert_eq!(two.status_flags() & SERVER_STATUS_IN_TRANS, 0);
    let (mut one, result) = waiting.join().unwrap();
    assert_eq!(result, Ok(()));
    run(&mut one, "COMMIT");
    assert_eq!(balances(&mut two), ["11", "21", "30"]);
}

/// Measured on MySQL 8.4.11: when the session closing the cycle wrote more
/// rows than the one already waiting, the waiting one is given up instead.
#[test]
fn a_deadlock_rolls_back_the_session_that_wrote_fewer_rows() {
    let Some(TwoSessions {
        _directory,
        mut one,
        mut two,
    }) = row_lock_sessions()
    else {
        return;
    };
    run(&mut one, "BEGIN");
    run(&mut two, "BEGIN");
    run(
        &mut one,
        "UPDATE accounts SET balance = balance + 1 WHERE id = 1",
    );
    run(
        &mut two,
        "UPDATE accounts SET balance = balance + 10 WHERE id = 2",
    );
    run(
        &mut two,
        "UPDATE accounts SET balance = balance + 10 WHERE id = 3",
    );
    let waiting = in_the_background(
        one,
        "UPDATE accounts SET balance = balance + 1 WHERE id = 2",
    );
    assert!(still_waiting(&waiting));
    run(
        &mut two,
        "UPDATE accounts SET balance = balance + 10 WHERE id = 1",
    );
    let (one, result) = waiting.join().unwrap();
    assert_eq!(result, Err(FrontendErrorKind::SerializationFailure));
    assert_eq!(one.status_flags() & SERVER_STATUS_IN_TRANS, 0);
    run(&mut two, "COMMIT");
    assert_eq!(balances(&mut two), ["20", "30", "40"]);
}

/// Measured on MySQL 8.4.11: a locking read waits for a row another
/// transaction changed and reads what it committed, while a plain read in the
/// same transaction keeps its snapshot.
#[test]
fn a_locking_read_reads_what_was_committed_and_a_plain_read_keeps_the_snapshot() {
    let Some(TwoSessions {
        _directory,
        mut one,
        mut two,
    }) = row_lock_sessions()
    else {
        return;
    };
    run(&mut two, "BEGIN");
    assert_eq!(balances(&mut two), ["10", "20", "30"]);
    run(&mut one, "BEGIN");
    run(&mut one, "UPDATE accounts SET balance = 11 WHERE id = 1");
    let waiting = std::thread::spawn(move || {
        let read = two.execute_query("SELECT balance FROM accounts WHERE id = 1 FOR UPDATE");
        (two, read)
    });
    assert!(still_waiting(&waiting));
    run(&mut one, "COMMIT");
    let (mut two, read) = waiting.join().unwrap();
    let Ok(CommandExecutionResult::ResultSet(read)) = read else {
        panic!("the locking read must return rows: {read:?}");
    };
    assert_eq!(read.rows, vec![vec![Some(b"11".to_vec())]]);
    assert_eq!(balances(&mut two), ["10", "20", "30"]);
    run(&mut two, "COMMIT");
}

/// Measured on MySQL 8.4.11: an `UPDATE` that reads the table through no
/// index locks every row it reads, the ones it does not change too, so
/// another session's update of any of them waits and answers 1205.
#[test]
fn an_update_through_no_index_keeps_every_row_it_read_from_other_writers() {
    let Some(TwoSessions {
        _directory,
        mut one,
        mut two,
    }) = row_lock_sessions()
    else {
        return;
    };
    run(&mut one, "BEGIN");
    run(
        &mut one,
        "UPDATE accounts SET balance = 0 WHERE balance = 999",
    );
    run(&mut two, "SET SESSION innodb_lock_wait_timeout = 1");
    assert_eq!(
        two.execute_query("UPDATE accounts SET balance = 5 WHERE id = 2")
            .map(|_| ()),
        Err(FrontendErrorKind::DatabaseBusy)
    );
    run(&mut one, "COMMIT");
    run(&mut two, "UPDATE accounts SET balance = 5 WHERE id = 2");
}

/// Measured on MySQL 8.4.11: an `UPDATE` whose condition matches a row only
/// as another open transaction changed it waits for that transaction, and
/// once it commits changes the row.
#[test]
fn an_update_waits_for_a_row_another_transaction_changed_to_match() {
    let Some(TwoSessions {
        _directory,
        mut one,
        two,
    }) = row_lock_sessions()
    else {
        return;
    };
    run(&mut one, "BEGIN");
    run(&mut one, "UPDATE accounts SET balance = 77 WHERE id = 2");
    let waiting = in_the_background(two, "UPDATE accounts SET balance = 777 WHERE balance = 77");
    assert!(still_waiting(&waiting));
    run(&mut one, "COMMIT");
    let (_two, result) = waiting.join().unwrap();
    assert_eq!(result, Ok(()));
    assert_eq!(balances(&mut one), ["10", "777", "30"]);
}

/// Measured on MySQL 8.4.11: `INSERT ... SELECT` locks the rows it reads in
/// share mode, so another session's update of one of them waits.
#[test]
fn an_insert_select_keeps_the_rows_it_read_from_other_writers() {
    let Some(TwoSessions {
        _directory,
        mut one,
        mut two,
    }) = row_lock_sessions()
    else {
        return;
    };
    run(
        &mut one,
        "CREATE TABLE copies (id INT NOT NULL PRIMARY KEY, balance INT)",
    );
    run(&mut one, "BEGIN");
    run(
        &mut one,
        "INSERT INTO copies (id, balance) SELECT id, balance FROM accounts WHERE id = 1",
    );
    run(&mut two, "SET SESSION innodb_lock_wait_timeout = 1");
    assert_eq!(
        two.execute_query("UPDATE accounts SET balance = 9 WHERE id = 1")
            .map(|_| ()),
        Err(FrontendErrorKind::DatabaseBusy)
    );
    run(&mut two, "UPDATE accounts SET balance = 9 WHERE id = 2");
    run(&mut one, "COMMIT");
}

/// Measured on MySQL 8.4.11: under `REPEATABLE READ` a locking read of a
/// range locks the gaps it read, up to the end of the table here, so another
/// session's insert into the range waits for it to commit.
#[test]
fn a_range_locking_read_keeps_an_insert_out_of_its_range_until_it_commits() {
    let Some(TwoSessions {
        _directory,
        mut one,
        two,
    }) = row_lock_sessions()
    else {
        return;
    };
    run(&mut one, "BEGIN");
    run(&mut one, "SELECT id FROM accounts WHERE id > 1 FOR UPDATE");
    let waiting = in_the_background(two, "INSERT INTO accounts (id, balance) VALUES (4, 40)");
    assert!(still_waiting(&waiting));
    run(&mut one, "COMMIT");
    let (_two, result) = waiting.join().unwrap();
    assert_eq!(result, Ok(()));
    assert_eq!(balances(&mut one), ["10", "20", "30", "40"]);
}

/// Measured on MySQL 8.4.11: under `READ COMMITTED` a locking read locks the
/// rows it read and no gap, so another session's insert into the range goes
/// ahead at once.
#[test]
fn read_committed_lets_an_insert_into_a_range_it_read_go_ahead() {
    let Some(TwoSessions {
        _directory,
        mut one,
        mut two,
    }) = row_lock_sessions()
    else {
        return;
    };
    run(
        &mut one,
        "SET SESSION TRANSACTION ISOLATION LEVEL READ COMMITTED",
    );
    run(&mut one, "BEGIN");
    run(&mut one, "SELECT id FROM accounts WHERE id > 1 FOR UPDATE");
    run(&mut two, "SET SESSION innodb_lock_wait_timeout = 1");
    run(
        &mut two,
        "INSERT INTO accounts (id, balance) VALUES (4, 40)",
    );
    assert_eq!(
        two.execute_query("UPDATE accounts SET balance = 0 WHERE id = 2")
            .map(|_| ()),
        Err(FrontendErrorKind::DatabaseBusy)
    );
    run(&mut one, "COMMIT");
    assert_eq!(balances(&mut one), ["10", "20", "30", "40"]);
}

/// Measured on MySQL 8.4.11: two sessions that each lock a missing key in one
/// gap and then insert into it end in 1213 for the one whose insert closes
/// the cycle, and the other's insert goes on.
#[test]
fn two_sessions_that_lock_one_gap_and_insert_into_it_end_in_a_deadlock() {
    let Some(TwoSessions {
        _directory,
        mut one,
        mut two,
    }) = row_lock_sessions()
    else {
        return;
    };
    run(&mut one, "BEGIN");
    run(&mut two, "BEGIN");
    run(&mut one, "SELECT id FROM accounts WHERE id = 5 FOR UPDATE");
    run(&mut two, "SELECT id FROM accounts WHERE id = 6 FOR UPDATE");
    let waiting = in_the_background(one, "INSERT INTO accounts (id, balance) VALUES (5, 50)");
    assert!(still_waiting(&waiting));
    assert_eq!(
        two.execute_query("INSERT INTO accounts (id, balance) VALUES (6, 60)")
            .map(|_| ()),
        Err(FrontendErrorKind::SerializationFailure)
    );
    assert_eq!(two.status_flags() & SERVER_STATUS_IN_TRANS, 0);
    let (mut one, result) = waiting.join().unwrap();
    assert_eq!(result, Ok(()));
    run(&mut one, "COMMIT");
    assert_eq!(balances(&mut one), ["10", "20", "30", "50"]);
}

fn row_lock_sessions() -> Option<TwoSessions> {
    if !turso_mysql::experimental_mvcc_is_on() {
        return None;
    }
    let mut sessions = two_sessions();
    run(
        &mut sessions.one,
        "CREATE TABLE accounts (id INT NOT NULL PRIMARY KEY, balance INT)",
    );
    run(
        &mut sessions.one,
        "INSERT INTO accounts (id, balance) VALUES (1, 10), (2, 20), (3, 30)",
    );
    Some(sessions)
}

fn balances(adapter: &mut Adapter) -> Vec<String> {
    let Ok(CommandExecutionResult::ResultSet(result)) =
        adapter.execute_query("SELECT balance FROM accounts ORDER BY id")
    else {
        panic!("the balances must read back");
    };
    result
        .rows
        .into_iter()
        .map(|row| String::from_utf8(row[0].clone().unwrap()).unwrap())
        .collect()
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
