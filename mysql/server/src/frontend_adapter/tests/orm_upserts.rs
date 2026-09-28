//! The `INSERT`s and upserts ORMs write, replayed as each one sent them.
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
    (directory, adapter)
}

fn run(adapter: &mut Adapter, sql: &str) {
    adapter
        .execute_query(sql)
        .unwrap_or_else(|error| panic!("{sql}: {error:?}"));
}

/// The affected rows and the id one write reports.
fn written(adapter: &mut Adapter, sql: &str) -> (u64, u64) {
    match adapter.execute_query(sql) {
        Ok(CommandExecutionResult::Ok(result)) => (result.affected_rows, result.last_insert_id),
        other => panic!("{sql} must answer OK, answered {other:?}"),
    }
}

fn rows(adapter: &mut Adapter, sql: &str) -> Vec<Vec<Option<String>>> {
    let result = adapter
        .execute_query(sql)
        .unwrap_or_else(|error| panic!("{sql}: {error:?}"));
    let CommandExecutionResult::ResultSet(result) = result else {
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

fn some(values: &[&str]) -> Vec<Option<String>> {
    values
        .iter()
        .map(|value| Some((*value).to_owned()))
        .collect()
}

/// The affected rows, the id and the count of warnings one write reports.
fn written_with_warnings(adapter: &mut Adapter, sql: &str) -> (u64, u64, u16) {
    match adapter.execute_query(sql) {
        Ok(CommandExecutionResult::Ok(result)) => {
            (result.affected_rows, result.last_insert_id, result.warnings)
        }
        other => panic!("{sql} must answer OK, answered {other:?}"),
    }
}

/// The codes `SHOW WARNINGS` lists for the last statement.
fn warning_codes(adapter: &mut Adapter) -> Vec<String> {
    rows(adapter, "SHOW WARNINGS")
        .into_iter()
        .map(|row| row[1].clone().unwrap())
        .collect()
}

const VALUES_DEPRECATED: &str = "'VALUES function' is deprecated and will be removed in a future release. Please use an alias (INSERT INTO ... VALUES (...) AS alias) and replace VALUES(col) in the ON DUPLICATE KEY UPDATE clause with alias.col instead";

/// The null bitmap, the new-parameters flag and one VAR_STRING parameter for
/// each word.
fn words(values: &[&str]) -> Vec<u8> {
    let mut payload = vec![0; values.len().div_ceil(8)];
    payload.push(1);
    for _ in values {
        payload.extend_from_slice(&[MYSQL_TYPE_VAR_STRING, 0]);
    }
    for value in values {
        payload.push(u8::try_from(value.len()).unwrap());
        payload.extend_from_slice(value.as_bytes());
    }
    payload
}

/// The affected rows and the id one execution of a prepared write reports,
/// with the count of warnings it raised.
fn executed(adapter: &mut Adapter, statement_id: u32, payload: &[u8]) -> (u64, u64, u16) {
    match adapter.execute_stmt_execute(statement_id, payload) {
        Ok(PreparedStatementExecutionResult::Ok(result)) => {
            (result.affected_rows, result.last_insert_id, result.warnings)
        }
        other => panic!("statement {statement_id} must answer OK, answered {other:?}"),
    }
}

const RAILS_TAGS: &str = "CREATE TABLE `tags` (`id` bigint NOT NULL AUTO_INCREMENT PRIMARY KEY, `name` varchar(64) NOT NULL, UNIQUE INDEX `index_tags_on_name` (`name`)) DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_0900_ai_ci";

/// Rails 8's `insert_all!` names the offered row, as every insert it builds
/// for MySQL 8.0.19 and later does, even with no upsert to use the name.
#[test]
fn rails_insert_all_bang_names_a_row_nothing_reads() {
    let (_directory, mut adapter) = adapter();
    run(&mut adapter, RAILS_TAGS);
    assert_eq!(
        written(
            &mut adapter,
            "INSERT INTO `tags` (`name`) VALUES ('bulk-3') AS `tags_values`"
        ),
        (1, 1)
    );
    assert_eq!(
        adapter.execute_query("INSERT INTO `tags` (`name`) VALUES ('bulk-3') AS `tags_values`"),
        Err(FrontendErrorKind::ConstraintViolation)
    );
    assert_eq!(
        written(
            &mut adapter,
            "INSERT INTO `tags` (`name`) VALUES ('b4'), ('b5') AS `tags_values`"
        ),
        (2, 3)
    );
    run(
        &mut adapter,
        "CREATE TABLE codes (code varchar(10) PRIMARY KEY, name varchar(20) NOT NULL, hits int NOT NULL DEFAULT 7)",
    );
    assert_eq!(
        written(
            &mut adapter,
            "INSERT INTO codes (code, name) VALUES ('a', 'A'), ('b', 'B') AS offered"
        ),
        (2, 0)
    );
    assert_eq!(
        rows(&mut adapter, "SELECT id, name FROM tags ORDER BY id"),
        vec![
            some(&["1", "bulk-3"]),
            some(&["3", "b4"]),
            some(&["4", "b5"])
        ]
    );
    assert_eq!(
        rows(
            &mut adapter,
            "SELECT code, name, hits FROM codes ORDER BY code"
        ),
        vec![some(&["a", "A", "7"]), some(&["b", "B", "7"])]
    );
    // Renaming what the row carries has not been measured beside an upsert,
    // and is refused without one too.
    assert_eq!(
        adapter.execute_query("INSERT INTO codes (code, name) VALUES ('c', 'C') AS offered (x, y)"),
        Err(FrontendErrorKind::Unsupported)
    );
}

/// MySQL takes an upsert's assignments left to right, so a later one reads
/// what an earlier one wrote, where the engine reads the row as it stood.
#[test]
fn an_upsert_reading_a_column_it_already_wrote_is_refused() {
    let (_directory, mut adapter) = adapter();
    run(
        &mut adapter,
        "CREATE TABLE pairs (code varchar(10) PRIMARY KEY, a int NOT NULL, b int NOT NULL)",
    );
    run(
        &mut adapter,
        "INSERT INTO pairs (code, a, b) VALUES ('x', 1, 1)",
    );
    // MySQL leaves 11 and 11 here, the engine would leave 11 and 1.
    for sql in [
        "INSERT INTO pairs (code, a, b) VALUES ('x', 5, 5) ON DUPLICATE KEY UPDATE a = a + 10, b = a",
        "INSERT INTO pairs (code, a, b) VALUES ('x', 5, 5) ON DUPLICATE KEY UPDATE a = 1, a = 2",
    ] {
        assert_eq!(
            adapter.execute_query(sql),
            Err(FrontendErrorKind::Unsupported),
            "{sql}"
        );
    }
    // Reading a column before the clause writes it, and reading the offered
    // row at any point, answer the same in both.
    assert_eq!(
        written(
            &mut adapter,
            "INSERT INTO pairs (code, a, b) VALUES ('x', 5, 5) ON DUPLICATE KEY UPDATE b = a, a = a + 10"
        ),
        (2, 0)
    );
    assert_eq!(
        rows(&mut adapter, "SELECT code, a, b FROM pairs"),
        vec![some(&["x", "11", "1"])]
    );
    assert_eq!(
        written(
            &mut adapter,
            "INSERT INTO pairs (code, a, b) VALUES ('x', 5, 7) AS o ON DUPLICATE KEY UPDATE a = o.a, b = o.a + o.b"
        ),
        (2, 0)
    );
    assert_eq!(
        rows(&mut adapter, "SELECT code, a, b FROM pairs"),
        vec![some(&["x", "5", "12"])]
    );
}

/// `VALUES(col)` in an upsert, which TypeORM and GORM still write, raises
/// MySQL's deprecation warning 1287 once for each call written — over one row
/// or several — and a prepared statement raises it when it is prepared, not
/// when it is executed.
#[test]
fn values_in_an_upsert_warns_once_for_each_call() {
    let (_directory, mut adapter) = adapter();
    run(
        &mut adapter,
        "CREATE TABLE codes (code varchar(10) PRIMARY KEY, name varchar(20) NOT NULL, hits int NOT NULL DEFAULT 7)",
    );
    assert_eq!(
        written_with_warnings(
            &mut adapter,
            "INSERT INTO codes (code, name, hits) VALUES ('a', 'A', 1) ON DUPLICATE KEY UPDATE hits = VALUES(hits) + VALUES(hits)"
        ),
        (1, 0, 2)
    );
    assert_eq!(warning_codes(&mut adapter), ["1287", "1287"]);
    assert_eq!(
        rows(&mut adapter, "SHOW WARNINGS")[0],
        some(&["Warning", "1287", VALUES_DEPRECATED])
    );
    assert_eq!(
        written_with_warnings(
            &mut adapter,
            "INSERT INTO codes (code, name, hits) VALUES ('a', 'A', 1), ('b', 'B', 2) ON DUPLICATE KEY UPDATE hits = VALUES(hits) + VALUES(hits)"
        ),
        (3, 0, 2)
    );
    assert_eq!(
        rows(&mut adapter, "SELECT code, hits FROM codes ORDER BY code"),
        vec![some(&["a", "2"]), some(&["b", "2"])]
    );
    // A name on the offered row is the spelling that replaces it, and warns
    // about nothing.
    assert_eq!(
        written_with_warnings(
            &mut adapter,
            "INSERT INTO codes (code, name, hits) VALUES ('a', 'A', 1) AS o ON DUPLICATE KEY UPDATE hits = o.hits"
        ),
        (2, 0, 0)
    );
    assert!(warning_codes(&mut adapter).is_empty());

    // GORM's `clause.OnConflict{UpdateAll: true}` on a table counting its own
    // ids, prepared.
    run(
        &mut adapter,
        "CREATE TABLE `gt` (`id` bigint unsigned AUTO_INCREMENT,`name` varchar(64) NOT NULL,PRIMARY KEY (`id`),UNIQUE INDEX `idx_tags_name` (`name`))",
    );
    let statement = adapter
        .execute_stmt_prepare(
            "INSERT INTO `gt` (`name`) VALUES (?) ON DUPLICATE KEY UPDATE `name`=VALUES(`name`)",
        )
        .unwrap();
    assert_eq!(statement.warnings, 1);
    assert_eq!(warning_codes(&mut adapter), ["1287"]);
    assert_eq!(
        executed(&mut adapter, statement.statement_id, &words(&["sql"])),
        (1, 1, 0)
    );
    assert!(warning_codes(&mut adapter).is_empty());
    assert_eq!(
        executed(&mut adapter, statement.statement_id, &words(&["sql"])),
        (0, 0, 0)
    );
    assert_eq!(
        executed(&mut adapter, statement.statement_id, &words(&["SQL"])),
        (2, 1, 0)
    );
    adapter.execute_stmt_close(statement.statement_id);
    assert_eq!(
        rows(&mut adapter, "SELECT id, name FROM gt"),
        vec![some(&["1", "SQL"])]
    );
}

/// TypeORM's `repository.upsert()` asks for the next id with `DEFAULT` in
/// every row, beside an upsert that reads the offered row through `VALUES()`.
#[test]
fn typeorms_upsert_asks_the_counter_for_each_rows_id() {
    let (_directory, mut adapter) = adapter();
    run(
        &mut adapter,
        "CREATE TABLE `tags` (`id` bigint NOT NULL AUTO_INCREMENT, `name` varchar(100) NOT NULL, UNIQUE INDEX `IDX_d90243459a697eadb8ad56e909` (`name`), PRIMARY KEY (`id`)) ENGINE=InnoDB",
    );
    assert_eq!(
        written(
            &mut adapter,
            "INSERT INTO `tags`(`id`, `name`) VALUES (DEFAULT, 'news')"
        ),
        (1, 1)
    );
    let upsert = |rows: &str| {
        format!(
            "INSERT INTO `tags`(`id`, `name`) VALUES {rows} ON DUPLICATE KEY UPDATE `name` = VALUES(`name`)"
        )
    };
    assert_eq!(
        written_with_warnings(&mut adapter, &upsert("(DEFAULT, 'go'), (DEFAULT, 'news')")),
        (1, 2, 1)
    );
    assert_eq!(
        rows(&mut adapter, "SELECT LAST_INSERT_ID()"),
        vec![some(&["2"])]
    );
    assert_eq!(
        written(&mut adapter, &upsert("(DEFAULT, 'go'), (DEFAULT, 'news')")),
        (0, 0)
    );
    assert_eq!(written(&mut adapter, &upsert("(DEFAULT, 'go')")), (0, 0));
    assert_eq!(written(&mut adapter, &upsert("(DEFAULT, 'rust')")), (1, 7));
    // A row it changed reports that row's own id.
    assert_eq!(written(&mut adapter, &upsert("(DEFAULT, 'Go')")), (2, 2));
    assert_eq!(
        rows(&mut adapter, "SELECT LAST_INSERT_ID()"),
        vec![some(&["7"])]
    );
    assert_eq!(
        rows(&mut adapter, "SELECT id, name FROM tags ORDER BY id"),
        vec![
            some(&["1", "news"]),
            some(&["2", "Go"]),
            some(&["7", "rust"])
        ]
    );
    // Measured, the offered row carries the number a colliding row spent,
    // which is not a number this is held to.
    assert_eq!(
        adapter.execute_query(
            "INSERT INTO `tags`(`id`, `name`) VALUES (DEFAULT, 'zz') ON DUPLICATE KEY UPDATE `name` = CONCAT(VALUES(`name`), VALUES(`id`))"
        ),
        Err(FrontendErrorKind::Unsupported)
    );
    assert_eq!(
        adapter.execute_query(
            "INSERT INTO `tags`(`id`, `name`) VALUES (DEFAULT, 'zz') AS o ON DUPLICATE KEY UPDATE `name` = o.id"
        ),
        Err(FrontendErrorKind::Unsupported)
    );
}

/// `DEFAULT` for an ordinary column beside an upsert offers the column's own
/// default.
#[test]
fn a_default_beside_an_upsert_offers_the_columns_default() {
    let (_directory, mut adapter) = adapter();
    run(
        &mut adapter,
        "CREATE TABLE codes (code varchar(10) PRIMARY KEY, name varchar(20) NOT NULL, hits int NOT NULL DEFAULT 7)",
    );
    assert_eq!(
        written(
            &mut adapter,
            "INSERT INTO codes (code, name, hits) VALUES ('b', 'B', DEFAULT) ON DUPLICATE KEY UPDATE name = VALUES(name)"
        ),
        (1, 0)
    );
    assert_eq!(
        written(
            &mut adapter,
            "INSERT INTO codes (code, name, hits) VALUES ('b', 'C', DEFAULT) ON DUPLICATE KEY UPDATE hits = VALUES(hits) + 1"
        ),
        (2, 0)
    );
    assert_eq!(
        rows(&mut adapter, "SELECT code, name, hits FROM codes"),
        vec![some(&["b", "B", "8"])]
    );
    assert_eq!(
        written(
            &mut adapter,
            "INSERT INTO codes (code, name, hits) VALUES ('b', 'C', DEFAULT) AS o ON DUPLICATE KEY UPDATE hits = o.hits + 2"
        ),
        (2, 0)
    );
    assert_eq!(
        rows(&mut adapter, "SELECT code, name, hits FROM codes"),
        vec![some(&["b", "B", "9"])]
    );
    // Every column given its default leaves the engine no row to hang the
    // clause on.
    assert_eq!(
        adapter.execute_query(
            "INSERT INTO codes (hits) VALUES (DEFAULT) ON DUPLICATE KEY UPDATE hits = 1"
        ),
        Err(FrontendErrorKind::Unsupported)
    );
}
