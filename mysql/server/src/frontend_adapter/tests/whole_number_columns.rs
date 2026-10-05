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
    Null,
}

fn prepared(
    adapter: &mut Adapter,
    sql: &str,
    values: &[Bound<'_>],
) -> Result<PreparedStatementExecutionResult, FrontendErrorKind> {
    let mut payload = vec![0; values.len().div_ceil(8)];
    for (at, value) in values.iter().enumerate() {
        if matches!(value, Bound::Null) {
            payload[at / 8] |= 1 << (at % 8);
        }
    }
    payload.push(1);
    for value in values {
        let code = match value {
            Bound::Real(_) => MYSQL_TYPE_DOUBLE,
            Bound::Word(_) => MYSQL_TYPE_VAR_STRING,
            Bound::Null => MYSQL_TYPE_NULL,
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
            Bound::Null => {}
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

/// A number written with an exponent is a double, rounded half to even, in
/// `VALUES`, `SET`, an upsert clause and an `UPDATE` alike; one written with a
/// point is a decimal, rounded half away from zero; a word is read as a word.
#[test]
fn a_written_double_is_rounded_half_to_even_and_a_decimal_half_away_from_zero() {
    let (_directory, mut adapter) = adapter();
    run(
        &mut adapter,
        "CREATE TABLE r (id INT PRIMARY KEY, i INT, u INT UNSIGNED, s VARCHAR(10))",
    );
    run(
        &mut adapter,
        "INSERT INTO r (id, i) VALUES (1, 2.5e0), (2, 3.5e0), (3, -2.5e0), (4, 2.5), (5, '2.5'), (6, '2.5e0'), (7, (1.5E0)), (8, - 2.5)",
    );
    run(&mut adapter, "INSERT INTO r SET id = 9, i = 4.5e0");
    run(&mut adapter, "INSERT INTO r VALUES (10, 0.5e0, 1.5e0, 'x')");
    assert_eq!(
        column(&mut adapter, "SELECT i FROM r ORDER BY id"),
        whole(&[2, 4, -2, 3, 3, 3, 2, -3, 4, 0])
    );
    assert_eq!(
        column(&mut adapter, "SELECT u FROM r WHERE id = 10"),
        whole(&[2])
    );
    run(&mut adapter, "UPDATE r SET i = 4.5e0 WHERE id = 1");
    run(
        &mut adapter,
        "INSERT INTO r (id, i) VALUES (2, 0) ON DUPLICATE KEY UPDATE i = 6.5e0",
    );
    assert_eq!(
        column(
            &mut adapter,
            "SELECT i FROM r WHERE id IN (1, 2) ORDER BY id"
        ),
        whole(&[4, 6])
    );

    // A counted key reads a double the same way.
    run(
        &mut adapter,
        "CREATE TABLE c (id INT AUTO_INCREMENT PRIMARY KEY, n INT)",
    );
    assert_eq!(
        run(&mut adapter, "INSERT INTO c (id, n) VALUES (2.5e0, 1)").last_insert_id,
        2
    );
    assert_eq!(
        run(&mut adapter, "INSERT INTO c (id, n) VALUES ('4.5e0', 1)").last_insert_id,
        5
    );
}

/// A double bound for a column of whole numbers is rounded half to even, and
/// a word bound for one is read as a written word.
#[test]
fn a_bound_double_is_rounded_half_to_even() {
    let (_directory, mut adapter) = adapter();
    run(
        &mut adapter,
        "CREATE TABLE b (id INT PRIMARY KEY, i INT, u INT UNSIGNED)",
    );
    let wrote = |adapter: &mut Adapter, sql: &str, values: &[Bound<'_>]| match prepared(
        adapter, sql, values,
    ) {
        Ok(PreparedStatementExecutionResult::Ok(result)) => result.affected_rows,
        other => panic!("{sql} answered {other:?}"),
    };
    wrote(
        &mut adapter,
        "INSERT INTO b (id, i) VALUES (1, ?), (2, ?), (3, ?), (4, ?), (5, ?), (6, ?)",
        &[
            Bound::Real(2.5),
            Bound::Real(-2.5),
            Bound::Word("2.5"),
            Bound::Word("-2.5"),
            Bound::Word(" 4.5 "),
            Bound::Word("0.49999999999999999999"),
        ],
    );
    wrote(
        &mut adapter,
        "INSERT INTO b SET id = 7, i = ?",
        &[Bound::Real(2.5)],
    );
    assert_eq!(
        column(&mut adapter, "SELECT i FROM b ORDER BY id"),
        whole(&[2, -2, 3, -3, 5, 0, 2])
    );
    wrote(
        &mut adapter,
        "UPDATE b SET i = ? WHERE id = 1",
        &[Bound::Real(4.5)],
    );
    wrote(
        &mut adapter,
        "UPDATE b SET i = ? WHERE id = 2",
        &[Bound::Word("4.5")],
    );
    wrote(
        &mut adapter,
        "INSERT INTO b (id, i) VALUES (3, 0) ON DUPLICATE KEY UPDATE i = ?",
        &[Bound::Real(6.5)],
    );
    wrote(
        &mut adapter,
        "INSERT INTO b (id, i) VALUES (4, 0) ON DUPLICATE KEY UPDATE i = ?",
        &[Bound::Word("6.5")],
    );
    assert_eq!(
        column(&mut adapter, "SELECT i FROM b WHERE id <= 4 ORDER BY id"),
        whole(&[4, 5, 6, 7])
    );
}

/// A number written with a point is a decimal: below zero it is 1264 in an
/// unsigned column even where it rounds to zero, and it is read exactly, past
/// the digits a double keeps. A word naming such a number stores 0 there, and
/// a bound word is 1264 as the written number is.
#[test]
fn a_decimal_below_zero_is_out_of_range_for_an_unsigned_column() {
    let (_directory, mut adapter) = adapter();
    run(
        &mut adapter,
        "CREATE TABLE n (id INT PRIMARY KEY, i INT, u INT UNSIGNED, tu TINYINT UNSIGNED, b BIGINT)",
    );
    for sql in [
        "INSERT INTO n (id, u) VALUES (1, -0.4)",
        "INSERT INTO n (id, tu) VALUES (1, -0.00001)",
        "INSERT INTO n SET id = 1, u = -0.4",
    ] {
        assert_eq!(
            adapter.execute_query(sql),
            Err(FrontendErrorKind::OutOfRange),
            "{sql}"
        );
    }
    run(
        &mut adapter,
        "INSERT INTO n (id, i, u, tu) VALUES (1, -0.4, '-0.4', -0.4e0), (2, 0.49999999999999999999, -0.0, -0)",
    );
    assert_eq!(
        rows(&mut adapter, "SELECT i, u, tu FROM n ORDER BY id"),
        vec![whole(&[0, 0, 0]), whole(&[0, 0, 0])]
    );
    run(
        &mut adapter,
        "INSERT INTO n (id, b) VALUES (3, 9223372036854775807.4), (4, '0.49999999999999999999')",
    );
    assert_eq!(
        column(&mut adapter, "SELECT b FROM n WHERE id >= 3 ORDER BY id"),
        whole(&[9_223_372_036_854_775_807, 0])
    );
    assert_eq!(
        adapter.execute_query("UPDATE n SET u = -0.4 WHERE id = 1"),
        Err(FrontendErrorKind::OutOfRange)
    );
    assert_eq!(
        run(&mut adapter, "UPDATE n SET u = -0.4 WHERE id = 100").affected_rows,
        0
    );
    assert_eq!(
        adapter
            .execute_query("INSERT INTO n (id, i) VALUES (1, 0) ON DUPLICATE KEY UPDATE u = -0.4"),
        Err(FrontendErrorKind::OutOfRange)
    );
    for (bound, refused) in [
        (Bound::Word("-0.4"), Some(FrontendErrorKind::OutOfRange)),
        (Bound::Word(" -0.1"), Some(FrontendErrorKind::OutOfRange)),
        (Bound::Word("-0"), None),
        (Bound::Real(-0.4), None),
    ] {
        let answered = prepared(&mut adapter, "UPDATE n SET u = ? WHERE id = 1", &[bound]);
        match refused {
            Some(kind) => assert_eq!(answered.err(), Some(kind)),
            None => assert!(answered.is_ok(), "{answered:?}"),
        }
    }
}

/// A word with more after its number is 1265, unless the number is past the
/// column's range, which is 1264; a word starting with no number is 1366.
#[test]
fn a_word_with_more_after_its_number_is_data_truncated() {
    let (_directory, mut adapter) = adapter();
    run(
        &mut adapter,
        "CREATE TABLE w (id INT PRIMARY KEY, i INT, t TINYINT, u INT UNSIGNED, s VARCHAR(30))",
    );
    for (sql, refused) in [
        (
            "INSERT INTO w (id, i) VALUES (1, '7x')",
            FrontendErrorKind::NotAMember,
        ),
        (
            "INSERT INTO w (id, i) VALUES (1, '2.5 x')",
            FrontendErrorKind::NotAMember,
        ),
        (
            "INSERT INTO w (id, i) VALUES (1, '1,5')",
            FrontendErrorKind::NotAMember,
        ),
        (
            "INSERT INTO w (id, i) VALUES (1, '0x10')",
            FrontendErrorKind::NotAMember,
        ),
        (
            "INSERT INTO w (id, u) VALUES (1, '-0.4x')",
            FrontendErrorKind::NotAMember,
        ),
        (
            "INSERT INTO w (id, t) VALUES (1, '999x')",
            FrontendErrorKind::OutOfRange,
        ),
        (
            "INSERT INTO w (id, u) VALUES (1, '-0.5x')",
            FrontendErrorKind::OutOfRange,
        ),
        (
            "INSERT INTO w (id, i) VALUES (1, '1e19x')",
            FrontendErrorKind::OutOfRange,
        ),
        (
            "INSERT INTO w (id, i) VALUES (1, 'x7')",
            FrontendErrorKind::IncorrectValue,
        ),
        (
            "INSERT INTO w (id, i) VALUES (1, '- 7')",
            FrontendErrorKind::IncorrectValue,
        ),
        (
            "INSERT INTO w (id, i) VALUES (1, '.e3')",
            FrontendErrorKind::IncorrectValue,
        ),
    ] {
        assert_eq!(adapter.execute_query(sql), Err(refused), "{sql}");
    }
    run(
        &mut adapter,
        "INSERT INTO w (id, i) VALUES (1, '1e'), (2, '1.e3'), (3, '2.5e+'), (4, '12.345e-'), (5, '\\t7\\t'), (6, '7.')",
    );
    assert_eq!(
        column(&mut adapter, "SELECT i FROM w ORDER BY id"),
        whole(&[1, 1000, 25, 12345, 7, 7])
    );
    assert_eq!(
        adapter.execute_query("UPDATE w SET i = '7x' WHERE id = 1"),
        Err(FrontendErrorKind::NotAMember)
    );
    assert_eq!(
        adapter
            .execute_query("INSERT INTO w (id, i) VALUES (1, 0) ON DUPLICATE KEY UPDATE i = '7x'"),
        Err(FrontendErrorKind::NotAMember)
    );
    run(&mut adapter, "INSERT INTO w (id, s) VALUES (10, '7x')");
    assert_eq!(
        adapter.execute_query("INSERT INTO w (id, i) SELECT 11, s FROM w WHERE id = 10"),
        Err(FrontendErrorKind::NotAMember)
    );
    assert_eq!(
        prepared(
            &mut adapter,
            "INSERT INTO w (id, i) VALUES (12, ?)",
            &[Bound::Word("7x")]
        )
        .err(),
        Some(FrontendErrorKind::NotAMember)
    );
}

/// The warnings the statement that ran last raised, each as its level, its
/// code and its message.
fn warnings(adapter: &mut Adapter) -> Vec<(String, String, String)> {
    rows(adapter, "SHOW WARNINGS")
        .into_iter()
        .map(|warning| {
            let [level, code, message] = <[Option<String>; 3]>::try_from(warning).unwrap();
            (level.unwrap(), code.unwrap(), message.unwrap())
        })
        .collect()
}

fn truncated(column: &str, row: u64) -> (String, String, String) {
    (
        "Note".to_owned(),
        "1265".to_owned(),
        format!("Data truncated for column '{column}' at row {row}"),
    )
}

fn out_of_range(column: &str, row: u64) -> (String, String, String) {
    (
        "Warning".to_owned(),
        "1264".to_owned(),
        format!("Out of range value for column '{column}' at row {row}"),
    )
}

fn cannot_be_null(column: &str) -> (String, String, String) {
    (
        "Warning".to_owned(),
        "1048".to_owned(),
        format!("Column '{column}' cannot be null"),
    )
}

fn warned(adapter: &mut Adapter, sql: &str, values: &[Bound<'_>]) -> u16 {
    match prepared(adapter, sql, values) {
        Ok(PreparedStatementExecutionResult::Ok(result)) => result.warnings,
        other => panic!("{sql} answered {other:?}"),
    }
}

/// A number rounded to a `DECIMAL`'s places raises note 1265 for its row,
/// written or bound, in `VALUES`, `SET`, an `UPDATE` and an upsert clause; one
/// whose places past the column's are zeros raises none.
#[test]
fn a_number_rounded_to_a_decimals_places_raises_note_1265() {
    let (_directory, mut adapter) = adapter();
    run(
        &mut adapter,
        "CREATE TABLE d (id INT PRIMARY KEY, d DECIMAL(10,2), e DECIMAL(5,0))",
    );
    assert_eq!(
        warned(
            &mut adapter,
            "INSERT INTO d (id, d) VALUES (1, ?), (2, ?), (3, ?)",
            &[
                Bound::Word("1.234"),
                Bound::Real(1.234),
                Bound::Word("1.230")
            ],
        ),
        2
    );
    assert_eq!(
        warnings(&mut adapter),
        vec![truncated("d", 1), truncated("d", 2)]
    );
    assert_eq!(
        run(
            &mut adapter,
            "INSERT INTO d (id, d, e) VALUES (4, 1.005, 2.5)"
        )
        .warnings,
        2
    );
    assert_eq!(
        warnings(&mut adapter),
        vec![truncated("d", 1), truncated("e", 1)]
    );
    assert_eq!(
        run(&mut adapter, "INSERT INTO d SET id = 5, d = 1.239").warnings,
        1
    );
    assert_eq!(warnings(&mut adapter), vec![truncated("d", 1)]);
    assert_eq!(
        run(
            &mut adapter,
            "INSERT INTO d (id, d) VALUES (6, '1.2e0'), (7, 1.239e0), (8, 1.25e0), (9, -0.004)"
        )
        .warnings,
        2
    );
    assert_eq!(
        warnings(&mut adapter),
        vec![truncated("d", 2), truncated("d", 4)]
    );
    assert_eq!(
        warned(
            &mut adapter,
            "UPDATE d SET d = ? WHERE id <= 3",
            &[Bound::Word("9.999")]
        ),
        3
    );
    assert_eq!(
        warnings(&mut adapter),
        vec![truncated("d", 1), truncated("d", 2), truncated("d", 3)]
    );
    assert_eq!(
        run(
            &mut adapter,
            "INSERT INTO d (id, d) VALUES (4, 1.111) ON DUPLICATE KEY UPDATE d = 2.222"
        )
        .warnings,
        2
    );
    assert_eq!(
        warnings(&mut adapter),
        vec![truncated("d", 1), truncated("d", 1)]
    );
    assert_eq!(
        rows(
            &mut adapter,
            "SELECT d, e FROM d WHERE id IN (1, 4, 9) ORDER BY id"
        ),
        vec![
            vec![Some("10.00".to_owned()), None],
            vec![Some("2.22".to_owned()), Some("3".to_owned())],
            vec![Some("0.00".to_owned()), None]
        ]
    );

    // `sql_notes = 0` leaves the notes out.
    run(&mut adapter, "SET sql_notes = 0");
    assert_eq!(
        run(&mut adapter, "INSERT INTO d (id, d) VALUES (20, 1.234)").warnings,
        0
    );
}

/// Under `IGNORE` a whole number past its column's range is cut to the
/// nearest one the column holds with warning 1264, and a NULL meeting a
/// column refusing NULL stores the column's empty value with warning 1048,
/// written or bound, row by row.
#[test]
fn ignore_cuts_a_whole_number_to_its_range_and_stores_null_as_empty() {
    let (_directory, mut adapter) = adapter();
    run(
        &mut adapter,
        "CREATE TABLE g (id INT PRIMARY KEY, n INT NOT NULL DEFAULT 5, t TINYINT, u INT UNSIGNED, s VARCHAR(5) NOT NULL DEFAULT 'q', x DOUBLE NOT NULL DEFAULT 1)",
    );
    assert_eq!(
        run(
            &mut adapter,
            "INSERT IGNORE INTO g (id, t, u) VALUES (10, 300, -5), (11, -129, 4294967296)"
        )
        .warnings,
        4
    );
    assert_eq!(
        warnings(&mut adapter),
        vec![
            out_of_range("t", 1),
            out_of_range("u", 1),
            out_of_range("t", 2),
            out_of_range("u", 2)
        ]
    );
    assert_eq!(
        run(
            &mut adapter,
            "INSERT IGNORE INTO g (id, t, u) VALUES (12, 127.6, -0.4), (13, '300', '-0.4'), (14, 2.5e3, 1)"
        )
        .warnings,
        4
    );
    assert_eq!(
        warnings(&mut adapter),
        vec![
            out_of_range("t", 1),
            out_of_range("u", 1),
            out_of_range("t", 2),
            out_of_range("t", 3)
        ]
    );
    assert_eq!(
        warned(
            &mut adapter,
            "INSERT IGNORE INTO g (id, t, u, n) VALUES (15, ?, ?, ?)",
            &[Bound::Real(300.0), Bound::Word("-0.4"), Bound::Null],
        ),
        3
    );
    assert_eq!(
        warnings(&mut adapter),
        vec![
            out_of_range("t", 1),
            out_of_range("u", 1),
            cannot_be_null("n")
        ]
    );
    assert_eq!(
        run(
            &mut adapter,
            "INSERT IGNORE INTO g (id, n, s, x) VALUES (16, NULL, NULL, NULL)"
        )
        .warnings,
        3
    );
    assert_eq!(
        warnings(&mut adapter),
        vec![
            cannot_be_null("n"),
            cannot_be_null("s"),
            cannot_be_null("x")
        ]
    );
    assert_eq!(
        run(
            &mut adapter,
            "INSERT IGNORE INTO g SET id = 17, n = NULL, t = 200"
        )
        .warnings,
        2
    );
    assert_eq!(
        warnings(&mut adapter),
        vec![cannot_be_null("n"), out_of_range("t", 1)]
    );
    let stored = |row: [Option<&str>; 5]| row.map(|value| value.map(str::to_owned)).to_vec();
    assert_eq!(
        rows(&mut adapter, "SELECT t, u, n, s, x FROM g ORDER BY id"),
        vec![
            stored([Some("127"), Some("0"), Some("5"), Some("q"), Some("1")]),
            stored([
                Some("-128"),
                Some("4294967295"),
                Some("5"),
                Some("q"),
                Some("1")
            ]),
            stored([Some("127"), Some("0"), Some("5"), Some("q"), Some("1")]),
            stored([Some("127"), Some("0"), Some("5"), Some("q"), Some("1")]),
            stored([Some("127"), Some("1"), Some("5"), Some("q"), Some("1")]),
            stored([Some("127"), Some("0"), Some("0"), Some("q"), Some("1")]),
            stored([None, None, Some("0"), Some(""), Some("0")]),
            stored([Some("127"), None, Some("0"), Some("q"), Some("1")]),
        ]
    );

    // An `UPDATE IGNORE` raises its warnings once for each row it matched,
    // a NULL bound for a `?` included.
    assert_eq!(
        run(
            &mut adapter,
            "UPDATE IGNORE g SET n = NULL WHERE id IN (10, 11)"
        )
        .warnings,
        2
    );
    assert_eq!(
        warnings(&mut adapter),
        vec![cannot_be_null("n"), cannot_be_null("n")]
    );
    assert_eq!(
        warned(
            &mut adapter,
            "UPDATE IGNORE g SET n = ? WHERE id IN (12, 13)",
            &[Bound::Null]
        ),
        2
    );
    assert_eq!(
        warned(
            &mut adapter,
            "UPDATE IGNORE g SET t = ? WHERE id IN (12, 13, 14)",
            &[Bound::Real(500.0)]
        ),
        3
    );
    assert_eq!(
        warnings(&mut adapter),
        vec![
            out_of_range("t", 1),
            out_of_range("t", 2),
            out_of_range("t", 3)
        ]
    );
    assert_eq!(
        column(
            &mut adapter,
            "SELECT n FROM g WHERE id BETWEEN 10 AND 13 ORDER BY id"
        ),
        whole(&[0, 0, 0, 0])
    );
}
