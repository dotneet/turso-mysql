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
    // And with no column list, where every column of the table is meant.
    assert_eq!(
        written(
            &mut adapter,
            "INSERT INTO users VALUES (NULL, 'Ed', 'ed@x.com', NOW(), NOW())"
        ),
        (1, 3)
    );
    assert_eq!(
        one(
            &mut adapter,
            "SELECT COUNT(*) FROM users WHERE id = 3 AND updated_at IS NOT NULL"
        ),
        "1"
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

fn eight_words(adapter: &mut Adapter) {
    run(adapter, "CREATE TABLE src (n INT, name VARCHAR(20))");
    run(
        adapter,
        "INSERT INTO src (n, name) VALUES (1, 'a'), (2, 'b'), (3, 'c'), (4, 'd'), (5, 'e'), (6, 'f'), (7, 'g'), (8, 'h')",
    );
}

/// MySQL cannot know how many rows a `SELECT` answers, so it takes numbers in
/// batches of 1, 2, 4 and on, and spends what the last batch leaves unused.
#[test]
fn a_select_copies_rows_into_a_counted_table_and_spends_numbers_in_batches() {
    let (_directory, mut adapter) = adapter();
    eight_words(&mut adapter);
    run(
        &mut adapter,
        "CREATE TABLE t1 (id INT NOT NULL AUTO_INCREMENT PRIMARY KEY, name VARCHAR(20))",
    );
    for (sql, reported, counted_to) in [
        (
            "INSERT INTO t1 (name) SELECT name FROM src WHERE n <= 1",
            (1, 1),
            "2",
        ),
        (
            "INSERT INTO t1 (name) SELECT name FROM src WHERE n <= 3",
            (3, 2),
            "5",
        ),
        (
            "INSERT INTO t1 (name) SELECT name FROM src WHERE n <= 4",
            (4, 5),
            "12",
        ),
        (
            "INSERT INTO t1 (name) SELECT name FROM src WHERE n <= 7",
            (7, 12),
            "19",
        ),
        (
            "INSERT INTO t1 (name) SELECT name FROM src WHERE n <= 8",
            (8, 19),
            "34",
        ),
    ] {
        assert_eq!(written(&mut adapter, sql), reported, "{sql}");
        assert_eq!(
            one(&mut adapter, "SELECT LAST_INSERT_ID()"),
            reported.1.to_string()
        );
        assert_eq!(counter(&mut adapter, "t1").as_deref(), Some(counted_to));
    }
    assert_eq!(
        written(&mut adapter, "INSERT INTO t1 (name) VALUES ('after')"),
        (1, 34)
    );
    // The rows take their numbers in the order the SELECT answers them.
    assert_eq!(
        written(
            &mut adapter,
            "INSERT INTO t1 (name) SELECT name FROM src WHERE n >= 6"
        ),
        (3, 35)
    );
    assert_eq!(
        rows(
            &mut adapter,
            "SELECT id, name FROM t1 WHERE id >= 35 ORDER BY id"
        ),
        vec![
            vec![Some("35".to_owned()), Some("f".to_owned())],
            vec![Some("36".to_owned()), Some("g".to_owned())],
            vec![Some("37".to_owned()), Some("h".to_owned())],
        ]
    );

    // A SELECT answering no rows writes none, reports no id and leaves the
    // counter and LAST_INSERT_ID() where they stood.
    assert_eq!(
        written(
            &mut adapter,
            "INSERT INTO t1 (name) SELECT name FROM src WHERE n > 100"
        ),
        (0, 0)
    );
    assert_eq!(one(&mut adapter, "SELECT LAST_INSERT_ID()"), "35");
    assert_eq!(counter(&mut adapter, "t1").as_deref(), Some("38"));
}

/// A SELECT reading the table it writes sees only the rows that stood before
/// the statement.
#[test]
fn a_select_reading_the_counted_table_it_writes_copies_what_stood() {
    let (_directory, mut adapter) = adapter();
    run(
        &mut adapter,
        "CREATE TABLE a1 (id INT NOT NULL AUTO_INCREMENT PRIMARY KEY, name VARCHAR(20))",
    );
    run(
        &mut adapter,
        "INSERT INTO a1 (name) VALUES ('a'), ('b'), ('c'), ('d'), ('e'), ('f'), ('g'), ('h'), ('i'), ('j'), ('k')",
    );
    assert_eq!(
        written(&mut adapter, "INSERT INTO a1 (name) SELECT name FROM a1"),
        (11, 12)
    );
    assert_eq!(
        rows(&mut adapter, "SELECT COUNT(*), MAX(id) FROM a1"),
        vec![vec![Some("22".to_owned()), Some("22".to_owned())]]
    );
    assert_eq!(counter(&mut adapter, "a1").as_deref(), Some("27"));
}

/// Laravel's `insertUsing` as it arrives, prepared with its bindings in the
/// `SELECT`'s `WHERE`, into a counted table and into one counting nothing.
///
/// Measured on MySQL 8.4.11: the `SELECT` finds the rows a bare `SELECT`
/// binding the same values finds — a word matching without regard to case, a
/// bound day meeting a `TIMESTAMP` — and copies them in the order its
/// `ORDER BY` names, a text column ordered without regard to case.
#[test]
fn laravels_prepared_insert_using_copies_the_rows_its_bindings_choose() {
    let (_directory, mut adapter) = adapter();
    run(&mut adapter, LARAVEL_USERS);
    run(
        &mut adapter,
        "create table `archive` (`name` varchar(255) not null, `email` varchar(255) null)",
    );
    assert_eq!(
        written(
            &mut adapter,
            "INSERT INTO users (name, email, created_at) VALUES ('Fi', 'fi@x.com', '2024-01-02 03:04:05'), ('bo', 'bo@x.com', '2023-06-01 00:00:00'), ('Al', NULL, NULL)"
        ),
        (3, 1)
    );
    assert_eq!(
        prepared_write(
            &mut adapter,
            "insert into `users` (`name`, `email`) select `name`, `email` from `users` where `id` = ?",
            &words(&["1"]),
        ),
        (1, 4)
    );
    assert_eq!(one(&mut adapter, "SELECT LAST_INSERT_ID()"), "4");
    assert_eq!(
        prepared_write(
            &mut adapter,
            "insert into `users` (`name`, `email`) select `name`, `email` from `users` where `name` = ?",
            &words(&["FI"]),
        ),
        (2, 5)
    );
    assert_eq!(
        prepared_write(
            &mut adapter,
            "insert into `archive` (`name`, `email`) select `name`, `email` from `users` where `created_at` > ?",
            &words(&["2024-01-01"]),
        ),
        (1, 0)
    );
    assert_eq!(
        prepared_write(
            &mut adapter,
            "insert into `archive` (`name`, `email`) select `name`, `email` from `users` where `name` = ?",
            &words(&["AL"]),
        ),
        (1, 0)
    );
    assert_eq!(
        written(
            &mut adapter,
            "insert into `users` (`name`) select `name` from `archive` order by `name`"
        ),
        (2, 8)
    );
    assert_eq!(
        prepared_write(
            &mut adapter,
            "insert into `users` (`name`) select `name` from `users` where `id` > ? order by `name` desc",
            &words(&["6"]),
        ),
        (2, 11)
    );
    assert_eq!(
        written(
            &mut adapter,
            "insert into `archive` (`name`) select `name` from `users` order by `name` limit 2"
        ),
        (2, 0)
    );
    assert_eq!(
        rows(
            &mut adapter,
            "SELECT id, name, email FROM users ORDER BY id"
        ),
        vec![
            some(&["1", "Fi", "fi@x.com"]),
            some(&["2", "bo", "bo@x.com"]),
            vec![Some("3".to_owned()), Some("Al".to_owned()), None],
            some(&["4", "Fi", "fi@x.com"]),
            some(&["5", "Fi", "fi@x.com"]),
            some(&["6", "Fi", "fi@x.com"]),
            vec![Some("8".to_owned()), Some("Al".to_owned()), None],
            vec![Some("9".to_owned()), Some("Fi".to_owned()), None],
            vec![Some("11".to_owned()), Some("Fi".to_owned()), None],
            vec![Some("12".to_owned()), Some("Al".to_owned()), None],
        ]
    );
    assert_eq!(
        rows(
            &mut adapter,
            "SELECT name, email FROM archive ORDER BY name"
        ),
        vec![
            vec![Some("Al".to_owned()), None],
            vec![Some("Al".to_owned()), None],
            vec![Some("Al".to_owned()), None],
            some(&["Fi", "fi@x.com"]),
        ]
    );
    assert_eq!(counter(&mut adapter, "users").as_deref(), Some("14"));
}

/// A prepared copy whose `SELECT` was read knowing its columns' types runs
/// again after another table is made, and is refused once a table it reads
/// changes, where it would have to be read again.
#[test]
fn a_prepared_copy_keeps_its_reading_of_the_tables_it_names() {
    let (_directory, mut adapter) = adapter();
    run(
        &mut adapter,
        "CREATE TABLE src (name VARCHAR(20) NOT NULL, score INT NOT NULL)",
    );
    run(&mut adapter, "CREATE TABLE dst (name VARCHAR(20) NOT NULL)");
    run(
        &mut adapter,
        "INSERT INTO src (name, score) VALUES ('b', 1), ('A', 2), ('c', 3)",
    );
    let statement = adapter
        .execute_stmt_prepare(
            "INSERT INTO dst (name) SELECT name FROM src WHERE score < ? ORDER BY name LIMIT ?",
        )
        .unwrap();
    // A word for the score, and a whole number for the row count, which is
    // all a `LIMIT ?` takes.
    let execute = |adapter: &mut Adapter, below: &str, limit: i64| {
        let mut payload = vec![0, 1, MYSQL_TYPE_VAR_STRING, 0, MYSQL_TYPE_LONGLONG, 0];
        payload.push(u8::try_from(below.len()).unwrap());
        payload.extend_from_slice(below.as_bytes());
        payload.extend_from_slice(&limit.to_le_bytes());
        match adapter.execute_stmt_execute(statement.statement_id, &payload) {
            Ok(PreparedStatementExecutionResult::Ok(result)) => Ok(result.affected_rows),
            Ok(_) => panic!("a copy answers OK"),
            Err(error) => Err(error),
        }
    };
    assert_eq!(execute(&mut adapter, "3", 1), Ok(1));
    run(&mut adapter, "CREATE TABLE other (x INT)");
    assert_eq!(execute(&mut adapter, "9", 2), Ok(2));
    assert_eq!(
        rows(&mut adapter, "SELECT name FROM dst"),
        vec![some(&["A"]), some(&["A"]), some(&["b"])]
    );
    run(&mut adapter, "ALTER TABLE src ADD COLUMN note TEXT");
    assert!(execute(&mut adapter, "9", 2).is_err());
    assert_eq!(
        rows(&mut adapter, "SELECT COUNT(*) FROM dst"),
        vec![some(&["3"])]
    );
}

/// Laravel's `insertUsing`, copying a row into a table Laravel counts.
#[test]
fn laravels_insert_using_copies_into_a_counted_table() {
    let (_directory, mut adapter) = adapter();
    run(&mut adapter, LARAVEL_USERS);
    run(
        &mut adapter,
        "INSERT INTO users (name, email) VALUES ('Fi', 'fi@x.com')",
    );
    assert_eq!(
        written(
            &mut adapter,
            "insert into `users` (`name`, `email`) select `name`, `email` from `users` where `id` = 1"
        ),
        (1, 2)
    );
    assert_eq!(
        rows(
            &mut adapter,
            "SELECT id, name, email FROM users ORDER BY id"
        ),
        vec![
            vec![
                Some("1".to_owned()),
                Some("Fi".to_owned()),
                Some("fi@x.com".to_owned())
            ],
            vec![
                Some("2".to_owned()),
                Some("Fi".to_owned()),
                Some("fi@x.com".to_owned())
            ],
        ]
    );
    assert_eq!(counter(&mut adapter, "users").as_deref(), Some("3"));
}

/// The ids a SELECT answers for the counted column itself.
#[test]
fn a_select_writing_the_counted_column_follows_the_values_rules() {
    let (_directory, mut adapter) = adapter();
    eight_words(&mut adapter);
    run(
        &mut adapter,
        "CREATE TABLE t2 (id INT NOT NULL AUTO_INCREMENT PRIMARY KEY, name VARCHAR(20))",
    );
    // Its own ids raise the counter past the highest, report the last row's
    // and leave LAST_INSERT_ID() alone.
    assert_eq!(
        written(
            &mut adapter,
            "INSERT INTO t2 (id, name) SELECT n + 100, name FROM src WHERE n <= 3"
        ),
        (3, 103)
    );
    assert_eq!(one(&mut adapter, "SELECT LAST_INSERT_ID()"), "0");
    assert_eq!(counter(&mut adapter, "t2").as_deref(), Some("104"));
    assert_eq!(
        written(
            &mut adapter,
            "INSERT INTO t2 (id, name) SELECT 200 - n, name FROM src WHERE n <= 3"
        ),
        (3, 197)
    );
    assert_eq!(counter(&mut adapter, "t2").as_deref(), Some("200"));
    // NULL and 0 ask for the next number, in batches.
    assert_eq!(
        written(
            &mut adapter,
            "INSERT INTO t2 (id, name) SELECT NULL, name FROM src WHERE n <= 3"
        ),
        (3, 200)
    );
    assert_eq!(counter(&mut adapter, "t2").as_deref(), Some("203"));
    assert_eq!(
        written(
            &mut adapter,
            "INSERT INTO t2 (id, name) SELECT 0, name FROM src WHERE n <= 2"
        ),
        (2, 203)
    );
    assert_eq!(counter(&mut adapter, "t2").as_deref(), Some("206"));
    // Every column of the table, the id among them, when no list is written.
    run(
        &mut adapter,
        "CREATE TABLE t3 (id INT NOT NULL AUTO_INCREMENT PRIMARY KEY, name VARCHAR(20))",
    );
    assert_eq!(
        written(
            &mut adapter,
            "INSERT INTO t3 SELECT * FROM t2 WHERE id < 150"
        ),
        (3, 103)
    );
    assert_eq!(counter(&mut adapter, "t3").as_deref(), Some("104"));

    // A copy colliding with a row already there writes none of its rows.
    assert!(adapter
        .execute_query("INSERT INTO t2 (id, name) SELECT n + 99, name FROM src WHERE n <= 3")
        .is_err());
    assert_eq!(
        one(&mut adapter, "SELECT COUNT(*) FROM t2 WHERE id = 100"),
        "0"
    );

    // A row naming its own id beside one asking for the next is refused.
    run(&mut adapter, "CREATE TABLE ids (n INT)");
    run(&mut adapter, "INSERT INTO ids (n) VALUES (NULL), (500)");
    assert_eq!(
        adapter.execute_query("INSERT INTO t2 (id, name) SELECT n, 'x' FROM ids"),
        Err(FrontendErrorKind::Unsupported)
    );
    assert_eq!(counter(&mut adapter, "t2").as_deref(), Some("206"));
}

/// A column the table needs and the copy leaves out, and the clauses that
/// decide what a colliding row does.
#[test]
fn a_counted_copy_is_held_to_the_rules_of_an_ordinary_insert() {
    let (_directory, mut adapter) = adapter();
    eight_words(&mut adapter);
    run(
        &mut adapter,
        "CREATE TABLE r1 (id INT NOT NULL AUTO_INCREMENT PRIMARY KEY, name VARCHAR(20), must INT NOT NULL)",
    );
    // Measured, no row means no complaint; a row means 1364, before a number
    // is spent.
    assert_eq!(
        written(
            &mut adapter,
            "INSERT INTO r1 (name) SELECT name FROM src WHERE n > 100"
        ),
        (0, 0)
    );
    assert_eq!(
        adapter.execute_query("INSERT INTO r1 (name) SELECT name FROM src WHERE n = 1"),
        Err(FrontendErrorKind::MissingRequiredDefault)
    );
    assert_eq!(counter(&mut adapter, "r1"), None);
    for sql in [
        "INSERT IGNORE INTO r1 (name, must) SELECT name, n FROM src",
        "REPLACE INTO r1 (name, must) SELECT name, n FROM src",
        "INSERT INTO r1 (name, must) SELECT name, n FROM src ON DUPLICATE KEY UPDATE must = 0",
    ] {
        assert_eq!(
            adapter.execute_query(sql),
            Err(FrontendErrorKind::Unsupported),
            "{sql}"
        );
    }
    assert_eq!(one(&mut adapter, "SELECT COUNT(*) FROM r1"), "0");
}

/// Ids past the engine's signed range, and a `DECIMAL` carried across.
#[test]
fn a_copy_between_wide_counted_tables_keeps_every_digit() {
    let (_directory, mut adapter) = adapter();
    run(
        &mut adapter,
        "CREATE TABLE w1 (id BIGINT UNSIGNED NOT NULL AUTO_INCREMENT PRIMARY KEY, name VARCHAR(20), price DECIMAL(10,2)) AUTO_INCREMENT=18446744073709551000",
    );
    run(
        &mut adapter,
        "CREATE TABLE w2 (id BIGINT UNSIGNED NOT NULL AUTO_INCREMENT PRIMARY KEY, name VARCHAR(20), price DECIMAL(10,2))",
    );
    assert_eq!(
        written(
            &mut adapter,
            "INSERT INTO w1 (name, price) VALUES ('a', 1.25), ('b', 2.50)"
        ),
        (2, 18446744073709551000)
    );
    assert_eq!(
        written(&mut adapter, "INSERT INTO w2 SELECT * FROM w1"),
        (2, 18446744073709551001)
    );
    assert_eq!(
        one(&mut adapter, "SELECT LAST_INSERT_ID()"),
        "18446744073709551000"
    );
    assert_eq!(
        counter(&mut adapter, "w2").as_deref(),
        Some("18446744073709551002")
    );
    assert_eq!(
        written(
            &mut adapter,
            "INSERT INTO w1 (name, price) SELECT name, price FROM w2"
        ),
        (2, 18446744073709551002)
    );
    assert_eq!(
        counter(&mut adapter, "w1").as_deref(),
        Some("18446744073709551005")
    );
    assert_eq!(
        rows(&mut adapter, "SELECT id, name, price FROM w1 ORDER BY id"),
        vec![
            some(&["18446744073709551000", "a", "1.25"]),
            some(&["18446744073709551001", "b", "2.50"]),
            some(&["18446744073709551002", "a", "1.25"]),
            some(&["18446744073709551003", "b", "2.50"]),
        ]
    );
}

fn some(values: &[&str]) -> Vec<Option<String>> {
    values
        .iter()
        .map(|value| Some((*value).to_owned()))
        .collect()
}

/// Laravel's `upsert()`, over the `bigint unsigned` key every Laravel table
/// counts with and over an `int` one, which MySQL answers alike.
#[test]
fn laravels_upsert_reports_what_mysql_reports_whatever_the_key_type() {
    for key in ["bigint unsigned", "int"] {
        let (_directory, mut adapter) = adapter();
        run(
            &mut adapter,
            &format!("create table `lu` (`id` {key} not null auto_increment primary key, `name` varchar(255) not null, `email` varchar(255) not null, `votes` int not null default '0')"),
        );
        run(
            &mut adapter,
            "alter table `lu` add unique `lu_email_unique`(`email`)",
        );
        assert_eq!(
            written(
                &mut adapter,
                "insert into `lu` (`email`, `name`) values ('a@x.com', 'A'), ('b@x.com', 'B'), ('c@x.com', 'C')"
            ),
            (3, 1)
        );
        let upsert = |rows: &str| {
            format!("insert into `lu` (`email`, `name`) values {rows} as laravel_upsert_alias on duplicate key update `name` = `laravel_upsert_alias`.`name`")
        };
        // A row it changed reports that row's own id and spends a number.
        assert_eq!(
            written(&mut adapter, &upsert("('b@x.com', 'B2')")),
            (2, 2),
            "{key}"
        );
        assert_eq!(one(&mut adapter, "SELECT LAST_INSERT_ID()"), "1");
        assert_eq!(counter(&mut adapter, "lu").as_deref(), Some("5"));
        assert_eq!(
            written(&mut adapter, &upsert("('d@x.com', 'D')")),
            (1, 5),
            "{key}"
        );
        assert_eq!(one(&mut adapter, "SELECT LAST_INSERT_ID()"), "5");
        // A row it left as it stood reports nothing.
        assert_eq!(
            written(&mut adapter, &upsert("('b@x.com', 'B2')")),
            (0, 0),
            "{key}"
        );
        // Over several rows, a row it changed gives its number back to the
        // next row it writes.
        assert_eq!(
            written(&mut adapter, &upsert("('c@x.com', 'C3'), ('e@x.com', 'E')")),
            (3, 7),
            "{key}"
        );
        assert_eq!(one(&mut adapter, "SELECT LAST_INSERT_ID()"), "7");
        assert_eq!(
            written(
                &mut adapter,
                &upsert("('a@x.com', 'A4'), ('c@x.com', 'C4')")
            ),
            (4, 3),
            "{key}"
        );
        assert_eq!(one(&mut adapter, "SELECT LAST_INSERT_ID()"), "7");
        assert_eq!(counter(&mut adapter, "lu").as_deref(), Some("11"));
        assert_eq!(
            rows(&mut adapter, "SELECT id, email, name FROM lu ORDER BY id"),
            vec![
                some(&["1", "a@x.com", "A4"]),
                some(&["2", "b@x.com", "B2"]),
                some(&["3", "c@x.com", "C4"]),
                some(&["5", "d@x.com", "D"]),
                some(&["7", "e@x.com", "E"]),
            ]
        );
    }
}

/// The id an upsert reports is read off the row it matched, however far past
/// the engine's signed range it runs, and a prepared upsert reports the same.
#[test]
fn an_upsert_reports_a_wide_unsigned_id_it_matched() {
    let (_directory, mut adapter) = adapter();
    run(
        &mut adapter,
        "create table `lw` (`id` bigint unsigned not null auto_increment primary key, `name` varchar(255) not null, `email` varchar(255) not null) AUTO_INCREMENT=18446744073709551000",
    );
    run(
        &mut adapter,
        "alter table `lw` add unique `lw_email_unique`(`email`)",
    );
    assert_eq!(
        written(
            &mut adapter,
            "insert into `lw` (`email`, `name`) values ('a@x.com', 'A'), ('b@x.com', 'B')"
        ),
        (2, 18446744073709551000)
    );
    let upsert = |rows: &str| {
        format!("insert into `lw` (`email`, `name`) values {rows} as laravel_upsert_alias on duplicate key update `name` = `laravel_upsert_alias`.`name`")
    };
    assert_eq!(
        written(&mut adapter, &upsert("('b@x.com', 'B2')")),
        (2, 18446744073709551001)
    );
    assert_eq!(
        one(&mut adapter, "SELECT LAST_INSERT_ID()"),
        "18446744073709551000"
    );
    assert_eq!(
        counter(&mut adapter, "lw").as_deref(),
        Some("18446744073709551003")
    );
    // The last row it matched, which it left as it stood.
    assert_eq!(
        written(
            &mut adapter,
            &upsert("('a@x.com', 'A2'), ('b@x.com', 'B2')")
        ),
        (2, 18446744073709551001)
    );
    assert_eq!(
        written(&mut adapter, &upsert("('c@x.com', 'C')")),
        (1, 18446744073709551005)
    );
    assert_eq!(
        prepared_write(&mut adapter, &upsert("(?, ?)"), &words(&["a@x.com", "A3"])),
        (2, 18446744073709551000)
    );
    assert_eq!(
        prepared_write(&mut adapter, &upsert("(?, ?)"), &words(&["a@x.com", "A3"])),
        (0, 0)
    );
    assert_eq!(
        rows(&mut adapter, "SELECT id, email, name FROM lw ORDER BY id"),
        vec![
            some(&["18446744073709551000", "a@x.com", "A3"]),
            some(&["18446744073709551001", "b@x.com", "B2"]),
            some(&["18446744073709551005", "c@x.com", "C"]),
        ]
    );
}

/// Laravel prepares every statement, so `upsert()` over several rows arrives
/// with its values bound, and so does `insertOrIgnore()`.
#[test]
fn a_prepared_upsert_of_several_rows_reports_what_the_text_one_does() {
    let (_directory, mut adapter) = adapter();
    run(
        &mut adapter,
        "create table `lp` (`id` bigint unsigned not null auto_increment primary key, `name` varchar(255) not null, `email` varchar(255) not null)",
    );
    run(
        &mut adapter,
        "alter table `lp` add unique `lp_email_unique`(`email`)",
    );
    assert_eq!(
        prepared_write(
            &mut adapter,
            "insert into `lp` (`email`, `name`) values (?, ?), (?, ?), (?, ?)",
            &words(&["a@x.com", "A", "b@x.com", "B", "c@x.com", "C"]),
        ),
        (3, 1)
    );
    let upsert = "insert into `lp` (`email`, `name`) values (?, ?), (?, ?) as laravel_upsert_alias on duplicate key update `name` = `laravel_upsert_alias`.`name`";
    assert_eq!(
        prepared_write(
            &mut adapter,
            upsert,
            &words(&["b@x.com", "B2", "d@x.com", "D"])
        ),
        (3, 4)
    );
    assert_eq!(one(&mut adapter, "SELECT LAST_INSERT_ID()"), "4");
    assert_eq!(
        prepared_write(
            &mut adapter,
            upsert,
            &words(&["a@x.com", "A2", "b@x.com", "B2"])
        ),
        (2, 2)
    );
    assert_eq!(one(&mut adapter, "SELECT LAST_INSERT_ID()"), "4");
    // A value bound in the upsert clause comes after every row's.
    assert_eq!(
        prepared_write(
            &mut adapter,
            "insert into `lp` (`email`, `name`) values (?, ?), (?, ?) as laravel_upsert_alias on duplicate key update `name` = ?",
            &words(&["e@x.com", "E", "a@x.com", "A9", "Z"]),
        ),
        (3, 8)
    );
    assert_eq!(
        prepared_write(
            &mut adapter,
            "insert ignore into `lp` (`email`, `name`) values (?, ?), (?, ?)",
            &words(&["a@x.com", "A", "f@x.com", "F"]),
        ),
        (1, 10)
    );
    assert_eq!(one(&mut adapter, "SELECT LAST_INSERT_ID()"), "10");
    assert_eq!(counter(&mut adapter, "lp").as_deref(), Some("12"));
    assert_eq!(
        rows(&mut adapter, "SELECT id, email, name FROM lp ORDER BY id"),
        vec![
            some(&["1", "a@x.com", "Z"]),
            some(&["2", "b@x.com", "B2"]),
            some(&["3", "c@x.com", "C"]),
            some(&["4", "d@x.com", "D"]),
            some(&["8", "e@x.com", "E"]),
            some(&["10", "f@x.com", "F"]),
        ]
    );
}

/// A counted table whose trigger writes into a table that counts nothing.
///
/// Measured on MySQL 8.4.11: the trigger sees the id the row took, and the
/// statement counts and reports only its own rows. A statement reading the
/// table its trigger writes is answered 1442.
#[test]
fn a_counted_table_carrying_a_trigger_takes_new_rows() {
    let (_directory, mut adapter) = adapter();
    run(
        &mut adapter,
        "CREATE TABLE posts (id INT NOT NULL AUTO_INCREMENT PRIMARY KEY, title VARCHAR(20) NOT NULL)",
    );
    run(
        &mut adapter,
        "CREATE TABLE post_log (post_id INT, note VARCHAR(20))",
    );
    run(&mut adapter, "CREATE TABLE drafts (title VARCHAR(20))");
    run(&mut adapter, "INSERT INTO drafts (title) VALUES ('e')");
    run(
        &mut adapter,
        "CREATE TRIGGER posts_log AFTER INSERT ON posts FOR EACH ROW BEGIN INSERT INTO post_log (post_id, note) VALUES (NEW.id, NEW.title); END",
    );
    assert_eq!(
        written(&mut adapter, "INSERT INTO posts (title) VALUES ('a')"),
        (1, 1)
    );
    assert_eq!(
        written(
            &mut adapter,
            "INSERT INTO posts (title) VALUES ('b'), ('c')"
        ),
        (2, 2)
    );
    assert_eq!(one(&mut adapter, "SELECT LAST_INSERT_ID()"), "2");
    assert_eq!(
        prepared_write(
            &mut adapter,
            "INSERT INTO posts (title) VALUES (?)",
            &words(&["d"])
        ),
        (1, 4)
    );
    assert_eq!(
        written(
            &mut adapter,
            "INSERT INTO posts (title) SELECT title FROM drafts"
        ),
        (1, 5)
    );
    assert_eq!(
        adapter
            .execute_query("INSERT INTO posts (title) SELECT note FROM post_log WHERE post_id = 1"),
        Err(FrontendErrorKind::Unsupported)
    );
    assert_eq!(
        written(
            &mut adapter,
            "INSERT INTO posts (id, title) VALUES (NULL, 'f'), (100, 'g')"
        ),
        (2, 6)
    );
    assert_eq!(one(&mut adapter, "SELECT LAST_INSERT_ID()"), "6");
    assert_eq!(
        rows(
            &mut adapter,
            "SELECT post_id, note FROM post_log ORDER BY post_id"
        ),
        vec![
            some(&["1", "a"]),
            some(&["2", "b"]),
            some(&["3", "c"]),
            some(&["4", "d"]),
            some(&["5", "e"]),
            some(&["6", "f"]),
            some(&["100", "g"]),
        ]
    );
    assert_eq!(counter(&mut adapter, "posts").as_deref(), Some("101"));
}

/// The trigger a `mysqldump` of a blog carries: every new post writes a row
/// into an audit table that counts its own ids too.
///
/// Measured on MySQL 8.4.11: each row the trigger writes takes the audit
/// table's next number, one at a time, and neither the statement's reported
/// id nor `LAST_INSERT_ID()` ever answers one of them. A row an upsert
/// changes, or one `IGNORE` skips, fires no trigger and spends nothing in the
/// audit table.
#[test]
fn a_trigger_writes_into_a_counted_table_with_that_tables_next_numbers() {
    let (_directory, mut adapter) = adapter();
    run(
        &mut adapter,
        "CREATE TABLE posts (id INT NOT NULL AUTO_INCREMENT PRIMARY KEY, title VARCHAR(20) NOT NULL, slug INT UNIQUE)",
    );
    run(
        &mut adapter,
        "CREATE TABLE audit (id INT NOT NULL AUTO_INCREMENT PRIMARY KEY, note VARCHAR(20) NOT NULL, post_id INT) AUTO_INCREMENT=100",
    );
    run(
        &mut adapter,
        "CREATE TABLE drafts (k INT PRIMARY KEY, title VARCHAR(20))",
    );
    run(
        &mut adapter,
        "CREATE TRIGGER posts_audit AFTER INSERT ON posts FOR EACH ROW BEGIN INSERT INTO audit (note, post_id) VALUES (NEW.title, NEW.id); END",
    );
    run(
        &mut adapter,
        "CREATE TRIGGER drafts_audit AFTER INSERT ON drafts FOR EACH ROW BEGIN INSERT INTO audit (note, post_id) VALUES (NEW.title, NEW.k); END",
    );

    assert_eq!(
        written(
            &mut adapter,
            "INSERT INTO drafts (k, title) VALUES (7, 'p')"
        ),
        (1, 0)
    );
    assert_eq!(one(&mut adapter, "SELECT LAST_INSERT_ID()"), "0");
    assert_eq!(
        written(
            &mut adapter,
            "INSERT INTO posts (title, slug) VALUES ('a', 1)"
        ),
        (1, 1)
    );
    assert_eq!(
        written(
            &mut adapter,
            "INSERT INTO posts (title) VALUES ('b'), ('c')"
        ),
        (2, 2)
    );
    assert_eq!(
        written(
            &mut adapter,
            "INSERT INTO drafts (k, title) VALUES (8, 'q')"
        ),
        (1, 0)
    );
    assert_eq!(one(&mut adapter, "SELECT LAST_INSERT_ID()"), "2");
    assert_eq!(
        written(
            &mut adapter,
            "INSERT INTO posts (title, slug) VALUES ('z', 1) ON DUPLICATE KEY UPDATE title = 'z'"
        ),
        (2, 1)
    );
    assert_eq!(
        written(
            &mut adapter,
            "INSERT IGNORE INTO posts (title, slug) VALUES ('i', 1), ('j', 3)"
        ),
        (1, 5)
    );
    assert_eq!(
        prepared_write(
            &mut adapter,
            "INSERT INTO posts (title) VALUES (?)",
            &words(&["d"])
        ),
        (1, 7)
    );
    assert_eq!(
        written(
            &mut adapter,
            "INSERT INTO posts (id, title) VALUES (50, 'x')"
        ),
        (1, 50)
    );
    assert_eq!(
        written(
            &mut adapter,
            "INSERT INTO posts (title) SELECT title FROM drafts ORDER BY k"
        ),
        (2, 51)
    );
    assert_eq!(one(&mut adapter, "SELECT LAST_INSERT_ID()"), "51");
    assert_eq!(
        rows(
            &mut adapter,
            "SELECT id, note, post_id FROM audit ORDER BY id"
        ),
        vec![
            some(&["100", "p", "7"]),
            some(&["101", "a", "1"]),
            some(&["102", "b", "2"]),
            some(&["103", "c", "3"]),
            some(&["104", "q", "8"]),
            some(&["105", "j", "5"]),
            some(&["106", "d", "7"]),
            some(&["107", "x", "50"]),
            some(&["108", "p", "51"]),
            some(&["109", "q", "52"]),
        ]
    );
    assert_eq!(counter(&mut adapter, "audit").as_deref(), Some("110"));
    assert_eq!(counter(&mut adapter, "posts").as_deref(), Some("54"));
}

/// A trigger's row sets off the trigger of the table it writes, each counted
/// table taking its own next number.
///
/// Measured on MySQL 8.4.11: a statement whose triggers would write a table it
/// writes or reads is answered 1442 — a trigger writing its own table, or an
/// `INSERT ... SELECT` reading a table a trigger down the line writes.
#[test]
fn triggers_one_after_another_number_each_counted_table_they_write() {
    let (_directory, mut adapter) = adapter();
    for sql in [
        "CREATE TABLE posts (id INT NOT NULL AUTO_INCREMENT PRIMARY KEY, title VARCHAR(20))",
        "CREATE TABLE audit (id INT NOT NULL AUTO_INCREMENT PRIMARY KEY, note VARCHAR(20) NOT NULL, post_id INT) AUTO_INCREMENT=100",
        "CREATE TABLE audit2 (id INT NOT NULL AUTO_INCREMENT PRIMARY KEY, note VARCHAR(20)) AUTO_INCREMENT=1000",
        "CREATE TABLE drafts (k INT PRIMARY KEY, title VARCHAR(20))",
        "CREATE TRIGGER posts_audit AFTER INSERT ON posts FOR EACH ROW BEGIN INSERT INTO audit (note, post_id) VALUES (NEW.title, NEW.id); END",
        "CREATE TRIGGER audit_audit AFTER INSERT ON audit FOR EACH ROW BEGIN INSERT INTO audit2 (note) VALUES (NEW.note); END",
        "CREATE TRIGGER drafts_audit AFTER INSERT ON drafts FOR EACH ROW BEGIN INSERT INTO audit (note, post_id) VALUES (NEW.title, NEW.k); END",
    ] {
        run(&mut adapter, sql);
    }
    assert_eq!(
        written(&mut adapter, "INSERT INTO posts (title) VALUES ('a')"),
        (1, 1)
    );
    assert_eq!(
        prepared_write(
            &mut adapter,
            "INSERT INTO posts (title) VALUES (?)",
            &words(&["b"])
        ),
        (1, 2)
    );
    assert_eq!(
        prepared_write(
            &mut adapter,
            "INSERT INTO drafts (k, title) VALUES (5, ?)",
            &words(&["d"])
        ),
        (1, 0)
    );
    for sql in [
        "INSERT INTO drafts (k, title) SELECT id, note FROM audit",
        "INSERT INTO drafts (k, title) SELECT id, note FROM audit2",
        "INSERT INTO posts (title) SELECT note FROM audit2",
    ] {
        assert_eq!(
            adapter.execute_query(sql),
            Err(FrontendErrorKind::Unsupported),
            "{sql}"
        );
    }
    // Written one row at a time while the counter's file is held, which a
    // trigger taking a number from that counter cannot wait for.
    assert_eq!(
        adapter.execute_query("INSERT INTO posts (id, title) VALUES (NULL, 'm'), (500, 'n')"),
        Err(FrontendErrorKind::Unsupported)
    );
    assert_eq!(one(&mut adapter, "SELECT LAST_INSERT_ID()"), "2");
    assert_eq!(
        rows(
            &mut adapter,
            "SELECT id, note, post_id FROM audit ORDER BY id"
        ),
        vec![
            some(&["100", "a", "1"]),
            some(&["101", "b", "2"]),
            some(&["102", "d", "5"]),
        ]
    );
    assert_eq!(
        rows(&mut adapter, "SELECT id, note FROM audit2 ORDER BY id"),
        vec![
            some(&["1000", "a"]),
            some(&["1001", "b"]),
            some(&["1002", "d"]),
        ]
    );

    run(
        &mut adapter,
        "CREATE TABLE selfish (id INT NOT NULL AUTO_INCREMENT PRIMARY KEY, n INT)",
    );
    run(
        &mut adapter,
        "CREATE TRIGGER selfish_again AFTER INSERT ON selfish FOR EACH ROW BEGIN INSERT INTO selfish (n) VALUES (NEW.n); END",
    );
    assert_eq!(
        adapter.execute_query("INSERT INTO selfish (n) VALUES (1)"),
        Err(FrontendErrorKind::Unsupported)
    );
    assert_eq!(
        rows(&mut adapter, "SELECT COUNT(*) FROM selfish"),
        vec![some(&["0"])]
    );
}

/// A trigger naming the id it writes into a counted table would have to move
/// that table's counter from inside the engine, which is refused.
#[test]
fn a_trigger_writing_a_counted_tables_id_itself_is_refused() {
    let (_directory, mut adapter) = adapter();
    for sql in [
        "CREATE TABLE posts (id INT NOT NULL AUTO_INCREMENT PRIMARY KEY, title VARCHAR(20))",
        "CREATE TABLE mirror (id INT NOT NULL AUTO_INCREMENT PRIMARY KEY, title VARCHAR(20))",
        "CREATE TRIGGER posts_mirror AFTER INSERT ON posts FOR EACH ROW BEGIN INSERT INTO mirror (id, title) VALUES (NEW.id, NEW.title); END",
    ] {
        run(&mut adapter, sql);
    }
    assert_eq!(
        adapter.execute_query("INSERT INTO posts (title) VALUES ('a')"),
        Err(FrontendErrorKind::Unsupported)
    );
    assert_eq!(
        rows(&mut adapter, "SELECT COUNT(*) FROM mirror"),
        vec![some(&["0"])]
    );
}
