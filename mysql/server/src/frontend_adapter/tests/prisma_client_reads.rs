//! What Prisma Client 6.19 reads through `mysql_async`, every statement
//! prepared and its values bound the way the client binds them.
//!
//! Every expected answer was measured on MySQL 8.4.11 over the same rows.

use super::*;

type Adapter = AuthorizedDatabaseCommandAdapter<RecordingAuthorizer>;

/// The schema `prisma migrate deploy` makes from the app's first migration,
/// with the rows the measurement read.
const PRISMA_SCHEMA: &[&str] = &[
    "CREATE TABLE `users` (`id` BIGINT NOT NULL AUTO_INCREMENT, `email` VARCHAR(191) NOT NULL, `name` VARCHAR(100) NOT NULL, `balance` DECIMAL(10, 2) NOT NULL DEFAULT 0, `is_active` BOOLEAN NOT NULL DEFAULT true, `profile` JSON NULL, `created_at` DATETIME(3) NOT NULL DEFAULT CURRENT_TIMESTAMP(3), `updated_at` DATETIME(3) NOT NULL, UNIQUE INDEX `users_email_key`(`email`), PRIMARY KEY (`id`)) DEFAULT CHARACTER SET utf8mb4 COLLATE utf8mb4_unicode_ci",
    "CREATE TABLE `posts` (`id` BIGINT NOT NULL AUTO_INCREMENT, `user_id` BIGINT NOT NULL, `title` VARCHAR(200) NOT NULL, `body` TEXT NULL, `published_at` DATETIME(3) NULL, `views` INTEGER NOT NULL DEFAULT 0, INDEX `posts_user_id_idx`(`user_id`), PRIMARY KEY (`id`)) DEFAULT CHARACTER SET utf8mb4 COLLATE utf8mb4_unicode_ci",
    "CREATE TABLE `tags` (`id` BIGINT NOT NULL AUTO_INCREMENT, `name` VARCHAR(100) NOT NULL, UNIQUE INDEX `tags_name_key`(`name`), PRIMARY KEY (`id`)) DEFAULT CHARACTER SET utf8mb4 COLLATE utf8mb4_unicode_ci",
    "CREATE TABLE `post_tag` (`post_id` BIGINT NOT NULL, `tag_id` BIGINT NOT NULL, INDEX `post_tag_tag_id_idx`(`tag_id`), PRIMARY KEY (`post_id`, `tag_id`)) DEFAULT CHARACTER SET utf8mb4 COLLATE utf8mb4_unicode_ci",
    "INSERT INTO tags (id, name) VALUES (1, 'news'), (2, 'rust'), (3, 'sql')",
    "INSERT INTO users (id, email, name, balance, is_active, profile, created_at, updated_at) VALUES (1, 'alice@example.com', 'Alice', 100.50, 1, '{\"city\": \"Tokyo\", \"tags\": [\"a\", \"b\"]}', '2026-09-28 04:02:18.123', '2026-09-28 04:02:18.123'), (2, 'bob@example.com', 'Bob', 20.25, 0, '{\"city\": \"Osaka\", \"tags\": [\"c\"]}', '2026-09-28 04:02:18.123', '2026-09-28 04:02:18.123'), (3, 'carol@example.com', 'Carol', 5.00, 1, NULL, '2026-09-28 04:02:18.123', '2026-09-28 04:02:18.123')",
    "INSERT INTO posts (id, user_id, title, body, published_at, views) VALUES (1, 1, 'Hello', 'First post', '2024-01-02 03:04:05', 10), (2, 1, 'Draft', 'Not yet', NULL, 0), (3, 2, 'Bob writes', NULL, NULL, 3)",
    "INSERT INTO post_tag VALUES (1, 1), (1, 2), (3, 3)",
];

fn adapter() -> (tempfile::TempDir, Adapter) {
    let authorizer = Arc::new(RecordingAuthorizer::default());
    let (directory, catalog, factory) = catalog_factory(authorizer);
    catalog.create("prisma").unwrap();
    let mut adapter = factory
        .build(AuthenticatedPrincipal::from_account_id_for_testing(
            AccountId::from_bytes([183; 32]),
        ))
        .unwrap();
    adapter.authorize_connection().unwrap();
    adapter.execute_init_db("prisma").unwrap();
    for sql in PRISMA_SCHEMA {
        adapter
            .execute_query(sql)
            .unwrap_or_else(|error| panic!("{sql}: {error:?}"));
    }
    (directory, adapter)
}

/// One value as `mysql_async` binds it: a JavaScript number Prisma knows is
/// whole as a `LONGLONG`, any other as a `DOUBLE`, and a string as a word.
#[derive(Clone, Copy, Debug)]
enum Bound<'a> {
    Word(&'a str),
    Whole(i64),
    Real(f64),
    Null,
}

/// Prepares `sql`, runs it with `values` bound, and answers each row's
/// values as the binary protocol carries them.
fn prepared_rows(
    adapter: &mut Adapter,
    sql: &str,
    values: &[Bound<'_>],
) -> Result<Vec<Vec<BinaryResultValue>>, FrontendErrorKind> {
    let statement = adapter.execute_stmt_prepare(sql)?;
    let result = adapter.execute_stmt_execute(statement.statement_id, &payload(values));
    adapter.execute_stmt_close(statement.statement_id);
    match result? {
        PreparedStatementExecutionResult::ResultSet(result) => Ok(result.rows),
        other => panic!("{sql} must return rows, answered {other:?}"),
    }
}

/// The null bitmap, the new-parameters flag, the types and the values.
fn payload(values: &[Bound<'_>]) -> Vec<u8> {
    if values.is_empty() {
        return Vec::new();
    }
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

/// The first value of each row, which every statement here reads an id or a
/// total into.
fn first_values(rows: &[Vec<BinaryResultValue>]) -> Vec<BinaryResultValue> {
    rows.iter().map(|row| row[0].clone()).collect()
}

/// `groupBy({ by: ['userId'], _sum: { views: true }, having: { views: {
/// _sum: { gt: 5 } } } })`. Measured on MySQL 8.4.11 with the value bound as
/// a whole number and as a word: each compares with the total as a number,
/// and NULL finds nothing.
#[test]
fn a_total_compared_in_having_with_a_bound_value_finds_the_groups_mysql_finds() {
    let (_directory, mut adapter) = adapter();
    const SQL: &str = "SELECT SUM(`prisma`.`posts`.`views`) AS `_sum$views`, `prisma`.`posts`.`user_id` FROM `prisma`.`posts` WHERE 1=1 GROUP BY `prisma`.`posts`.`user_id` HAVING SUM(`prisma`.`posts`.`views`) > ?";
    let user_ids = |adapter: &mut Adapter, value| {
        prepared_rows(adapter, SQL, &[value]).map(|rows| {
            let mut ids = rows.iter().map(|row| row[1].clone()).collect::<Vec<_>>();
            ids.sort_by_key(|id| format!("{id:?}"));
            ids
        })
    };

    assert_eq!(
        user_ids(&mut adapter, Bound::Whole(5)),
        Ok(vec![BinaryResultValue::Integer(1)])
    );
    assert_eq!(
        user_ids(&mut adapter, Bound::Word("5")),
        Ok(vec![BinaryResultValue::Integer(1)])
    );
    // MySQL finds both groups for 2.9. A fraction bound against a whole-number
    // column is refused here, as it is against the column itself.
    assert_eq!(
        user_ids(&mut adapter, Bound::Real(2.9)),
        Err(FrontendErrorKind::Unsupported)
    );
    // MySQL reads `'abc'` as 0 here and warns nothing, and `'2.9'` as 2.9;
    // a word naming no whole number is refused, as against the column.
    for word in ["abc", "2.9"] {
        assert_eq!(
            user_ids(&mut adapter, Bound::Word(word)),
            Err(FrontendErrorKind::Unsupported),
            "{word}"
        );
    }
    assert_eq!(user_ids(&mut adapter, Bound::Null), Ok(vec![]));

    let rows = prepared_rows(&mut adapter, SQL, &[Bound::Whole(5)]).unwrap();
    assert_eq!(
        first_values(&rows),
        [BinaryResultValue::Text("10".to_owned())]
    );
}

/// Prisma's JSON filters over `profile`, the path and the value each bound as
/// a word. Measured on MySQL 8.4.11 over the rows above and a fourth whose
/// `city` is an array holding `"Tokyo"` and whose `tags` is a word.
#[test]
fn json_filters_with_bound_paths_find_the_rows_mysql_finds() {
    let (_directory, mut adapter) = adapter();
    adapter
        .execute_query(
            "INSERT INTO users (id, email, name, profile, updated_at) VALUES (4, 'dan@example.com', 'Dan', '{\"city\": [\"Tokyo\"], \"tags\": \"a\"}', '2026-09-28 04:02:18.123')",
        )
        .unwrap();
    const EQUALS: &str = "SELECT `prisma`.`users`.`id`, `prisma`.`users`.`email`, `prisma`.`users`.`name`, `prisma`.`users`.`balance`, `prisma`.`users`.`is_active`, `prisma`.`users`.`profile`, `prisma`.`users`.`created_at`, `prisma`.`users`.`updated_at` FROM `prisma`.`users` WHERE (JSON_CONTAINS(JSON_EXTRACT(`prisma`.`users`.`profile`, ?), ?) AND JSON_CONTAINS(?, JSON_EXTRACT(`prisma`.`users`.`profile`, ?)))";
    const ARRAY_CONTAINS: &str = "SELECT `prisma`.`users`.`id`, `prisma`.`users`.`email`, `prisma`.`users`.`name`, `prisma`.`users`.`balance`, `prisma`.`users`.`is_active`, `prisma`.`users`.`profile`, `prisma`.`users`.`created_at`, `prisma`.`users`.`updated_at` FROM `prisma`.`users` WHERE (JSON_CONTAINS(JSON_EXTRACT(`prisma`.`users`.`profile`, ?), ?) AND (JSON_TYPE(JSON_EXTRACT(`prisma`.`users`.`profile`, ?)) = ?))";
    let equals = |adapter: &mut Adapter, path, value| {
        ids(
            adapter,
            EQUALS,
            &[
                Bound::Word(path),
                Bound::Word(value),
                Bound::Word(value),
                Bound::Word(path),
            ],
        )
    };
    let array_contains = |adapter: &mut Adapter, value| {
        ids(
            adapter,
            ARRAY_CONTAINS,
            &[
                Bound::Word("$.tags"),
                Bound::Word(value),
                Bound::Word("$.tags"),
                Bound::Word("ARRAY"),
            ],
        )
    };

    assert_eq!(equals(&mut adapter, "$.city", "\"Tokyo\""), [1]);
    assert_eq!(equals(&mut adapter, "$.city", "\"Osaka\""), [2]);
    assert_eq!(equals(&mut adapter, "$.city", "\"tokyo\""), NO_ROWS);
    assert_eq!(equals(&mut adapter, "$.missing", "\"Tokyo\""), NO_ROWS);
    assert_eq!(equals(&mut adapter, "$.city", "[\"Tokyo\"]"), [4]);

    assert_eq!(array_contains(&mut adapter, "[\"a\"]"), [1]);
    assert_eq!(array_contains(&mut adapter, "\"a\""), [1]);
    assert_eq!(array_contains(&mut adapter, "[\"a\", \"b\"]"), [1]);
    assert_eq!(array_contains(&mut adapter, "[\"z\"]"), NO_ROWS);
}

/// Prisma's `string_starts_with` and friends on a JSON member match the text
/// the member unquotes to under `utf8mb4_bin`, telling case apart. Measured on
/// MySQL 8.4.11 over the same rows.
#[test]
fn a_like_over_a_json_member_tells_case_apart_the_way_mysql_does() {
    let (_directory, mut adapter) = adapter();
    const STARTS_WITH: &str = "SELECT `prisma`.`users`.`id`, `prisma`.`users`.`email`, `prisma`.`users`.`name`, `prisma`.`users`.`balance`, `prisma`.`users`.`is_active`, `prisma`.`users`.`profile`, `prisma`.`users`.`created_at`, `prisma`.`users`.`updated_at` FROM `prisma`.`users` WHERE (JSON_UNQUOTE(JSON_EXTRACT(`prisma`.`users`.`profile`, ?)) LIKE ? AND (JSON_TYPE(JSON_EXTRACT(`prisma`.`users`.`profile`, ?)) = ?))";
    let matching = |adapter: &mut Adapter, pattern| {
        ids(
            adapter,
            STARTS_WITH,
            &[
                Bound::Word("$.city"),
                Bound::Word(pattern),
                Bound::Word("$.city"),
                Bound::Word("STRING"),
            ],
        )
    };

    assert_eq!(matching(&mut adapter, "Osa%"), [2]);
    assert_eq!(matching(&mut adapter, "osa%"), NO_ROWS);
    assert_eq!(matching(&mut adapter, "%o"), [1]);
    assert_eq!(matching(&mut adapter, "T_kyo"), [1]);
    assert_eq!(matching(&mut adapter, "T\\_kyo"), NO_ROWS);

    // A pattern binds as a word; MySQL would read a number as its digits.
    assert_eq!(
        prepared_rows(
            &mut adapter,
            STARTS_WITH,
            &[
                Bound::Word("$.city"),
                Bound::Whole(5),
                Bound::Word("$.city"),
                Bound::Word("STRING"),
            ],
        ),
        Err(FrontendErrorKind::Unsupported)
    );
    assert_eq!(
        ids(
            &mut adapter,
            "SELECT id FROM users WHERE JSON_UNQUOTE(JSON_EXTRACT(profile, '$.city')) NOT LIKE 'T%'",
            &[],
        ),
        [2]
    );
}

/// `findMany({ include: { _count: { select: { posts: true } } } })` counts
/// each user's posts in a grouped derived table and falls the users with none
/// back onto 0. Measured on MySQL 8.4.11: 2, 1 and 0, the count a `LONGLONG`
/// of 21 with the NOT NULL and binary flags, naming no table.
#[test]
fn a_relation_count_falls_back_onto_zero_the_way_mysql_counts_it() {
    let (_directory, mut adapter) = adapter();
    const COUNTS: &str = "SELECT `prisma`.`users`.`id`, `prisma`.`users`.`email`, `prisma`.`users`.`name`, `prisma`.`users`.`balance`, `prisma`.`users`.`is_active`, `prisma`.`users`.`profile`, `prisma`.`users`.`created_at`, `prisma`.`users`.`updated_at`, COALESCE(`aggr_selection_0_Post`.`_aggr_count_posts`, 0) AS `_aggr_count_posts` FROM `prisma`.`users` LEFT JOIN (SELECT `prisma`.`posts`.`user_id`, COUNT(*) AS `_aggr_count_posts` FROM `prisma`.`posts` WHERE 1=1 GROUP BY `prisma`.`posts`.`user_id`) AS `aggr_selection_0_Post` ON (`prisma`.`users`.`id` = `aggr_selection_0_Post`.`user_id`) WHERE 1=1 ORDER BY `prisma`.`users`.`id` ASC";

    let prepared = adapter.execute_stmt_prepare(COUNTS).unwrap();
    let count = prepared.columns.last().unwrap();
    assert_eq!(
        (
            count.column_type,
            count.column_length,
            count.decimals,
            count.flags,
            count.table.as_str(),
            count.original_table.as_str(),
            count.original_name.as_str(),
        ),
        (
            MYSQL_TYPE_LONGLONG,
            21,
            0,
            // The numeric flag rides on every number this server answers.
            MYSQL_NOT_NULL_FLAG | MYSQL_BINARY_FLAG | MYSQL_NUM_FLAG,
            "",
            "",
            "",
        )
    );
    let Ok(CommandExecutionResult::ResultSet(text)) = adapter.execute_query(COUNTS) else {
        panic!("the counts must read back as text");
    };
    assert_eq!(text.columns, prepared.columns);
    let counts = text
        .rows
        .iter()
        .map(|row| String::from_utf8(row[8].clone().unwrap()).unwrap())
        .collect::<Vec<_>>();
    assert_eq!(counts, ["2", "1", "0"]);

    let rows = prepared_rows(&mut adapter, COUNTS, &[]).unwrap();
    let counts = rows.iter().map(|row| row[8].clone()).collect::<Vec<_>>();
    assert_eq!(
        counts,
        [
            BinaryResultValue::Integer(2),
            BinaryResultValue::Integer(1),
            BinaryResultValue::Integer(0)
        ]
    );

    // Measured the same for IFNULL and for any whole number to fall back on.
    const FALLS_BACK: &str = "SELECT users.id, IFNULL(a.c, -1) AS c FROM users LEFT JOIN (SELECT user_id, COUNT(*) AS c FROM posts GROUP BY user_id) AS a ON users.id = a.user_id ORDER BY users.id";
    let Ok(CommandExecutionResult::ResultSet(text)) = adapter.execute_query(FALLS_BACK) else {
        panic!("{FALLS_BACK} must read back");
    };
    assert_eq!(
        (text.columns[1].column_type, text.columns[1].column_length),
        (MYSQL_TYPE_LONGLONG, 21)
    );
    let counts = text
        .rows
        .iter()
        .map(|row| String::from_utf8(row[1].clone().unwrap()).unwrap())
        .collect::<Vec<_>>();
    assert_eq!(counts, ["2", "1", "-1"]);

    // A total a derived table worked out, or a table's own column, falls
    // back onto a shape of its own that has not been measured.
    for unmeasured in [
        "SELECT users.id, COALESCE(v.total, 0) AS total FROM users LEFT JOIN (SELECT user_id, SUM(views) AS total FROM posts GROUP BY user_id) AS v ON users.id = v.user_id",
        "SELECT users.id, COALESCE(p.views, 0) AS views FROM users LEFT JOIN posts AS p ON users.id = p.user_id",
    ] {
        assert!(adapter.execute_query(unmeasured).is_err(), "{unmeasured}");
    }
}

/// `findMany({ cursor: { id }, skip: 1, take })` reads from the row the
/// cursor names through a subquery picking that row by its key. Measured on
/// MySQL 8.4.11: cursor 2 skipping one answers post 3, a cursor naming no row
/// answers nothing, a cursor bound as a word finds its row, and a subquery
/// that is not picked by a key answers 1242 once it finds two rows — refused
/// here, where the engine would take the first.
#[test]
fn a_cursor_picked_by_its_key_pages_from_the_row_mysql_pages_from() {
    let (_directory, mut adapter) = adapter();
    const CURSOR: &str = "SELECT `prisma`.`posts`.`id`, `prisma`.`posts`.`user_id`, `prisma`.`posts`.`title`, `prisma`.`posts`.`body`, `prisma`.`posts`.`published_at`, `prisma`.`posts`.`views` FROM `prisma`.`posts` WHERE `prisma`.`posts`.`id` >= (SELECT `prisma`.`posts`.`id` FROM `prisma`.`posts` WHERE (`prisma`.`posts`.`id`) = (?)) ORDER BY `prisma`.`posts`.`id` ASC LIMIT ? OFFSET ?";
    let page = |adapter: &mut Adapter, cursor, take, skip| {
        let rows = prepared_rows(
            adapter,
            CURSOR,
            &[cursor, Bound::Whole(take), Bound::Whole(skip)],
        )
        .unwrap_or_else(|error| panic!("cursor {cursor:?}: {error:?}"));
        first_values(&rows)
    };
    let id = BinaryResultValue::Integer;

    assert_eq!(page(&mut adapter, Bound::Whole(2), 1, 1), [id(3)]);
    assert_eq!(page(&mut adapter, Bound::Whole(2), 10, 0), [id(2), id(3)]);
    assert_eq!(page(&mut adapter, Bound::Whole(99), 10, 0), []);
    assert_eq!(page(&mut adapter, Bound::Word("2"), 10, 0), [id(2), id(3)]);

    for unpicked in [
        "SELECT id FROM posts WHERE id >= (SELECT id FROM posts WHERE user_id = 1)",
        "SELECT id FROM posts WHERE id >= (SELECT id FROM posts WHERE id = 1 OR id = 2)",
    ] {
        assert!(adapter.execute_query(unpicked).is_err(), "{unpicked}");
        assert_eq!(
            adapter.execute_stmt_prepare(unpicked).map(|_| ()),
            Err(FrontendErrorKind::Unsupported),
            "{unpicked}"
        );
    }
    // A unique key over a column that is never NULL picks one row too.
    assert_eq!(
        ids(
            &mut adapter,
            "SELECT id FROM posts WHERE id >= (SELECT id FROM tags WHERE name = 'rust')",
            &[],
        ),
        [2, 3]
    );
}

/// A subquery's column is held to the kind of the column it meets whichever
/// protocol the statement comes by; a prepared one was not held at all.
#[test]
fn a_prepared_subquery_is_held_to_the_kind_of_the_column_it_meets() {
    let (_directory, mut adapter) = adapter();
    for sql in [
        "SELECT id FROM posts WHERE title IN (SELECT id FROM tags)",
        "SELECT id FROM posts WHERE title >= (SELECT id FROM posts WHERE id = 2)",
    ] {
        assert!(adapter.execute_query(sql).is_err(), "{sql}");
        assert_eq!(
            adapter.execute_stmt_prepare(sql).map(|_| ()),
            Err(FrontendErrorKind::Unsupported),
            "{sql}"
        );
    }
}

const NO_ROWS: [i64; 0] = [];

/// The ids of the rows a prepared statement answers, in order.
fn ids(adapter: &mut Adapter, sql: &str, values: &[Bound<'_>]) -> Vec<i64> {
    let mut ids = prepared_rows(adapter, sql, values)
        .unwrap_or_else(|error| panic!("{sql} with {values:?}: {error:?}"))
        .iter()
        .map(|row| match row[0] {
            BinaryResultValue::Integer(id) => id,
            ref other => panic!("an id is a whole number, not {other:?}"),
        })
        .collect::<Vec<_>>();
    ids.sort_unstable();
    ids
}
