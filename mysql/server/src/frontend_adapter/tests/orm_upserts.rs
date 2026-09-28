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
