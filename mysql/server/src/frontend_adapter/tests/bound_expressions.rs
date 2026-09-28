//! A bound value (`?`) inside an expression, the way GORM sends every value
//! through server-side prepared statements.
//!
//! Every expectation here was measured on MySQL 8.4.11 with go-sql-driver
//! 1.8.1, which binds a Go string as `MYSQL_TYPE_STRING`, an `int64` as
//! `MYSQL_TYPE_LONGLONG` and a `float64` as `MYSQL_TYPE_DOUBLE`.

use super::*;

type Adapter = AuthorizedDatabaseCommandAdapter<RecordingAuthorizer>;

fn adapter() -> (tempfile::TempDir, Adapter) {
    let authorizer = Arc::new(RecordingAuthorizer::default());
    let (directory, _catalog, factory) = catalog_factory(authorizer);
    let mut adapter = factory
        .build(AuthenticatedPrincipal::from_account_id_for_testing(
            AccountId::from_bytes([151; 32]),
        ))
        .unwrap();
    adapter.authorize_connection().unwrap();
    adapter.execute_init_db("REPORTS").unwrap();
    for sql in [GORM_USERS, GORM_POSTS] {
        adapter
            .execute_query(sql)
            .unwrap_or_else(|error| panic!("{sql}: {error:?}"));
    }
    (directory, adapter)
}

const GORM_USERS: &str = "CREATE TABLE `users` (`id` bigint unsigned AUTO_INCREMENT,`email` varchar(191) NOT NULL,`name` varchar(100) NOT NULL,`balance` decimal(10,2) NOT NULL DEFAULT 0,`is_active` boolean NOT NULL,`profile` JSON,`created_at` datetime(3) NULL,`updated_at` datetime(3) NULL,PRIMARY KEY (`id`),UNIQUE INDEX `idx_users_email` (`email`))";
const GORM_POSTS: &str = "CREATE TABLE `posts` (`id` bigint unsigned AUTO_INCREMENT,`user_id` bigint unsigned NOT NULL,`title` varchar(200) NOT NULL,`body` text,`published_at` datetime(3) NULL,`views` bigint NOT NULL DEFAULT 0,`created_at` datetime(3) NULL,`updated_at` datetime(3) NULL,PRIMARY KEY (`id`),INDEX `idx_posts_user_id` (`user_id`),CONSTRAINT `fk_users_posts` FOREIGN KEY (`user_id`) REFERENCES `users`(`id`) ON DELETE CASCADE)";

/// One value as go-sql-driver binds it.
#[derive(Clone, Copy)]
enum Bound<'a> {
    Word(&'a str),
    Whole(i64),
    Real(f64),
    Null,
}

/// The null bitmap, the new-parameters flag, the types and the values.
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
            Bound::Real(_) => MYSQL_TYPE_DOUBLE,
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
            Bound::Real(number) => payload.extend_from_slice(&number.to_le_bytes()),
            Bound::Null => {}
        }
    }
    payload
}

fn prepared(
    adapter: &mut Adapter,
    sql: &str,
    values: &[Bound<'_>],
) -> Result<PreparedStatementExecutionResult, FrontendErrorKind> {
    let statement = adapter
        .execute_stmt_prepare(sql)
        .inspect_err(|_| eprintln!("  (at prepare)"))?;
    let result = adapter.execute_stmt_execute(statement.statement_id, &payload(values));
    adapter.execute_stmt_close(statement.statement_id);
    result
}

fn result_rows(
    adapter: &mut Adapter,
    sql: &str,
    values: &[Bound<'_>],
) -> Vec<Vec<BinaryResultValue>> {
    match prepared(adapter, sql, values) {
        Ok(PreparedStatementExecutionResult::ResultSet(result)) => result.rows,
        other => panic!("{sql} must answer rows, answered {other:?}"),
    }
}

fn words(row: &[&str]) -> Vec<BinaryResultValue> {
    row.iter()
        .map(|word| BinaryResultValue::Text((*word).to_owned()))
        .collect()
}

/// GORM's Migrator lists a database's tables and a table's indexes with the
/// names bound. Measured on MySQL 8.4.11: each index column comes back in the
/// order of its index's name, `PRIMARY` after `idx_posts_user_id` because the
/// names compare without regard to case.
#[test]
fn gorm_reads_the_catalog_with_the_names_bound() {
    let (_directory, mut adapter) = adapter();
    assert_eq!(
        result_rows(
            &mut adapter,
            "SELECT TABLE_NAME FROM information_schema.tables where TABLE_SCHEMA=?",
            &[Bound::Word("REPORTS")],
        ),
        vec![words(&["posts"]), words(&["records"]), words(&["users"])]
    );
    let indexes = "\nSELECT\n\tTABLE_NAME,\n\tCOLUMN_NAME,\n\tINDEX_NAME,\n\tNON_UNIQUE \nFROM\n\tinformation_schema.STATISTICS \nWHERE\n\tTABLE_SCHEMA = ? \n\tAND TABLE_NAME = ? \nORDER BY\n\tINDEX_NAME,\n\tSEQ_IN_INDEX";
    let index_row = |table: &str, column: &str, index: &str, non_unique: i64| {
        let mut row = words(&[table, column, index]);
        row.push(BinaryResultValue::Integer(non_unique));
        row
    };
    assert_eq!(
        result_rows(
            &mut adapter,
            indexes,
            &[Bound::Word("REPORTS"), Bound::Word("users")]
        ),
        vec![
            index_row("users", "email", "idx_users_email", 0),
            index_row("users", "id", "PRIMARY", 0),
        ]
    );
    assert_eq!(
        result_rows(
            &mut adapter,
            indexes,
            &[Bound::Word("REPORTS"), Bound::Word("posts")]
        ),
        vec![
            index_row("posts", "user_id", "idx_posts_user_id", 1),
            index_row("posts", "id", "PRIMARY", 0),
        ]
    );
}

fn run(adapter: &mut Adapter, sql: &str) {
    adapter
        .execute_query(sql)
        .unwrap_or_else(|error| panic!("{sql}: {error:?}"));
}

/// Two users, the first with two posts and the second with one.
fn two_users_with_posts(adapter: &mut Adapter) {
    run(
        adapter,
        "INSERT INTO users (id, email, name, balance, is_active) VALUES (1, 'a@x', 'A', 100.50, 1), (2, 'b@x', 'B', 0.00, 1)",
    );
    run(
        adapter,
        "INSERT INTO posts (user_id, title, views) VALUES (1, 'p1', 5), (1, 'p2', 7), (2, 'p3', 1)",
    );
}

/// GORM's `Group("user_id").Having("COUNT(*) > ?", 1)`. Measured on MySQL
/// 8.4.11: a bound whole number and a bound word naming one compare with the
/// count as that number, a bound double as a number — 1.5 finds the groups of
/// two and 0.5 every group — and NULL finds nothing.
#[test]
fn gorm_compares_a_count_with_a_bound_value() {
    let (_directory, mut adapter) = adapter();
    two_users_with_posts(&mut adapter);
    assert_eq!(
        result_rows(
            &mut adapter,
            "SELECT user_id, COUNT(*) AS n, SUM(views) AS total, AVG(views) AS average FROM `posts` GROUP BY `user_id` HAVING COUNT(*) > ? ORDER BY user_id",
            &[Bound::Whole(1)],
        ),
        vec![vec![
            BinaryResultValue::UnsignedInteger(1),
            BinaryResultValue::Integer(2),
            BinaryResultValue::Text("12".to_owned()),
            BinaryResultValue::Text("6.0000".to_owned()),
        ]]
    );
    let having =
        "SELECT user_id FROM `posts` GROUP BY `user_id` HAVING COUNT(*) > ? ORDER BY user_id";
    let users = |ids: &[u64]| {
        ids.iter()
            .map(|id| vec![BinaryResultValue::UnsignedInteger(*id)])
            .collect::<Vec<_>>()
    };
    for (bound, found) in [
        (Bound::Whole(1), users(&[1])),
        (Bound::Whole(-1), users(&[1, 2])),
        (Bound::Word("1"), users(&[1])),
        (Bound::Real(1.5), users(&[1])),
        (Bound::Real(0.5), users(&[1, 2])),
        (Bound::Null, users(&[])),
    ] {
        assert_eq!(result_rows(&mut adapter, having, &[bound]), found);
    }
}

/// A bound word that is no whole number is read by MySQL's own conversion —
/// `'abc'` as 0 with warning 1292, `'1.5'` by a rule not measured — so it is
/// refused rather than compared as a word, which the engine orders after
/// every number.
#[test]
fn a_word_that_is_no_whole_number_is_refused_against_a_count() {
    let (_directory, mut adapter) = adapter();
    two_users_with_posts(&mut adapter);
    for word in ["abc", "1.5"] {
        assert_eq!(
            prepared(
                &mut adapter,
                "SELECT user_id FROM `posts` GROUP BY `user_id` HAVING COUNT(*) > ?",
                &[Bound::Word(word)],
            ),
            Err(FrontendErrorKind::Unsupported),
            "{word}"
        );
    }
}

/// The first column of every row a text statement answers.
fn first_column(adapter: &mut Adapter, sql: &str) -> Vec<String> {
    let Ok(CommandExecutionResult::ResultSet(result)) = adapter.execute_query(sql) else {
        panic!("{sql} must answer rows");
    };
    result
        .rows
        .into_iter()
        .map(|row| String::from_utf8(row[0].clone().unwrap()).unwrap())
        .collect()
}

fn one_value(adapter: &mut Adapter, sql: &str) -> String {
    first_column(adapter, sql).remove(0)
}

fn changed(adapter: &mut Adapter, sql: &str, values: &[Bound<'_>]) -> u64 {
    match prepared(adapter, sql, values) {
        Ok(PreparedStatementExecutionResult::Ok(result)) => result.affected_rows,
        other => panic!("{sql} must answer OK, answered {other:?}"),
    }
}

/// GORM's `UpdateColumn("balance", gorm.Expr("balance - ?", 10))` into a
/// `DECIMAL(10,2)`. Measured on MySQL 8.4.11: a bound whole number and a bound
/// word naming a number are taken exactly, the answer rounded half away from
/// zero into the column — 90.50 less `'0.005'` is 90.495, which stores as
/// 90.50 and changes nothing. A bound double makes the arithmetic a double's
/// (1.97 plus 0.145 stores 2.11 there, and 2.12 when done exactly), a word
/// naming no number fails with 1292, and NULL with 1048.
#[test]
fn gorm_takes_a_bound_amount_from_a_decimal() {
    let (_directory, mut adapter) = adapter();
    two_users_with_posts(&mut adapter);
    run(&mut adapter, "UPDATE users SET is_active = 0 WHERE id = 2");
    let take = "UPDATE `users` SET `balance`=balance - ? WHERE is_active = ?";
    let balance = "SELECT balance FROM users WHERE id = 1";
    for (amount, rows, stored) in [
        (Bound::Whole(10), 1, "90.50"),
        (Bound::Word("0.005"), 0, "90.50"),
        (Bound::Word("-1"), 1, "91.50"),
        (Bound::Word("+1.5"), 1, "90.00"),
    ] {
        assert_eq!(
            changed(&mut adapter, take, &[amount, Bound::Whole(1)]),
            rows
        );
        assert_eq!(one_value(&mut adapter, balance), stored);
    }
    for amount in [Bound::Real(0.015), Bound::Word("abc"), Bound::Word(" 2 ")] {
        assert_eq!(
            prepared(&mut adapter, take, &[amount, Bound::Whole(1)]),
            Err(FrontendErrorKind::Unsupported)
        );
    }
    assert_eq!(
        prepared(&mut adapter, take, &[Bound::Null, Bound::Whole(1)]),
        Err(FrontendErrorKind::NotNullViolation)
    );
    assert_eq!(one_value(&mut adapter, balance), "90.00");
}

/// GORM's `UpdateColumn("views", gorm.Expr("views + ?", 3))` into a
/// `BIGINT`. Measured on MySQL 8.4.11: a bound whole number, or a word naming
/// one, is added exactly; a bound double or a word naming a fraction is added
/// as a double and the answer rounded into the column, 8 plus 1.5 storing 10,
/// which the engine refuses to store; a word naming no number fails with 1292.
#[test]
fn gorm_adds_a_bound_count_to_a_whole_number() {
    let (_directory, mut adapter) = adapter();
    two_users_with_posts(&mut adapter);
    let add = "UPDATE `posts` SET `views`=views + ? WHERE views > ?";
    assert_eq!(
        changed(&mut adapter, add, &[Bound::Whole(3), Bound::Whole(4)]),
        2
    );
    assert_eq!(
        changed(&mut adapter, add, &[Bound::Word("2"), Bound::Whole(4)]),
        2
    );
    assert_eq!(
        first_column(&mut adapter, "SELECT views FROM posts ORDER BY id"),
        ["10", "12", "1"]
    );
    for amount in [Bound::Real(1.5), Bound::Word("2.5"), Bound::Word("x")] {
        assert_eq!(
            prepared(&mut adapter, add, &[amount, Bound::Whole(4)]),
            Err(FrontendErrorKind::Unsupported)
        );
    }
}

/// Each `?` in a `SET` holds its own place among the statement's parameters,
/// so the rule for the one an arithmetic reads falls on that one alone.
#[test]
fn a_bound_value_before_the_arithmetic_keeps_its_own_place() {
    let (_directory, mut adapter) = adapter();
    two_users_with_posts(&mut adapter);
    let update = "UPDATE users SET name = ?, balance = balance - ? WHERE is_active = ?";
    assert_eq!(
        changed(
            &mut adapter,
            update,
            &[Bound::Word("abc"), Bound::Whole(1), Bound::Whole(1)]
        ),
        2
    );
    assert_eq!(
        prepared(
            &mut adapter,
            update,
            &[Bound::Whole(7), Bound::Real(1.5), Bound::Whole(1)]
        ),
        Err(FrontendErrorKind::Unsupported)
    );
    assert_eq!(
        first_column(&mut adapter, "SELECT name FROM users ORDER BY id"),
        ["abc", "abc"]
    );
    assert_eq!(
        first_column(&mut adapter, "SELECT balance FROM users ORDER BY id"),
        ["99.50", "-1.00"]
    );
}

/// A row an `UPDATE` changes takes the moment in its `ON UPDATE
/// CURRENT_TIMESTAMP` column whichever value changed it, the second of two
/// bound ones included; one it leaves as it was keeps its moment.
#[test]
fn the_second_bound_value_in_a_set_stamps_the_row() {
    let (_directory, mut adapter) = adapter();
    run(
        &mut adapter,
        "CREATE TABLE stamped (id INT PRIMARY KEY, a INT, b INT, updated_at DATETIME NULL ON UPDATE CURRENT_TIMESTAMP)",
    );
    run(
        &mut adapter,
        "INSERT INTO stamped (id, a, b) VALUES (1, 1, 1)",
    );
    let update = "UPDATE stamped SET a = ?, b = ? WHERE id = ?";
    let stamped = "SELECT updated_at IS NOT NULL FROM stamped";
    assert_eq!(
        changed(
            &mut adapter,
            update,
            &[Bound::Whole(1), Bound::Whole(1), Bound::Whole(1)]
        ),
        0
    );
    assert_eq!(one_value(&mut adapter, stamped), "0");
    assert_eq!(
        changed(
            &mut adapter,
            update,
            &[Bound::Whole(1), Bound::Whole(2), Bound::Whole(1)]
        ),
        1
    );
    assert_eq!(one_value(&mut adapter, stamped), "1");
}
