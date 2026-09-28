//! Reports over a join — a count, a total or a largest for each user — as the
//! ORMs write them, each replayed as the framework harness saw it on the wire.
//!
//! Every expectation here was measured on MySQL 8.4.11 over the same tables
//! and rows, in the text and the binary protocol.

use super::*;

type Adapter = AuthorizedDatabaseCommandAdapter<RecordingAuthorizer>;

/// The tables Django, SQLAlchemy and TypeORM make: signed ids.
const SIGNED_IDS: &[&str] = &[
    "CREATE TABLE users (id BIGINT AUTO_INCREMENT NOT NULL PRIMARY KEY, email VARCHAR(191) NOT NULL UNIQUE, name VARCHAR(100) NOT NULL, balance DECIMAL(10,2) NOT NULL, is_active BOOL NOT NULL)",
    "CREATE TABLE posts (id BIGINT AUTO_INCREMENT NOT NULL PRIMARY KEY, title VARCHAR(200) NOT NULL, views INTEGER NOT NULL, user_id BIGINT NOT NULL, KEY posts_user_id (user_id))",
    "CREATE TABLE tags (id BIGINT AUTO_INCREMENT NOT NULL PRIMARY KEY, name VARCHAR(50) NOT NULL UNIQUE)",
    "CREATE TABLE post_tag (post_id BIGINT NOT NULL, tag_id BIGINT NOT NULL, PRIMARY KEY (post_id, tag_id), KEY post_tag_tag_id (tag_id))",
];

/// The tables Laravel and the mysql client's script make: unsigned ids.
const UNSIGNED_IDS: &[&str] = &[
    "CREATE TABLE users (id BIGINT UNSIGNED NOT NULL AUTO_INCREMENT, email VARCHAR(191) NOT NULL, name VARCHAR(100) NOT NULL, balance DECIMAL(10,2) NOT NULL DEFAULT '0.00', is_active TINYINT(1) NOT NULL DEFAULT 1, PRIMARY KEY (id), UNIQUE KEY users_email_unique (email)) DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_unicode_ci",
    "CREATE TABLE posts (id BIGINT UNSIGNED NOT NULL AUTO_INCREMENT, user_id BIGINT UNSIGNED NOT NULL, title VARCHAR(200) NOT NULL, views INT NOT NULL DEFAULT 0, PRIMARY KEY (id), KEY posts_user_id_index (user_id)) DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_unicode_ci",
    "CREATE TABLE tags (id INT UNSIGNED NOT NULL AUTO_INCREMENT PRIMARY KEY, name VARCHAR(50) NOT NULL UNIQUE) DEFAULT CHARSET=utf8mb4",
    "CREATE TABLE post_tag (post_id BIGINT UNSIGNED NOT NULL, tag_id INT UNSIGNED NOT NULL, PRIMARY KEY (post_id, tag_id)) DEFAULT CHARSET=utf8mb4",
];

const ROWS: &[&str] = &[
    "INSERT INTO users (email, name, balance, is_active) VALUES ('alice@example.com', 'Alice', 100.50, 1), ('bob@example.com', 'Bob', 20.00, 1), ('carol@example.com', 'Carol', 5.25, 0)",
    "INSERT INTO posts (user_id, title, views) VALUES (1, 'Hello', 10), (1, 'Second', 5), (2, 'Bob post', 1), (2, 'Draft', 0)",
    "INSERT INTO tags (name) VALUES ('python'), ('sql'), ('news')",
    "INSERT INTO post_tag (post_id, tag_id) VALUES (1, 1), (1, 3), (2, 1), (2, 2), (3, 3)",
];

fn adapter_over(tables: &[&str]) -> (tempfile::TempDir, Adapter) {
    let authorizer = Arc::new(RecordingAuthorizer::default());
    let (directory, _catalog, factory) = catalog_factory(authorizer);
    let mut adapter = factory
        .build(AuthenticatedPrincipal::from_account_id_for_testing(
            AccountId::from_bytes([163; 32]),
        ))
        .unwrap();
    adapter.authorize_connection().unwrap();
    adapter.execute_init_db("reports").unwrap();
    for sql in tables.iter().chain(ROWS) {
        adapter
            .execute_query(sql)
            .unwrap_or_else(|error| panic!("{sql}: {error:?}"));
    }
    (directory, adapter)
}

/// A result column's name, type, length, decimals and flags.
type Shape = (String, u8, u32, u8, u16);

/// Every row, each value as the text protocol writes it.
type Rows = Vec<Vec<Option<String>>>;

/// Runs a statement in both protocols, holds the two to the same columns and
/// the same number of rows, and answers the text protocol's.
fn report(adapter: &mut Adapter, sql: &str) -> (Vec<Shape>, Rows) {
    let text = match adapter.execute_query(sql) {
        Ok(CommandExecutionResult::ResultSet(text)) => text,
        other => panic!("{sql} must answer rows, answered {other:?}"),
    };
    let prepared = adapter
        .execute_stmt_prepare(sql)
        .unwrap_or_else(|error| panic!("{sql} must prepare: {error:?}"));
    assert_eq!(prepared.columns, text.columns, "{sql}");
    let binary = prepared_result_set(
        adapter
            .execute_stmt_execute(prepared.statement_id, &[])
            .unwrap(),
    );
    adapter.execute_stmt_close(prepared.statement_id);
    assert_eq!(binary.columns, text.columns, "{sql}");
    assert_eq!(binary.rows.len(), text.rows.len(), "{sql}");
    let shapes = text
        .columns
        .iter()
        .map(|column| {
            (
                column.name.clone(),
                column.column_type,
                column.column_length,
                column.decimals,
                column.flags,
            )
        })
        .collect();
    let rows = text
        .rows
        .iter()
        .map(|row| {
            row.iter()
                .map(|value| value.clone().map(|value| String::from_utf8(value).unwrap()))
                .collect()
        })
        .collect();
    (shapes, rows)
}

/// Whether a statement is refused in both protocols.
fn is_refused(adapter: &mut Adapter, sql: &str) -> bool {
    adapter.execute_query(sql).is_err() && adapter.execute_stmt_prepare(sql).is_err()
}

fn shape(name: &str, column_type: u8, length: u32, decimals: u8, flags: u16) -> Shape {
    (name.to_owned(), column_type, length, decimals, flags)
}

fn rows(written: &[&[Option<&str>]]) -> Rows {
    written
        .iter()
        .map(|row| row.iter().map(|value| value.map(str::to_owned)).collect())
        .collect()
}

/// A value bound the way PHP's PDO binds an integer or a string.
enum Bound<'a> {
    Whole(i64),
    Word(&'a str),
}

/// The null bitmap, the new-parameters flag, the types and the values.
fn payload(values: &[Bound<'_>]) -> Vec<u8> {
    let mut payload = vec![0; values.len().div_ceil(8)];
    payload.push(1);
    for value in values {
        payload.extend_from_slice(&match value {
            Bound::Whole(_) => [MYSQL_TYPE_LONGLONG, 0],
            Bound::Word(_) => [MYSQL_TYPE_STRING, 0],
        });
    }
    for value in values {
        match value {
            Bound::Whole(number) => payload.extend_from_slice(&number.to_le_bytes()),
            Bound::Word(word) => {
                payload.push(u8::try_from(word.len()).unwrap());
                payload.extend_from_slice(word.as_bytes());
            }
        }
    }
    payload
}

/// Runs a prepared statement with bound values and answers its rows, each
/// value written out as text.
fn bound_rows(adapter: &mut Adapter, sql: &str, values: &[Bound<'_>]) -> Rows {
    let prepared = adapter
        .execute_stmt_prepare(sql)
        .unwrap_or_else(|error| panic!("{sql} must prepare: {error:?}"));
    let result = prepared_result_set(
        adapter
            .execute_stmt_execute(prepared.statement_id, &payload(values))
            .unwrap_or_else(|error| panic!("{sql} must run: {error:?}")),
    );
    adapter.execute_stmt_close(prepared.statement_id);
    result
        .rows
        .iter()
        .map(|row| {
            row.iter()
                .map(|value| match value {
                    BinaryResultValue::Null => None,
                    BinaryResultValue::Integer(number) => Some(number.to_string()),
                    BinaryResultValue::UnsignedInteger(number) => Some(number.to_string()),
                    BinaryResultValue::Text(text) => Some(text.clone()),
                    other => panic!("{sql} answered {other:?}"),
                })
                .collect()
        })
        .collect()
}

const WORDS: u16 = MYSQL_NOT_NULL_FLAG | MYSQL_NO_DEFAULT_VALUE_FLAG;
const COUNTED: u16 = MYSQL_NOT_NULL_FLAG | MYSQL_BINARY_FLAG | MYSQL_NUM_FLAG;
const AGGREGATED: u16 = MYSQL_BINARY_FLAG | MYSQL_NUM_FLAG;

/// SQLAlchemy's `session.query(User.name, func.count(Post.id),
/// func.sum(Post.views)).join(User.posts).group_by(User.id, User.name)` and
/// TypeORM's `maximum("views", { user: { email } })`. A `MIN`, `MAX` or `SUM`
/// over a joined table's column answers the shape it answers over that table
/// alone: a total over an `INT` a `NEWDECIMAL` of 33, over a `BIGINT` one of
/// 42, a largest the column's own type, each nullable with the binary flag.
///
/// Measured, MySQL drops the binary flag from the count and the total of
/// SQLAlchemy's statement, which it groups through a temporary table because
/// its key names a column no index holds; this reports the shape it reports
/// when an index answers the grouping, as Django's grouping by the primary
/// key alone is answered below.
#[test]
fn an_aggregate_over_a_joined_column_answers_the_shape_it_has_over_its_table() {
    let (_directory, mut adapter) = adapter_over(SIGNED_IDS);

    let (shapes, answered) = report(
        &mut adapter,
        "SELECT users.name, count(posts.id) AS count_1, sum(posts.views) AS sum_1 FROM users INNER JOIN posts ON users.id = posts.user_id GROUP BY users.id, users.name HAVING count(posts.id) >= 2 ORDER BY users.id",
    );
    assert_eq!(
        shapes,
        [
            shape("name", MYSQL_TYPE_VAR_STRING, 400, 0, WORDS),
            shape("count_1", MYSQL_TYPE_LONGLONG, 21, 0, COUNTED),
            shape("sum_1", MYSQL_TYPE_NEWDECIMAL, 33, 0, AGGREGATED),
        ]
    );
    assert_eq!(
        answered,
        rows(&[
            &[Some("Alice"), Some("2"), Some("15")],
            &[Some("Bob"), Some("2"), Some("1")],
        ])
    );

    let typeorm = "SELECT MAX(`Post`.`views`) AS `MAX` FROM `posts` `Post` LEFT JOIN `users` `Post__Post_user` ON `Post__Post_user`.`id`=`Post`.`user_id` WHERE ((((`Post__Post_user`.`email` = 'alice@example.com'))))";
    let (shapes, answered) = report(&mut adapter, typeorm);
    assert_eq!(shapes, [shape("MAX", MYSQL_TYPE_LONG, 11, 0, AGGREGATED)]);
    assert_eq!(answered, rows(&[&[Some("10")]]));
    let (_, answered) = report(
        &mut adapter,
        &typeorm.replace("alice@example.com", "nobody@example.com"),
    );
    assert_eq!(answered, rows(&[&[None]]));

    let (shapes, answered) = report(
        &mut adapter,
        "SELECT MIN(p.views), MAX(p.id), SUM(p.id) FROM users u JOIN posts p ON p.user_id = u.id",
    );
    assert_eq!(
        shapes,
        [
            shape("MIN(p.views)", MYSQL_TYPE_LONG, 11, 0, AGGREGATED),
            shape("MAX(p.id)", MYSQL_TYPE_LONGLONG, 20, 0, AGGREGATED),
            shape("SUM(p.id)", MYSQL_TYPE_NEWDECIMAL, 42, 0, AGGREGATED),
        ]
    );
    assert_eq!(answered, rows(&[&[Some("0"), Some("4"), Some("10")]]));

    // A user with no posts answers NULL for each, on the outer side of a
    // `LEFT JOIN`. Measured, MySQL groups this through a temporary table too,
    // where the largest loses the binary flag and carries NO_DEFAULT_VALUE.
    let (shapes, answered) = report(
        &mut adapter,
        "SELECT u.name, SUM(p.views) AS s, MAX(p.views) AS m FROM users u LEFT JOIN posts p ON p.user_id = u.id GROUP BY u.id, u.name ORDER BY u.id",
    );
    assert_eq!(
        shapes,
        [
            shape("name", MYSQL_TYPE_VAR_STRING, 400, 0, WORDS),
            shape("s", MYSQL_TYPE_NEWDECIMAL, 33, 0, AGGREGATED),
            shape("m", MYSQL_TYPE_LONG, 11, 0, AGGREGATED),
        ]
    );
    assert_eq!(
        answered,
        rows(&[
            &[Some("Alice"), Some("15"), Some("10")],
            &[Some("Bob"), Some("1"), Some("1")],
            &[Some("Carol"), None, None],
        ])
    );
}

/// The engine keeps a `DECIMAL` and a `BIGINT UNSIGNED` in stored forms of
/// their own and compares words by their bytes, so an aggregate over a joined
/// column of those kinds, which MySQL answers as numbers and under the
/// column's collation, is refused.
#[test]
fn an_aggregate_over_a_joined_column_of_another_kind_is_refused() {
    let (_directory, mut adapter) = adapter_over(SIGNED_IDS);
    for sql in [
        "SELECT MAX(u.name) FROM users u JOIN posts p ON p.user_id = u.id",
        "SELECT SUM(u.balance) FROM users u JOIN posts p ON p.user_id = u.id",
        "SELECT MIN(u.balance) FROM users u JOIN posts p ON p.user_id = u.id",
    ] {
        assert!(is_refused(&mut adapter, sql), "{sql}");
    }
    let (_unsigned_directory, mut unsigned) = adapter_over(UNSIGNED_IDS);
    for sql in [
        "SELECT MAX(p.user_id) FROM users u JOIN posts p ON p.user_id = u.id",
        "SELECT SUM(p.id) FROM users u JOIN posts p ON p.user_id = u.id",
    ] {
        assert!(is_refused(&mut unsigned, sql), "{sql}");
    }
}

/// Laravel's `Post::join('users', ...)->groupBy('users.email')
/// ->having('post_count', '>=', 2)`, which binds the 2. The email is a
/// unique key, so it may stand beside the aggregates it groups; the `HAVING`
/// names the count by its alias, and a bound whole number or a word naming
/// one compares with it as that number.
#[test]
fn laravel_groups_a_join_by_a_unique_column_and_filters_by_an_alias() {
    let (_directory, mut adapter) = adapter_over(UNSIGNED_IDS);
    let laravel = "select `users`.`email`, count(*) as post_count, sum(posts.views) as total_views from `posts` inner join `users` on `users`.`id` = `posts`.`user_id` group by `users`.`email` having `post_count` >= ? order by `users`.`email` asc";
    let expected = rows(&[
        &[Some("alice@example.com"), Some("2"), Some("15")],
        &[Some("bob@example.com"), Some("2"), Some("1")],
    ]);
    assert_eq!(
        bound_rows(&mut adapter, laravel, &[Bound::Whole(2)]),
        expected
    );
    assert_eq!(
        bound_rows(&mut adapter, laravel, &[Bound::Word("2")]),
        expected
    );
    let (shapes, answered) = report(&mut adapter, &laravel.replace('?', "2"));
    assert_eq!(
        shapes,
        [
            shape(
                "email",
                MYSQL_TYPE_VAR_STRING,
                764,
                0,
                WORDS | MYSQL_UNIQUE_KEY_FLAG | MYSQL_PART_KEY_FLAG
            ),
            shape("post_count", MYSQL_TYPE_LONGLONG, 21, 0, COUNTED),
            shape("total_views", MYSQL_TYPE_NEWDECIMAL, 33, 0, AGGREGATED),
        ]
    );
    assert_eq!(answered, expected);
}
