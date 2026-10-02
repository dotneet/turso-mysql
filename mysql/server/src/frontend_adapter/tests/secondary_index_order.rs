use super::*;

type Adapter = AuthorizedDatabaseCommandAdapter<RecordingAuthorizer>;

fn adapter() -> (tempfile::TempDir, Adapter) {
    let authorizer = Arc::new(RecordingAuthorizer::default());
    let (directory, _catalog, factory) = catalog_factory(authorizer);
    let mut adapter = factory
        .build(AuthenticatedPrincipal::from_account_id_for_testing(
            AccountId::from_bytes([123; 32]),
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

fn rows(adapter: &mut Adapter, sql: &str) -> Vec<Vec<Option<String>>> {
    let answer = adapter.execute_query(sql);
    let Ok(CommandExecutionResult::ResultSet(result)) = answer else {
        panic!("{sql} must return a result set: {answer:?}");
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

fn firsts(adapter: &mut Adapter, sql: &str) -> Vec<String> {
    rows(adapter, sql)
        .into_iter()
        .map(|mut row| row.remove(0).unwrap())
        .collect()
}

fn pairs(adapter: &mut Adapter, sql: &str) -> Vec<(String, String)> {
    rows(adapter, sql)
        .into_iter()
        .map(|mut row| (row.remove(0).unwrap(), row.remove(0).unwrap()))
        .collect()
}

fn key_columns(adapter: &mut Adapter, table: &str) -> Vec<String> {
    rows(adapter, &format!("SHOW INDEX FROM {table}"))
        .into_iter()
        .map(|row| {
            format!(
                "{}.{}",
                row[2].as_deref().unwrap(),
                row[4].as_deref().unwrap()
            )
        })
        .collect()
}

fn table_with_equal_keys(adapter: &mut Adapter) {
    run(
        adapter,
        "CREATE TABLE q (id INT NOT NULL PRIMARY KEY, k INT, v INT, KEY q_k (k))",
    );
    for row in ["(30, 5, 0)", "(10, 5, 0)", "(1, 1, 0)", "(20, 5, 0)"] {
        run(adapter, &format!("INSERT INTO q (id, k, v) VALUES {row}"));
    }
}

#[test]
fn rows_with_equal_values_in_a_plain_index_come_back_in_primary_key_order() {
    let (_directory, mut adapter) = adapter();
    table_with_equal_keys(&mut adapter);
    assert_eq!(
        firsts(&mut adapter, "SELECT id FROM q WHERE k = 5"),
        ["10", "20", "30"]
    );
    assert_eq!(
        firsts(&mut adapter, "SELECT id FROM q WHERE k >= 1"),
        ["1", "10", "20", "30"]
    );
    assert_eq!(
        firsts(
            &mut adapter,
            "SELECT id FROM q WHERE k = 5 ORDER BY k LIMIT 1"
        ),
        ["10"]
    );
}

#[test]
fn a_row_whose_primary_key_changes_moves_among_equal_values() {
    let (_directory, mut adapter) = adapter();
    table_with_equal_keys(&mut adapter);
    run(&mut adapter, "UPDATE q SET id = 5 WHERE id = 30");
    assert_eq!(
        firsts(&mut adapter, "SELECT id FROM q WHERE k = 5"),
        ["5", "10", "20"]
    );
    assert_eq!(
        rows(&mut adapter, "CHECK TABLE q")[0][3].as_deref(),
        Some("OK")
    );
}

#[test]
fn a_renamed_primary_key_column_stays_out_of_the_plain_index_columns() {
    let (_directory, mut adapter) = adapter();
    table_with_equal_keys(&mut adapter);
    run(&mut adapter, "ALTER TABLE q RENAME COLUMN id TO pk");
    assert_eq!(key_columns(&mut adapter, "q"), ["PRIMARY.pk", "q_k.k"]);
    assert_eq!(
        firsts(&mut adapter, "SELECT pk FROM q WHERE k = 5"),
        ["10", "20", "30"]
    );
}

#[test]
fn a_composite_primary_key_orders_equal_values_by_each_of_its_columns() {
    let (_directory, mut adapter) = adapter();
    run(
        &mut adapter,
        "CREATE TABLE c (a INT NOT NULL, b INT NOT NULL, k INT, PRIMARY KEY (a, b), KEY c_k (k))",
    );
    run(
        &mut adapter,
        "CREATE TABLE c2 (a INT NOT NULL, b INT NOT NULL, k INT, PRIMARY KEY (a, b), KEY c2_kb (k, b))",
    );
    for table in ["c", "c2"] {
        run(
            &mut adapter,
            &format!(
                "INSERT INTO {table} (a, b, k) VALUES (2, 1, 5), (1, 2, 5), (1, 1, 5), (2, 0, 5), (3, 1, 6)"
            ),
        );
    }
    let expected = |rows: [(&str, &str); 4]| rows.map(|(a, b)| (a.to_owned(), b.to_owned()));
    assert_eq!(
        pairs(&mut adapter, "SELECT a, b FROM c WHERE k = 5"),
        expected([("1", "1"), ("1", "2"), ("2", "0"), ("2", "1")])
    );
    assert_eq!(
        pairs(&mut adapter, "SELECT a, b FROM c2 WHERE k = 5"),
        expected([("2", "0"), ("1", "1"), ("2", "1"), ("1", "2")])
    );
}

#[test]
fn a_text_primary_key_orders_equal_values_without_regard_to_case() {
    let (_directory, mut adapter) = adapter();
    run(
        &mut adapter,
        "CREATE TABLE s (id VARCHAR(10) NOT NULL PRIMARY KEY, k INT, KEY s_k (k))",
    );
    run(
        &mut adapter,
        "INSERT INTO s (id, k) VALUES ('b', 5), ('A', 5), ('a2', 5), ('C', 5)",
    );
    assert_eq!(
        firsts(&mut adapter, "SELECT id FROM s WHERE k = 5"),
        ["A", "a2", "b", "C"]
    );
}

#[test]
fn an_index_shows_only_the_columns_it_was_declared_with() {
    let (_directory, mut adapter) = adapter();
    table_with_equal_keys(&mut adapter);
    run(&mut adapter, "CREATE INDEX q_kid ON q (k, id)");
    run(&mut adapter, "ALTER TABLE q ADD INDEX q_v (v)");
    assert_eq!(
        rows(&mut adapter, "SHOW CREATE TABLE q")[0][1].as_deref(),
        Some(
            "CREATE TABLE `q` (\n  `id` int NOT NULL,\n  `k` int DEFAULT NULL,\n  `v` int DEFAULT NULL,\n  PRIMARY KEY (`id`),\n  KEY `q_k` (`k`),\n  KEY `q_kid` (`k`,`id`),\n  KEY `q_v` (`v`)\n) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_0900_ai_ci"
        )
    );
    assert_eq!(
        key_columns(&mut adapter, "q"),
        ["PRIMARY.id", "q_k.k", "q_kid.k", "q_kid.id", "q_v.v",]
    );
    assert_eq!(
        rows(
            &mut adapter,
            "SELECT INDEX_NAME, SEQ_IN_INDEX, COLUMN_NAME FROM information_schema.STATISTICS \
             WHERE TABLE_SCHEMA = 'reports' AND TABLE_NAME = 'q' \
             ORDER BY INDEX_NAME, SEQ_IN_INDEX"
        ),
        [
            ["PRIMARY", "1", "id"],
            ["q_k", "1", "k"],
            ["q_kid", "1", "k"],
            ["q_kid", "2", "id"],
            ["q_v", "1", "v"],
        ]
        .map(|row| row.map(|value| Some(value.to_owned())).to_vec())
    );
}

#[test]
fn an_index_written_again_keeps_its_columns_and_its_order() {
    let (_directory, mut adapter) = adapter();
    table_with_equal_keys(&mut adapter);
    run(&mut adapter, "ALTER TABLE q RENAME INDEX q_k TO q_k2");
    run(&mut adapter, "ALTER TABLE q ADD COLUMN w INT");
    assert_eq!(key_columns(&mut adapter, "q"), ["PRIMARY.id", "q_k2.k"]);
    assert_eq!(
        firsts(&mut adapter, "SELECT id FROM q WHERE k = 5"),
        ["10", "20", "30"]
    );
    run(&mut adapter, "TRUNCATE TABLE q");
    for row in ["(30, 5, 0)", "(10, 5, 0)"] {
        run(
            &mut adapter,
            &format!("INSERT INTO q (id, k, v) VALUES {row}"),
        );
    }
    assert_eq!(
        firsts(&mut adapter, "SELECT id FROM q WHERE k = 5"),
        ["10", "30"]
    );
    assert_eq!(
        rows(&mut adapter, "CHECK TABLE q")[0][3].as_deref(),
        Some("OK")
    );
}

#[test]
fn a_foreign_key_over_a_plain_index_and_the_primary_key_gets_an_index_of_its_own() {
    let (_directory, mut adapter) = adapter();
    run(
        &mut adapter,
        "CREATE TABLE pa (a INT NOT NULL, b INT NOT NULL, PRIMARY KEY (a, b))",
    );
    run(
        &mut adapter,
        "CREATE TABLE ch (id INT NOT NULL PRIMARY KEY, k INT, KEY ch_k (k), \
         CONSTRAINT fk2 FOREIGN KEY (k, id) REFERENCES pa (a, b))",
    );
    assert_eq!(
        key_columns(&mut adapter, "ch"),
        ["PRIMARY.id", "ch_k.k", "fk2.k", "fk2.id"]
    );
}
