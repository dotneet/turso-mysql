//! Derived tables and CTEs — a whole statement standing where a table does —
//! and what their columns report: a column of the body's table, under its own
//! name or an alias, and an answer the body worked out.
//!
//! Every expectation here was measured on MySQL 8.4.11 over the same rows.
//! MySQL reads a body that aggregates by writing it out into a table of its
//! own first, and any other body straight through, and the two report
//! different shapes.

use super::*;

fn adapter() -> (
    tempfile::TempDir,
    AuthorizedDatabaseCommandAdapter<RecordingAuthorizer>,
) {
    let authorizer = Arc::new(RecordingAuthorizer::default());
    let (directory, _catalog, factory) = catalog_factory(authorizer);
    let mut adapter = factory
        .build(AuthenticatedPrincipal::from_account_id_for_testing(
            AccountId::from_bytes([152; 32]),
        ))
        .unwrap();
    adapter.authorize_connection().unwrap();
    adapter.execute_init_db("REPORTS").unwrap();
    for sql in [
        "CREATE TABLE users (id BIGINT UNSIGNED AUTO_INCREMENT PRIMARY KEY, name VARCHAR(100), status VARCHAR(20), age INT NULL, balance DECIMAL(10,2), created_at DATETIME NULL)",
        "CREATE TABLE posts (id BIGINT UNSIGNED AUTO_INCREMENT PRIMARY KEY, user_id BIGINT UNSIGNED, title VARCHAR(200), views INT NOT NULL DEFAULT 0, published TINYINT(1), created_at DATETIME NULL)",
        "INSERT INTO users (name, status, age, balance, created_at) VALUES ('ann', 'active', 30, 10.50, '2026-01-05 10:00:00'), ('bob', 'inactive', NULL, 0.00, '2026-02-01 09:30:00'), ('cid', 'active', 41, 99.99, NULL), ('dee', NULL, 25, NULL, '2026-02-14 23:59:59')",
        "INSERT INTO posts (user_id, title, views, published, created_at) VALUES (1, 'a', 10, 1, '2026-01-05 10:00:00'), (1, 'b', 3, 0, '2026-01-05 18:00:00'), (2, 'c', 7, 1, '2026-02-01 09:30:00'), (3, 'd', 0, NULL, NULL), (NULL, 'e', 5, 1, '2025-12-31 23:59:59')",
    ] {
        adapter
            .execute_query(sql)
            .unwrap_or_else(|error| panic!("{sql}: {error:?}"));
    }
    (directory, adapter)
}

/// One result column as MySQL describes it: its name, the table it names, the
/// table and column it came from, whether it names the database, its type,
/// length, decimals, flags and character set.
#[derive(Debug, PartialEq, Eq)]
struct Column {
    name: &'static str,
    table: &'static str,
    original_table: &'static str,
    original_name: &'static str,
    names_the_database: bool,
    column_type: u8,
    length: u32,
    decimals: u8,
    flags: u16,
    character_set: u16,
}

/// Every row, each value as the text protocol writes it.
type Rows = Vec<Vec<Option<String>>>;

/// Runs a statement in both protocols, holds the two to the same columns, and
/// answers the columns and the text protocol's rows.
fn read(
    adapter: &mut AuthorizedDatabaseCommandAdapter<RecordingAuthorizer>,
    sql: &str,
) -> (Vec<ColumnDefinitionConfig>, Rows) {
    let Ok(CommandExecutionResult::ResultSet(text)) = adapter.execute_query(sql) else {
        panic!("{sql} must return a result set");
    };
    let prepared = adapter.execute_stmt_prepare(sql).unwrap();
    assert_eq!(prepared.columns, text.columns, "{sql}");
    let binary = prepared_result_set(
        adapter
            .execute_stmt_execute(prepared.statement_id, &[])
            .unwrap(),
    );
    assert_eq!(binary.columns, text.columns, "{sql}");
    assert_eq!(binary.rows.len(), text.rows.len(), "{sql}");
    adapter.execute_stmt_close(prepared.statement_id);
    let rows = text
        .rows
        .iter()
        .map(|row| {
            row.iter()
                .map(|value| value.clone().map(|value| String::from_utf8(value).unwrap()))
                .collect()
        })
        .collect();
    (text.columns, rows)
}

fn assert_columns(sql: &str, columns: &[ColumnDefinitionConfig], expected: &[Column]) {
    assert_eq!(columns.len(), expected.len(), "{sql}");
    for (column, expected) in columns.iter().zip(expected) {
        assert_eq!(
            Column {
                name: expected.name,
                table: expected.table,
                original_table: expected.original_table,
                original_name: expected.original_name,
                names_the_database: !column.schema.is_empty(),
                column_type: column.column_type,
                length: column.column_length,
                decimals: column.decimals,
                flags: column.flags,
                character_set: column.character_set,
            },
            *expected,
            "{sql}: {}",
            expected.name
        );
        assert_eq!(
            (
                column.name.as_str(),
                column.table.as_str(),
                column.original_table.as_str(),
                column.original_name.as_str()
            ),
            (
                expected.name,
                expected.table,
                expected.original_table,
                expected.original_name
            ),
            "{sql}"
        );
    }
}

fn rows(written: &[&[Option<&str>]]) -> Rows {
    written
        .iter()
        .map(|row| row.iter().map(|value| value.map(str::to_owned)).collect())
        .collect()
}

const WORDS: u16 = DEFAULT_UTF8MB4_COLLATION as u16;
const BINARY: u16 = MYSQL_BINARY_COLLATION;

/// `user_id` of `posts` read through a derived table named `t`.
fn user_id(table: &'static str, flags: u16) -> Column {
    Column {
        name: "user_id",
        table,
        original_table: "posts",
        original_name: "user_id",
        names_the_database: true,
        column_type: MYSQL_TYPE_LONGLONG,
        length: 20,
        decimals: 0,
        flags,
        character_set: BINARY,
    }
}

/// A count a derived table's body worked out: stored, it loses the binary
/// flag and keeps its NOT NULL, and it names no table of its own.
fn count(name: &'static str, table: &'static str) -> Column {
    Column {
        name,
        table,
        original_table: "",
        original_name: name,
        names_the_database: false,
        column_type: MYSQL_TYPE_LONGLONG,
        length: 21,
        decimals: 0,
        flags: MYSQL_NOT_NULL_FLAG | MYSQL_NUM_FLAG,
        character_set: BINARY,
    }
}

#[test]
fn a_derived_table_that_aggregates_answers_its_totals_as_stored_columns() {
    let (_directory, mut adapter) = adapter();

    let sql = "SELECT t.user_id, t.c FROM (SELECT user_id, COUNT(*) AS c FROM posts GROUP BY user_id) t WHERE t.c > 0 ORDER BY t.user_id";
    let (columns, answered) = read(&mut adapter, sql);
    assert_columns(
        sql,
        &columns,
        &[user_id("t", MYSQL_UNSIGNED_FLAG), count("c", "t")],
    );
    assert_eq!(
        answered,
        rows(&[
            &[None, Some("1")],
            &[Some("1"), Some("2")],
            &[Some("2"), Some("1")],
            &[Some("3"), Some("1")],
        ])
    );

    // A total, an average and a largest lose the binary flag once stored,
    // and words lose the 31 decimals a call's words carry.
    let sql = "SELECT t.user_id, t.c, t.s, t.a, t.m, t.n FROM (SELECT user_id, COUNT(*) AS c, SUM(views) AS s, AVG(views) AS a, MAX(views) AS m, MIN(title) AS n FROM posts GROUP BY user_id) t ORDER BY t.user_id";
    let (columns, answered) = read(&mut adapter, sql);
    let stored = |name, column_type, length, decimals, flags, character_set| Column {
        name,
        table: "t",
        original_table: "",
        original_name: name,
        names_the_database: false,
        column_type,
        length,
        decimals,
        flags,
        character_set,
    };
    assert_columns(
        sql,
        &columns,
        &[
            user_id("t", MYSQL_UNSIGNED_FLAG),
            count("c", "t"),
            stored("s", MYSQL_TYPE_NEWDECIMAL, 33, 0, MYSQL_NUM_FLAG, BINARY),
            stored("a", MYSQL_TYPE_NEWDECIMAL, 16, 4, MYSQL_NUM_FLAG, BINARY),
            stored("m", MYSQL_TYPE_LONG, 11, 0, MYSQL_NUM_FLAG, BINARY),
            stored("n", MYSQL_TYPE_VAR_STRING, 800, 0, 0, WORDS),
        ],
    );
    assert_eq!(
        answered,
        rows(&[
            &[
                None,
                Some("1"),
                Some("5"),
                Some("5.0000"),
                Some("5"),
                Some("e")
            ],
            &[
                Some("1"),
                Some("2"),
                Some("13"),
                Some("6.5000"),
                Some("10"),
                Some("a")
            ],
            &[
                Some("2"),
                Some("1"),
                Some("7"),
                Some("7.0000"),
                Some("7"),
                Some("c")
            ],
            &[
                Some("3"),
                Some("1"),
                Some("0"),
                Some("0.0000"),
                Some("0"),
                Some("d")
            ],
        ])
    );

    // An unaliased count goes by its source text, and `*` reads every column.
    let sql =
        "SELECT * FROM (SELECT user_id, COUNT(*) FROM posts GROUP BY user_id) t ORDER BY user_id";
    let (columns, _) = read(&mut adapter, sql);
    assert_columns(
        sql,
        &columns,
        &[user_id("t", MYSQL_UNSIGNED_FLAG), count("COUNT(*)", "t")],
    );
    let sql = "SELECT * FROM (SELECT user_id, COUNT(*) AS c FROM posts GROUP BY user_id) t ORDER BY c DESC, user_id";
    let (_, answered) = read(&mut adapter, sql);
    assert_eq!(
        answered,
        rows(&[
            &[Some("1"), Some("2")],
            &[None, Some("1")],
            &[Some("2"), Some("1")],
            &[Some("3"), Some("1")],
        ])
    );

    // A comparison on the count is held to a whole number.
    let sql = "SELECT t.user_id FROM (SELECT user_id, COUNT(*) AS c FROM posts GROUP BY user_id) t WHERE t.c > 1";
    let (_, answered) = read(&mut adapter, sql);
    assert_eq!(answered, rows(&[&[Some("1")]]));

    // A day, a total over a DECIMAL and a largest moment keep their own
    // shapes once stored, the day and the moment their binary flag.
    let sql = "SELECT t.d, t.c FROM (SELECT DATE(created_at) AS d, COUNT(*) AS c FROM posts GROUP BY DATE(created_at)) t ORDER BY t.d";
    let (columns, answered) = read(&mut adapter, sql);
    assert_eq!(
        (
            columns[0].column_type,
            columns[0].column_length,
            columns[0].flags,
            columns[0].original_name.as_str()
        ),
        (MYSQL_TYPE_DATE, 10, MYSQL_BINARY_FLAG, "d")
    );
    assert_eq!(
        answered[2],
        [Some("2026-01-05".to_owned()), Some("2".to_owned())]
    );
    let sql = "SELECT t.s, t.a FROM (SELECT SUM(balance) AS s, AVG(balance) AS a FROM users) t";
    let (columns, answered) = read(&mut adapter, sql);
    assert_columns(
        sql,
        &columns,
        &[
            stored("s", MYSQL_TYPE_NEWDECIMAL, 34, 2, MYSQL_NUM_FLAG, BINARY),
            stored("a", MYSQL_TYPE_NEWDECIMAL, 16, 6, MYSQL_NUM_FLAG, BINARY),
        ],
    );
    assert_eq!(answered, rows(&[&[Some("110.49"), Some("36.830000")]]));
    let sql = "SELECT t.c, t.m FROM (SELECT COUNT(*) AS c, MAX(created_at) AS m FROM posts) t";
    let (columns, answered) = read(&mut adapter, sql);
    assert_columns(
        sql,
        &columns,
        &[
            count("c", "t"),
            stored("m", MYSQL_TYPE_DATETIME, 19, 0, MYSQL_BINARY_FLAG, BINARY),
        ],
    );
    assert_eq!(answered, rows(&[&[Some("5"), Some("2026-02-01 09:30:00")]]));

    // A column of the table in a body that aggregates keeps its NOT NULL and
    // its sign, but none of its keys and no auto-increment.
    let sql =
        "SELECT t.id, t.c FROM (SELECT id, COUNT(*) AS c FROM posts GROUP BY id) t ORDER BY t.id";
    let (columns, _) = read(&mut adapter, sql);
    assert_eq!(columns[0].flags, MYSQL_NOT_NULL_FLAG | MYSQL_UNSIGNED_FLAG);
}

/// TypeORM pages a query by reading the ids it would answer out of the whole
/// query standing as a derived table, each column renamed on the way.
#[test]
fn typeorms_pagination_query_answers_what_mysql_answers() {
    let (_directory, mut adapter) = adapter();
    let sql = "SELECT DISTINCT `distinctAlias`.`User_id` AS `ids_User_id` FROM (SELECT `User`.`id` AS `User_id` FROM `users` `User`) `distinctAlias` ORDER BY `User_id` ASC LIMIT 10";
    let (columns, answered) = read(&mut adapter, sql);
    // Read straight through, the column keeps every flag; it names the table
    // the body read under the body's own name for it, and goes by the body's
    // name for the column.
    assert_columns(
        sql,
        &columns,
        &[Column {
            name: "ids_User_id",
            table: "distinctAlias",
            original_table: "User",
            original_name: "User_id",
            names_the_database: true,
            column_type: MYSQL_TYPE_LONGLONG,
            length: 20,
            decimals: 0,
            flags: MYSQL_NOT_NULL_FLAG
                | MYSQL_PRI_KEY_FLAG
                | MYSQL_UNSIGNED_FLAG
                | MYSQL_AUTO_INCREMENT_FLAG
                | MYSQL_PART_KEY_FLAG,
            character_set: BINARY,
        }],
    );
    assert_eq!(
        answered,
        rows(&[&[Some("1")], &[Some("2")], &[Some("3")], &[Some("4")]])
    );

    let sql =
        "SELECT x.n FROM (SELECT name AS n FROM users WHERE status = 'active') x WHERE x.n = 'ANN'";
    let (columns, answered) = read(&mut adapter, sql);
    assert_eq!(
        (
            columns[0].original_table.as_str(),
            columns[0].original_name.as_str()
        ),
        ("users", "n")
    );
    assert_eq!(answered, rows(&[&[Some("ann")]]));
    let (_, answered) = read(
        &mut adapter,
        "SELECT x.n FROM (SELECT name AS n FROM users) x ORDER BY x.n DESC",
    );
    assert_eq!(
        answered,
        rows(&[
            &[Some("dee")],
            &[Some("cid")],
            &[Some("bob")],
            &[Some("ann")]
        ])
    );
}

/// A body read straight through reports a day, a moment and a time of day in
/// the connection's character set, four bytes to each character they spell.
#[test]
fn a_moment_read_straight_through_a_derived_table_reports_it_as_words() {
    let (_directory, mut adapter) = adapter();
    for sql in [
        "SELECT a.created_at FROM (SELECT id, created_at FROM users) a ORDER BY a.id",
        "WITH a AS (SELECT id, created_at FROM users) SELECT created_at FROM a ORDER BY id",
        "WITH a AS (SELECT * FROM users) SELECT created_at FROM a ORDER BY id",
    ] {
        let (columns, answered) = read(&mut adapter, sql);
        assert_eq!(
            (
                columns[0].column_type,
                columns[0].column_length,
                columns[0].character_set,
                columns[0].flags
            ),
            (MYSQL_TYPE_DATETIME, 76, WORDS, MYSQL_BINARY_FLAG),
            "{sql}"
        );
        assert_eq!(
            answered[0],
            [Some("2026-01-05 10:00:00".to_owned())],
            "{sql}"
        );
    }
    // A body that aggregates is written out first, and keeps the moment's own
    // shape.
    let sql = "SELECT t.created_at, t.c FROM (SELECT created_at, COUNT(*) AS c FROM posts GROUP BY created_at) t ORDER BY 1";
    let (columns, _) = read(&mut adapter, sql);
    assert_eq!(
        (
            columns[0].column_length,
            columns[0].character_set,
            columns[0].flags
        ),
        (19, BINARY, MYSQL_BINARY_FLAG)
    );
}

#[test]
fn a_cte_projecting_its_whole_table_or_totals_answers_what_mysql_answers() {
    let (_directory, mut adapter) = adapter();
    let (columns, answered) = read(
        &mut adapter,
        "WITH active AS (SELECT * FROM users WHERE status = 'active') SELECT COUNT(*) FROM active",
    );
    assert_eq!(
        columns[0].flags,
        MYSQL_NOT_NULL_FLAG | MYSQL_BINARY_FLAG | MYSQL_NUM_FLAG
    );
    assert_eq!(answered, rows(&[&[Some("2")]]));

    let sql = "WITH active AS (SELECT * FROM users WHERE status = 'active') SELECT * FROM active ORDER BY id";
    let (columns, answered) = read(&mut adapter, sql);
    assert_eq!(
        columns
            .iter()
            .map(|column| (
                column.name.as_str(),
                column.table.as_str(),
                column.original_table.as_str()
            ))
            .collect::<Vec<_>>(),
        [
            ("id", "active", "users"),
            ("name", "active", "users"),
            ("status", "active", "users"),
            ("age", "active", "users"),
            ("balance", "active", "users"),
            ("created_at", "active", "users"),
        ]
    );
    assert_eq!(
        columns[0].flags,
        MYSQL_NOT_NULL_FLAG
            | MYSQL_PRI_KEY_FLAG
            | MYSQL_UNSIGNED_FLAG
            | MYSQL_AUTO_INCREMENT_FLAG
            | MYSQL_PART_KEY_FLAG
    );
    assert_eq!(
        (
            columns[4].column_type,
            columns[4].column_length,
            columns[4].decimals
        ),
        (MYSQL_TYPE_NEWDECIMAL, 12, 2)
    );
    assert_eq!(
        answered,
        rows(&[
            &[
                Some("1"),
                Some("ann"),
                Some("active"),
                Some("30"),
                Some("10.50"),
                Some("2026-01-05 10:00:00"),
            ],
            &[
                Some("3"),
                Some("cid"),
                Some("active"),
                Some("41"),
                Some("99.99"),
                None,
            ],
        ])
    );

    let sql = "WITH totals AS (SELECT user_id, COUNT(*) AS c, SUM(views) AS s FROM posts GROUP BY user_id) SELECT user_id, c, s FROM totals ORDER BY user_id";
    let (columns, answered) = read(&mut adapter, sql);
    assert_columns(
        sql,
        &columns,
        &[
            user_id("totals", MYSQL_UNSIGNED_FLAG),
            count("c", "totals"),
            Column {
                name: "s",
                table: "totals",
                original_table: "",
                original_name: "s",
                names_the_database: false,
                column_type: MYSQL_TYPE_NEWDECIMAL,
                length: 33,
                decimals: 0,
                flags: MYSQL_NUM_FLAG,
                character_set: BINARY,
            },
        ],
    );
    assert_eq!(
        answered,
        rows(&[
            &[None, Some("1"), Some("5")],
            &[Some("1"), Some("2"), Some("13")],
            &[Some("2"), Some("1"), Some("7")],
            &[Some("3"), Some("1"), Some("0")],
        ])
    );
    let (_, answered) = read(
        &mut adapter,
        "WITH totals AS (SELECT user_id, COUNT(*) AS c FROM posts GROUP BY user_id) SELECT user_id FROM totals WHERE c > 1",
    );
    assert_eq!(answered, rows(&[&[Some("1")]]));
}

#[test]
fn a_derived_table_refuses_what_has_not_been_measured() {
    let (_directory, mut adapter) = adapter();
    for sql in [
        // A call in a body read straight through.
        "SELECT x.d FROM (SELECT DATE(created_at) AS d FROM posts) x",
        "SELECT x.more FROM (SELECT views + 1 AS more FROM posts) x",
        // 1060: two columns going by one name.
        "SELECT t.c FROM (SELECT COUNT(*) AS c, MAX(views) AS c FROM posts) t",
        // A total or an average compared against a value.
        "SELECT t.s FROM (SELECT SUM(views) AS s FROM posts) t WHERE t.s > 1",
        // An aggregate over a column the derived table worked out.
        "WITH totals AS (SELECT user_id, COUNT(*) AS c FROM posts GROUP BY user_id) SELECT SUM(c) FROM totals",
        // A DISTINCT body, whose stored shapes have not been measured.
        "SELECT x.title FROM (SELECT DISTINCT title FROM posts) x",
    ] {
        assert!(
            matches!(
                adapter.execute_query(sql),
                Err(FrontendErrorKind::Syntax
                    | FrontendErrorKind::Unsupported
                    | FrontendErrorKind::UnknownColumn)
            ),
            "{sql}"
        );
    }
}

/// `WITH RECURSIVE n(x) AS (SELECT 1 UNION ALL SELECT x + 1 FROM n WHERE x <
/// 5)` is how a statement asks for a run of numbers.
#[test]
fn a_recursive_cte_counting_through_numbers_answers_what_mysql_answers() {
    let (_directory, mut adapter) = adapter();
    // Each column is a nullable LONGLONG as wide as its first value's digits
    // and one more, naming the sequence and itself.
    let counted = |name, table, length| Column {
        name,
        table,
        original_table: "",
        original_name: name,
        names_the_database: false,
        column_type: MYSQL_TYPE_LONGLONG,
        length,
        decimals: 0,
        flags: MYSQL_NUM_FLAG,
        character_set: BINARY,
    };
    let one_to_five = rows(&[
        &[Some("1")],
        &[Some("2")],
        &[Some("3")],
        &[Some("4")],
        &[Some("5")],
    ]);
    for sql in [
        "WITH RECURSIVE n(x) AS (SELECT 1 UNION ALL SELECT x + 1 FROM n WHERE x < 5) SELECT x FROM n",
        "WITH RECURSIVE n AS (SELECT 1 AS x UNION ALL SELECT x + 1 FROM n WHERE x < 5) SELECT x FROM n",
        "WITH RECURSIVE n(x) AS (SELECT 1 UNION ALL SELECT x + 1 FROM n WHERE x < 5) SELECT * FROM n",
        "WITH RECURSIVE n(x) AS (SELECT 1 UNION SELECT x + 1 FROM n WHERE x < 5) SELECT x FROM n",
        "WITH RECURSIVE n(x) AS (SELECT 1 UNION ALL SELECT x + 1 FROM n WHERE x <= 4) SELECT n.x FROM n",
    ] {
        let (columns, answered) = read(&mut adapter, sql);
        assert_columns(sql, &columns, &[counted("x", "n", 2)]);
        assert_eq!(answered, one_to_five, "{sql}");
    }

    let sql = "WITH RECURSIVE n(x) AS (SELECT 10 UNION ALL SELECT x + 1 FROM n WHERE x < 15) SELECT x FROM n";
    let (columns, answered) = read(&mut adapter, sql);
    assert_columns(sql, &columns, &[counted("x", "n", 3)]);
    assert_eq!(answered.len(), 6);
    let sql = "WITH RECURSIVE n(x, y) AS (SELECT -1, 2 UNION ALL SELECT x + 1, y * 2 FROM n WHERE x < 3) SELECT x, y FROM n";
    let (columns, answered) = read(&mut adapter, sql);
    assert_columns(sql, &columns, &[counted("x", "n", 2), counted("y", "n", 2)]);
    assert_eq!(
        answered,
        rows(&[
            &[Some("-1"), Some("2")],
            &[Some("0"), Some("4")],
            &[Some("1"), Some("8")],
            &[Some("2"), Some("16")],
            &[Some("3"), Some("32")],
        ])
    );

    let (_, answered) = read(
        &mut adapter,
        "WITH RECURSIVE n(x) AS (SELECT 1 UNION ALL SELECT x + 1 FROM n WHERE x < 5) SELECT x FROM n WHERE x > 2 ORDER BY x DESC LIMIT 2",
    );
    assert_eq!(answered, rows(&[&[Some("5")], &[Some("4")]]));
    // 999 rows past the first is as deep as MySQL goes by default.
    let (columns, answered) = read(
        &mut adapter,
        "WITH RECURSIVE n(x) AS (SELECT 1 UNION ALL SELECT x + 1 FROM n WHERE x < 1000) SELECT COUNT(*) FROM n",
    );
    assert_eq!(
        columns[0].flags,
        MYSQL_NOT_NULL_FLAG | MYSQL_BINARY_FLAG | MYSQL_NUM_FLAG
    );
    assert_eq!(answered, rows(&[&[Some("1000")]]));

    for sql in [
        // 3636: a thousand rows past the first.
        "WITH RECURSIVE n(x) AS (SELECT 1 UNION ALL SELECT x + 1 FROM n WHERE x < 1001) SELECT COUNT(*) FROM n",
        "WITH RECURSIVE n(x) AS (SELECT 0 UNION ALL SELECT x + 5 FROM n WHERE x < 5005) SELECT COUNT(*) FROM n",
        // 1690 once a value runs past a BIGINT, and a recursion with no
        // bound the statement names.
        "WITH RECURSIVE n(x, y) AS (SELECT 1, 1 UNION ALL SELECT x + 1, y * 1000000 FROM n WHERE x < 10) SELECT y FROM n",
        "WITH RECURSIVE n(x) AS (SELECT 1 UNION ALL SELECT x + 1 FROM n) SELECT x FROM n LIMIT 3",
        // A recursion whose depth depends on the rows it reads.
        "WITH RECURSIVE t AS (SELECT id, user_id FROM posts WHERE user_id IS NULL UNION ALL SELECT p.id, p.user_id FROM posts p JOIN t ON p.user_id = t.id) SELECT id FROM t",
        // An aggregate over the sequence's column, and a word against it.
        "WITH RECURSIVE n(x) AS (SELECT 1 UNION ALL SELECT x + 1 FROM n WHERE x < 5) SELECT SUM(x) FROM n",
        "WITH RECURSIVE n(x) AS (SELECT 1 UNION ALL SELECT x + 1 FROM n WHERE x < 5) SELECT x FROM n WHERE x = 'a'",
        // The sequence beside a table.
        "WITH RECURSIVE n(x) AS (SELECT 1 UNION ALL SELECT x + 1 FROM n WHERE x < 5) SELECT x, id FROM n JOIN posts ON posts.id = n.x",
    ] {
        assert!(
            matches!(
                adapter.execute_query(sql),
                Err(FrontendErrorKind::Syntax | FrontendErrorKind::Unsupported)
            ),
            "{sql}"
        );
    }
}

/// TypeORM's tables as its schema sync writes them, holding the rows its
/// harness run writes: two posts carrying tags and one carrying none. The
/// foreign keys TypeORM adds afterwards are left out, the index MySQL makes
/// for one changing a column's key flags.
fn typeorm_adapter() -> (
    tempfile::TempDir,
    AuthorizedDatabaseCommandAdapter<RecordingAuthorizer>,
) {
    let authorizer = Arc::new(RecordingAuthorizer::default());
    let (directory, _catalog, factory) = catalog_factory(authorizer);
    let mut adapter = factory
        .build(AuthenticatedPrincipal::from_account_id_for_testing(
            AccountId::from_bytes([153; 32]),
        ))
        .unwrap();
    adapter.authorize_connection().unwrap();
    adapter.execute_init_db("REPORTS").unwrap();
    for sql in [
        "CREATE TABLE `users` (`id` bigint NOT NULL AUTO_INCREMENT, `email` varchar(191) NOT NULL, `name` varchar(100) NOT NULL, `balance` decimal(10,2) NOT NULL DEFAULT '0.00', `is_active` tinyint NOT NULL DEFAULT 1, `profile` json NULL, `created_at` datetime(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6), `updated_at` datetime(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6) ON UPDATE CURRENT_TIMESTAMP(6), UNIQUE INDEX `IDX_97672ac88f789774dd47f7c8be` (`email`), PRIMARY KEY (`id`)) ENGINE=InnoDB",
        "CREATE TABLE `posts` (`id` bigint NOT NULL AUTO_INCREMENT, `user_id` bigint NOT NULL, `title` varchar(200) NOT NULL, `body` text NULL, `published_at` datetime NULL, `views` int NOT NULL DEFAULT '0', PRIMARY KEY (`id`)) ENGINE=InnoDB",
        "CREATE TABLE `tags` (`id` bigint NOT NULL AUTO_INCREMENT, `name` varchar(100) NOT NULL, UNIQUE INDEX `IDX_d90243459a697eadb8ad56e909` (`name`), PRIMARY KEY (`id`)) ENGINE=InnoDB",
        "CREATE TABLE `post_tag` (`post_id` bigint NOT NULL, `tag_id` bigint NOT NULL, INDEX `IDX_b5ec92f15aaa1e371f2662f681` (`post_id`), INDEX `IDX_d2fd5340bb68556fe93650fedc` (`tag_id`), PRIMARY KEY (`post_id`, `tag_id`)) ENGINE=InnoDB",
        "INSERT INTO `tags`(`id`, `name`) VALUES (DEFAULT, 'news'), (DEFAULT, 'rust'), (DEFAULT, 'sql')",
        "INSERT INTO `users`(`email`, `name`, `balance`, `profile`) VALUES ('alice@example.com', 'Alice', '100.50', '{\"city\": \"Tokyo\"}'), ('bob@example.com', 'Bob', '20.25', NULL), ('carol@example.com', 'Carol', '5.00', NULL)",
        "INSERT INTO `posts`(`id`, `user_id`, `title`, `body`, `published_at`, `views`) VALUES (DEFAULT, '1', 'Hello', 'First post', '2024-01-02 03:04:05.000', 10)",
        "INSERT INTO `posts`(`id`, `user_id`, `title`, `body`, `published_at`, `views`) VALUES (DEFAULT, '1', 'Draft', 'Not yet', DEFAULT, 0)",
        "INSERT INTO `post_tag`(`post_id`, `tag_id`) VALUES ('1', '1'), ('1', '2')",
        "INSERT INTO `posts`(`id`, `user_id`, `title`, `body`, `published_at`, `views`) VALUES (DEFAULT, '2', 'Bob writes', DEFAULT, DEFAULT, 3)",
        "INSERT INTO `post_tag`(`post_id`, `tag_id`) VALUES ('3', '3')",
    ] {
        adapter
            .execute_query(sql)
            .unwrap_or_else(|error| panic!("{sql}: {error:?}"));
    }
    (directory, adapter)
}

/// The body TypeORM pages a post and its tags through, each column renamed
/// after the relation it belongs to.
const POSTS_WITH_TAGS: &str = "SELECT `Post`.`id` AS `Post_id`, `Post`.`user_id` AS `Post_user_id`, `Post`.`title` AS `Post_title`, `Post`.`body` AS `Post_body`, `Post`.`published_at` AS `Post_published_at`, `Post`.`views` AS `Post_views`, `Post__Post_tags`.`id` AS `Post__Post_tags_id`, `Post__Post_tags`.`name` AS `Post__Post_tags_name` FROM `posts` `Post` LEFT JOIN `post_tag` `Post_Post__Post_tags` ON `Post_Post__Post_tags`.`post_id`=`Post`.`id` LEFT JOIN `tags` `Post__Post_tags` ON `Post__Post_tags`.`id`=`Post_Post__Post_tags`.`tag_id`";

/// A body reading every user with the posts each wrote.
const USERS_WITH_POSTS: &str = "SELECT u.id AS u_id, u.email AS u_email, u.balance AS u_balance, u.is_active AS u_active, u.profile AS u_profile, u.created_at AS u_created, p.id AS p_id, p.published_at AS p_published FROM users u LEFT JOIN posts p ON p.user_id = u.id";

/// A column MySQL reads out of the table it writes the rows into to drop the
/// repeated ones: it names its own table's name rather than the body's alias,
/// goes by the statement's name for it, names no database and keeps none of
/// its keys.
#[allow(clippy::too_many_arguments)]
fn written_out(
    name: &'static str,
    table: &'static str,
    original_table: &'static str,
    column_type: u8,
    length: u32,
    decimals: u8,
    flags: u16,
    character_set: u16,
) -> Column {
    Column {
        name,
        table,
        original_table,
        original_name: name,
        names_the_database: false,
        column_type,
        length,
        decimals,
        flags,
        character_set,
    }
}

/// A column read straight through a derived table joining tables: it names
/// the body's alias for its table and the body's name for it.
#[allow(clippy::too_many_arguments)]
fn read_through(
    name: &'static str,
    original_table: &'static str,
    original_name: &'static str,
    column_type: u8,
    length: u32,
    decimals: u8,
    flags: u16,
    character_set: u16,
) -> Column {
    Column {
        name,
        table: "d",
        original_table,
        original_name,
        names_the_database: true,
        column_type,
        length,
        decimals,
        flags,
        character_set,
    }
}

/// Every row, sorted, for a statement whose order neither engine promises.
fn sorted(mut answered: Rows) -> Rows {
    answered.sort();
    answered
}

/// `findAndCount` with `skip` and `take` over a post loaded with its tags:
/// TypeORM reads the page's ids out of the whole query standing as a derived
/// table, reads those posts with their tags, and counts the posts. Measured
/// on MySQL 8.4.11 over these rows, each column of the page reporting the
/// table MySQL writes the rows into to drop the repeated ones.
#[test]
fn typeorms_pagination_over_a_relation_answers_what_mysql_answers() {
    let (_directory, mut adapter) = typeorm_adapter();
    let page = |limit: &str| {
        format!("SELECT DISTINCT `distinctAlias`.`Post_id` AS `ids_Post_id`, `distinctAlias`.`Post_id` FROM ({POSTS_WITH_TAGS}) `distinctAlias` ORDER BY `distinctAlias`.`Post_id` ASC, `Post_id` ASC {limit}")
    };
    let id = |name| {
        written_out(
            name,
            "distinctAlias",
            "posts",
            MYSQL_TYPE_LONGLONG,
            20,
            0,
            MYSQL_NOT_NULL_FLAG,
            BINARY,
        )
    };
    let sql = page("LIMIT 1 OFFSET 0");
    let (columns, answered) = read(&mut adapter, &sql);
    assert_columns(&sql, &columns, &[id("ids_Post_id"), id("Post_id")]);
    assert_eq!(answered, rows(&[&[Some("1"), Some("1")]]));
    let sql = page("LIMIT 10");
    let (columns, answered) = read(&mut adapter, &sql);
    assert_columns(&sql, &columns, &[id("ids_Post_id"), id("Post_id")]);
    assert_eq!(
        answered,
        rows(&[
            &[Some("1"), Some("1")],
            &[Some("2"), Some("2")],
            &[Some("3"), Some("3")]
        ])
    );
    let (_, answered) = read(&mut adapter, &page("LIMIT 1 OFFSET 1"));
    assert_eq!(answered, rows(&[&[Some("2"), Some("2")]]));

    // Ordered by a column, TypeORM reads that column beside the id; ordered
    // by a relation's column, the column is on the side a LEFT JOIN can leave
    // missing, and a post without tags sorts last going down.
    let sql = format!("SELECT DISTINCT `distinctAlias`.`Post_id` AS `ids_Post_id`, `distinctAlias`.`Post_title` FROM ({POSTS_WITH_TAGS}) `distinctAlias` ORDER BY `distinctAlias`.`Post_title` ASC, `Post_id` ASC LIMIT 10");
    let (columns, answered) = read(&mut adapter, &sql);
    let title = written_out(
        "Post_title",
        "distinctAlias",
        "posts",
        MYSQL_TYPE_VAR_STRING,
        800,
        0,
        MYSQL_NOT_NULL_FLAG | MYSQL_NO_DEFAULT_VALUE_FLAG,
        WORDS,
    );
    assert_columns(&sql, &columns, &[id("ids_Post_id"), title]);
    assert_eq!(
        answered,
        rows(&[
            &[Some("3"), Some("Bob writes")],
            &[Some("2"), Some("Draft")],
            &[Some("1"), Some("Hello")]
        ])
    );
    let sql = format!("SELECT DISTINCT `distinctAlias`.`Post_id` AS `ids_Post_id`, `distinctAlias`.`Post__Post_tags_name` FROM ({POSTS_WITH_TAGS}) `distinctAlias` ORDER BY `distinctAlias`.`Post__Post_tags_name` DESC, `Post_id` ASC LIMIT 10");
    let (columns, answered) = read(&mut adapter, &sql);
    let tag_name = written_out(
        "Post__Post_tags_name",
        "distinctAlias",
        "tags",
        MYSQL_TYPE_VAR_STRING,
        400,
        0,
        MYSQL_NO_DEFAULT_VALUE_FLAG,
        WORDS,
    );
    assert_columns(&sql, &columns, &[id("ids_Post_id"), tag_name]);
    assert_eq!(
        answered,
        rows(&[
            &[Some("3"), Some("sql")],
            &[Some("1"), Some("rust")],
            &[Some("1"), Some("news")],
            &[Some("2"), None]
        ])
    );

    // TypeORM then reads the page's posts, naming each id as a word. Each
    // column names its table's alias as written, `Post`.
    let sql = format!("{POSTS_WITH_TAGS} WHERE `Post`.`id` IN ('1', '2') ORDER BY `Post`.`id` ASC");
    let (columns, answered) = read(&mut adapter, &sql);
    let joined =
        |name, table, original_table, original_name, column_type, length, flags, character_set| {
            Column {
                name,
                table,
                original_table,
                original_name,
                names_the_database: true,
                column_type,
                length,
                decimals: 0,
                flags,
                character_set,
            }
        };
    let no_default = MYSQL_NOT_NULL_FLAG | MYSQL_NO_DEFAULT_VALUE_FLAG;
    assert_columns(
        &sql,
        &columns,
        &[
            joined(
                "Post_id",
                "Post",
                "posts",
                "id",
                MYSQL_TYPE_LONGLONG,
                20,
                MYSQL_NOT_NULL_FLAG
                    | MYSQL_PRI_KEY_FLAG
                    | MYSQL_AUTO_INCREMENT_FLAG
                    | MYSQL_PART_KEY_FLAG,
                BINARY,
            ),
            joined(
                "Post_user_id",
                "Post",
                "posts",
                "user_id",
                MYSQL_TYPE_LONGLONG,
                20,
                no_default,
                BINARY,
            ),
            joined(
                "Post_title",
                "Post",
                "posts",
                "title",
                MYSQL_TYPE_VAR_STRING,
                800,
                no_default,
                WORDS,
            ),
            joined(
                "Post_body",
                "Post",
                "posts",
                "body",
                MYSQL_TYPE_BLOB,
                262_140,
                MYSQL_BLOB_FLAG,
                WORDS,
            ),
            joined(
                "Post_published_at",
                "Post",
                "posts",
                "published_at",
                MYSQL_TYPE_DATETIME,
                19,
                MYSQL_BINARY_FLAG,
                BINARY,
            ),
            joined(
                "Post_views",
                "Post",
                "posts",
                "views",
                MYSQL_TYPE_LONG,
                11,
                MYSQL_NOT_NULL_FLAG,
                BINARY,
            ),
            joined(
                "Post__Post_tags_id",
                "Post__Post_tags",
                "tags",
                "id",
                MYSQL_TYPE_LONGLONG,
                20,
                MYSQL_PRI_KEY_FLAG | MYSQL_AUTO_INCREMENT_FLAG | MYSQL_PART_KEY_FLAG,
                BINARY,
            ),
            joined(
                "Post__Post_tags_name",
                "Post__Post_tags",
                "tags",
                "name",
                MYSQL_TYPE_VAR_STRING,
                400,
                MYSQL_UNIQUE_KEY_FLAG | MYSQL_NO_DEFAULT_VALUE_FLAG | MYSQL_PART_KEY_FLAG,
                WORDS,
            ),
        ],
    );
    assert_eq!(
        answered,
        rows(&[
            &[
                Some("1"),
                Some("1"),
                Some("Hello"),
                Some("First post"),
                Some("2024-01-02 03:04:05"),
                Some("10"),
                Some("1"),
                Some("news")
            ],
            &[
                Some("1"),
                Some("1"),
                Some("Hello"),
                Some("First post"),
                Some("2024-01-02 03:04:05"),
                Some("10"),
                Some("2"),
                Some("rust")
            ],
            &[
                Some("2"),
                Some("1"),
                Some("Draft"),
                Some("Not yet"),
                None,
                Some("0"),
                None,
                None
            ],
        ])
    );

    // And counts the posts.
    let sql = "SELECT COUNT(DISTINCT `Post`.`id`) AS `cnt` FROM `posts` `Post` LEFT JOIN `post_tag` `Post_Post__Post_tags` ON `Post_Post__Post_tags`.`post_id`=`Post`.`id` LEFT JOIN `tags` `Post__Post_tags` ON `Post__Post_tags`.`id`=`Post_Post__Post_tags`.`tag_id`";
    let (columns, answered) = read(&mut adapter, sql);
    assert_columns(
        sql,
        &columns,
        &[Column {
            name: "cnt",
            table: "",
            original_table: "",
            original_name: "",
            names_the_database: false,
            column_type: MYSQL_TYPE_LONGLONG,
            length: 21,
            decimals: 0,
            flags: MYSQL_NOT_NULL_FLAG | MYSQL_BINARY_FLAG | MYSQL_NUM_FLAG,
            character_set: BINARY,
        }],
    );
    assert_eq!(answered, rows(&[&[Some("3")]]));
}

/// With nothing dropping repeated rows, MySQL reads a derived table joining
/// tables straight through: each column keeps every flag of its own table's
/// column, names the body's alias for that table and the body's name for the
/// column, and a day or a moment reports in words, as it does through a
/// derived table reading one table. A column on the side a LEFT JOIN can leave
/// missing loses its NOT NULL. Measured on MySQL 8.4.11.
#[test]
fn a_derived_table_joining_tables_reads_each_column_through_its_own_table() {
    let (_directory, mut adapter) = typeorm_adapter();
    let sql = format!("SELECT d.Post_id AS ids_Post_id, d.Post_user_id, d.Post_title, d.Post_body, d.Post_published_at, d.Post_views, d.Post__Post_tags_id, d.Post__Post_tags_name FROM ({POSTS_WITH_TAGS}) d");
    let (columns, answered) = read(&mut adapter, &sql);
    assert_columns(
        &sql,
        &columns,
        &[
            read_through(
                "ids_Post_id",
                "Post",
                "Post_id",
                MYSQL_TYPE_LONGLONG,
                20,
                0,
                MYSQL_NOT_NULL_FLAG
                    | MYSQL_PRI_KEY_FLAG
                    | MYSQL_AUTO_INCREMENT_FLAG
                    | MYSQL_PART_KEY_FLAG,
                BINARY,
            ),
            read_through(
                "Post_user_id",
                "Post",
                "Post_user_id",
                MYSQL_TYPE_LONGLONG,
                20,
                0,
                MYSQL_NOT_NULL_FLAG | MYSQL_NO_DEFAULT_VALUE_FLAG,
                BINARY,
            ),
            read_through(
                "Post_title",
                "Post",
                "Post_title",
                MYSQL_TYPE_VAR_STRING,
                800,
                0,
                MYSQL_NOT_NULL_FLAG | MYSQL_NO_DEFAULT_VALUE_FLAG,
                WORDS,
            ),
            read_through(
                "Post_body",
                "Post",
                "Post_body",
                MYSQL_TYPE_BLOB,
                262_140,
                0,
                MYSQL_BLOB_FLAG,
                WORDS,
            ),
            read_through(
                "Post_published_at",
                "Post",
                "Post_published_at",
                MYSQL_TYPE_DATETIME,
                76,
                0,
                MYSQL_BINARY_FLAG,
                WORDS,
            ),
            read_through(
                "Post_views",
                "Post",
                "Post_views",
                MYSQL_TYPE_LONG,
                11,
                0,
                MYSQL_NOT_NULL_FLAG,
                BINARY,
            ),
            read_through(
                "Post__Post_tags_id",
                "Post__Post_tags",
                "Post__Post_tags_id",
                MYSQL_TYPE_LONGLONG,
                20,
                0,
                MYSQL_PRI_KEY_FLAG | MYSQL_AUTO_INCREMENT_FLAG | MYSQL_PART_KEY_FLAG,
                BINARY,
            ),
            read_through(
                "Post__Post_tags_name",
                "Post__Post_tags",
                "Post__Post_tags_name",
                MYSQL_TYPE_VAR_STRING,
                400,
                0,
                MYSQL_UNIQUE_KEY_FLAG | MYSQL_NO_DEFAULT_VALUE_FLAG | MYSQL_PART_KEY_FLAG,
                WORDS,
            ),
        ],
    );
    assert_eq!(
        sorted(answered),
        rows(&[
            &[
                Some("1"),
                Some("1"),
                Some("Hello"),
                Some("First post"),
                Some("2024-01-02 03:04:05"),
                Some("10"),
                Some("1"),
                Some("news")
            ],
            &[
                Some("1"),
                Some("1"),
                Some("Hello"),
                Some("First post"),
                Some("2024-01-02 03:04:05"),
                Some("10"),
                Some("2"),
                Some("rust")
            ],
            &[
                Some("2"),
                Some("1"),
                Some("Draft"),
                Some("Not yet"),
                None,
                Some("0"),
                None,
                None
            ],
            &[
                Some("3"),
                Some("2"),
                Some("Bob writes"),
                None,
                None,
                Some("3"),
                Some("3"),
                Some("sql")
            ],
        ])
    );

    // A column the body names without renaming goes by its own name.
    let sql = "SELECT d.name, d.Post_id FROM (SELECT `Post`.`id` AS `Post_id`, `Post__Post_tags`.`name` FROM `posts` `Post` LEFT JOIN `post_tag` `Post_Post__Post_tags` ON `Post_Post__Post_tags`.`post_id`=`Post`.`id` LEFT JOIN `tags` `Post__Post_tags` ON `Post__Post_tags`.`id`=`Post_Post__Post_tags`.`tag_id`) d";
    let (columns, _) = read(&mut adapter, sql);
    assert_eq!(
        (
            columns[0].original_table.as_str(),
            columns[0].original_name.as_str(),
            columns[0].flags
        ),
        (
            "Post__Post_tags",
            "name",
            MYSQL_UNIQUE_KEY_FLAG | MYSQL_NO_DEFAULT_VALUE_FLAG | MYSQL_PART_KEY_FLAG
        )
    );

    // A number with decimals, a small whole number, a document and a moment
    // with a fraction, read through the same way.
    let sql = format!("SELECT d.u_id, d.u_balance, d.u_active, d.u_profile, d.u_created, d.p_id, d.p_published FROM ({USERS_WITH_POSTS}) d");
    let (columns, answered) = read(&mut adapter, &sql);
    assert_columns(
        &sql,
        &columns,
        &[
            read_through(
                "u_id",
                "u",
                "u_id",
                MYSQL_TYPE_LONGLONG,
                20,
                0,
                MYSQL_NOT_NULL_FLAG
                    | MYSQL_PRI_KEY_FLAG
                    | MYSQL_AUTO_INCREMENT_FLAG
                    | MYSQL_PART_KEY_FLAG,
                BINARY,
            ),
            read_through(
                "u_balance",
                "u",
                "u_balance",
                MYSQL_TYPE_NEWDECIMAL,
                12,
                2,
                MYSQL_NOT_NULL_FLAG,
                BINARY,
            ),
            read_through(
                "u_active",
                "u",
                "u_active",
                MYSQL_TYPE_TINY,
                4,
                0,
                MYSQL_NOT_NULL_FLAG,
                BINARY,
            ),
            read_through(
                "u_profile",
                "u",
                "u_profile",
                MYSQL_TYPE_JSON,
                u32::MAX - 3,
                0,
                MYSQL_BLOB_FLAG | MYSQL_BINARY_FLAG,
                WORDS,
            ),
            read_through(
                "u_created",
                "u",
                "u_created",
                MYSQL_TYPE_DATETIME,
                104,
                6,
                MYSQL_NOT_NULL_FLAG | MYSQL_BINARY_FLAG,
                WORDS,
            ),
            read_through(
                "p_id",
                "p",
                "p_id",
                MYSQL_TYPE_LONGLONG,
                20,
                0,
                MYSQL_PRI_KEY_FLAG | MYSQL_AUTO_INCREMENT_FLAG | MYSQL_PART_KEY_FLAG,
                BINARY,
            ),
            read_through(
                "p_published",
                "p",
                "p_published",
                MYSQL_TYPE_DATETIME,
                76,
                0,
                MYSQL_BINARY_FLAG,
                WORDS,
            ),
        ],
    );
    let balances = sorted(answered)
        .into_iter()
        .map(|row| (row[0].clone(), row[1].clone(), row[5].clone()))
        .collect::<Vec<_>>();
    let written = |id: &str, balance: &str, post: Option<&str>| {
        (
            Some(id.to_owned()),
            Some(balance.to_owned()),
            post.map(str::to_owned),
        )
    };
    assert_eq!(
        balances,
        [
            written("1", "100.50", Some("1")),
            written("1", "100.50", Some("2")),
            written("2", "20.25", Some("3")),
            written("3", "5.00", None),
        ]
    );
}

/// `DISTINCT` over a derived table joining tables makes MySQL write the rows
/// into a table of its own, and each column reports that table's: its own
/// table's name, the statement's name for it, no database and no keys, while
/// its type, length and every other flag stay those of its table's column —
/// a moment is not turned into words. Measured on MySQL 8.4.11 over empty
/// tables, tables of one row and tables of thousands, which all answer alike.
#[test]
fn dropping_repeats_over_a_derived_table_joining_tables_reports_mysqls_own_table() {
    let (_directory, mut adapter) = typeorm_adapter();
    let sql = format!("SELECT DISTINCT d.Post_id AS ids_Post_id, d.Post_user_id, d.Post_title, d.Post_published_at, d.Post_views, d.Post__Post_tags_id, d.Post__Post_tags_name FROM ({POSTS_WITH_TAGS}) d ORDER BY d.Post_id, d.Post__Post_tags_id");
    let (columns, answered) = read(&mut adapter, &sql);
    let no_default = MYSQL_NOT_NULL_FLAG | MYSQL_NO_DEFAULT_VALUE_FLAG;
    assert_columns(
        &sql,
        &columns,
        &[
            written_out(
                "ids_Post_id",
                "d",
                "posts",
                MYSQL_TYPE_LONGLONG,
                20,
                0,
                MYSQL_NOT_NULL_FLAG,
                BINARY,
            ),
            written_out(
                "Post_user_id",
                "d",
                "posts",
                MYSQL_TYPE_LONGLONG,
                20,
                0,
                no_default,
                BINARY,
            ),
            written_out(
                "Post_title",
                "d",
                "posts",
                MYSQL_TYPE_VAR_STRING,
                800,
                0,
                no_default,
                WORDS,
            ),
            written_out(
                "Post_published_at",
                "d",
                "posts",
                MYSQL_TYPE_DATETIME,
                19,
                0,
                MYSQL_BINARY_FLAG,
                BINARY,
            ),
            written_out(
                "Post_views",
                "d",
                "posts",
                MYSQL_TYPE_LONG,
                11,
                0,
                MYSQL_NOT_NULL_FLAG,
                BINARY,
            ),
            written_out(
                "Post__Post_tags_id",
                "d",
                "tags",
                MYSQL_TYPE_LONGLONG,
                20,
                0,
                0,
                BINARY,
            ),
            written_out(
                "Post__Post_tags_name",
                "d",
                "tags",
                MYSQL_TYPE_VAR_STRING,
                400,
                0,
                MYSQL_NO_DEFAULT_VALUE_FLAG,
                WORDS,
            ),
        ],
    );
    assert_eq!(
        answered,
        rows(&[
            &[
                Some("1"),
                Some("1"),
                Some("Hello"),
                Some("2024-01-02 03:04:05"),
                Some("10"),
                Some("1"),
                Some("news")
            ],
            &[
                Some("1"),
                Some("1"),
                Some("Hello"),
                Some("2024-01-02 03:04:05"),
                Some("10"),
                Some("2"),
                Some("rust")
            ],
            &[
                Some("2"),
                Some("1"),
                Some("Draft"),
                None,
                Some("0"),
                None,
                None
            ],
            &[
                Some("3"),
                Some("2"),
                Some("Bob writes"),
                None,
                Some("3"),
                Some("3"),
                Some("sql")
            ],
        ])
    );
    let sql = format!("SELECT DISTINCT d.Post_body FROM ({POSTS_WITH_TAGS}) d");
    let (columns, answered) = read(&mut adapter, &sql);
    assert_columns(
        &sql,
        &columns,
        &[written_out(
            "Post_body",
            "d",
            "posts",
            MYSQL_TYPE_BLOB,
            262_140,
            0,
            MYSQL_BLOB_FLAG,
            WORDS,
        )],
    );
    assert_eq!(
        sorted(answered),
        rows(&[&[None], &[Some("First post")], &[Some("Not yet")]])
    );

    // A number with decimals, a small whole number, a document and a moment
    // with a fraction keep their own shapes.
    let sql = format!("SELECT DISTINCT d.u_id, d.u_email, d.u_balance, d.u_active, d.u_created, d.p_id FROM ({USERS_WITH_POSTS}) d ORDER BY d.u_id, d.p_id");
    let (columns, answered) = read(&mut adapter, &sql);
    assert_columns(
        &sql,
        &columns,
        &[
            written_out(
                "u_id",
                "d",
                "users",
                MYSQL_TYPE_LONGLONG,
                20,
                0,
                MYSQL_NOT_NULL_FLAG,
                BINARY,
            ),
            written_out(
                "u_email",
                "d",
                "users",
                MYSQL_TYPE_VAR_STRING,
                764,
                0,
                no_default,
                WORDS,
            ),
            written_out(
                "u_balance",
                "d",
                "users",
                MYSQL_TYPE_NEWDECIMAL,
                12,
                2,
                MYSQL_NOT_NULL_FLAG,
                BINARY,
            ),
            written_out(
                "u_active",
                "d",
                "users",
                MYSQL_TYPE_TINY,
                4,
                0,
                MYSQL_NOT_NULL_FLAG,
                BINARY,
            ),
            written_out(
                "u_created",
                "d",
                "users",
                MYSQL_TYPE_DATETIME,
                26,
                6,
                MYSQL_NOT_NULL_FLAG | MYSQL_BINARY_FLAG,
                BINARY,
            ),
            written_out("p_id", "d", "posts", MYSQL_TYPE_LONGLONG, 20, 0, 0, BINARY),
        ],
    );
    let picked = answered
        .into_iter()
        .map(|row| (row[0].clone(), row[2].clone(), row[5].clone()))
        .collect::<Vec<_>>();
    let written = |id: &str, balance: &str, post: Option<&str>| {
        (
            Some(id.to_owned()),
            Some(balance.to_owned()),
            post.map(str::to_owned),
        )
    };
    assert_eq!(
        picked,
        [
            written("1", "100.50", Some("1")),
            written("1", "100.50", Some("2")),
            written("2", "20.25", Some("3")),
            written("3", "5.00", None),
        ]
    );
    let sql = format!("SELECT DISTINCT d.u_profile FROM ({USERS_WITH_POSTS}) d");
    let (columns, _) = read(&mut adapter, &sql);
    assert_columns(
        &sql,
        &columns,
        &[written_out(
            "u_profile",
            "d",
            "users",
            MYSQL_TYPE_JSON,
            u32::MAX,
            0,
            MYSQL_BLOB_FLAG | MYSQL_BINARY_FLAG,
            BINARY,
        )],
    );

    // An order by place, and by a name the body gave without renaming.
    let sql = format!("SELECT DISTINCT d.Post_id FROM ({POSTS_WITH_TAGS}) d ORDER BY 1 DESC");
    let (_, answered) = read(&mut adapter, &sql);
    assert_eq!(answered, rows(&[&[Some("3")], &[Some("2")], &[Some("1")]]));
    let sql = "SELECT DISTINCT Post_id FROM (SELECT `Post`.`id` AS `Post_id`, `Post__Post_tags`.`name` FROM `posts` `Post` LEFT JOIN `post_tag` `Post_Post__Post_tags` ON `Post_Post__Post_tags`.`post_id`=`Post`.`id` LEFT JOIN `tags` `Post__Post_tags` ON `Post__Post_tags`.`id`=`Post_Post__Post_tags`.`tag_id`) d ORDER BY Post_id";
    let (columns, answered) = read(&mut adapter, sql);
    assert_eq!(
        (
            columns[0].original_table.as_str(),
            columns[0].original_name.as_str()
        ),
        ("posts", "Post_id")
    );
    assert_eq!(answered, rows(&[&[Some("1")], &[Some("2")], &[Some("3")]]));

    // Words differing only in case are one value to drop, and the first met
    // is the one kept; ordered, they sort without regard to case.
    adapter
        .execute_query("INSERT INTO posts (user_id, title) VALUES (1, 'beta'), (1, 'Alpha'), (2, 'alpha'), (2, 'Zulu'), (3, 'apple'), (3, 'ALPHA')")
        .unwrap();
    let sql =
        format!("SELECT DISTINCT d.Post_title FROM ({POSTS_WITH_TAGS}) d ORDER BY d.Post_title");
    let (_, answered) = read(&mut adapter, &sql);
    assert_eq!(
        answered,
        rows(&[
            &[Some("Alpha")],
            &[Some("apple")],
            &[Some("beta")],
            &[Some("Bob writes")],
            &[Some("Draft")],
            &[Some("Hello")],
            &[Some("Zulu")]
        ])
    );
    let sql = format!("SELECT DISTINCT `distinctAlias`.`Post_id` AS `ids_Post_id`, `distinctAlias`.`Post_title` FROM ({POSTS_WITH_TAGS}) `distinctAlias` ORDER BY `distinctAlias`.`Post_title` ASC, `Post_id` ASC LIMIT 4");
    let (_, answered) = read(&mut adapter, &sql);
    assert_eq!(
        answered,
        rows(&[
            &[Some("5"), Some("Alpha")],
            &[Some("6"), Some("alpha")],
            &[Some("9"), Some("ALPHA")],
            &[Some("8"), Some("apple")]
        ])
    );
}

/// A table holding one column of each kind, and notes that can be missing
/// for a row of it.
#[test]
fn dropping_repeats_keeps_every_kind_of_column_as_its_table_declares_it() {
    let (_directory, mut adapter) = typeorm_adapter();
    for sql in [
        "CREATE TABLE kinds (id BIGINT UNSIGNED AUTO_INCREMENT PRIMARY KEY, code CHAR(3) NOT NULL, day DATE NULL, stamp TIMESTAMP NULL, moved DATETIME NOT NULL DEFAULT CURRENT_TIMESTAMP ON UPDATE CURRENT_TIMESTAMP, mood ENUM('low','high') NOT NULL DEFAULT 'low', ratio DOUBLE NULL, small SMALLINT UNSIGNED NULL, price DECIMAL(8,3) UNSIGNED NULL, clock TIME NULL, blobby BLOB NULL, vb VARBINARY(16) NULL)",
        "CREATE TABLE notes (id INT AUTO_INCREMENT PRIMARY KEY, kind_id BIGINT UNSIGNED NOT NULL, note VARCHAR(50) NULL, KEY (kind_id))",
        "INSERT INTO kinds (code, day, stamp, mood, ratio, small, price, clock, blobby, vb) VALUES ('abc', '2024-01-02', '2024-01-02 03:04:05', 'high', 1.5, 7, 1.25, '10:11:12', 'xy', 'ab'), ('def', NULL, NULL, 'low', NULL, NULL, NULL, NULL, NULL, NULL)",
        "INSERT INTO notes (kind_id, note) VALUES (1, 'n1'), (1, 'n2')",
    ] {
        adapter.execute_query(sql).unwrap();
    }
    let body = "SELECT k.id AS k_id, k.code AS k_code, k.day AS k_day, k.stamp AS k_stamp, k.moved AS k_moved, k.mood AS k_mood, k.ratio AS k_ratio, k.small AS k_small, k.price AS k_price, k.clock AS k_clock, k.blobby AS k_blobby, k.vb AS k_vb, n.id AS n_id, n.kind_id AS n_kind, n.note AS n_note FROM kinds k LEFT JOIN notes n ON n.kind_id = k.id";
    let sql = format!("SELECT DISTINCT d.k_id, d.k_code, d.k_day, d.k_stamp, d.k_moved, d.k_mood, d.k_ratio, d.k_small, d.k_price, d.k_clock, d.k_blobby, d.k_vb, d.n_id, d.n_kind, d.n_note FROM ({body}) d");
    let (columns, answered) = read(&mut adapter, &sql);
    let kind = |name, column_type, length, decimals, flags, character_set| {
        written_out(
            name,
            "d",
            "kinds",
            column_type,
            length,
            decimals,
            flags,
            character_set,
        )
    };
    let note = |name, column_type, length, flags, character_set| {
        written_out(
            name,
            "d",
            "notes",
            column_type,
            length,
            0,
            flags,
            character_set,
        )
    };
    assert_columns(
        &sql,
        &columns,
        &[
            kind(
                "k_id",
                MYSQL_TYPE_LONGLONG,
                20,
                0,
                MYSQL_NOT_NULL_FLAG | MYSQL_UNSIGNED_FLAG,
                BINARY,
            ),
            kind(
                "k_code",
                MYSQL_TYPE_STRING,
                12,
                0,
                MYSQL_NOT_NULL_FLAG | MYSQL_NO_DEFAULT_VALUE_FLAG,
                WORDS,
            ),
            kind("k_day", MYSQL_TYPE_DATE, 10, 0, MYSQL_BINARY_FLAG, BINARY),
            kind(
                "k_stamp",
                MYSQL_TYPE_TIMESTAMP,
                19,
                0,
                MYSQL_BINARY_FLAG,
                BINARY,
            ),
            kind(
                "k_moved",
                MYSQL_TYPE_DATETIME,
                19,
                0,
                MYSQL_NOT_NULL_FLAG | MYSQL_BINARY_FLAG,
                BINARY,
            ),
            kind(
                "k_mood",
                MYSQL_TYPE_STRING,
                16,
                0,
                MYSQL_NOT_NULL_FLAG | MYSQL_ENUM_FLAG,
                WORDS,
            ),
            kind(
                "k_ratio",
                MYSQL_TYPE_DOUBLE,
                22,
                NOT_FIXED_DECIMALS,
                0,
                BINARY,
            ),
            kind(
                "k_small",
                MYSQL_TYPE_SHORT,
                5,
                0,
                MYSQL_UNSIGNED_FLAG,
                BINARY,
            ),
            kind(
                "k_price",
                MYSQL_TYPE_NEWDECIMAL,
                9,
                3,
                MYSQL_UNSIGNED_FLAG,
                BINARY,
            ),
            kind("k_clock", MYSQL_TYPE_TIME, 10, 0, MYSQL_BINARY_FLAG, BINARY),
            kind(
                "k_blobby",
                MYSQL_TYPE_BLOB,
                65_535,
                0,
                MYSQL_BLOB_FLAG | MYSQL_BINARY_FLAG,
                BINARY,
            ),
            kind(
                "k_vb",
                MYSQL_TYPE_VAR_STRING,
                16,
                0,
                MYSQL_BINARY_FLAG,
                BINARY,
            ),
            note("n_id", MYSQL_TYPE_LONG, 11, 0, BINARY),
            note(
                "n_kind",
                MYSQL_TYPE_LONGLONG,
                20,
                MYSQL_UNSIGNED_FLAG | MYSQL_NO_DEFAULT_VALUE_FLAG,
                BINARY,
            ),
            note("n_note", MYSQL_TYPE_VAR_STRING, 200, 0, WORDS),
        ],
    );
    assert_eq!(answered.len(), 3);
}

#[test]
fn a_derived_table_joining_tables_refuses_what_has_not_been_measured() {
    let (_directory, mut adapter) = typeorm_adapter();
    let refused = [
        // MySQL sorts through a table of its own when it matches a joined
        // table by hash, which it picks by how many rows each table holds.
        format!("SELECT d.Post_id FROM ({POSTS_WITH_TAGS}) d ORDER BY d.Post_id"),
        // Which rows a limit with no order keeps is each engine's own.
        format!("SELECT d.Post_id FROM ({POSTS_WITH_TAGS}) d LIMIT 1"),
        format!("SELECT DISTINCT d.Post_id FROM ({POSTS_WITH_TAGS}) d LIMIT 1"),
        // 3065: an order by a column a DISTINCT does not project.
        format!("SELECT DISTINCT d.Post_id FROM ({POSTS_WITH_TAGS}) d ORDER BY d.Post_title"),
        // A condition naming a key against a value makes MySQL read that
        // table as one constant row.
        "SELECT DISTINCT `distinctAlias`.`Post_id` FROM (SELECT `Post`.`id` AS `Post_id` FROM `posts` `Post` LEFT JOIN `post_tag` `pt` ON `pt`.`post_id`=`Post`.`id` WHERE `Post`.`id` = 1) `distinctAlias` ORDER BY `distinctAlias`.`Post_id`".to_owned(),
        format!("SELECT DISTINCT d.Post_id FROM ({POSTS_WITH_TAGS}) d WHERE d.Post_id > 1"),
        // Only columns matched against each other have been measured.
        "SELECT DISTINCT d.Post_id FROM (SELECT `Post`.`id` AS `Post_id`, `t`.`name` AS `n` FROM `posts` `Post` LEFT JOIN `tags` `t` ON `t`.`id` = 1) d ORDER BY d.Post_id".to_owned(),
        // MySQL picks its own order for an inner join.
        "SELECT DISTINCT d.p_id FROM (SELECT p.id AS p_id, u.name AS u_name FROM posts p JOIN users u ON u.id = p.user_id) d".to_owned(),
        // Anything but the derived table's own columns.
        format!("SELECT COUNT(*) FROM ({POSTS_WITH_TAGS}) d"),
        format!("SELECT COUNT(DISTINCT d.Post_id) FROM ({POSTS_WITH_TAGS}) d"),
        format!("SELECT DISTINCT d.Post_id + 1 FROM ({POSTS_WITH_TAGS}) d"),
        format!("SELECT * FROM ({POSTS_WITH_TAGS}) d"),
        format!("SELECT d.Post_id, COUNT(*) FROM ({POSTS_WITH_TAGS}) d GROUP BY d.Post_id"),
        format!("SELECT DISTINCT d.Post_id, t.name FROM ({POSTS_WITH_TAGS}) d JOIN tags t ON t.id = d.Post_id"),
        // A derived table joining tables inside another statement.
        format!("SELECT p.id FROM posts p WHERE p.id IN (SELECT d.Post_id FROM ({POSTS_WITH_TAGS}) d)"),
        // 1060: two columns going by one name.
        "SELECT d.id FROM (SELECT `Post`.`id`, `t`.`id` FROM `posts` `Post` LEFT JOIN `tags` `t` ON `t`.`id`=`Post`.`id`) d".to_owned(),
        // A bare name in the body belongs to whichever table has the column.
        "SELECT DISTINCT d.Post_id FROM (SELECT `Post`.`id` AS `Post_id`, `title` FROM `posts` `Post` LEFT JOIN `tags` `t` ON `t`.`id`=`Post`.`id`) d".to_owned(),
        // A derived table joined inside the body.
        "SELECT DISTINCT d.Post_id FROM (SELECT `Post`.`id` AS `Post_id` FROM `posts` `Post` LEFT JOIN (SELECT id FROM tags) `t` ON `t`.`id`=`Post`.`id`) d".to_owned(),
    ];
    for sql in refused {
        assert!(
            matches!(
                adapter.execute_query(&sql),
                Err(FrontendErrorKind::Syntax | FrontendErrorKind::Unsupported)
            ),
            "{sql}"
        );
    }
}
