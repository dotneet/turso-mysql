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
