//! `CHECK` constraints: added and dropped with `ALTER TABLE`, as Rails'
//! `add_check_constraint` and Django's `CheckConstraint` write them, broken by
//! a row, and read back from `information_schema`.
//!
//! Every expectation here was measured on MySQL 8.4.11.

use super::*;

type Adapter = AuthorizedDatabaseCommandAdapter<RecordingAuthorizer>;

fn adapter() -> (tempfile::TempDir, Adapter) {
    let authorizer = Arc::new(RecordingAuthorizer::default());
    let (directory, _catalog, factory) = catalog_factory(authorizer);
    let mut adapter = factory
        .build(AuthenticatedPrincipal::from_account_id_for_testing(
            AccountId::from_bytes([171; 32]),
        ))
        .unwrap();
    adapter.authorize_connection().unwrap();
    adapter.execute_init_db("reports").unwrap();
    run(
        &mut adapter,
        "CREATE TABLE orders (id INT NOT NULL AUTO_INCREMENT PRIMARY KEY, \
         a INT CHECK (a > 0), b INT, c INT CHECK (c >= 1), s VARCHAR(10), \
         CHECK (b > 0), CONSTRAINT named CHECK (a < 100), CHECK (s <> ''), \
         CHECK (a * 2 > 0))",
    );
    (directory, adapter)
}

fn run(adapter: &mut Adapter, sql: &str) {
    adapter
        .execute_query(sql)
        .unwrap_or_else(|error| panic!("{sql}: {error:?}"));
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

fn names(adapter: &mut Adapter) -> Vec<String> {
    rows(
        adapter,
        "SELECT CONSTRAINT_NAME FROM information_schema.CHECK_CONSTRAINTS ORDER BY CONSTRAINT_NAME",
    )
    .into_iter()
    .map(|row| row[0].clone().unwrap())
    .collect()
}

/// Measured on MySQL 8.4.11: an unnamed constraint is `<table>_chk_<n>`,
/// counted over the unnamed ones in the order written, a column's own among
/// them, and the expression reads back the way MySQL writes it.
#[test]
fn each_check_is_read_back_under_the_name_mysql_gives_it() {
    let (_directory, mut adapter) = adapter();
    let read = |value: &str| Some(value.to_owned());
    assert_eq!(
        rows(
            &mut adapter,
            "SELECT * FROM information_schema.CHECK_CONSTRAINTS ORDER BY CONSTRAINT_NAME"
        ),
        [
            vec![
                read("def"),
                read("reports"),
                read("named"),
                read("(`a` < 100)")
            ],
            vec![
                read("def"),
                read("reports"),
                read("orders_chk_1"),
                read("(`a` > 0)")
            ],
            vec![
                read("def"),
                read("reports"),
                read("orders_chk_2"),
                read("(`c` >= 1)")
            ],
            vec![
                read("def"),
                read("reports"),
                read("orders_chk_3"),
                read("(`b` > 0)")
            ],
            vec![
                read("def"),
                read("reports"),
                read("orders_chk_4"),
                read("(`s` <> _utf8mb4\\'\\')")
            ],
            vec![
                read("def"),
                read("reports"),
                read("orders_chk_5"),
                read("((`a` * 2) > 0)")
            ],
        ]
    );
    assert_eq!(
        rows(
            &mut adapter,
            "SELECT CONSTRAINT_NAME, CONSTRAINT_TYPE, ENFORCED FROM information_schema.TABLE_CONSTRAINTS \
             WHERE TABLE_NAME = 'orders' AND CONSTRAINT_TYPE = 'CHECK' ORDER BY CONSTRAINT_NAME"
        )
        .len(),
        6
    );
    let shapes = match adapter
        .execute_query("SELECT CHECK_CLAUSE FROM information_schema.CHECK_CONSTRAINTS")
        .unwrap()
    {
        CommandExecutionResult::ResultSet(result) => result.columns,
        other => panic!("{other:?}"),
    };
    assert_eq!(
        (shapes[0].column_type, shapes[0].column_length),
        (MYSQL_TYPE_BLOB, u32::MAX)
    );
}

/// Adding a constraint checks the rows already there — measured, one they
/// break is 3819 and the table is left as it was — and every row written after
/// it. The counted table goes on counting from where it stood.
#[test]
fn adding_a_check_holds_the_rows_there_and_to_come() {
    let (_directory, mut adapter) = adapter();
    run(
        &mut adapter,
        "INSERT INTO orders (a, b, c, s) VALUES (1, 1, 1, 'x'), (2, 2, 2, 'y')",
    );
    assert_eq!(
        adapter.execute_query("ALTER TABLE orders ADD CONSTRAINT small_b CHECK (b < 2)"),
        Err(FrontendErrorKind::CheckConstraintViolated)
    );
    assert!(!names(&mut adapter).contains(&"small_b".to_owned()));
    run(
        &mut adapter,
        "ALTER TABLE orders ADD CONSTRAINT small_b CHECK (b < 10)",
    );
    assert_eq!(
        adapter.execute_query("INSERT INTO orders (a, b, c, s) VALUES (3, 30, 3, 'z')"),
        Err(FrontendErrorKind::CheckConstraintViolated)
    );
    run(
        &mut adapter,
        "INSERT INTO orders (a, b, c, s) VALUES (3, 3, 3, 'z')",
    );
    assert_eq!(
        rows(&mut adapter, "SELECT a, b FROM orders ORDER BY id"),
        [
            [Some("1".to_owned()), Some("1".to_owned())],
            [Some("2".to_owned()), Some("2".to_owned())],
            [Some("3".to_owned()), Some("3".to_owned())],
        ]
    );
    assert!(names(&mut adapter).contains(&"small_b".to_owned()));
}

/// Measured on MySQL 8.4.11: an unnamed constraint added later takes one past
/// the highest number the table's names carry, a dropped constraint's number
/// is not handed out again, a name is matched without regard to case, and a
/// dropped constraint no longer holds.
#[test]
fn a_check_is_dropped_by_name_and_the_others_keep_theirs() {
    let (_directory, mut adapter) = adapter();
    run(
        &mut adapter,
        "ALTER TABLE orders ADD CONSTRAINT small_b CHECK (b < 10)",
    );
    run(&mut adapter, "ALTER TABLE orders ADD CHECK (c < 100)");
    run(&mut adapter, "ALTER TABLE orders DROP CHECK orders_chk_3");
    run(&mut adapter, "ALTER TABLE orders DROP CONSTRAINT SMALL_B");
    run(
        &mut adapter,
        "INSERT INTO orders (a, b, c, s) VALUES (4, 40, 4, 'w')",
    );
    assert_eq!(
        names(&mut adapter),
        [
            "named",
            "orders_chk_1",
            "orders_chk_2",
            "orders_chk_4",
            "orders_chk_5",
            "orders_chk_6"
        ]
    );
}

/// What MySQL answers with an error of its own, and what this refuses.
#[test]
fn a_check_mysql_would_not_take_is_refused() {
    let (_directory, mut adapter) = adapter();
    // Measured: 3821 for a name the table has not got, and 3822 for one a
    // constraint of any table already has.
    assert_eq!(
        adapter.execute_query("ALTER TABLE orders DROP CHECK nope"),
        Err(FrontendErrorKind::NoSuchCheck)
    );
    run(
        &mut adapter,
        "CREATE TABLE other (x INT, CONSTRAINT taken CHECK (x > 0))",
    );
    assert_eq!(
        adapter.execute_query("ALTER TABLE orders ADD CONSTRAINT taken CHECK (a > 1)"),
        Err(FrontendErrorKind::DuplicateCheckName)
    );
    assert_eq!(
        adapter.execute_query("ALTER TABLE orders ADD CONSTRAINT Named CHECK (a > 1)"),
        Err(FrontendErrorKind::DuplicateCheckName)
    );
    for sql in [
        // Measured: 3818, the counted number being given only once the check
        // has run.
        "ALTER TABLE orders ADD CONSTRAINT id_pos CHECK (id > 0)",
        "CREATE TABLE counted (id INT NOT NULL AUTO_INCREMENT PRIMARY KEY, CHECK (id > 0))",
        // A constraint MySQL keeps without checking, which the engine cannot.
        "ALTER TABLE orders ADD CONSTRAINT loose CHECK (a > 1) NOT ENFORCED",
        // MySQL numbers these two in the order written and the table is
        // stored with its columns first.
        "CREATE TABLE out_of_order (a INT, CHECK (a > 0), b INT CHECK (b > 0))",
    ] {
        assert!(adapter.execute_query(sql).is_err(), "{sql}");
    }
}

/// A clause whose writing has not been measured is refused when it is read,
/// while its constraint is still listed by name.
#[test]
fn a_clause_mysql_writes_some_unmeasured_way_is_refused_when_read() {
    let (_directory, mut adapter) = adapter();
    run(
        &mut adapter,
        "CREATE TABLE odd (n INT, CONSTRAINT even CHECK (n % 2 = 0))",
    );
    assert!(names(&mut adapter).contains(&"even".to_owned()));
    assert!(adapter
        .execute_query(
            "SELECT CHECK_CLAUSE FROM information_schema.CHECK_CONSTRAINTS \
             WHERE CONSTRAINT_NAME = 'even'"
        )
        .is_err());
}
