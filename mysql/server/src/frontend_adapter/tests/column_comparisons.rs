//! A column or a call compared with another column or call in a `WHERE` or a
//! join's `ON`.
//!
//! Every expectation here was measured on MySQL 8.4.11.

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
    for sql in [
        "CREATE TABLE users (id BIGINT UNSIGNED AUTO_INCREMENT PRIMARY KEY, name VARCHAR(100) NOT NULL, email VARCHAR(191) NOT NULL, age INT NULL, score DOUBLE NULL, created_at DATETIME NULL)",
        "INSERT INTO users (name, email, age, score, created_at) VALUES ('Ann', 'ann', 30, 29.5, '2024-01-01 10:00:00'), ('Bob', 'bob@x', NULL, 1.5, NULL), ('cat', 'CAT', 10, 10, '2023-05-05 00:00:00'), ('Dan', 'dan', 5, 5.0000001, '2024-01-01'), ('é', 'E', 11, NULL, NULL)",
        "CREATE TABLE words (id INT PRIMARY KEY, a VARCHAR(10), b VARCHAR(10) COLLATE utf8mb4_bin, c VARCHAR(10) COLLATE utf8mb4_unicode_ci, d VARCHAR(10), t TEXT, ch CHAR(5), b2 VARCHAR(10) COLLATE utf8mb4_bin, c2 VARCHAR(20) COLLATE utf8mb4_unicode_ci)",
        "INSERT INTO words VALUES (1, 'a', 'A', 'a', 'A', 'a', 'a ', 'a', 'A'), (2, 'ss', 'ß', 'ß', 'ß', 'ss', 'ss', 'ss', 'ss'), (3, 'a ', 'a', 'a ', 'a', 'a ', 'a', 'a ', 'a')",
        "CREATE TABLE pairs (id INT PRIMARY KEY, i INT, j INT, b BIGINT, b2 BIGINT, u BIGINT UNSIGNED, u2 BIGINT UNSIGNED, r DOUBLE, d DATE, d2 DATE, dt DATETIME, dt2 DATETIME, dt3 DATETIME(3), ts TIMESTAMP NULL, ts2 TIMESTAMP NULL, tm TIME, tm2 TIME, y YEAR, y2 YEAR, v VARCHAR(20))",
        "INSERT INTO pairs VALUES (1, 1, 2, 9007199254740993, 9007199254740992, 18446744073709551615, 1, 9007199254740992, '2024-01-01', '2024-01-02', '2024-01-01 00:00:00', '2024-01-01 00:00:00', '2024-01-01 00:00:00.000', '2024-01-01 00:00:00', '2024-01-01 00:00:01', '-01:00:00', '100:00:00', 2024, 2023, '1'), (2, 2, 2, -1, 5, 5, 5, 2.0, '2024-01-02', '2024-01-02', '2024-01-01 10:00:00', '2024-01-01 09:00:00', '2024-01-01 10:00:00.500', '2024-01-01 10:00:00', '2024-01-01 10:00:00', '10:00:00', '10:00:00', 2024, 2024, '2abc'), (3, NULL, 3, 3, 3, 0, 18446744073709551615, NULL, NULL, '2024-01-01', NULL, '2024-01-01 00:00:00', NULL, NULL, NULL, NULL, NULL, NULL, NULL, ' 3')",
        "CREATE TABLE posts (id INT PRIMARY KEY, user_id INT, n INT)",
        "INSERT INTO posts VALUES (1, 1, 5), (2, 1, 50), (3, 2, 1)",
        "CREATE TABLE ranks (id INT PRIMARY KEY, n INT)",
        "INSERT INTO ranks VALUES (1, 4), (2, 10)",
        "CREATE TABLE people (id INT PRIMARY KEY, name VARCHAR(100) NOT NULL, email VARCHAR(191) NOT NULL, nick TEXT, b VARCHAR(10) COLLATE utf8mb4_bin, d DATE, dt DATETIME)",
        "INSERT INTO people (id, name, email, nick, b, d, dt) VALUES (1, 'Ann', 'ann', 'ANN', 'ann', '2024-01-01', '2024-01-01 00:00:00'), (2, 'Bob', 'bob@x', 'bobby', 'Bob', '2024-01-02', '2024-01-01 10:00:00'), (3, 'cat', 'CAT', NULL, 'x', '2023-05-05', '2023-05-05 00:00:00'), (4, 'Dan', 'dan', 'dan ', 'dan', '2024-01-01', NULL)",
        "CREATE TABLE amounts (id INT PRIMARY KEY, p DECIMAL(30,20), q DECIMAL(30,20), n INT, s DECIMAL(5,2))",
        "INSERT INTO amounts VALUES (1, 1.00000000000000000001, 1.00000000000000000002, 1, 1.00), (2, 1.00000000000000000000, 1.0, 1, 1.01), (3, 12345678.00000000000001, 12345678.00000000000001, 12345678, 0.10)",
    ] {
        adapter
            .execute_query(sql)
            .unwrap_or_else(|error| panic!("{sql}: {error:?}"));
    }
    (directory, adapter)
}

/// Each row of the result, its values joined by `|`.
fn rows(adapter: &mut Adapter, sql: &str) -> Vec<String> {
    let Ok(CommandExecutionResult::ResultSet(result)) = adapter.execute_query(sql) else {
        panic!("{sql} must return a result set");
    };
    result
        .rows
        .into_iter()
        .map(|row| {
            row.into_iter()
                .map(|value| {
                    value.map_or("NULL".to_owned(), |value| String::from_utf8(value).unwrap())
                })
                .collect::<Vec<_>>()
                .join("|")
        })
        .collect()
}

fn affected(adapter: &mut Adapter, sql: &str) -> u64 {
    match adapter.execute_query(sql) {
        Ok(CommandExecutionResult::Ok(result)) => result.affected_rows,
        other => panic!("{sql}: {other:?}"),
    }
}

#[test]
fn two_words_compare_under_the_collation_both_columns_carry() {
    let (_directory, mut adapter) = adapter();
    for (sql, expected) in [
        // Case and accents are folded under utf8mb4_0900_ai_ci: 'Dan' is
        // 'dan' and 'é' is 'E'.
        (
            "SELECT id FROM users WHERE name = email ORDER BY id",
            &["1", "3", "4", "5"][..],
        ),
        (
            "SELECT id FROM users WHERE name < email ORDER BY id",
            &["2"],
        ),
        (
            "SELECT id FROM users u WHERE u.name = u.email ORDER BY id",
            &["1", "3", "4", "5"],
        ),
        (
            "SELECT id FROM users WHERE name <> email ORDER BY id",
            &["2"],
        ),
        // utf8mb4_0900_ai_ci does not pad: 'a ' is not 'a'. 'ss' is 'ß'.
        ("SELECT id FROM words WHERE a = d ORDER BY id", &["1", "2"]),
        ("SELECT id FROM words WHERE a < d ORDER BY id", &[]),
        (
            "SELECT id FROM words WHERE a = t ORDER BY id",
            &["1", "2", "3"],
        ),
        // utf8mb4_bin pads with spaces and keeps case: only 'a' and 'a '.
        ("SELECT id FROM words WHERE b = b2 ORDER BY id", &["3"]),
        // utf8mb4_unicode_ci pads and folds.
        (
            "SELECT id FROM words WHERE c = c2 ORDER BY id",
            &["1", "2", "3"],
        ),
    ] {
        assert_eq!(rows(&mut adapter, sql), expected, "{sql}");
    }
}

#[test]
fn two_numbers_or_two_moments_of_one_kind_compare_as_they_are_stored() {
    let (_directory, mut adapter) = adapter();
    for (sql, expected) in [
        (
            "SELECT id FROM users WHERE age > score ORDER BY id",
            &["1"][..],
        ),
        ("SELECT id FROM users WHERE age = score ORDER BY id", &["3"]),
        (
            "SELECT id FROM users WHERE age <=> score ORDER BY id",
            &["3"],
        ),
        ("SELECT id FROM pairs WHERE i < j ORDER BY id", &["1"]),
        ("SELECT id FROM pairs WHERE i <=> j ORDER BY id", &["2"]),
        ("SELECT id FROM pairs WHERE b > b2 ORDER BY id", &["1"]),
        ("SELECT id FROM pairs WHERE r > i ORDER BY id", &["1"]),
        ("SELECT id FROM pairs WHERE u > u2 ORDER BY id", &["1"]),
        ("SELECT id FROM pairs WHERE u = u2 ORDER BY id", &["2"]),
        // A signed and an unsigned BIGINT compare exactly in both.
        ("SELECT id FROM pairs WHERE b < u ORDER BY id", &["1", "2"]),
        ("SELECT id FROM pairs WHERE d < d2 ORDER BY id", &["1"]),
        ("SELECT id FROM pairs WHERE dt > dt2 ORDER BY id", &["2"]),
        ("SELECT id FROM pairs WHERE dt = dt2 ORDER BY id", &["1"]),
        ("SELECT id FROM pairs WHERE ts < ts2 ORDER BY id", &["1"]),
        ("SELECT id FROM pairs WHERE tm = tm2 ORDER BY id", &["2"]),
        ("SELECT id FROM pairs WHERE y > y2 ORDER BY id", &["1"]),
        (
            "SELECT id FROM pairs WHERE i BETWEEN i AND j ORDER BY id",
            &["1", "2"],
        ),
        // Two DECIMALs compare exactly, whatever their scales.
        (
            "SELECT id FROM amounts WHERE p = q ORDER BY id",
            &["2", "3"],
        ),
        ("SELECT id FROM amounts WHERE p < q ORDER BY id", &["1"]),
        ("SELECT id FROM amounts WHERE s > p ORDER BY id", &["2"]),
    ] {
        assert_eq!(rows(&mut adapter, sql), expected, "{sql}");
    }
}

/// MySQL converts one side of each of these to the other's type, where the
/// engine compares them as they are stored.
#[test]
fn a_pair_mysql_converts_before_comparing_is_refused() {
    let (_directory, mut adapter) = adapter();
    for sql in [
        // Measured: 1267, "Illegal mix of collations".
        "SELECT id FROM words WHERE a = c",
        // Measured: compared under utf8mb4_bin, finding row 3.
        "SELECT id FROM words WHERE a = b",
        // Measured: MySQL takes the trailing spaces off a CHAR.
        "SELECT id FROM words WHERE a = ch",
        // Measured: a BIGINT of 9007199254740993 equals a DOUBLE of
        // 9007199254740992, both read as doubles.
        "SELECT id FROM pairs WHERE b = r",
        // Measured: a DATETIME(0) equals a DATETIME(3) holding the same
        // moment, and a DATETIME holding midnight equals the DATE of it.
        "SELECT id FROM pairs WHERE dt = dt3",
        "SELECT id FROM pairs WHERE dt = d",
        "SELECT id FROM pairs WHERE ts = dt",
        // Measured: an INT of 2 equals the word '2abc'.
        "SELECT id FROM pairs WHERE i = v",
        "SELECT p.id FROM pairs p, words w WHERE p.v = w.id",
        // Measured: a DECIMAL(30,20) of 1.00000000000000000001 is more than
        // an INT of 1, where the engine compares the two as doubles.
        "SELECT id FROM amounts WHERE p = n",
        "SELECT a.id FROM amounts a JOIN ranks r ON a.p = r.n",
        "SELECT p.id FROM pairs p JOIN words w ON p.v = w.id",
        // A span's stored form does not read in order.
        "SELECT id FROM pairs WHERE tm < tm2",
        "SELECT id FROM users WHERE name = email COLLATE utf8mb4_bin",
        "DELETE FROM pairs WHERE i = v",
        "UPDATE pairs SET j = 0 WHERE b = r",
    ] {
        assert!(adapter.execute_query(sql).is_err(), "{sql}");
        assert!(adapter.execute_stmt_prepare(sql).is_err(), "{sql}");
    }
    assert_eq!(
        rows(&mut adapter, "SELECT id, j FROM pairs ORDER BY id"),
        ["1|2", "2|2", "3|3"]
    );
}

#[test]
fn an_update_or_a_delete_reads_a_pair_the_way_a_select_does() {
    let (_directory, mut adapter) = adapter();
    assert_eq!(
        affected(&mut adapter, "UPDATE pairs SET j = 9 WHERE i < j"),
        1
    );
    assert_eq!(
        affected(&mut adapter, "DELETE FROM users WHERE name = email"),
        4
    );
    assert_eq!(rows(&mut adapter, "SELECT id FROM users"), ["2"]);
    assert_eq!(affected(&mut adapter, "DELETE FROM pairs WHERE b > b2"), 1);
    assert_eq!(
        rows(&mut adapter, "SELECT id, j FROM pairs ORDER BY id"),
        ["2|2", "3|3"]
    );
}

/// A join matching one column against another by anything but equality, which
/// a comma join says in its `WHERE`.
#[test]
fn a_join_matches_on_an_ordering_of_two_columns() {
    let (_directory, mut adapter) = adapter();
    for sql in [
        "SELECT p.id, r.id FROM posts p JOIN ranks r ON p.n > r.n ORDER BY p.id, r.id",
        "SELECT p.id, r.id FROM posts p, ranks r WHERE p.n > r.n ORDER BY p.id, r.id",
    ] {
        assert_eq!(rows(&mut adapter, sql), ["1|1", "2|1", "2|2"], "{sql}");
    }
    assert_eq!(
        rows(
            &mut adapter,
            "SELECT p.id FROM posts p, users u WHERE p.user_id = u.id ORDER BY p.id"
        ),
        ["1", "2", "3"]
    );
    assert!(adapter
        .execute_query("SELECT p.id FROM posts p JOIN users u ON p.n > u.name")
        .is_err());
}

#[test]
fn a_prepared_statement_compares_a_pair_beside_a_bound_value() {
    let (_directory, mut adapter) = adapter();
    let statement = adapter
        .execute_stmt_prepare("SELECT id FROM users WHERE name = email AND age > ? ORDER BY id")
        .unwrap();
    let mut payload = vec![0, 1, MYSQL_TYPE_LONGLONG, 0];
    payload.extend_from_slice(&10_i64.to_le_bytes());
    let PreparedStatementExecutionResult::ResultSet(result) = adapter
        .execute_stmt_execute(statement.statement_id, &payload)
        .unwrap()
    else {
        panic!("a SELECT must answer rows");
    };
    let ids = result
        .rows
        .iter()
        .map(|row| match row.as_slice() {
            [BinaryResultValue::UnsignedInteger(id)] => *id,
            other => panic!("unexpected row {other:?}"),
        })
        .collect::<Vec<u64>>();
    assert_eq!(ids, [1, 5]);

    let statement = adapter
        .execute_stmt_prepare("DELETE FROM pairs WHERE i < j")
        .unwrap();
    assert!(matches!(
        adapter.execute_stmt_execute(statement.statement_id, &[]),
        Ok(PreparedStatementExecutionResult::Ok(CommandOkResult {
            affected_rows: 1,
            ..
        }))
    ));
}

/// A call beside another call or a column, and a written word in another
/// case, which MySQL answers before it compares.
#[test]
fn a_call_meets_another_call_or_a_column_of_its_kind() {
    let (_directory, mut adapter) = adapter();
    for (sql, expected) in [
        (
            "SELECT id FROM people WHERE LOWER(name) = LOWER('ANN') ORDER BY id",
            &["1"][..],
        ),
        (
            "SELECT id FROM people WHERE UPPER(name) > UPPER('b') ORDER BY id",
            &["2", "3", "4"],
        ),
        (
            "SELECT id FROM people WHERE name = UPPER('ann') ORDER BY id",
            &["1"],
        ),
        // utf8mb4_bin keeps case, so the lowered word is not 'Bob'.
        (
            "SELECT id FROM people WHERE b = LOWER('BOB') ORDER BY id",
            &[],
        ),
        (
            "SELECT id FROM people WHERE LOWER(name) = UPPER(email) ORDER BY id",
            &["1", "3", "4"],
        ),
        (
            "SELECT id FROM people WHERE LOWER(name) < LOWER(email) ORDER BY id",
            &["2"],
        ),
        (
            "SELECT id FROM people WHERE LOWER(name) = LOWER(nick) ORDER BY id",
            &["1"],
        ),
        (
            "SELECT id FROM people WHERE CHAR_LENGTH(name) < CHAR_LENGTH(nick) ORDER BY id",
            &["2", "4"],
        ),
        (
            "SELECT id FROM people WHERE DATE(dt) = DATE(d) ORDER BY id",
            &["1", "3"],
        ),
        (
            "SELECT id FROM people WHERE YEAR(dt) = YEAR(d) ORDER BY id",
            &["1", "2", "3"],
        ),
        (
            "SELECT id FROM people WHERE LOWER(name) = email ORDER BY id",
            &["1", "3", "4"],
        ),
        (
            "SELECT id FROM people WHERE email = LOWER(name) ORDER BY id",
            &["1", "3", "4"],
        ),
        (
            "SELECT id FROM people WHERE email < UPPER(name) ORDER BY id",
            &[],
        ),
        (
            "SELECT id FROM people WHERE CHAR_LENGTH(name) = id ORDER BY id",
            &["3"],
        ),
        (
            "SELECT id FROM people WHERE id < CHAR_LENGTH(email) ORDER BY id",
            &["1", "2"],
        ),
        (
            "SELECT id FROM people WHERE DATE(dt) = d ORDER BY id",
            &["1", "3"],
        ),
    ] {
        assert_eq!(rows(&mut adapter, sql), expected, "{sql}");
    }
    for sql in [
        // Measured: compared under utf8mb4_bin, finding rows 1 and 4.
        "SELECT id FROM people WHERE LOWER(name) = LOWER(b)",
        "SELECT id FROM people WHERE LOWER(name) = b",
        // A word against a number, which MySQL reads as a number first.
        "SELECT id FROM people WHERE LOWER(name) = CHAR_LENGTH(email)",
        "SELECT id FROM people WHERE CHAR_LENGTH(name) = name",
        "SELECT id FROM people WHERE LOWER(name) = id",
        // MySQL changes the case of a word outside ASCII by Unicode rules.
        "SELECT id FROM people WHERE LOWER(name) = LOWER('\u{e9}')",
    ] {
        assert!(adapter.execute_query(sql).is_err(), "{sql}");
    }
    assert_eq!(
        affected(&mut adapter, "DELETE FROM people WHERE LOWER(name) = 'bob'"),
        1
    );
    assert_eq!(
        affected(
            &mut adapter,
            "UPDATE people SET nick = 'same' WHERE LOWER(name) = LOWER(email)"
        ),
        3
    );
    assert_eq!(
        affected(&mut adapter, "DELETE FROM people WHERE email = LOWER(name)"),
        3
    );
    assert!(rows(&mut adapter, "SELECT id FROM people").is_empty());
}
