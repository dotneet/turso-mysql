//! The `INSERT`s a framework writes into a table that counts its own ids.
//!
//! Every expectation here was measured on MySQL 8.4.11.

use super::*;

type Adapter = AuthorizedDatabaseCommandAdapter<RecordingAuthorizer>;

fn adapter() -> (tempfile::TempDir, Adapter) {
    let authorizer = Arc::new(RecordingAuthorizer::default());
    let (directory, _catalog, factory) = catalog_factory(authorizer);
    let mut adapter = factory
        .build(AuthenticatedPrincipal::from_account_id_for_testing(
            AccountId::from_bytes([143; 32]),
        ))
        .unwrap();
    adapter.authorize_connection().unwrap();
    adapter.execute_init_db("REPORTS").unwrap();
    (directory, adapter)
}

fn run(adapter: &mut Adapter, sql: &str) {
    adapter
        .execute_query(sql)
        .unwrap_or_else(|error| panic!("{sql}: {error:?}"));
}

/// The affected rows and the id one write reports.
fn written(adapter: &mut Adapter, sql: &str) -> (u64, u64) {
    match adapter.execute_query(sql) {
        Ok(CommandExecutionResult::Ok(result)) => (result.affected_rows, result.last_insert_id),
        other => panic!("{sql} must answer OK, answered {other:?}"),
    }
}

fn rows(adapter: &mut Adapter, sql: &str) -> Vec<Vec<Option<String>>> {
    let result = adapter
        .execute_query(sql)
        .unwrap_or_else(|error| panic!("{sql}: {error:?}"));
    let CommandExecutionResult::ResultSet(result) = result else {
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

fn one(adapter: &mut Adapter, sql: &str) -> String {
    rows(adapter, sql)[0][0].clone().unwrap()
}

fn counter(adapter: &mut Adapter, table: &str) -> Option<String> {
    let printed = rows(adapter, &format!("SHOW CREATE TABLE `{table}`"))[0][1]
        .clone()
        .unwrap();
    printed
        .split_whitespace()
        .find_map(|word| word.strip_prefix("AUTO_INCREMENT="))
        .map(str::to_owned)
}

const LARAVEL_USERS: &str = "create table `users` (`id` bigint unsigned not null auto_increment primary key, `name` varchar(255) not null, `email` varchar(255) null, `created_at` timestamp null, `updated_at` timestamp null)";

/// The null bitmap, the new-parameters flag and one VAR_STRING parameter for
/// each word.
fn words(values: &[&str]) -> Vec<u8> {
    let mut payload = vec![0; values.len().div_ceil(8)];
    payload.push(1);
    for _ in values {
        payload.extend_from_slice(&[MYSQL_TYPE_VAR_STRING, 0]);
    }
    for value in values {
        payload.push(u8::try_from(value.len()).unwrap());
        payload.extend_from_slice(value.as_bytes());
    }
    payload
}

fn prepared_write(adapter: &mut Adapter, sql: &str, payload: &[u8]) -> (u64, u64) {
    let statement = adapter
        .execute_stmt_prepare(sql)
        .unwrap_or_else(|error| panic!("prepare {sql}: {error:?}"));
    let result = adapter
        .execute_stmt_execute(statement.statement_id, payload)
        .unwrap_or_else(|error| panic!("execute {sql}: {error:?}"));
    adapter.execute_stmt_close(statement.statement_id);
    let PreparedStatementExecutionResult::Ok(result) = result else {
        panic!("{sql} must answer OK");
    };
    (result.affected_rows, result.last_insert_id)
}

/// A row stamped with the moment it was written, which is how a hand-written
/// `INSERT` fills Laravel's `created_at` and `updated_at`.
#[test]
fn a_counted_row_takes_the_clock_as_a_value() {
    let (_directory, mut adapter) = adapter();
    run(&mut adapter, LARAVEL_USERS);
    run(
        &mut adapter,
        "CREATE TABLE posts (id INT NOT NULL AUTO_INCREMENT PRIMARY KEY, title VARCHAR(20), posted_at DATETIME NULL)",
    );

    assert_eq!(
        written(
            &mut adapter,
            "INSERT INTO users (name, created_at, updated_at) VALUES ('Fi', NOW(), NOW())"
        ),
        (1, 1)
    );
    // Several rows report the first number they took. `CURDATE()` into a
    // `DATETIME` is that day's midnight.
    assert_eq!(
        written(
            &mut adapter,
            "INSERT INTO posts (title, posted_at) VALUES ('a', NOW()), ('b', CURDATE()), ('c', NOW() + INTERVAL 1 DAY)"
        ),
        (3, 1)
    );
    assert_eq!(one(&mut adapter, "SELECT LAST_INSERT_ID()"), "1");
    // A written NULL and a written 0 ask for the next number, beside the clock
    // as anywhere else.
    assert_eq!(
        written(
            &mut adapter,
            "INSERT INTO posts (id, title, posted_at) VALUES (NULL, 'd', CURRENT_TIMESTAMP), (0, 'e', NOW())"
        ),
        (2, 4)
    );
    assert_eq!(
        rows(
            &mut adapter,
            "SELECT id, title FROM posts WHERE posted_at IS NOT NULL ORDER BY id"
        ),
        vec![
            vec![Some("1".to_owned()), Some("a".to_owned())],
            vec![Some("2".to_owned()), Some("b".to_owned())],
            vec![Some("3".to_owned()), Some("c".to_owned())],
            vec![Some("4".to_owned()), Some("d".to_owned())],
            vec![Some("5".to_owned()), Some("e".to_owned())],
        ]
    );
    assert!(one(&mut adapter, "SELECT posted_at FROM posts WHERE id = 2").ends_with(" 00:00:00"));
    assert_eq!(
        one(
            &mut adapter,
            "SELECT COUNT(*) FROM users WHERE created_at IS NOT NULL AND updated_at IS NOT NULL"
        ),
        "1"
    );
    assert_eq!(counter(&mut adapter, "posts").as_deref(), Some("6"));

    // The same through `SET`, which is written out as the column-list form.
    assert_eq!(
        written(
            &mut adapter,
            "INSERT INTO users SET name = 'Cy', email = 'cy@x.com', created_at = NOW()"
        ),
        (1, 2)
    );
}

/// Each row of these is written by a statement of its own, where MySQL reads
/// the clock once for the whole statement.
#[test]
fn a_clock_reading_is_refused_where_the_rows_are_written_one_at_a_time() {
    let (_directory, mut adapter) = adapter();
    run(
        &mut adapter,
        "CREATE TABLE posts (id INT NOT NULL AUTO_INCREMENT PRIMARY KEY, title VARCHAR(20) UNIQUE, posted_at DATETIME NULL)",
    );
    assert_eq!(
        adapter.execute_query(
            "INSERT INTO posts (title, posted_at) VALUES ('a', NOW()), ('b', NOW()) ON DUPLICATE KEY UPDATE posted_at = NOW()"
        ),
        Err(FrontendErrorKind::Unsupported)
    );
    // A written id past the counter beside rows asking for the next one.
    assert_eq!(
        adapter.execute_query(
            "INSERT INTO posts (id, title, posted_at) VALUES (NULL, 'a', NOW()), (50, 'b', NOW())"
        ),
        Err(FrontendErrorKind::Unsupported)
    );
    assert_eq!(one(&mut adapter, "SELECT COUNT(*) FROM posts"), "0");
}

/// A bound value beside the clock, over several rows.
#[test]
fn a_prepared_counted_row_takes_the_clock_beside_bound_values() {
    let (_directory, mut adapter) = adapter();
    run(&mut adapter, LARAVEL_USERS);
    assert_eq!(
        prepared_write(
            &mut adapter,
            "insert into `users` (`name`, `email`, `created_at`, `updated_at`) values (?, ?, now(), now()), (?, ?, now(), now())",
            &words(&["a", "a@x.com", "b", "b@x.com"]),
        ),
        (2, 1)
    );
    assert_eq!(
        rows(
            &mut adapter,
            "SELECT id, name, email FROM users WHERE created_at IS NOT NULL ORDER BY id"
        ),
        vec![
            vec![
                Some("1".to_owned()),
                Some("a".to_owned()),
                Some("a@x.com".to_owned())
            ],
            vec![
                Some("2".to_owned()),
                Some("b".to_owned()),
                Some("b@x.com".to_owned())
            ],
        ]
    );
}
