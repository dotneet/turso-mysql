//! Reports — a count, a total or a largest for each user or group — as the
//! ORMs write them over a join or a grouping, each replayed as the framework
//! harness saw it on the wire.
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

/// Laravel's own tables, whose every id is a `BIGINT UNSIGNED`.
const LARAVEL: &[&str] = &[
    UNSIGNED_IDS[0],
    UNSIGNED_IDS[1],
    "CREATE TABLE tags (id BIGINT UNSIGNED NOT NULL AUTO_INCREMENT PRIMARY KEY, name VARCHAR(50) NOT NULL UNIQUE) DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_unicode_ci",
    "CREATE TABLE post_tag (post_id BIGINT UNSIGNED NOT NULL, tag_id BIGINT UNSIGNED NOT NULL, PRIMARY KEY (post_id, tag_id)) DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_unicode_ci",
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

/// Django's `User.objects.annotate(post_count=Count("posts"),
/// views=Sum("posts__views")).filter(post_count__gte=2)` groups by the
/// primary key alone and projects the name beside it, which MySQL's
/// `ONLY_FULL_GROUP_BY` takes because the key decides the row. Its tag
/// counts group a `LEFT OUTER JOIN` by the tag's key the same way.
///
/// Measured, MySQL keeps the binary flag on the first statement's count and
/// total. It sorts the second through a temporary table for its `ORDER BY 2
/// DESC`, where the count loses the binary flag and the name its unique-key
/// flags; this reports the shapes a statement read without one answers.
#[test]
fn django_projects_a_column_the_primary_key_it_groups_by_decides() {
    let (_directory, mut adapter) = adapter_over(SIGNED_IDS);
    let (shapes, answered) = report(
        &mut adapter,
        "SELECT `users`.`name` AS `name`, COUNT(`posts`.`id`) AS `post_count`, SUM(`posts`.`views`) AS `views` FROM `users` LEFT OUTER JOIN `posts` ON (`users`.`id` = `posts`.`user_id`) GROUP BY `users`.`id` HAVING COUNT(`posts`.`id`) >= 2 ORDER BY `users`.`id` ASC",
    );
    assert_eq!(
        shapes,
        [
            shape("name", MYSQL_TYPE_VAR_STRING, 400, 0, WORDS),
            shape("post_count", MYSQL_TYPE_LONGLONG, 21, 0, COUNTED),
            shape("views", MYSQL_TYPE_NEWDECIMAL, 33, 0, AGGREGATED),
        ]
    );
    assert_eq!(
        answered,
        rows(&[
            &[Some("Alice"), Some("2"), Some("15")],
            &[Some("Bob"), Some("2"), Some("1")],
        ])
    );

    let (shapes, answered) = report(
        &mut adapter,
        "SELECT `tags`.`name` AS `name`, COUNT(`post_tag`.`post_id`) AS `n` FROM `tags` LEFT OUTER JOIN `post_tag` ON (`tags`.`id` = `post_tag`.`tag_id`) GROUP BY `tags`.`id` HAVING COUNT(`post_tag`.`post_id`) > 0 ORDER BY 2 DESC, 1 ASC",
    );
    assert_eq!(
        shapes,
        [
            shape(
                "name",
                MYSQL_TYPE_VAR_STRING,
                200,
                0,
                WORDS | MYSQL_UNIQUE_KEY_FLAG | MYSQL_PART_KEY_FLAG
            ),
            shape("n", MYSQL_TYPE_LONGLONG, 21, 0, COUNTED),
        ]
    );
    assert_eq!(
        answered,
        rows(&[
            &[Some("news"), Some("2")],
            &[Some("python"), Some("2")],
            &[Some("sql"), Some("1")],
        ])
    );
}

/// Django's `values("is_active").annotate(...).order_by("is_active")` over
/// one table groups by the place of the column in the projection, `GROUP BY
/// 1`, which MySQL answers as `GROUP BY users.is_active`. A place holding an
/// aggregate (1056), one past the last or `0` (1054) and one holding a call
/// are refused.
///
/// Measured, MySQL groups this through a temporary table, no index holding
/// `is_active`, where the count and the total lose the binary flag; this
/// reports the shapes it reports when an index answers the grouping.
#[test]
fn django_groups_by_the_place_of_a_projected_column() {
    let (_directory, mut adapter) = adapter_over(SIGNED_IDS);
    let (shapes, answered) = report(
        &mut adapter,
        "SELECT `users`.`is_active` AS `is_active`, COUNT(`users`.`id`) AS `n`, SUM(`users`.`balance`) AS `total` FROM `users` GROUP BY 1 ORDER BY 1 ASC",
    );
    assert_eq!(
        shapes,
        [
            shape("is_active", MYSQL_TYPE_TINY, 1, 0, WORDS),
            shape("n", MYSQL_TYPE_LONGLONG, 21, 0, COUNTED),
            shape("total", MYSQL_TYPE_NEWDECIMAL, 34, 2, AGGREGATED),
        ]
    );
    assert_eq!(
        answered,
        rows(&[
            &[Some("0"), Some("1"), Some("5.25")],
            &[Some("1"), Some("2"), Some("120.50")],
        ])
    );
    let (_, answered) = report(
        &mut adapter,
        "SELECT name, is_active, COUNT(*) FROM users GROUP BY 2, 1 ORDER BY 2, 1",
    );
    assert_eq!(
        answered,
        rows(&[
            &[Some("Carol"), Some("0"), Some("1")],
            &[Some("Alice"), Some("1"), Some("1")],
            &[Some("Bob"), Some("1"), Some("1")],
        ])
    );
    let (_, answered) = report(
        &mut adapter,
        "SELECT is_active, COUNT(*) AS c FROM users GROUP BY 1 HAVING is_active > 0",
    );
    assert_eq!(answered, rows(&[&[Some("1"), Some("2")]]));
    for sql in [
        "SELECT is_active, COUNT(*) FROM users GROUP BY 2",
        "SELECT is_active, COUNT(*) FROM users GROUP BY 0",
        "SELECT is_active, COUNT(*) FROM users GROUP BY 3",
        "SELECT UPPER(name), COUNT(*) FROM users GROUP BY 1",
        "SELECT is_active, name, COUNT(*) FROM users GROUP BY 1",
        "SELECT is_active, COUNT(*) FROM users GROUP BY 1 WITH ROLLUP",
    ] {
        assert!(is_refused(&mut adapter, sql), "{sql}");
    }
}

/// Which columns a grouping's keys decide, measured on MySQL 8.4.11 over the
/// same tables: a primary key and a unique key over `NOT NULL` columns decide
/// their table's row, a join's `ON` carries a decided column to the column it
/// matches, and a `LEFT JOIN` carries one only to the table it adds, when
/// every column of the tables before it that its `ON` names is decided. Each
/// refused statement is one MySQL answers 1055 for.
#[test]
fn a_column_stands_beside_the_keys_only_when_they_decide_it() {
    let (_directory, mut adapter) = adapter_over(UNSIGNED_IDS);
    for sql in [
        "CREATE TABLE loose (a INT, b INT UNIQUE, c INT)",
        "INSERT INTO loose VALUES (1, 1, 1), (1, 2, 2)",
    ] {
        adapter
            .execute_query(sql)
            .unwrap_or_else(|error| panic!("{sql}: {error:?}"));
    }
    let mut sorted = |sql: &str| {
        let (_, mut answered) = report(&mut adapter, sql);
        answered.sort();
        answered
    };
    assert_eq!(
        sorted("SELECT id, name FROM users GROUP BY id"),
        rows(&[
            &[Some("1"), Some("Alice")],
            &[Some("2"), Some("Bob")],
            &[Some("3"), Some("Carol")],
        ])
    );
    assert_eq!(
        sorted("SELECT email, name FROM users GROUP BY email"),
        rows(&[
            &[Some("alice@example.com"), Some("Alice")],
            &[Some("bob@example.com"), Some("Bob")],
            &[Some("carol@example.com"), Some("Carol")],
        ])
    );
    let every_post = rows(&[
        &[Some("Bob post"), Some("Bob")],
        &[Some("Draft"), Some("Bob")],
        &[Some("Hello"), Some("Alice")],
        &[Some("Second"), Some("Alice")],
    ]);
    assert_eq!(
        sorted(
            "SELECT p.title, u.name FROM users u JOIN posts p ON u.id = p.user_id GROUP BY p.id"
        ),
        every_post
    );
    assert_eq!(
        sorted(
            "SELECT p.title, u.name FROM posts p LEFT JOIN users u ON u.id = p.user_id GROUP BY p.id"
        ),
        every_post
    );
    assert_eq!(
        sorted("SELECT p.title FROM users u LEFT JOIN posts p ON u.id = p.user_id GROUP BY p.id"),
        rows(&[
            &[None],
            &[Some("Bob post")],
            &[Some("Draft")],
            &[Some("Hello")],
            &[Some("Second")],
        ])
    );
    assert_eq!(
        sorted(
            "SELECT u.name FROM posts p LEFT JOIN users u ON p.user_id = u.id AND u.is_active = 1 GROUP BY p.user_id"
        ),
        rows(&[&[Some("Alice")], &[Some("Bob")]])
    );
    assert_eq!(
        sorted(
            "SELECT pt.tag_id, p.title FROM post_tag pt JOIN posts p ON p.id = pt.post_id GROUP BY pt.post_id, pt.tag_id"
        ),
        rows(&[
            &[Some("1"), Some("Hello")],
            &[Some("1"), Some("Second")],
            &[Some("2"), Some("Second")],
            &[Some("3"), Some("Bob post")],
            &[Some("3"), Some("Hello")],
        ])
    );
    assert_eq!(
        sorted("SELECT UPPER(name) FROM users GROUP BY id"),
        rows(&[&[Some("ALICE")], &[Some("BOB")], &[Some("CAROL")]])
    );
    assert_eq!(
        sorted(
            "SELECT u.name, t.name FROM users u JOIN posts p ON p.user_id = u.id JOIN post_tag pt ON pt.post_id = p.id JOIN tags t ON t.id = pt.tag_id GROUP BY p.id, t.id"
        ),
        rows(&[
            &[Some("Alice"), Some("news")],
            &[Some("Alice"), Some("python")],
            &[Some("Alice"), Some("python")],
            &[Some("Alice"), Some("sql")],
            &[Some("Bob"), Some("news")],
        ])
    );

    for sql in [
        // A unique key over a column that may be NULL decides nothing.
        "SELECT b, c FROM loose GROUP BY b",
        "SELECT a, c FROM loose GROUP BY a",
        // The table a `LEFT JOIN` adds decides nothing before it.
        "SELECT p.title, u.name FROM users u LEFT JOIN posts p ON u.id = p.user_id GROUP BY p.id",
        "SELECT u.name FROM users u LEFT JOIN posts p ON u.id = p.user_id GROUP BY p.user_id",
        "SELECT p.title FROM posts p LEFT JOIN users u ON u.id = p.user_id GROUP BY u.id",
        // The `ON` names `p.id`, which the key does not decide.
        "SELECT u.name FROM posts p LEFT JOIN users u ON p.user_id = p.id AND u.id = p.user_id GROUP BY p.user_id",
        // Part of a primary key decides nothing, and a key inside an
        // expression is no key.
        "SELECT pt.tag_id FROM post_tag pt GROUP BY pt.post_id",
        "SELECT name FROM users GROUP BY id + 0",
    ] {
        assert!(is_refused(&mut adapter, sql), "{sql}");
    }
    // How MySQL writes such a view out and reads it back has not been
    // measured.
    assert!(adapter
        .execute_query("CREATE VIEW named_by_id AS SELECT id, name FROM users GROUP BY id")
        .is_err());
}

/// Laravel's `User::withCount('posts')` and `withSum('posts', 'views')`,
/// `withMax('posts', 'views')`: each relation is a subquery standing as a
/// result column, correlated to the user it counts for, beside `users.*`.
///
/// Measured on MySQL 8.4.11: the count is a nullable `LONGLONG` of 21, the
/// total and the largest the shapes they have over the table alone, and every
/// `users` column read beside them loses its `NOT_NULL` flag, keeping its key
/// flags, because the subquery names the table from inside it.
#[test]
fn laravel_counts_and_totals_each_users_posts_beside_the_user() {
    let (_directory, mut adapter) = adapter_over(UNSIGNED_IDS);
    let user_columns = [
        shape(
            "id",
            MYSQL_TYPE_LONGLONG,
            20,
            0,
            MYSQL_PRI_KEY_FLAG
                | MYSQL_UNSIGNED_FLAG
                | MYSQL_AUTO_INCREMENT_FLAG
                | MYSQL_PART_KEY_FLAG,
        ),
        shape(
            "email",
            MYSQL_TYPE_VAR_STRING,
            764,
            0,
            MYSQL_UNIQUE_KEY_FLAG | MYSQL_NO_DEFAULT_VALUE_FLAG | MYSQL_PART_KEY_FLAG,
        ),
        shape(
            "name",
            MYSQL_TYPE_VAR_STRING,
            400,
            0,
            MYSQL_NO_DEFAULT_VALUE_FLAG,
        ),
        shape("balance", MYSQL_TYPE_NEWDECIMAL, 12, 2, 0),
        shape("is_active", MYSQL_TYPE_TINY, 1, 0, 0),
    ];

    let (shapes, answered) = report(
        &mut adapter,
        "select `users`.*, (select count(*) from `posts` where `users`.`id` = `posts`.`user_id`) as `posts_count` from `users` order by `id` asc",
    );
    let mut expected = user_columns.to_vec();
    expected.push(shape("posts_count", MYSQL_TYPE_LONGLONG, 21, 0, AGGREGATED));
    assert_eq!(shapes, expected);
    assert_eq!(
        answered,
        rows(&[
            &[
                Some("1"),
                Some("alice@example.com"),
                Some("Alice"),
                Some("100.50"),
                Some("1"),
                Some("2")
            ],
            &[
                Some("2"),
                Some("bob@example.com"),
                Some("Bob"),
                Some("20.00"),
                Some("1"),
                Some("2")
            ],
            &[
                Some("3"),
                Some("carol@example.com"),
                Some("Carol"),
                Some("5.25"),
                Some("0"),
                Some("0")
            ],
        ])
    );

    let (shapes, answered) = report(
        &mut adapter,
        "select `users`.*, (select sum(`posts`.`views`) from `posts` where `users`.`id` = `posts`.`user_id`) as `posts_sum_views`, (select max(`posts`.`views`) from `posts` where `users`.`id` = `posts`.`user_id`) as `posts_max_views` from `users` order by `id` asc",
    );
    let mut expected = user_columns.to_vec();
    expected.push(shape(
        "posts_sum_views",
        MYSQL_TYPE_NEWDECIMAL,
        33,
        0,
        AGGREGATED,
    ));
    expected.push(shape("posts_max_views", MYSQL_TYPE_LONG, 11, 0, AGGREGATED));
    assert_eq!(shapes, expected);
    assert_eq!(
        answered
            .iter()
            .map(|row| row[5..].to_vec())
            .collect::<Vec<_>>(),
        rows(&[
            &[Some("15"), Some("10")],
            &[Some("1"), Some("1")],
            &[None, None],
        ])
    );
}

/// Only the tables a subquery in the projection names from inside it lose
/// their `NOT_NULL` flags, measured on MySQL 8.4.11: `u.id` keeps it beside a
/// subquery naming `p`, and a subquery naming nothing outside it, or an
/// `EXISTS`, changes nothing.
#[test]
fn a_subquery_in_the_projection_takes_not_null_off_the_tables_it_names() {
    let (_directory, mut adapter) = adapter_over(UNSIGNED_IDS);
    let id =
        MYSQL_PRI_KEY_FLAG | MYSQL_UNSIGNED_FLAG | MYSQL_AUTO_INCREMENT_FLAG | MYSQL_PART_KEY_FLAG;
    let (shapes, _) = report(
        &mut adapter,
        "SELECT u.id, p.id, (SELECT COUNT(*) FROM tags t WHERE t.id = p.id) AS c FROM users u JOIN posts p ON p.user_id = u.id",
    );
    assert_eq!(
        shapes[..2],
        [
            shape("id", MYSQL_TYPE_LONGLONG, 20, 0, id | MYSQL_NOT_NULL_FLAG),
            shape("id", MYSQL_TYPE_LONGLONG, 20, 0, id),
        ]
    );
    for sql in [
        "SELECT users.id, (SELECT COUNT(*) FROM posts) AS a FROM users",
        "SELECT users.id, EXISTS (SELECT 1 FROM posts WHERE posts.user_id = users.id) AS e FROM users",
    ] {
        let (shapes, _) = report(&mut adapter, sql);
        assert_eq!(
            shapes[0],
            shape("id", MYSQL_TYPE_LONGLONG, 20, 0, id | MYSQL_NOT_NULL_FLAG),
            "{sql}"
        );
    }
    // A bare name is the subquery's own column first; one its table does not
    // have names the statement's table, which is refused.
    let (shapes, _) = report(
        &mut adapter,
        "SELECT id, (SELECT COUNT(*) FROM posts WHERE user_id = users.id) AS c FROM users",
    );
    assert_eq!(shapes[0], shape("id", MYSQL_TYPE_LONGLONG, 20, 0, id));
    assert!(is_refused(
        &mut adapter,
        "SELECT id, (SELECT COUNT(*) FROM posts WHERE posts.user_id = email) AS c FROM users"
    ));
}

/// Laravel's `User::whereHas('posts.tags', fn ($q) => $q->where('name',
/// $tag))` asks through the pivot table, joined inside the innermost
/// `EXISTS`, with the tag's name bound. Measured on MySQL 8.4.11: the bare
/// `name` there is `tags.name`, the one of the joined tables that has it,
/// compared without regard to case; `whereDoesntHave` is the `NOT EXISTS`;
/// and a bare name two joined tables both have answers 1052.
#[test]
fn laravel_asks_through_a_pivot_table_joined_inside_an_exists() {
    let (_directory, mut adapter) = adapter_over(LARAVEL);
    let where_has = "select `name` from `users` where exists (select * from `posts` where `users`.`id` = `posts`.`user_id` and exists (select * from `tags` inner join `post_tag` on `tags`.`id` = `post_tag`.`tag_id` where `posts`.`id` = `post_tag`.`post_id` and `name` = ?))";
    for (tag, users) in [
        ("python", rows(&[&[Some("Alice")]])),
        ("PYTHON", rows(&[&[Some("Alice")]])),
        ("news", rows(&[&[Some("Alice")], &[Some("Bob")]])),
        ("rust", Rows::new()),
    ] {
        let mut answered = bound_rows(&mut adapter, where_has, &[Bound::Word(tag)]);
        answered.sort();
        assert_eq!(answered, users, "{tag}");
    }
    let prepared = adapter.execute_stmt_prepare(where_has).unwrap();
    assert_eq!(
        prepared
            .columns
            .iter()
            .map(|column| (
                column.name.as_str(),
                column.column_type,
                column.column_length,
                column.flags
            ))
            .collect::<Vec<_>>(),
        [("name", MYSQL_TYPE_VAR_STRING, 400, WORDS)]
    );
    adapter.execute_stmt_close(prepared.statement_id);
    assert_eq!(
        bound_rows(
            &mut adapter,
            &where_has
                .replace("where exists", "where not exists")
                .replace("= ?))", "= ?)) order by `name`"),
            &[Bound::Word("python")]
        ),
        rows(&[&[Some("Bob")], &[Some("Carol")]])
    );
    assert!(is_refused(
        &mut adapter,
        "select `name` from `users` where exists (select * from `tags` inner join `post_tag` on `tags`.`id` = `post_tag`.`tag_id` join posts on posts.id = post_tag.post_id where id = 1)"
    ));
}
