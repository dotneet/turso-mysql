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
