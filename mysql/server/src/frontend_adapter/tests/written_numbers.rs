//! A word naming a number compared against a column holding numbers, which
//! is how PHP applications write every value: WordPress's
//! `$wpdb->prepare('%s')` and PDO with emulated prepares send `WHERE id =
//! '1'`, and Laravel binds what it reads from a request as a word.
//!
//! Every expectation here was measured on MySQL 8.4.11.

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
        "CREATE TABLE users (id BIGINT UNSIGNED NOT NULL AUTO_INCREMENT PRIMARY KEY, name VARCHAR(100) NOT NULL, age INT NULL, small SMALLINT NOT NULL DEFAULT 0, big BIGINT NULL, balance DECIMAL(10,2) NOT NULL DEFAULT '0.00', score DOUBLE NULL, created_at DATETIME NULL)",
        "INSERT INTO users (name, age, small, big, balance, score) VALUES ('alice', 30, 3, 9007199254740993, 10.50, 1.5)",
        "INSERT INTO users (name, age, small, big, balance, score) VALUES ('bob', NULL, -2, NULL, 0, NULL)",
        "INSERT INTO users (name, age, small, big, balance, score) VALUES ('carol', 17, 7, 5, 3.25, 2.25)",
        "INSERT INTO users (name, age, small, big, balance, score) VALUES ('dave', 40, 0, NULL, 7, NULL)",
        "UPDATE users SET created_at = '2026-01-02 03:04:05' WHERE name = 'alice'",
        "UPDATE users SET created_at = '2025-06-01 00:00:00' WHERE name = 'carol'",
    ] {
        adapter
            .execute_query(sql)
            .unwrap_or_else(|error| panic!("{sql}: {error:?}"));
    }
    (directory, adapter)
}

/// The names a statement finds, in the order it answers them.
fn names(
    adapter: &mut AuthorizedDatabaseCommandAdapter<RecordingAuthorizer>,
    sql: &str,
) -> Vec<String> {
    let Ok(CommandExecutionResult::ResultSet(result)) = adapter.execute_query(sql) else {
        panic!("{sql} must return a result set");
    };
    result
        .rows
        .into_iter()
        .map(|row| String::from_utf8(row[0].clone().unwrap()).unwrap())
        .collect()
}

#[test]
fn a_word_naming_a_whole_number_finds_what_the_number_finds() {
    let (_directory, mut adapter) = adapter();
    for (sql, found) in [
        (
            "SELECT name FROM users WHERE id = '1' ORDER BY id",
            &["alice"][..],
        ),
        (
            "SELECT name FROM users WHERE age = '30' ORDER BY id",
            &["alice"],
        ),
        (
            "SELECT name FROM users WHERE id IN ('1', '2') ORDER BY id",
            &["alice", "bob"],
        ),
        (
            "SELECT name FROM users WHERE age NOT IN ('30', '17') ORDER BY id",
            &["dave"],
        ),
        (
            "SELECT name FROM users WHERE age <> '30' ORDER BY id",
            &["carol", "dave"],
        ),
        (
            "SELECT name FROM users WHERE age BETWEEN '18' AND '35' ORDER BY id",
            &["alice"],
        ),
        // A sign and leading zeroes name the same number.
        (
            "SELECT name FROM users WHERE small = '-0' ORDER BY id",
            &["dave"],
        ),
        (
            "SELECT name FROM users WHERE small = '+3' ORDER BY id",
            &["alice"],
        ),
        (
            "SELECT name FROM users WHERE id = '01' ORDER BY id",
            &["alice"],
        ),
        // Exact, not a comparison between doubles, which would find both.
        (
            "SELECT name FROM users WHERE big = '9007199254740993' ORDER BY id",
            &["alice"],
        ),
        (
            "SELECT name FROM users WHERE big = '9007199254740992' ORDER BY id",
            &[],
        ),
        (
            "SELECT name FROM users WHERE big IN ('9007199254740992') ORDER BY id",
            &[],
        ),
        // A number no INT holds finds nothing, and lies above every row.
        (
            "SELECT name FROM users WHERE age = '99999999999' ORDER BY id",
            &[],
        ),
        (
            "SELECT name FROM users WHERE age < '99999999999' ORDER BY id",
            &["alice", "carol", "dave"],
        ),
        // Laravel's `whereYear`.
        (
            "SELECT name FROM users WHERE year(created_at) = '2026' ORDER BY id",
            &["alice"],
        ),
    ] {
        assert_eq!(names(&mut adapter, sql), found, "{sql}");
    }
}

#[test]
fn a_word_naming_a_decimal_is_compared_exactly_against_a_decimal() {
    let (_directory, mut adapter) = adapter();
    for (sql, found) in [
        (
            "SELECT name FROM users WHERE balance > '0' ORDER BY id",
            &["alice", "carol", "dave"][..],
        ),
        (
            "SELECT name FROM users WHERE balance = '10.5' ORDER BY id",
            &["alice"],
        ),
        (
            "SELECT name FROM users WHERE balance = '007.00' ORDER BY id",
            &["dave"],
        ),
        (
            "SELECT name FROM users WHERE balance = '10.500000000000000001' ORDER BY id",
            &[],
        ),
        (
            "SELECT name FROM users WHERE balance > '10.499999999999999999' ORDER BY id",
            &["alice"],
        ),
        (
            "SELECT name FROM users WHERE balance IN ('10.5', '7') ORDER BY id",
            &["alice", "dave"],
        ),
        (
            "SELECT name FROM users WHERE balance BETWEEN '3.25' AND '7.0' ORDER BY id",
            &["carol", "dave"],
        ),
    ] {
        assert_eq!(names(&mut adapter, sql), found, "{sql}");
    }
}

/// A comparison standing as a result column reads the word the same way.
#[test]
fn a_comparison_answered_as_a_column_reads_the_word_as_its_number() {
    let (_directory, mut adapter) = adapter();
    let Ok(CommandExecutionResult::ResultSet(result)) = adapter.execute_query(
        "SELECT age > '18' AS a, age > 18 AS b, big = '9007199254740992' AS c, balance > '5' AS d FROM users ORDER BY id",
    ) else {
        panic!("the comparisons must answer a result set");
    };
    let nullable = MYSQL_BINARY_FLAG | MYSQL_NUM_FLAG;
    assert_eq!(
        result
            .columns
            .iter()
            .map(|column| (column.column_type, column.column_length, column.flags))
            .collect::<Vec<_>>(),
        [
            (MYSQL_TYPE_LONGLONG, 1, nullable),
            (MYSQL_TYPE_LONGLONG, 1, nullable),
            (MYSQL_TYPE_LONGLONG, 1, nullable),
            (MYSQL_TYPE_LONGLONG, 1, nullable | MYSQL_NOT_NULL_FLAG),
        ]
    );
    assert_eq!(
        result
            .rows
            .iter()
            .map(|row| row
                .iter()
                .map(|value| value.as_ref().map_or("NULL".to_owned(), |value| {
                    String::from_utf8(value.clone()).unwrap()
                }))
                .collect::<Vec<_>>()
                .join(" "))
            .collect::<Vec<_>>(),
        ["1 1 0 1", "NULL NULL NULL 0", "0 0 0 0", "1 1 NULL 1"]
    );
}

/// An `UPDATE` and a `DELETE` read the word the way a `SELECT` does.
#[test]
fn a_write_reads_a_word_naming_a_number_as_that_number() {
    let (_directory, mut adapter) = adapter();
    let Ok(CommandExecutionResult::Ok(updated)) =
        adapter.execute_query("UPDATE users SET name = 'alicia' WHERE id = '1'")
    else {
        panic!("the UPDATE must answer OK");
    };
    assert_eq!(updated.affected_rows, 1);
    let Ok(CommandExecutionResult::Ok(deleted)) =
        adapter.execute_query("DELETE FROM users WHERE balance < '5' AND age = '17'")
    else {
        panic!("the DELETE must answer OK");
    };
    assert_eq!(deleted.affected_rows, 1);
    assert_eq!(
        names(&mut adapter, "SELECT name FROM users ORDER BY id"),
        ["alicia", "bob", "dave"]
    );
}

/// MySQL reads each of these by a rule of its own — a comparison between
/// doubles, a word it reads with warning 1292, or a number this does not
/// spell out — so each stays refused.
#[test]
fn a_word_mysql_reads_by_another_rule_is_refused() {
    let (_directory, mut adapter) = adapter();
    for sql in [
        "SELECT name FROM users WHERE age = '1.5'",
        "SELECT name FROM users WHERE age = '30.0'",
        "SELECT name FROM users WHERE age = ' 30'",
        "SELECT name FROM users WHERE age = '30abc'",
        "SELECT name FROM users WHERE age = 'abc'",
        "SELECT name FROM users WHERE age = '3e1'",
        "SELECT name FROM users WHERE small = ''",
        "SELECT name FROM users WHERE big < '9223372036854775808'",
        "SELECT name FROM users WHERE id = '18446744073709551615'",
        "SELECT name FROM users WHERE score = '1.5'",
        "SELECT name FROM users WHERE balance = '.5'",
        "SELECT name FROM users WHERE age = '30' COLLATE utf8mb4_bin",
        "UPDATE users SET name = 'x' WHERE age = '30abc'",
    ] {
        assert!(adapter.execute_query(sql).is_err(), "{sql}");
    }
}

/// The null bitmap, the new-parameters flag and one VAR_STRING parameter for
/// each word, which is what PDO sends for a value bound as a string.
fn bound_words(words: &[&str]) -> Vec<u8> {
    let mut payload = vec![0; words.len().div_ceil(8)];
    payload.push(1);
    for _ in words {
        payload.extend_from_slice(&[MYSQL_TYPE_VAR_STRING, 0]);
    }
    for word in words {
        payload.push(u8::try_from(word.len()).unwrap());
        payload.extend_from_slice(word.as_bytes());
    }
    payload
}

fn bound_names(
    adapter: &mut AuthorizedDatabaseCommandAdapter<RecordingAuthorizer>,
    sql: &str,
    words: &[&str],
) -> Result<Vec<String>, FrontendErrorKind> {
    let statement = adapter
        .execute_stmt_prepare(sql)
        .unwrap_or_else(|error| panic!("prepare {sql}: {error:?}"));
    let result = adapter.execute_stmt_execute(statement.statement_id, &bound_words(words));
    adapter.execute_stmt_close(statement.statement_id);
    Ok(prepared_result_set(result?)
        .rows
        .into_iter()
        .map(|row| match &row[0] {
            BinaryResultValue::Text(name) => name.clone(),
            other => panic!("{sql} answered {other:?}"),
        })
        .collect())
}

/// A word bound against a column holding whole numbers is read as the number
/// it names, as MySQL reads it.
#[test]
fn a_bound_word_naming_a_whole_number_finds_what_the_number_finds() {
    let (_directory, mut adapter) = adapter();
    for (sql, words, found) in [
        (
            "SELECT name FROM users WHERE id = ? ORDER BY id",
            &["1"][..],
            &["alice"][..],
        ),
        (
            "SELECT name FROM users WHERE age = ? ORDER BY id",
            &["30"],
            &["alice"],
        ),
        (
            "SELECT name FROM users WHERE age IN (?, ?) ORDER BY id",
            &["30", "17"],
            &["alice", "carol"],
        ),
        (
            "SELECT name FROM users WHERE big = ? ORDER BY id",
            &["9007199254740993"],
            &["alice"],
        ),
        (
            "SELECT name FROM users WHERE big = ? ORDER BY id",
            &["9007199254740992"],
            &[],
        ),
        (
            "SELECT name FROM users WHERE balance > ? ORDER BY id",
            &["0"],
            &["alice", "carol", "dave"],
        ),
    ] {
        assert_eq!(
            bound_names(&mut adapter, sql, words).unwrap(),
            found,
            "{sql} {words:?}"
        );
    }
    // MySQL reads these with warning 1292, or by a rule of its own.
    for word in ["30abc", "1.5", " 30"] {
        assert!(
            bound_names(
                &mut adapter,
                "SELECT name FROM users WHERE age = ?",
                &[word]
            )
            .is_err(),
            "{word:?}"
        );
    }
}
