//! A number with a fraction, or a word naming one, written into a column of
//! whole numbers.
//!
//! Every expectation here was measured on MySQL 8.4.11 in strict mode, the
//! bound values through mysql2, which binds a number as a `DOUBLE` and a word
//! as a `VAR_STRING`.

use super::*;

type Adapter = AuthorizedDatabaseCommandAdapter<RecordingAuthorizer>;

fn adapter() -> (tempfile::TempDir, Adapter) {
    let authorizer = Arc::new(RecordingAuthorizer::default());
    let (directory, _catalog, factory) = catalog_factory(authorizer);
    let mut adapter = factory
        .build(AuthenticatedPrincipal::from_account_id_for_testing(
            AccountId::from_bytes([191; 32]),
        ))
        .unwrap();
    adapter.authorize_connection().unwrap();
    adapter.execute_init_db("REPORTS").unwrap();
    (directory, adapter)
}

fn run(adapter: &mut Adapter, sql: &str) -> CommandOkResult {
    match adapter.execute_query(sql) {
        Ok(CommandExecutionResult::Ok(result)) => result,
        other => panic!("{sql} must answer OK, answered {other:?}"),
    }
}

fn rows(adapter: &mut Adapter, sql: &str) -> Vec<Vec<Option<String>>> {
    let Ok(CommandExecutionResult::ResultSet(result)) = adapter.execute_query(sql) else {
        panic!("{sql} must return a result set");
    };
    result
        .rows
        .into_iter()
        .map(|row| {
            row.into_iter()
                .map(|value| value.map(|value| String::from_utf8(value).unwrap()))
                .collect()
        })
        .collect()
}

fn column(adapter: &mut Adapter, sql: &str) -> Vec<Option<String>> {
    rows(adapter, sql)
        .into_iter()
        .map(|mut row| row.remove(0))
        .collect()
}

fn whole(numbers: &[i64]) -> Vec<Option<String>> {
    numbers
        .iter()
        .map(|number| Some(number.to_string()))
        .collect()
}

/// One value as mysql2 binds it.
enum Bound<'a> {
    Real(f64),
    Word(&'a str),
}

fn prepared(
    adapter: &mut Adapter,
    sql: &str,
    values: &[Bound<'_>],
) -> Result<PreparedStatementExecutionResult, FrontendErrorKind> {
    let mut payload = vec![0; values.len().div_ceil(8)];
    payload.push(1);
    for value in values {
        let code = match value {
            Bound::Real(_) => MYSQL_TYPE_DOUBLE,
            Bound::Word(_) => MYSQL_TYPE_VAR_STRING,
        };
        payload.extend_from_slice(&[code, 0]);
    }
    for value in values {
        match value {
            Bound::Real(number) => payload.extend_from_slice(&number.to_le_bytes()),
            Bound::Word(word) => {
                payload.push(u8::try_from(word.len()).unwrap());
                payload.extend_from_slice(word.as_bytes());
            }
        }
    }
    let statement = adapter.execute_stmt_prepare(sql)?;
    let result = adapter.execute_stmt_execute(statement.statement_id, &payload);
    adapter.execute_stmt_close(statement.statement_id);
    result
}

/// A number written with a point, or a word naming one, is rounded half away
/// from zero, with no warning, and only then held to the column's range.
#[test]
fn a_fraction_is_rounded_half_away_from_zero_into_a_whole_number_column() {
    let (_directory, mut adapter) = adapter();
    run(
        &mut adapter,
        "CREATE TABLE n (id INT NOT NULL AUTO_INCREMENT PRIMARY KEY, i INT, t TINYINT, tu TINYINT UNSIGNED, iu INT UNSIGNED, b BIGINT)",
    );
    for value in [
        "1.5", "2.5", "-0.5", "-1.5", "0.49", "'2.5'", "'3.5'", "'-0.5'", "' 7.5 '", "' 7 '",
        "'.5'", "'5.'", "'+5'", "'1e3'", "'2.5e0'", "'1.4'",
    ] {
        let sql = format!("INSERT INTO n (i) VALUES ({value})");
        assert_eq!(run(&mut adapter, &sql).warnings, 0, "{sql}");
    }
    assert_eq!(
        column(&mut adapter, "SELECT i FROM n ORDER BY id"),
        whole(&[2, 3, -1, -2, 0, 3, 4, -1, 8, 7, 1, 5, 5, 1000, 3, 1])
    );

    run(
        &mut adapter,
        "INSERT INTO n (t, tu, iu) VALUES (127.4, 255.4, '-0.4')",
    );
    assert_eq!(
        rows(&mut adapter, "SELECT t, tu, iu FROM n WHERE t IS NOT NULL"),
        vec![whole(&[127, 255, 0])]
    );
    for (sql, refused) in [
        (
            "INSERT INTO n (t) VALUES (127.5)",
            FrontendErrorKind::OutOfRange,
        ),
        (
            "INSERT INTO n (i) VALUES (2147483647.5)",
            FrontendErrorKind::OutOfRange,
        ),
        (
            "INSERT INTO n (i) VALUES ('2147483647.5')",
            FrontendErrorKind::OutOfRange,
        ),
        (
            "INSERT INTO n (iu) VALUES ('-0.5')",
            FrontendErrorKind::OutOfRange,
        ),
        (
            "INSERT INTO n (b) VALUES (9223372036854775808)",
            FrontendErrorKind::OutOfRange,
        ),
        (
            "INSERT INTO n (i) VALUES ('')",
            FrontendErrorKind::IncorrectValue,
        ),
        (
            "INSERT INTO n (i) VALUES ('abc')",
            FrontendErrorKind::IncorrectValue,
        ),
    ] {
        assert_eq!(adapter.execute_query(sql), Err(refused), "{sql}");
    }
}

/// A key takes the rounded number too: `1.5` into an `INT PRIMARY KEY` is
/// row 2, and into an `AUTO_INCREMENT` key it names id 2, which the counter
/// goes on past — `'5.5'` names 6 and the next row asking for one gets 7.
#[test]
fn a_key_and_a_counted_key_take_the_rounded_number() {
    let (_directory, mut adapter) = adapter();
    run(&mut adapter, "CREATE TABLE k (id INT PRIMARY KEY, n INT)");
    run(&mut adapter, "INSERT INTO k VALUES (1.5, 1)");
    run(&mut adapter, "INSERT INTO k VALUES ('2.5', 2)");
    assert_eq!(
        column(&mut adapter, "SELECT id FROM k ORDER BY id"),
        whole(&[2, 3])
    );

    run(
        &mut adapter,
        "CREATE TABLE c (id INT AUTO_INCREMENT PRIMARY KEY, n INT)",
    );
    assert_eq!(
        run(&mut adapter, "INSERT INTO c VALUES (1.5, 1)").last_insert_id,
        2
    );
    assert_eq!(
        run(&mut adapter, "INSERT INTO c (n) VALUES (9)").last_insert_id,
        3
    );
    assert_eq!(
        run(&mut adapter, "INSERT INTO c VALUES ('5.5', 2)").last_insert_id,
        6
    );
    assert_eq!(
        run(&mut adapter, "INSERT INTO c (n) VALUES (10)").last_insert_id,
        7
    );
    assert_eq!(
        rows(&mut adapter, "SELECT id, n FROM c ORDER BY id"),
        vec![
            whole(&[2, 1]),
            whole(&[3, 9]),
            whole(&[6, 2]),
            whole(&[7, 10])
        ]
    );
}

/// An `UPDATE` and an upsert round what they write the same way, a sum with
/// a written fraction included: MySQL adds it as a decimal.
#[test]
fn an_update_and_an_upsert_round_what_they_write() {
    let (_directory, mut adapter) = adapter();
    run(&mut adapter, "CREATE TABLE u (id INT PRIMARY KEY, n INT)");
    run(&mut adapter, "INSERT INTO u VALUES (1, 1), (2, 2), (3, -1)");
    assert_eq!(
        run(&mut adapter, "UPDATE u SET n = 2.5 WHERE id = 1").affected_rows,
        1
    );
    assert_eq!(
        run(&mut adapter, "UPDATE u SET n = '6.5' WHERE id = 2").affected_rows,
        1
    );
    run(&mut adapter, "UPDATE u SET n = n + 0.5");
    assert_eq!(
        column(&mut adapter, "SELECT n FROM u ORDER BY id"),
        whole(&[4, 8, -1])
    );
    run(&mut adapter, "UPDATE u SET n = n / 2");
    assert_eq!(
        column(&mut adapter, "SELECT n FROM u ORDER BY id"),
        whole(&[2, 4, -1])
    );

    assert_eq!(
        run(
            &mut adapter,
            "INSERT INTO u VALUES (1, 0) ON DUPLICATE KEY UPDATE n = 4.5"
        )
        .affected_rows,
        2
    );
    run(
        &mut adapter,
        "INSERT INTO u VALUES (1, 0) ON DUPLICATE KEY UPDATE n = n + 0.5",
    );
    assert_eq!(
        column(&mut adapter, "SELECT n FROM u WHERE id = 1"),
        whole(&[6])
    );
}

/// `INSERT ... SELECT` rounds a `DECIMAL` column and a column of words it
/// copies into a column of whole numbers half away from zero, and a value
/// past the column's range is 1264.
#[test]
fn a_decimal_and_words_copied_into_a_whole_number_column_are_rounded() {
    let (_directory, mut adapter) = adapter();
    run(
        &mut adapter,
        "CREATE TABLE src (id INT PRIMARY KEY, d DECIMAL(10,2), s VARCHAR(20))",
    );
    run(
        &mut adapter,
        "INSERT INTO src VALUES (1, 2.50, '2.5'), (2, 3.50, '3.5'), (3, -0.50, '-0.5'), (4, 1.49, '1.49')",
    );
    run(
        &mut adapter,
        "CREATE TABLE dst (a INT, e INT, u INT UNSIGNED)",
    );
    assert_eq!(
        run(&mut adapter, "INSERT INTO dst (a, e) SELECT d, s FROM src").affected_rows,
        4
    );
    assert_eq!(
        rows(&mut adapter, "SELECT a, e FROM dst"),
        vec![
            whole(&[3, 3]),
            whole(&[4, 4]),
            whole(&[-1, -1]),
            whole(&[1, 1])
        ]
    );
    assert_eq!(
        adapter.execute_query("INSERT INTO dst (u) SELECT s FROM src WHERE id = 3"),
        Err(FrontendErrorKind::OutOfRange)
    );
}

/// An id bound for a counted key as a double or as a word is read as a
/// double and rounded half to even: `100.5` names 100 and `'200.5'` names
/// 200, and the counter goes on past each. A whole number bound as a word
/// names that id.
#[test]
fn an_id_bound_for_a_counted_key_is_rounded_half_to_even() {
    let (_directory, mut adapter) = adapter();
    run(
        &mut adapter,
        "CREATE TABLE c (id INT AUTO_INCREMENT PRIMARY KEY, n INT)",
    );
    let insert = "INSERT INTO c (id, n) VALUES (?, 1)";
    for (bound, id) in [
        (Bound::Real(100.5), 100),
        (Bound::Word("200.5"), 200),
        (Bound::Real(301.5), 302),
        (Bound::Word("400"), 400),
    ] {
        match prepared(&mut adapter, insert, &[bound]) {
            Ok(PreparedStatementExecutionResult::Ok(result)) => {
                assert_eq!(result.last_insert_id, id)
            }
            other => panic!("{insert} answered {other:?}"),
        }
    }
    assert_eq!(
        run(&mut adapter, "INSERT INTO c (n) VALUES (2)").last_insert_id,
        401
    );
    assert_eq!(
        column(&mut adapter, "SELECT id FROM c ORDER BY id"),
        whole(&[100, 200, 302, 400, 401])
    );
}
