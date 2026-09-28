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

/// One value as go-sql-driver binds it. GORM's ids are Go `uint`s, which it
/// binds as a `LONGLONG` flagged unsigned.
#[derive(Clone, Copy)]
enum Bound<'a> {
    Word(&'a str),
    Whole(i64),
    Id(u64),
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
        let (code, flags) = match value {
            Bound::Word(_) => (MYSQL_TYPE_STRING, 0),
            Bound::Whole(_) => (MYSQL_TYPE_LONGLONG, 0),
            Bound::Id(_) => (MYSQL_TYPE_LONGLONG, 0x80),
            Bound::Real(_) => (MYSQL_TYPE_DOUBLE, 0),
            Bound::Null => (MYSQL_TYPE_NULL, 0),
        };
        payload.extend_from_slice(&[code, flags]);
    }
    for value in values {
        match value {
            Bound::Word(word) => {
                payload.push(u8::try_from(word.len()).unwrap());
                payload.extend_from_slice(word.as_bytes());
            }
            Bound::Whole(number) => payload.extend_from_slice(&number.to_le_bytes()),
            Bound::Id(number) => payload.extend_from_slice(&number.to_le_bytes()),
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

/// The first column of every row a text statement answers, NULL spelled out.
fn first_column(adapter: &mut Adapter, sql: &str) -> Vec<String> {
    let Ok(CommandExecutionResult::ResultSet(result)) = adapter.execute_query(sql) else {
        panic!("{sql} must answer rows");
    };
    result
        .rows
        .into_iter()
        .map(|row| {
            row[0].clone().map_or_else(
                || "NULL".to_owned(),
                |value| String::from_utf8(value).unwrap(),
            )
        })
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

/// GORM finds every row it changes by an id, a `BIGINT UNSIGNED` it binds.
/// The engine keeps such a column in a form of its own, and compared it with
/// a bound number by kind: `WHERE user_id = ?` changed no row, `WHERE id > ?`
/// every row, and a written `id IN (1, 2)` none either. Measured on MySQL
/// 8.4.11, each finds the rows whose ids compare as numbers.
#[test]
fn gorm_finds_the_rows_it_changes_by_a_bound_id() {
    let (_directory, mut adapter) = adapter();
    two_users_with_posts(&mut adapter);
    run(
        &mut adapter,
        "CREATE TABLE `tags` (`id` bigint unsigned AUTO_INCREMENT,`name` varchar(64) NOT NULL,PRIMARY KEY (`id`),UNIQUE INDEX `idx_tags_name` (`name`))",
    );
    run(
        &mut adapter,
        "CREATE TABLE `post_tags` (`post_id` bigint unsigned,`tag_id` bigint unsigned,PRIMARY KEY (`post_id`,`tag_id`),CONSTRAINT `fk_post_tags_post` FOREIGN KEY (`post_id`) REFERENCES `posts`(`id`),CONSTRAINT `fk_post_tags_tag` FOREIGN KEY (`tag_id`) REFERENCES `tags`(`id`))",
    );
    run(
        &mut adapter,
        "INSERT INTO tags (id, name) VALUES (1, 'go'), (2, 'sql'), (3, 'db')",
    );
    run(
        &mut adapter,
        "INSERT INTO post_tags (post_id, tag_id) VALUES (1, 1), (1, 2), (1, 3), (2, 1)",
    );
    let views = "SELECT views FROM posts ORDER BY id";

    assert_eq!(
        changed(
            &mut adapter,
            "UPDATE `posts` SET `views`=views + ? WHERE user_id = ?",
            &[Bound::Whole(3), Bound::Id(1)]
        ),
        2
    );
    assert_eq!(first_column(&mut adapter, views), ["8", "10", "1"]);
    assert_eq!(
        changed(
            &mut adapter,
            "DELETE FROM `post_tags` WHERE `post_tags`.`post_id` = ? AND `post_tags`.`tag_id` NOT IN (?,?)",
            &[Bound::Id(1), Bound::Id(1), Bound::Id(2)]
        ),
        1
    );
    assert_eq!(
        first_column(
            &mut adapter,
            "SELECT CONCAT(post_id, '-', tag_id) FROM post_tags ORDER BY post_id, tag_id"
        ),
        ["1-1", "1-2", "2-1"]
    );
    assert_eq!(
        changed(
            &mut adapter,
            "UPDATE `posts` SET `views`=0 WHERE id > ?",
            &[Bound::Id(1)]
        ),
        2
    );
    assert_eq!(first_column(&mut adapter, views), ["8", "0", "0"]);
    assert_eq!(
        changed(
            &mut adapter,
            "UPDATE `posts` SET `views`=1 WHERE id BETWEEN ? AND ?",
            &[Bound::Id(2), Bound::Id(3)]
        ),
        2
    );
    assert_eq!(first_column(&mut adapter, views), ["8", "1", "1"]);
    assert!(matches!(
        adapter.execute_query("UPDATE posts SET views = 100 WHERE id IN (1, 2)"),
        Ok(CommandExecutionResult::Ok(result)) if result.affected_rows == 2
    ));
    assert_eq!(first_column(&mut adapter, views), ["100", "100", "1"]);
    assert_eq!(
        changed(
            &mut adapter,
            "UPDATE `users` SET `name`=?,`updated_at`=? WHERE `id` = ?",
            &[
                Bound::Word("Rob"),
                Bound::Word("2026-09-28 01:46:13.123"),
                Bound::Id(2)
            ]
        ),
        1
    );
    assert_eq!(
        first_column(&mut adapter, "SELECT name FROM users ORDER BY id"),
        ["A", "Rob"]
    );
    assert_eq!(
        changed(
            &mut adapter,
            "DELETE FROM `users` WHERE `users`.`id` = ?",
            &[Bound::Id(2)]
        ),
        1
    );
    assert_eq!(
        first_column(&mut adapter, "SELECT id FROM posts ORDER BY id"),
        ["1", "2"]
    );
}

const GORM_CREATE_USER: &str = "INSERT INTO `users` (`email`,`name`,`balance`,`is_active`,`profile`,`created_at`,`updated_at`) VALUES (?,?,?,?,CAST(? AS JSON),?,?)";

/// The row GORM's `Create` binds, with the profile bound as given.
fn gorm_user<'a>(email: &'a str, profile: Bound<'a>) -> [Bound<'a>; 7] {
    [
        Bound::Word(email),
        Bound::Word("Someone"),
        Bound::Real(100.5),
        Bound::Whole(1),
        profile,
        Bound::Word("2026-09-28 01:46:13.123"),
        Bound::Word("2026-09-28 01:46:13.123"),
    ]
}

/// GORM writes a `datatypes.JSON` as `CAST(? AS JSON)`. Measured on MySQL
/// 8.4.11: a bound word is parsed and stored as MySQL stores a document —
/// keys shortest first, one space after each colon and comma — a bound whole
/// number as that JSON number and NULL as NULL. A word that is no document,
/// the empty word among them, fails with 3141 and writes nothing, which is
/// refused here; so is a bound double, whose JSON spelling was not measured.
#[test]
fn gorm_writes_a_bound_document_through_a_json_cast() {
    let (_directory, mut adapter) = adapter();
    for (email, profile) in [
        (
            "a@x",
            Bound::Word("{\"city\": \"Paris\", \"tags\": [\"a\", \"b\"]}"),
        ),
        (
            "b@x",
            Bound::Word("  [1, 2.50, \"x\", null, true, {\"b\":1,\"a\":2}]  "),
        ),
        ("c@x", Bound::Whole(42)),
        ("d@x", Bound::Null),
    ] {
        assert_eq!(
            changed(&mut adapter, GORM_CREATE_USER, &gorm_user(email, profile)),
            1
        );
    }
    for profile in [
        Bound::Word("not json"),
        Bound::Word(""),
        Bound::Word("{bad"),
        Bound::Real(1.5),
    ] {
        assert_eq!(
            prepared(&mut adapter, GORM_CREATE_USER, &gorm_user("e@x", profile)),
            Err(FrontendErrorKind::Unsupported)
        );
    }
    assert_eq!(
        first_column(&mut adapter, "SELECT profile FROM users ORDER BY id"),
        [
            "{\"city\": \"Paris\", \"tags\": [\"a\", \"b\"]}",
            "[1, 2.5, \"x\", null, true, {\"a\": 2, \"b\": 1}]",
            "42",
            "NULL"
        ]
    );
}

/// GORM's `Save` writes every column again, the document through the same
/// cast, and a text statement may write one with a written word.
#[test]
fn a_json_cast_writes_a_document_in_an_update_and_in_text() {
    let (_directory, mut adapter) = adapter();
    assert_eq!(
        changed(
            &mut adapter,
            GORM_CREATE_USER,
            &gorm_user("a@x", Bound::Word("{\"x\": 0}"))
        ),
        1
    );
    let save = "UPDATE `users` SET `email`=?,`name`=?,`balance`=?,`is_active`=?,`profile`=CAST(? AS JSON),`created_at`=?,`updated_at`=? WHERE `id` = ?";
    let saved = |profile| {
        let [email, name, balance, active, _, created, updated] = gorm_user("a@x", profile);
        [
            email,
            name,
            balance,
            active,
            profile,
            created,
            updated,
            Bound::Id(1),
        ]
    };
    assert_eq!(
        changed(&mut adapter, save, &saved(Bound::Word("{\"x\":1}"))),
        1
    );
    assert_eq!(
        prepared(&mut adapter, save, &saved(Bound::Word("{bad"))),
        Err(FrontendErrorKind::Unsupported)
    );
    assert_eq!(
        first_column(&mut adapter, "SELECT profile FROM users"),
        ["{\"x\": 1}"]
    );
    run(
        &mut adapter,
        "INSERT INTO users (email, name, is_active, profile) VALUES ('t@x', 'T', 1, CAST('{\"k\": [1,2]}' AS JSON))",
    );
    assert_eq!(
        adapter.execute_query(
            "INSERT INTO users (email, name, is_active, profile) VALUES ('u@x', 'U', 1, CAST('bad' AS JSON))"
        ),
        Err(FrontendErrorKind::Unsupported)
    );
    assert_eq!(
        first_column(&mut adapter, "SELECT profile FROM users ORDER BY id"),
        ["{\"x\": 1}", "{\"k\": [1, 2]}"]
    );
}

/// What a `CAST(... AS JSON)` writes into a column that holds no document
/// has not been measured, so it is refused.
#[test]
fn a_json_cast_into_another_kind_of_column_is_refused() {
    let (_directory, mut adapter) = adapter();
    assert_eq!(
        prepared(
            &mut adapter,
            "INSERT INTO `users` (`email`,`name`,`is_active`) VALUES (?,CAST(? AS JSON),?)",
            &[Bound::Word("a@x"), Bound::Word("\"n\""), Bound::Whole(1)]
        ),
        Err(FrontendErrorKind::Unsupported)
    );
    assert_eq!(
        adapter.execute_query("UPDATE users SET name = CAST('\"n\"' AS JSON) WHERE is_active = 1"),
        Err(FrontendErrorKind::Unsupported)
    );
}

/// Four users whose profiles GORM's `JSONQuery` reads.
fn users_with_profiles(adapter: &mut Adapter) {
    run(
        adapter,
        r#"INSERT INTO users (id, email, name, balance, is_active, profile) VALUES (1, 'a@x', 'Alice', 1, 1, '{"city": "Paris", "tags": ["a", "b"], "age": 30, "n": null, "ok": true, "s": "30"}'), (2, 'b@x', 'Bob', 1, 1, '{"city": "Berlin", "age": 30.0}'), (3, 'c@x', 'Carol', 1, 1, NULL), (4, 'd@x', 'Dave', 1, 1, '[1, 2]')"#,
    );
}

fn ids(adapter: &mut Adapter, sql: &str, values: &[Bound<'_>]) -> Vec<u64> {
    result_rows(adapter, sql, values)
        .into_iter()
        .map(|row| match row[0] {
            BinaryResultValue::UnsignedInteger(id) => id,
            ref other => panic!("{sql} answered {other:?} for an id"),
        })
        .collect()
}

const GORM_JSON_EQUALS: &str = "SELECT id FROM `users` WHERE JSON_EXTRACT(`profile`,?) = ?";

/// GORM's `datatypes.JSONQuery("profile").Equals(value, "city")` binds the
/// path and the value. Measured on MySQL 8.4.11: a bound word finds the JSON
/// string holding exactly those bytes and nothing else — not a number, not
/// `true`, not the JSON null, not another case or a trailing space — a bound
/// whole number finds a JSON number of that value, 30.0 included, and NULL
/// finds nothing, as does a NULL path; `$[0]` over an array is its first
/// element. A path MySQL refuses (3143), one read as more than one value, and
/// a number bound as the path (3144) are refused here, and so is a bound
/// double, which was not measured past one value.
#[test]
fn gorm_compares_a_json_value_read_by_a_bound_path() {
    let (_directory, mut adapter) = adapter();
    users_with_profiles(&mut adapter);
    for (path, value, found) in [
        (Bound::Word("$.city"), Bound::Word("Paris"), vec![1]),
        (Bound::Word("$.age"), Bound::Whole(30), vec![1, 2]),
        (Bound::Word("$.age"), Bound::Word("30"), vec![]),
        (Bound::Word("$.s"), Bound::Word("30"), vec![1]),
        (Bound::Word("$.s"), Bound::Whole(30), vec![]),
        (Bound::Word("$.ok"), Bound::Whole(1), vec![]),
        (Bound::Word("$.ok"), Bound::Word("true"), vec![]),
        (Bound::Word("$.n"), Bound::Word("null"), vec![]),
        (Bound::Word("$.city"), Bound::Null, vec![]),
        (Bound::Word("$.city"), Bound::Word("paris"), vec![]),
        (Bound::Word("$.city"), Bound::Word("Paris "), vec![]),
        (Bound::Word("$[0]"), Bound::Whole(1), vec![4]),
        (Bound::Null, Bound::Word("Paris"), vec![]),
    ] {
        assert_eq!(ids(&mut adapter, GORM_JSON_EQUALS, &[path, value]), found);
    }
    for (path, value) in [
        (Bound::Word("$.*"), Bound::Word("Paris")),
        (Bound::Word("bad path"), Bound::Word("Paris")),
        (Bound::Whole(1), Bound::Word("Paris")),
        (Bound::Word("$.age"), Bound::Real(30.0)),
    ] {
        assert_eq!(
            prepared(&mut adapter, GORM_JSON_EQUALS, &[path, value]),
            Err(FrontendErrorKind::Unsupported)
        );
    }
    let every_column = result_rows(
        &mut adapter,
        "SELECT * FROM `users` WHERE JSON_EXTRACT(`profile`,?) = ?",
        &[Bound::Word("$.city"), Bound::Word("Paris")],
    );
    assert_eq!(every_column.len(), 1);
    assert_eq!(every_column[0][0], BinaryResultValue::UnsignedInteger(1));
}

/// GORM's `JSONQuery("profile").HasKey("tags")`. Measured on MySQL 8.4.11: a
/// member holding the JSON null is there, and a path an array does not have
/// is not.
#[test]
fn gorm_asks_whether_a_bound_json_path_is_there() {
    let (_directory, mut adapter) = adapter();
    users_with_profiles(&mut adapter);
    let is_there = "SELECT id FROM `users` WHERE JSON_EXTRACT(`profile`,?) IS NOT NULL";
    assert_eq!(ids(&mut adapter, is_there, &[Bound::Word("$.tags")]), [1]);
    assert_eq!(ids(&mut adapter, is_there, &[Bound::Word("$.n")]), [1]);
    assert_eq!(
        ids(
            &mut adapter,
            "SELECT id FROM `users` WHERE JSON_EXTRACT(`profile`,?) IS NULL ORDER BY id",
            &[Bound::Word("$.nope")]
        ),
        [1, 2, 3, 4]
    );
}

/// MySQL settles how it reads what binds against a JSON value by what the
/// statement bound before. Measured on 8.4.11: once a whole number has bound
/// there, a word bound later finds nothing — `'Paris'` no longer finds
/// Paris — so a word after a number is refused until the statement is
/// prepared again. A NULL bound first settles nothing.
#[test]
fn a_word_bound_after_a_number_against_a_json_value_is_refused() {
    let (_directory, mut adapter) = adapter();
    users_with_profiles(&mut adapter);
    let statement = adapter.execute_stmt_prepare(GORM_JSON_EQUALS).unwrap();
    let mut run_with = |values: &[Bound<'_>]| {
        adapter.execute_stmt_execute(statement.statement_id, &payload(values))
    };
    for values in [
        [Bound::Word("$.city"), Bound::Null],
        [Bound::Word("$.city"), Bound::Word("Paris")],
        [Bound::Word("$.age"), Bound::Whole(30)],
    ] {
        assert!(run_with(&values).is_ok());
    }
    assert_eq!(
        run_with(&[Bound::Word("$.city"), Bound::Word("Paris")]),
        Err(FrontendErrorKind::Unsupported)
    );
}

/// Django's lookup on a `JSONField` names the column through its table and
/// writes the value as a document read back out of itself. Measured on MySQL
/// 8.4.11: a JSON string finds the same string and a JSON whole number finds
/// the numbers of that value; a document of any other kind is refused here.
#[test]
fn django_compares_a_json_value_with_a_written_document() {
    let (_directory, mut adapter) = adapter();
    users_with_profiles(&mut adapter);
    let lookup = |member: &str, document: &str| {
        format!(
            "SELECT `users`.`name` AS `name` FROM `users` WHERE JSON_EXTRACT(`users`.`profile`, '$.\"{member}\"') = JSON_EXTRACT('{document}', '$') ORDER BY `users`.`id`"
        )
    };
    assert_eq!(
        first_column(&mut adapter, &lookup("city", "\"Paris\"")),
        ["Alice"]
    );
    assert_eq!(
        first_column(&mut adapter, &lookup("age", "30")),
        ["Alice", "Bob"]
    );
    for (member, document) in [("tags", "[\"a\", \"b\"]"), ("ok", "true"), ("age", "1.5")] {
        assert!(
            adapter.execute_query(&lookup(member, document)).is_err(),
            "{document}"
        );
    }
}

/// Rails' `increment_counter` counts a column up through a fallback,
/// `SET views = COALESCE(views, 0) + 1`, naming every column through its
/// table. Measured on MySQL 8.4.11: a NULL counts up to 1, 5 to 6, and a
/// fallback on its own writes itself over a NULL.
#[test]
fn rails_counts_a_column_up_through_a_fallback() {
    let (_directory, mut adapter) = adapter();
    run(
        &mut adapter,
        "CREATE TABLE counters (id bigint NOT NULL AUTO_INCREMENT PRIMARY KEY, views int, hits bigint NOT NULL DEFAULT 0, big bigint, uid bigint unsigned, label varchar(20), price decimal(10,2))",
    );
    run(
        &mut adapter,
        "INSERT INTO counters (id, views, hits) VALUES (1, NULL, 5), (2, 5, 0)",
    );
    run(
        &mut adapter,
        "UPDATE `counters` SET `counters`.`views` = COALESCE(`counters`.`views`, 0) + 1 WHERE `counters`.`id` = 1",
    );
    run(
        &mut adapter,
        "UPDATE `counters` SET `counters`.`views` = COALESCE(`counters`.`views`, 0) + 1, `counters`.`hits` = COALESCE(`counters`.`hits`, 0) - 2 WHERE `counters`.`id` IN (1, 2)",
    );
    run(
        &mut adapter,
        "UPDATE counters SET big = COALESCE(big, 7) WHERE id = 2",
    );
    assert_eq!(
        first_column(&mut adapter, "SELECT views FROM counters ORDER BY id"),
        ["2", "6"]
    );
    assert_eq!(
        first_column(&mut adapter, "SELECT hits FROM counters ORDER BY id"),
        ["3", "-2"]
    );
    assert_eq!(
        first_column(&mut adapter, "SELECT big FROM counters ORDER BY id"),
        ["NULL", "7"]
    );
    assert_eq!(
        changed(
            &mut adapter,
            "UPDATE counters SET views = COALESCE(views, 0) + ? WHERE id = ?",
            &[Bound::Whole(10), Bound::Whole(2)]
        ),
        1
    );
    assert_eq!(
        first_column(&mut adapter, "SELECT views FROM counters ORDER BY id"),
        ["2", "16"]
    );
}

/// A fallback naming its column through its table over a column holding
/// anything but whole numbers is refused: MySQL reads a word or a `DECIMAL`
/// by rules of its own there, and the engine would hand a `BIGINT UNSIGNED`
/// on in its stored form.
#[test]
fn a_fallback_in_a_set_over_another_kind_of_column_is_refused() {
    let (_directory, mut adapter) = adapter();
    run(
        &mut adapter,
        "CREATE TABLE counters (id bigint NOT NULL AUTO_INCREMENT PRIMARY KEY, views int, uid bigint unsigned, label varchar(20), price decimal(10,2))",
    );
    for sql in [
        "UPDATE counters SET counters.uid = COALESCE(counters.uid, 0) + 1 WHERE id = 1",
        "UPDATE counters SET counters.label = COALESCE(counters.label, 0) WHERE id = 1",
        "UPDATE counters SET counters.price = COALESCE(counters.price, 0) + 1 WHERE id = 1",
        "UPDATE counters SET counters.views = COALESCE(counters.views, 'x') WHERE id = 1",
    ] {
        assert!(adapter.execute_query(sql).is_err(), "{sql}");
    }
}

/// SQLAlchemy compares a JSON member as text through a `CASE` that answers
/// no value for the JSON null. Measured on MySQL 8.4.11 over the same rows:
/// the text compares byte for byte with trailing spaces ignored, the JSON
/// null answers nothing while the JSON string `"null"` answers the word, and
/// a number reads both sides as numbers.
#[test]
fn sqlalchemy_compares_a_json_member_as_text_unless_it_is_null() {
    let (_directory, mut adapter) = adapter();
    users_with_profiles(&mut adapter);
    run(
        &mut adapter,
        r#"INSERT INTO users (id, email, name, balance, is_active, profile) VALUES (5, 'e@x', 'Eve', 1, 1, '{"city": "null"}')"#,
    );
    let compared = |member: &str, comparison: &str| {
        format!(
            "SELECT users.name FROM users WHERE CASE JSON_EXTRACT(users.profile, '$.\"{member}\"') WHEN 'null' THEN NULL ELSE JSON_UNQUOTE(JSON_EXTRACT(users.profile, '$.\"{member}\"')) END {comparison} ORDER BY users.id"
        )
    };
    for (member, comparison, found) in [
        ("city", "= 'Paris'", vec!["Alice"]),
        ("city", "= 'paris'", vec![]),
        ("city", "= 'Paris '", vec!["Alice"]),
        ("city", "!= 'Paris'", vec!["Bob", "Eve"]),
        ("city", "> 'C'", vec!["Alice", "Eve"]),
        ("city", "= 'null'", vec!["Eve"]),
        ("n", "= 'null'", vec![]),
        ("age", "= 30", vec!["Alice", "Bob"]),
        ("s", "= 30", vec!["Alice"]),
        ("ok", "= 'true'", vec!["Alice"]),
    ] {
        assert_eq!(
            first_column(&mut adapter, &compared(member, comparison)),
            found,
            "{member} {comparison}"
        );
    }
}
