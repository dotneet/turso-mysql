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

/// The second session's insert comes while the first still holds its write,
/// so here it waits for the first to commit. MySQL takes both rows without
/// waiting, since they are different rows.
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
    std::thread::sleep(std::time::Duration::from_millis(200));
    assert!(!waiting.is_finished());
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
/// both write without waiting.
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
        std::thread::sleep(std::time::Duration::from_millis(200));
        assert!(!waiting.is_finished());
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
