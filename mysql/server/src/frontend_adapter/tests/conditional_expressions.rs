//! `CASE`, `IF`, `IFNULL` and `COALESCE` over columns, and the aggregates a
//! report wraps around a `CASE` to count or total the rows meeting a
//! condition.
//!
//! Every expectation here was measured on MySQL 8.4.11, over both the text
//! and the binary protocol.

use super::*;

fn adapter() -> (
    tempfile::TempDir,
    AuthorizedDatabaseCommandAdapter<RecordingAuthorizer>,
) {
    let authorizer = Arc::new(RecordingAuthorizer::default());
    let (directory, _catalog, factory) = catalog_factory(authorizer);
    let mut adapter = factory
        .build(AuthenticatedPrincipal::from_account_id_for_testing(
            AccountId::from_bytes([151; 32]),
        ))
        .unwrap();
    adapter.authorize_connection().unwrap();
    adapter.execute_init_db("REPORTS").unwrap();
    for sql in [
        "CREATE TABLE users (id BIGINT UNSIGNED NOT NULL AUTO_INCREMENT PRIMARY KEY, name VARCHAR(100) NOT NULL, email VARCHAR(191) NOT NULL, status VARCHAR(20) NOT NULL DEFAULT 'active', age INT NULL, score DOUBLE NULL, balance DECIMAL(10,2) NOT NULL DEFAULT '0.00', nick VARCHAR(30) NULL, bio TEXT NULL, small SMALLINT NOT NULL DEFAULT 0, big BIGINT NULL, fee DECIMAL(6,3) NULL, code CHAR(5) NULL, tiny TINYINT NULL, utiny TINYINT UNSIGNED NULL)",
        "INSERT INTO users (name, email, status, age, score, balance, nick, small, big, fee, tiny) VALUES ('alice', 'a@x', 'active', 30, 1.5, 10.50, 'al', 3, 9007199254740993, 1.125, 5)",
        "INSERT INTO users (name, email, status, age, score, balance, nick, small, big, fee) VALUES ('bob', 'b@x', 'inactive', NULL, NULL, 0, NULL, -2, NULL, NULL)",
        "INSERT INTO users (name, email, status, age, score, balance, nick, small, big, fee, code) VALUES ('carol', 'c@x', 'active', 17, 2.25, 3.25, 'cc', 7, 5, 2.5, 'ab')",
        "INSERT INTO users (name, email, status, age, score, balance, nick, small, big, fee) VALUES ('dave', 'd@x', 'active', 40, NULL, 7, NULL, 0, NULL, NULL)",
    ] {
        adapter
            .execute_query(sql)
            .unwrap_or_else(|error| panic!("{sql}: {error:?}"));
    }
    (directory, adapter)
}

fn result(
    adapter: &mut AuthorizedDatabaseCommandAdapter<RecordingAuthorizer>,
    sql: &str,
) -> TextResultSet {
    match adapter.execute_query(sql) {
        Ok(CommandExecutionResult::ResultSet(result)) => result,
        other => panic!("{sql} must return a result set, got {other:?}"),
    }
}

/// Each column's type, length, decimals and flags.
fn shapes(result: &TextResultSet) -> Vec<(u8, u32, u8, u16)> {
    result
        .columns
        .iter()
        .map(|column| {
            (
                column.column_type,
                column.column_length,
                column.decimals,
                column.flags,
            )
        })
        .collect()
}

/// Each row of the result, its NULLs written as `NULL`.
fn rows(result: &TextResultSet) -> Vec<String> {
    result
        .rows
        .iter()
        .map(|row| {
            row.iter()
                .map(|value| {
                    value.as_ref().map_or("NULL".to_owned(), |value| {
                        String::from_utf8(value.clone()).unwrap()
                    })
                })
                .collect::<Vec<_>>()
                .join(" ")
        })
        .collect()
}

/// The same statement prepared and executed, as a client binding nothing
/// sends it.
fn prepared(
    adapter: &mut AuthorizedDatabaseCommandAdapter<RecordingAuthorizer>,
    sql: &str,
) -> BinaryResultSet {
    let statement = adapter
        .execute_stmt_prepare(sql)
        .unwrap_or_else(|error| panic!("prepare {sql}: {error:?}"));
    let result = adapter
        .execute_stmt_execute(statement.statement_id, &[])
        .unwrap_or_else(|error| panic!("execute {sql}: {error:?}"));
    adapter.execute_stmt_close(statement.statement_id);
    prepared_result_set(result)
}

const NUMBER: u16 = MYSQL_BINARY_FLAG | MYSQL_NUM_FLAG;
const NOT_NULL_NUMBER: u16 = NUMBER | MYSQL_NOT_NULL_FLAG;

/// A `CASE` answers the kind its branches share, as wide as the widest of
/// them. A `DECIMAL` answer keeps each branch at its own scale — `0` beside
/// `10.50` — and a `DOUBLE` one answers a whole number branch as a double.
#[test]
fn a_case_over_columns_answers_the_kind_its_branches_share() {
    let (_directory, mut adapter) = adapter();
    let sql = "SELECT CASE WHEN status = 'active' THEN balance ELSE 0 END AS d1, CASE WHEN status = 'active' THEN balance ELSE age END AS d2, CASE WHEN status = 'active' THEN fee ELSE balance END AS d3, CASE WHEN status = 'active' THEN score ELSE 0 END AS f1, CASE WHEN status = 'active' THEN small ELSE big END AS i1, CASE WHEN status = 'active' THEN age ELSE small END AS i2, IF(status = 'active', small, tiny) AS i3, CASE WHEN status = 'active' THEN 5 ELSE small END AS i4 FROM users";
    let text = result(&mut adapter, sql);
    assert_eq!(
        shapes(&text),
        [
            (MYSQL_TYPE_NEWDECIMAL, 12, 2, NOT_NULL_NUMBER),
            (MYSQL_TYPE_NEWDECIMAL, 14, 2, NUMBER),
            (MYSQL_TYPE_NEWDECIMAL, 13, 3, NUMBER),
            (MYSQL_TYPE_DOUBLE, 23, NOT_FIXED_DECIMALS, NUMBER),
            (MYSQL_TYPE_LONGLONG, 20, 0, NUMBER),
            (MYSQL_TYPE_LONG, 11, 0, NUMBER),
            (MYSQL_TYPE_SHORT, 6, 0, NUMBER),
            (MYSQL_TYPE_LONGLONG, 6, 0, NOT_NULL_NUMBER),
        ]
    );
    assert_eq!(
        rows(&text),
        [
            "10.50 10.50 1.125 1.5 3 30 3 5",
            "0 NULL 0.00 0 NULL -2 NULL -2",
            "3.25 3.25 2.500 2.25 7 17 7 5",
            "7.00 7.00 NULL NULL 0 40 0 5",
        ]
    );
    let binary = prepared(&mut adapter, sql);
    assert_eq!(
        binary.rows[1],
        [
            BinaryResultValue::Text("0".to_owned()),
            BinaryResultValue::Null,
            BinaryResultValue::Text("0.00".to_owned()),
            BinaryResultValue::Real(0.0),
            BinaryResultValue::Null,
            BinaryResultValue::Integer(-2),
            BinaryResultValue::Null,
            BinaryResultValue::Integer(-2),
        ]
    );

    // Words and columns of words answer the widest of them, four bytes a
    // character, and a `CHAR` answers a `VAR_STRING` beside a `VARCHAR`.
    let sql = "SELECT CASE WHEN age > 18 THEN name ELSE 'minor' END AS c1, IF(id > 1, name, nick) AS c2, CASE WHEN age < 18 THEN code ELSE nick END AS c3 FROM users";
    let text = result(&mut adapter, sql);
    assert_eq!(
        shapes(&text),
        [
            (
                MYSQL_TYPE_VAR_STRING,
                400,
                NOT_FIXED_DECIMALS,
                MYSQL_NOT_NULL_FLAG
            ),
            (MYSQL_TYPE_VAR_STRING, 400, NOT_FIXED_DECIMALS, 0),
            (MYSQL_TYPE_VAR_STRING, 120, NOT_FIXED_DECIMALS, 0),
        ]
    );
    assert_eq!(
        rows(&text),
        [
            "alice al al",
            "minor bob NULL",
            "minor carol ab",
            "dave dave NULL",
        ]
    );
    assert_eq!(
        prepared(&mut adapter, sql).rows[2],
        [
            BinaryResultValue::Text("minor".to_owned()),
            BinaryResultValue::Text("carol".to_owned()),
            BinaryResultValue::Text("ab".to_owned()),
        ]
    );
}

/// `CASE col WHEN v` compares the way `col = v` does in a `WHERE`, without
/// regard to case under MySQL's default collation.
#[test]
fn a_case_comparing_its_operand_compares_as_a_where_does() {
    let (_directory, mut adapter) = adapter();
    let text = result(
        &mut adapter,
        "SELECT CASE status WHEN 'ACTIVE' THEN 'yes' ELSE 'no' END AS a, CASE age WHEN 30 THEN 'thirty' WHEN 40 THEN 'forty' END AS b FROM users",
    );
    assert_eq!(
        shapes(&text),
        [
            (
                MYSQL_TYPE_VAR_STRING,
                12,
                NOT_FIXED_DECIMALS,
                MYSQL_NOT_NULL_FLAG
            ),
            (MYSQL_TYPE_VAR_STRING, 24, NOT_FIXED_DECIMALS, 0),
        ]
    );
    assert_eq!(
        rows(&text),
        ["yes thirty", "no NULL", "yes NULL", "yes forty"]
    );
}

/// `IFNULL` and `COALESCE` over columns answer what a `CASE` over the same
/// columns answers, NOT NULL when any one column is — and, unlike a `CASE`,
/// a `DECIMAL` answer brings every value to its scale.
#[test]
fn ifnull_and_coalesce_fall_one_column_back_onto_another() {
    let (_directory, mut adapter) = adapter();
    let sql = "SELECT COALESCE(score, age) AS f1, IFNULL(name, email) AS t1, IFNULL(nick, email) AS t2, COALESCE(nick, code) AS t3, COALESCE(age, small) AS i1, COALESCE(age, big) AS i2, COALESCE(tiny, tiny) AS i3, COALESCE(fee, balance) AS d1, IFNULL(age, balance) AS d2, COALESCE(nick, code, name) AS t4 FROM users";
    let text = result(&mut adapter, sql);
    assert_eq!(
        shapes(&text),
        [
            (MYSQL_TYPE_DOUBLE, 23, NOT_FIXED_DECIMALS, NUMBER),
            (
                MYSQL_TYPE_VAR_STRING,
                764,
                NOT_FIXED_DECIMALS,
                MYSQL_NOT_NULL_FLAG
            ),
            (
                MYSQL_TYPE_VAR_STRING,
                764,
                NOT_FIXED_DECIMALS,
                MYSQL_NOT_NULL_FLAG
            ),
            (MYSQL_TYPE_VAR_STRING, 120, NOT_FIXED_DECIMALS, 0),
            (MYSQL_TYPE_LONG, 11, 0, NOT_NULL_NUMBER),
            (MYSQL_TYPE_LONGLONG, 20, 0, NUMBER),
            (MYSQL_TYPE_TINY, 4, 0, NUMBER),
            (MYSQL_TYPE_NEWDECIMAL, 13, 3, NOT_NULL_NUMBER),
            (MYSQL_TYPE_NEWDECIMAL, 14, 2, NOT_NULL_NUMBER),
            (
                MYSQL_TYPE_VAR_STRING,
                400,
                NOT_FIXED_DECIMALS,
                MYSQL_NOT_NULL_FLAG
            ),
        ]
    );
    assert_eq!(
        rows(&text),
        [
            "1.5 alice al al 30 30 5 1.125 30.00 al",
            "NULL bob b@x NULL -2 NULL NULL 0.000 0.00 bob",
            "2.25 carol cc cc 17 17 NULL 2.500 17.00 cc",
            "40 dave d@x NULL 40 40 NULL 7.000 40.00 dave",
        ]
    );
    let binary = prepared(&mut adapter, sql);
    assert_eq!(binary.rows[3][0], BinaryResultValue::Real(40.0));
    assert_eq!(
        binary.rows[1][7],
        BinaryResultValue::Text("0.000".to_owned())
    );
}

/// Measured on MySQL 8.4.11, a whole number falling back onto a `DOUBLE` or a
/// `FLOAT` answers the column's kind at a length of 23, the fallback row
/// included: `IFNULL(score, 0)` reads `0` as a `DOUBLE`. A day, a moment or a
/// JSON document answers a type of its own, which is refused.
#[test]
fn a_whole_number_falls_back_onto_a_real_column_as_a_real() {
    let (_directory, mut adapter) = adapter();
    for sql in [
        "CREATE TABLE readings (id INT NOT NULL PRIMARY KEY, f FLOAT NULL, at DATETIME NULL, day DATE NULL, doc JSON NULL)",
        "INSERT INTO readings (id, f, at, day, doc) VALUES (1, 2.5, '2026-01-02 03:04:05', '2026-01-02', '[1]'), (2, NULL, NULL, NULL, NULL)",
    ] {
        adapter
            .execute_query(sql)
            .unwrap_or_else(|error| panic!("{sql}: {error:?}"));
    }
    let sql = "SELECT IFNULL(score, 0) AS s, COALESCE(score, 0) AS c FROM users ORDER BY id";
    let text = result(&mut adapter, sql);
    assert_eq!(
        shapes(&text),
        [(MYSQL_TYPE_DOUBLE, 23, NOT_FIXED_DECIMALS, NOT_NULL_NUMBER); 2]
    );
    assert_eq!(rows(&text), ["1.5 1.5", "0 0", "2.25 2.25", "0 0"]);
    let binary = prepared(&mut adapter, sql);
    assert_eq!(binary.rows[1][0], BinaryResultValue::Real(0.0));

    let sql = "SELECT IFNULL(f, 0) FROM readings ORDER BY id";
    let text = result(&mut adapter, sql);
    assert_eq!(
        shapes(&text),
        [(MYSQL_TYPE_FLOAT, 23, NOT_FIXED_DECIMALS, NOT_NULL_NUMBER)]
    );
    assert_eq!(rows(&text), ["2.5", "0"]);
    assert_eq!(
        prepared(&mut adapter, sql).rows[1][0],
        BinaryResultValue::Real(0.0)
    );

    for sql in [
        "SELECT IFNULL(at, 0) FROM readings",
        "SELECT IFNULL(day, 0) FROM readings",
        "SELECT IFNULL(doc, 0) FROM readings",
    ] {
        assert!(adapter.execute_query(sql).is_err(), "{sql}");
    }
}

/// `SUM(CASE WHEN ... THEN 1 ELSE 0 END)` is how a report counts the rows
/// meeting a condition. `SUM` and `AVG` answer a NEWDECIMAL over whole
/// numbers, at the `CASE`'s scale over a `DECIMAL` whatever the rows held,
/// and a `DOUBLE` over a `DOUBLE`; `COUNT` answers what any `COUNT` does.
#[test]
fn aggregates_over_a_case_count_and_total_the_rows_meeting_a_condition() {
    let (_directory, mut adapter) = adapter();
    let sql = "SELECT SUM(CASE WHEN status = 'active' THEN 1 ELSE 0 END) AS s1, SUM(CASE WHEN status = 'active' THEN balance ELSE 0 END) AS s2, SUM(CASE WHEN id > 100 THEN balance ELSE 0 END) AS s3, SUM(CASE WHEN status = 'active' THEN fee ELSE balance END) AS s4, COUNT(CASE WHEN age IS NULL THEN 1 END) AS c1, AVG(CASE WHEN status = 'active' THEN 1 ELSE 0 END) AS a1, AVG(IF(status = 'active', balance, 0)) AS a2, MAX(CASE WHEN status = 'active' THEN age ELSE 0 END) AS m1, SUM(CASE WHEN status = 'active' THEN score ELSE 0 END) AS f1, SUM(CASE WHEN id > 100 THEN balance END) AS n1 FROM users";
    let text = result(&mut adapter, sql);
    assert_eq!(
        shapes(&text),
        [
            (MYSQL_TYPE_NEWDECIMAL, 24, 0, NUMBER),
            (MYSQL_TYPE_NEWDECIMAL, 34, 2, NUMBER),
            (MYSQL_TYPE_NEWDECIMAL, 34, 2, NUMBER),
            (MYSQL_TYPE_NEWDECIMAL, 35, 3, NUMBER),
            (MYSQL_TYPE_LONGLONG, 21, 0, NOT_NULL_NUMBER),
            (MYSQL_TYPE_NEWDECIMAL, 7, 4, NUMBER),
            (MYSQL_TYPE_NEWDECIMAL, 16, 6, NUMBER),
            (MYSQL_TYPE_LONGLONG, 11, 0, NUMBER),
            (MYSQL_TYPE_DOUBLE, 23, NOT_FIXED_DECIMALS, NUMBER),
            (MYSQL_TYPE_NEWDECIMAL, 34, 2, NUMBER),
        ]
    );
    assert_eq!(
        rows(&text),
        ["3 20.75 0.00 3.625 1 0.7500 5.187500 40 3.75 NULL"]
    );
    assert_eq!(
        prepared(&mut adapter, sql).rows[0],
        [
            BinaryResultValue::Text("3".to_owned()),
            BinaryResultValue::Text("20.75".to_owned()),
            BinaryResultValue::Text("0.00".to_owned()),
            BinaryResultValue::Text("3.625".to_owned()),
            BinaryResultValue::Integer(1),
            BinaryResultValue::Text("0.7500".to_owned()),
            BinaryResultValue::Text("5.187500".to_owned()),
            BinaryResultValue::Integer(40),
            BinaryResultValue::Real(3.75),
            BinaryResultValue::Null,
        ]
    );

    // Grouped, and ordered by the count a group holds.
    let text = result(
        &mut adapter,
        "SELECT status, SUM(CASE WHEN age > 18 THEN 1 ELSE 0 END) AS adults, COUNT(CASE WHEN age IS NULL THEN 1 END) AS unknown, SUM(IF(age > 18, balance, 0)) AS adult_balance FROM users GROUP BY status ORDER BY adults DESC",
    );
    assert_eq!(rows(&text), ["active 2 0 17.50", "inactive 0 1 0.00"]);
}

/// What MySQL answers by a rule this has not measured stays refused.
#[test]
fn conditionals_mysql_answers_by_an_unmeasured_rule_are_refused() {
    let (_directory, mut adapter) = adapter();
    for sql in [
        // A word beside a number is a coercion.
        "SELECT CASE WHEN id > 1 THEN name ELSE 0 END FROM users",
        "SELECT COALESCE(name, age) FROM users",
        // An unsigned column beside a signed one answers a type of its own.
        "SELECT CASE WHEN id > 1 THEN utiny ELSE tiny END FROM users",
        "SELECT CASE WHEN id > 1 THEN id ELSE 0 END FROM users",
        // A TEXT answers a width of its own.
        "SELECT CASE WHEN id > 1 THEN bio ELSE name END FROM users",
        // The engine would compare written decimals and words as words.
        "SELECT MIN(CASE WHEN status = 'active' THEN balance ELSE 0 END) FROM users",
        "SELECT MAX(CASE WHEN age > 18 THEN name ELSE 'minor' END) FROM users",
        "SELECT status, AVG(CASE WHEN age > 18 THEN 1 ELSE 0 END) AS a FROM users GROUP BY status ORDER BY a",
        // `CASE col WHEN` over values of two kinds compares by one rule
        // chosen over all of them together.
        "SELECT CASE age WHEN 30 THEN 'x' WHEN '40' THEN 'y' END FROM users",
        // A column read through a join has no single table to take its type
        // from.
        "SELECT CASE WHEN u.age > 18 THEN u.balance ELSE 0 END FROM users u JOIN users v ON u.id = v.id",
    ] {
        assert!(adapter.execute_query(sql).is_err(), "{sql}");
    }
    // Written into a column, a fallback onto another column has not been
    // measured.
    assert!(adapter
        .execute_query("UPDATE users SET nick = COALESCE(nick, name)")
        .is_err());
}
