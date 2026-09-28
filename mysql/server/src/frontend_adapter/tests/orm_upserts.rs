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

/// Two sessions on one database, the second one asking for the rows an
/// update matched rather than the ones it changed, as GORM's driver can.
fn adapter_and_one_counting_found_rows() -> (tempfile::TempDir, Adapter, Adapter) {
    let authorizer = Arc::new(RecordingAuthorizer::default());
    let (directory, catalog, factory) = catalog_factory(authorizer.clone());
    let principal =
        || AuthenticatedPrincipal::from_account_id_for_testing(AccountId::from_bytes([151; 32]));
    let mut adapter = factory.build(principal()).unwrap();
    let mut found_rows =
        AuthorizedDatabaseAdapterFactory::new(catalog, binary_context(), authorizer)
            .build_with_options(
                principal(),
                CommandExecutionOptions::from_capability_flags(CLIENT_FOUND_ROWS),
            )
            .unwrap();
    for adapter in [&mut adapter, &mut found_rows] {
        adapter.authorize_connection().unwrap();
        adapter.execute_init_db("REPORTS").unwrap();
    }
    (directory, adapter, found_rows)
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

/// GORM spells `clause.OnConflict{DoNothing: true}` on MySQL as writing the
/// counted id to itself, and prepares every statement. Measured over GORM's
/// own `BIGINT UNSIGNED` table with go-sql-driver, one statement after the
/// other on one table, the `CLIENT_FOUND_ROWS` session last.
#[test]
fn gorms_do_nothing_upsert_writes_the_counted_id_to_itself() {
    let (_directory, mut adapter, mut found_rows) = adapter_and_one_counting_found_rows();
    run(
        &mut adapter,
        "CREATE TABLE `tags` (`id` bigint unsigned AUTO_INCREMENT,`name` varchar(64) NOT NULL,PRIMARY KEY (`id`),UNIQUE INDEX `idx_tags_name` (`name`))",
    );
    let do_nothing = |adapter: &mut Adapter, names: &[&str]| {
        let rows = vec!["(?)"; names.len()].join(",");
        let statement = adapter
            .execute_stmt_prepare(&format!(
                "INSERT INTO `tags` (`name`) VALUES {rows} ON DUPLICATE KEY UPDATE `id`=`id`"
            ))
            .unwrap();
        assert_eq!(statement.warnings, 0);
        let written = executed(adapter, statement.statement_id, &words(names));
        adapter.execute_stmt_close(statement.statement_id);
        written
    };
    assert_eq!(do_nothing(&mut adapter, &["go"]), (1, 1, 0));
    // A row the clause leaves as it stood counts nothing and reports no id,
    // and the number it asked for is spent.
    assert_eq!(do_nothing(&mut adapter, &["go"]), (0, 0, 0));
    assert_eq!(
        rows(&mut adapter, "SELECT LAST_INSERT_ID()"),
        vec![some(&["1"])]
    );
    assert_eq!(do_nothing(&mut adapter, &["go", "x1"]), (1, 3, 0));
    let update_all = adapter
        .execute_stmt_prepare(
            "INSERT INTO `tags` (`name`) VALUES (?) ON DUPLICATE KEY UPDATE `name`=VALUES(`name`)",
        )
        .unwrap();
    assert_eq!(
        executed(&mut adapter, update_all.statement_id, &words(&["sql"])),
        (1, 5, 0)
    );
    assert_eq!(
        executed(&mut adapter, update_all.statement_id, &words(&["sql"])),
        (0, 0, 0)
    );
    assert_eq!(
        executed(&mut adapter, update_all.statement_id, &words(&["SQL"])),
        (2, 5, 0)
    );
    adapter.execute_stmt_close(update_all.statement_id);

    // Counting the rows it found, a row left as it stood counts 1 and still
    // reports no id.
    assert_eq!(do_nothing(&mut found_rows, &["go"]), (1, 0, 0));
    assert_eq!(do_nothing(&mut found_rows, &["new1"]), (1, 9, 0));
    assert_eq!(do_nothing(&mut found_rows, &["go", "new2"]), (2, 10, 0));
    assert_eq!(do_nothing(&mut found_rows, &["go", "sql"]), (2, 0, 0));
    assert_eq!(
        rows(&mut adapter, "SELECT id, name FROM tags ORDER BY id"),
        vec![
            some(&["1", "go"]),
            some(&["3", "x1"]),
            some(&["5", "SQL"]),
            some(&["9", "new1"]),
            some(&["10", "new2"]),
        ]
    );
    // Writing the id anything else is still refused.
    assert_eq!(
        adapter.execute_query(
            "INSERT INTO `tags` (`name`) VALUES ('go') ON DUPLICATE KEY UPDATE `id`=`id` + 1"
        ),
        Err(FrontendErrorKind::Unsupported)
    );
}

/// The text form, over a counted table whose id is the engine's own row
/// number, as a signed `BIGINT` is.
#[test]
fn a_counted_id_written_to_itself_leaves_a_signed_table_as_it_stood() {
    let (_directory, mut adapter) = adapter();
    run(&mut adapter, RAILS_TAGS);
    let do_nothing = |rows: &str| {
        format!("INSERT INTO `tags` (`name`) VALUES {rows} ON DUPLICATE KEY UPDATE `id`=`id`")
    };
    assert_eq!(written(&mut adapter, &do_nothing("('go')")), (1, 1));
    assert_eq!(written(&mut adapter, &do_nothing("('go')")), (0, 0));
    assert_eq!(written(&mut adapter, &do_nothing("('go'), ('x1')")), (1, 3));
    // Rails' `insert_all` writes its first column to itself, qualified.
    assert_eq!(
        written(
            &mut adapter,
            "INSERT INTO `tags` (`id`, `name`) VALUES (DEFAULT, 'go'), (DEFAULT, 'x2') AS `tags_values` ON DUPLICATE KEY UPDATE `id`=`tags`.`id`"
        ),
        (1, 5)
    );
    assert_eq!(
        rows(&mut adapter, "SELECT id, name FROM tags ORDER BY id"),
        vec![some(&["1", "go"]), some(&["3", "x1"]), some(&["5", "x2"])]
    );
}

const RAILS_USERS: &str = "CREATE TABLE `users` (`id` bigint NOT NULL AUTO_INCREMENT PRIMARY KEY, `email` varchar(255) NOT NULL, `name` varchar(100) NOT NULL, `balance` decimal(10,2) DEFAULT 0.0 NOT NULL, `is_active` tinyint(1) DEFAULT TRUE NOT NULL, `profile` json, `created_at` datetime(6) NOT NULL, `updated_at` datetime(6) NOT NULL, UNIQUE INDEX `index_users_on_email` (`email`)) DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_0900_ai_ci";

/// Rails 8's `insert!` and `insert_all` stamp `created_at` and `updated_at`
/// with `CURRENT_TIMESTAMP(6)`, which MySQL reads once for the whole
/// statement. The engine's clock reads to the millisecond, so the places past
/// the third are zeros.
#[test]
fn rails_stamps_a_row_with_one_moment_to_the_microsecond() {
    let (_directory, mut adapter) = adapter();
    run(&mut adapter, RAILS_USERS);
    let insert = "INSERT INTO `users` (`email`,`name`,`created_at`,`updated_at`) VALUES ('alice@example.com', 'Again', CURRENT_TIMESTAMP(6), CURRENT_TIMESTAMP(6)) AS `users_values`";
    assert_eq!(written(&mut adapter, insert), (1, 1));
    let stamped = rows(&mut adapter, "SELECT created_at, updated_at FROM users");
    let created = stamped[0][0].clone().unwrap();
    assert_eq!(stamped[0][1].as_deref(), Some(created.as_str()));
    assert_eq!(created.len(), 26, "{created}");
    assert!(created.ends_with("000"), "{created}");
    // Rails' `insert!` of a duplicate is how it finds one.
    assert_eq!(
        adapter.execute_query(insert),
        Err(FrontendErrorKind::ConstraintViolation)
    );
    // Several rows, written one at a time beside an upsert, read one moment
    // between them. Measured: the new row takes 3, the duplicate before
    // having spent 2.
    assert_eq!(
        written(
            &mut adapter,
            "INSERT INTO `users` (`email`,`name`,`created_at`,`updated_at`) VALUES ('alice@example.com', 'A2', CURRENT_TIMESTAMP(6), CURRENT_TIMESTAMP(6)), ('bob@example.com', 'B', CURRENT_TIMESTAMP(6), CURRENT_TIMESTAMP(6)) AS v ON DUPLICATE KEY UPDATE name = v.name, updated_at = v.updated_at"
        ),
        (3, 3)
    );
    let bob = rows(
        &mut adapter,
        "SELECT id, name, created_at, updated_at FROM users WHERE email = 'bob@example.com'",
    );
    assert_eq!(bob[0][..2], some(&["3", "B"]));
    assert_eq!(bob[0][2], bob[0][3]);
    assert_eq!(
        rows(&mut adapter, "SELECT COUNT(DISTINCT updated_at) FROM users"),
        vec![some(&["1"])]
    );
}

/// `NOW(6)` into an ordinary table, held to each column's own places the way a
/// written moment is: measured on 8.4.11, a `DATETIME(2)` rounds it to two
/// places and a `DATETIME` to the second, and a column of words takes all
/// twenty-six characters.
#[test]
fn the_moment_to_the_microsecond_is_held_to_its_column() {
    let (_directory, mut adapter) = adapter();
    run(
        &mut adapter,
        "CREATE TABLE ev (code varchar(10) PRIMARY KEY, at6 datetime(6) NULL, at2 datetime(2) NULL, at0 datetime NULL, w varchar(40) NULL, n bigint NULL)",
    );
    assert_eq!(
        written(
            &mut adapter,
            "INSERT INTO ev (code, at6, at2, at0, w) VALUES ('a', CURRENT_TIMESTAMP(6), NOW(6), LOCALTIMESTAMP(6), NOW(6)), ('b', NOW(3), NOW(3), NOW(0), NOW(1))"
        ),
        (2, 0)
    );
    let read = rows(
        &mut adapter,
        "SELECT at6, at2, at0, w FROM ev ORDER BY code",
    );
    let [first, second] = read.as_slice() else {
        panic!("{read:?}");
    };
    let first = first
        .iter()
        .map(|value| value.clone().unwrap())
        .collect::<Vec<_>>();
    let second = second
        .iter()
        .map(|value| value.clone().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(first[0].len(), 26, "{first:?}");
    assert!(first[0].ends_with("000"), "{first:?}");
    assert_eq!(first[1].len(), 22, "{first:?}");
    assert_eq!(first[2].len(), 19, "{first:?}");
    assert_eq!(first[3], first[0]);
    // `NOW(3)` is the same moment cut to three places, `NOW(1)` to one and
    // `NOW(0)` to the second, where writing it into a `DATETIME` rounds.
    assert_eq!(second[0], first[0]);
    assert_eq!(second[1], first[1]);
    assert_eq!(second[2], first[0][..19]);
    assert_eq!(second[3], first[0][..21]);
    // The moment as a number, seven places and the time of day to places are
    // refused, as the moment itself into a number is.
    for sql in [
        "INSERT INTO ev (code, n) VALUES ('c', NOW(6))",
        "INSERT INTO ev (code, at6) VALUES ('d', NOW(7))",
        "INSERT INTO ev (code, at6) VALUES ('e', CURTIME(6))",
    ] {
        assert!(adapter.execute_query(sql).is_err(), "{sql}");
    }
    assert_eq!(
        rows(&mut adapter, "SELECT COUNT(*) FROM ev"),
        vec![some(&["2"])]
    );
}

/// Rails 8's `upsert_all`, which touches `updated_at` only for a row whose
/// named columns it changes, and names the offered row `users_values`.
/// Measured on MySQL 8.4.11 statement by statement, the found-rows session
/// last.
#[test]
fn rails_upsert_all_touches_only_the_rows_it_changes() {
    let (_directory, mut adapter, mut found_rows) = adapter_and_one_counting_found_rows();
    run(&mut adapter, RAILS_USERS);
    assert_eq!(
        written(
            &mut adapter,
            "INSERT INTO `users` (`email`,`name`,`balance`,`is_active`,`created_at`,`updated_at`) VALUES ('alice@example.com', 'Alice', 100.50, TRUE, '2026-01-01 00:00:00.000001', '2026-01-01 00:00:00.000001'), ('bob@example.com', 'Bob', 20, FALSE, '2026-01-01 00:00:00', '2026-01-01 00:00:00')"
        ),
        (2, 1)
    );
    let upsert_all = |rows: &str| {
        format!(
            "INSERT INTO `users` (`email`,`name`,`balance`,`is_active`,`created_at`,`updated_at`) VALUES {rows} AS `users_values` ON DUPLICATE KEY UPDATE updated_at=(CASE WHEN (`users`.`name`<=>`users_values`.`name` AND `users`.`balance`<=>`users_values`.`balance`) THEN `users`.updated_at ELSE CURRENT_TIMESTAMP(6) END),`name`=`users_values`.`name`,`balance`=`users_values`.`balance`"
        )
    };
    let alice_and_dave = upsert_all(
        "('alice@example.com', 'Alice Updated', 1.0, TRUE, CURRENT_TIMESTAMP(6), CURRENT_TIMESTAMP(6)), ('dave@example.com', 'Dave', 2.0, TRUE, CURRENT_TIMESTAMP(6), CURRENT_TIMESTAMP(6))",
    );
    assert_eq!(written(&mut adapter, &alice_and_dave), (3, 3));
    assert_eq!(
        rows(&mut adapter, "SELECT LAST_INSERT_ID()"),
        vec![some(&["3"])]
    );
    let read = rows(
        &mut adapter,
        "SELECT id, name, balance, created_at, updated_at FROM users ORDER BY id",
    );
    assert_eq!(read[0][..3], some(&["1", "Alice Updated", "1.00"]));
    assert_eq!(read[0][3].as_deref(), Some("2026-01-01 00:00:00.000001"));
    assert_ne!(read[0][4], read[0][3]);
    assert_eq!(
        read[1],
        some(&[
            "2",
            "Bob",
            "20.00",
            "2026-01-01 00:00:00.000000",
            "2026-01-01 00:00:00.000000"
        ])
    );
    assert_eq!(read[2][..3], some(&["3", "Dave", "2.00"]));
    // One moment for the whole statement: the row it touched and the row it
    // added.
    assert_eq!(read[2][3], read[0][4]);
    assert_eq!(read[2][4], read[0][4]);
    // The same rows again change nothing, and so touch nothing.
    assert_eq!(written(&mut adapter, &alice_and_dave), (0, 0));
    // A name differing only in case is written, and under the column's
    // collation it is the same name, so the row is not touched.
    assert_eq!(
        written(
            &mut adapter,
            &upsert_all(
                "('bob@example.com', 'BOB', 20, TRUE, CURRENT_TIMESTAMP(6), CURRENT_TIMESTAMP(6))"
            )
        ),
        (2, 2)
    );
    assert_eq!(
        rows(
            &mut adapter,
            "SELECT name, updated_at FROM users WHERE email = 'bob@example.com'"
        ),
        vec![some(&["BOB", "2026-01-01 00:00:00.000000"])]
    );
    assert_eq!(written(&mut found_rows, &alice_and_dave), (2, 0));
    assert_eq!(
        rows(&mut adapter, "SELECT COUNT(*) FROM users"),
        vec![some(&["3"])]
    );
}

/// MySQL puts the offered value into the column's type before `<=>` compares
/// it, and the engine compares it as written, so only the pairs that answer
/// alike are taken. Measured on 8.4.11: `'2026-01-01'` offered for a
/// `DATETIME` holding that midnight compares equal, and the row is left as it
/// stood.
#[test]
fn an_upsert_comparison_the_engine_answers_otherwise_is_refused() {
    let (_directory, mut adapter) = adapter();
    run(
        &mut adapter,
        "CREATE TABLE k (code varchar(10) PRIMARY KEY, d datetime NULL, name varchar(20) NULL, n int NULL, j json NULL, touched datetime(6) NULL)",
    );
    run(
        &mut adapter,
        "INSERT INTO k (code, d, name, n, touched) VALUES ('a', '2026-01-01 00:00:00', 'Ann', 5, '2000-01-01 00:00:00')",
    );
    let touch = |column: &str, offered: &str| {
        format!(
            "INSERT INTO k (code, {column}, touched) VALUES ('a', {offered}, NOW(6)) AS o ON DUPLICATE KEY UPDATE touched = (CASE WHEN (k.{column} <=> o.{column}) THEN k.touched ELSE CURRENT_TIMESTAMP(6) END), {column} = o.{column}"
        )
    };
    for sql in [
        touch("d", "'2026-01-01'"),
        touch("j", "'{\"a\":1}'"),
        // A number offered for words, and a word for a number.
        touch("name", "5"),
        touch("n", "'5'"),
    ] {
        assert_eq!(
            adapter.execute_query(&sql),
            Err(FrontendErrorKind::Unsupported),
            "{sql}"
        );
        let statement = sql.replacen("VALUES ('a', ", "VALUES (?, ", 1);
        assert!(
            adapter.execute_stmt_prepare(&statement).is_err(),
            "{statement}"
        );
    }
    // A bound value says nothing of its kind until it is bound.
    assert!(adapter.execute_stmt_prepare(&touch("name", "?")).is_err());
    assert_eq!(written(&mut adapter, &touch("name", "'ANN'")), (2, 0));
    assert_eq!(written(&mut adapter, &touch("n", "5")), (0, 0));
    assert_eq!(
        rows(&mut adapter, "SELECT name, n, touched FROM k"),
        vec![some(&["ANN", "5", "2000-01-01 00:00:00.000000"])]
    );
}

/// MySQL writes the moment into an `ON UPDATE CURRENT_TIMESTAMP` column
/// whenever an upsert changes the row, which the engine's upsert does not, so
/// such an upsert is refused unless its clause writes the column itself.
/// Measured on MySQL 8.4.11 over a row stamped in 2000: offering the same name
/// leaves it, a new name stamps it now, and a clause writing the column writes
/// what it says.
#[test]
fn an_upsert_leaving_an_on_update_column_to_mysql_is_refused() {
    let (_directory, mut adapter) = adapter();
    run(
        &mut adapter,
        "CREATE TABLE plain (code varchar(10) PRIMARY KEY, name varchar(20) NOT NULL, updated_at datetime(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6) ON UPDATE CURRENT_TIMESTAMP(6))",
    );
    run(
        &mut adapter,
        "INSERT INTO plain (code, name, updated_at) VALUES ('a', 'A', '2000-01-01 00:00:00')",
    );
    assert_eq!(
        adapter.execute_query(
            "INSERT INTO plain (code, name) VALUES ('a', 'B') ON DUPLICATE KEY UPDATE name = VALUES(name)"
        ),
        Err(FrontendErrorKind::Unsupported)
    );
    assert!(adapter
        .execute_stmt_prepare(
            "INSERT INTO plain (code, name) VALUES (?, ?) ON DUPLICATE KEY UPDATE name = VALUES(name)"
        )
        .is_err());
    assert_eq!(
        written(
            &mut adapter,
            "INSERT INTO plain (code, name) VALUES ('a', 'C') ON DUPLICATE KEY UPDATE name = VALUES(name), updated_at = '2000-01-01 00:00:00'"
        ),
        (2, 0)
    );
    assert_eq!(
        rows(&mut adapter, "SELECT code, name, updated_at FROM plain"),
        vec![some(&["a", "C", "2000-01-01 00:00:00.000000"])]
    );
    // TypeORM's `@UpdateDateColumn`, on a table counting its own ids.
    run(
        &mut adapter,
        "CREATE TABLE `users` (`id` bigint NOT NULL AUTO_INCREMENT, `email` varchar(191) NOT NULL, `name` varchar(100) NOT NULL, `updated_at` datetime(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6) ON UPDATE CURRENT_TIMESTAMP(6), UNIQUE INDEX `IDX_97672ac88f789774dd47f7c8be` (`email`), PRIMARY KEY (`id`)) ENGINE=InnoDB",
    );
    assert_eq!(
        adapter.execute_query(
            "INSERT INTO `users`(`id`, `email`, `name`, `updated_at`) VALUES (DEFAULT, 'a@x', 'A', DEFAULT), (DEFAULT, 'b@x', 'B', DEFAULT) ON DUPLICATE KEY UPDATE `email` = VALUES(`email`), `name` = VALUES(`name`)"
        ),
        Err(FrontendErrorKind::Unsupported)
    );
    assert_eq!(
        rows(&mut adapter, "SELECT COUNT(*) FROM users"),
        vec![some(&["0"])]
    );
}

/// One value bound to a prepared statement.
enum Bound<'a> {
    Id(i64),
    Word(&'a str),
}

/// The null bitmap, the new-parameters flag, and each value as a LONGLONG or
/// a VAR_STRING.
fn bound(values: &[Bound<'_>]) -> Vec<u8> {
    let mut payload = vec![0; values.len().div_ceil(8)];
    payload.push(1);
    for value in values {
        payload.extend_from_slice(&[
            match value {
                Bound::Id(_) => MYSQL_TYPE_LONGLONG,
                Bound::Word(_) => MYSQL_TYPE_VAR_STRING,
            },
            0,
        ]);
    }
    for value in values {
        match value {
            Bound::Id(id) => payload.extend_from_slice(&id.to_le_bytes()),
            Bound::Word(word) => {
                payload.push(u8::try_from(word.len()).unwrap());
                payload.extend_from_slice(word.as_bytes());
            }
        }
    }
    payload
}

fn prepared_write(adapter: &mut Adapter, sql: &str, payload: &[u8]) -> (u64, u64) {
    let statement = adapter
        .execute_stmt_prepare(sql)
        .unwrap_or_else(|error| panic!("prepare {sql}: {error:?}"));
    let result = adapter.execute_stmt_execute(statement.statement_id, payload);
    adapter.execute_stmt_close(statement.statement_id);
    match result {
        Ok(PreparedStatementExecutionResult::Ok(result)) => {
            (result.affected_rows, result.last_insert_id)
        }
        other => panic!("{sql} must answer OK, answered {other:?}"),
    }
}

/// Prisma prepares every insert with the id among its columns and binds the
/// id it writes. Measured on MySQL 8.4.11 with go-sql-driver, one statement
/// after the other: a row naming its own id reports it and leaves
/// `LAST_INSERT_ID()` alone, the counter moves past it once it is written,
/// and a row refused as a duplicate of another key moves it nowhere.
#[test]
fn a_prepared_row_naming_its_own_id_moves_the_counter_past_it() {
    let (_directory, mut adapter) = adapter();
    run(
        &mut adapter,
        "CREATE TABLE `tags` (`id` BIGINT NOT NULL AUTO_INCREMENT, `name` VARCHAR(100) NOT NULL, UNIQUE INDEX `tags_name_key`(`name`), PRIMARY KEY (`id`)) DEFAULT CHARACTER SET utf8mb4 COLLATE utf8mb4_unicode_ci",
    );
    let with_id = "INSERT INTO `tags` (`id`,`name`) VALUES (?,?)";
    let without_id = "INSERT INTO `tags` (`name`) VALUES (?)";
    let last_insert_id = |adapter: &mut Adapter| rows(adapter, "SELECT LAST_INSERT_ID()");
    assert_eq!(
        prepared_write(
            &mut adapter,
            with_id,
            &bound(&[Bound::Id(5), Bound::Word("a")])
        ),
        (1, 5)
    );
    assert_eq!(last_insert_id(&mut adapter), vec![some(&["0"])]);
    assert_eq!(
        prepared_write(&mut adapter, without_id, &bound(&[Bound::Word("b")])),
        (1, 6)
    );
    assert_eq!(
        prepared_write(
            &mut adapter,
            with_id,
            &bound(&[Bound::Id(3), Bound::Word("c")])
        ),
        (1, 3)
    );
    assert_eq!(last_insert_id(&mut adapter), vec![some(&["6"])]);
    assert_eq!(
        prepared_write(&mut adapter, without_id, &bound(&[Bound::Word("d")])),
        (1, 7)
    );
    assert_eq!(
        prepared_write(
            &mut adapter,
            "INSERT INTO `tags` (`id`,`name`) VALUES (?,?),(?,?)",
            &bound(&[
                Bound::Id(10),
                Bound::Word("e"),
                Bound::Id(9),
                Bound::Word("f")
            ])
        ),
        (2, 9)
    );
    assert_eq!(last_insert_id(&mut adapter), vec![some(&["7"])]);
    let statement = adapter.execute_stmt_prepare(with_id).unwrap();
    assert_eq!(
        adapter.execute_stmt_execute(
            statement.statement_id,
            &bound(&[Bound::Id(20), Bound::Word("a")])
        ),
        Err(FrontendErrorKind::ConstraintViolation)
    );
    adapter.execute_stmt_close(statement.statement_id);
    assert_eq!(
        prepared_write(&mut adapter, without_id, &bound(&[Bound::Word("g")])),
        (1, 11)
    );
    assert_eq!(
        rows(&mut adapter, "SELECT id, name FROM tags ORDER BY id"),
        vec![
            some(&["3", "c"]),
            some(&["5", "a"]),
            some(&["6", "b"]),
            some(&["7", "d"]),
            some(&["9", "f"]),
            some(&["10", "e"]),
            some(&["11", "g"]),
        ]
    );
    let printed = rows(&mut adapter, "SHOW CREATE TABLE `tags`")[0][1]
        .clone()
        .unwrap();
    assert!(printed.contains("AUTO_INCREMENT=12"), "{printed}");
}

/// SQLAlchemy's `insert().on_duplicate_key_update()` names the offered row
/// `new` and writes a `DECIMAL` from it, over several rows of a table counting
/// its own ids. Measured on MySQL 8.4.11: the changed row counts 2 and the new
/// one 1, the statement reports the new row's number, and the same rows again
/// count nothing.
#[test]
fn sqlalchemys_upsert_names_the_offered_row_new() {
    let (_directory, mut adapter) = adapter();
    run(
        &mut adapter,
        "CREATE TABLE users (id BIGINT NOT NULL AUTO_INCREMENT, email VARCHAR(191) NOT NULL, name VARCHAR(100) NOT NULL, balance NUMERIC(10, 2) NOT NULL, is_active BOOL NOT NULL, PRIMARY KEY (id), UNIQUE (email))",
    );
    assert_eq!(
        written(
            &mut adapter,
            "INSERT INTO users (email, name, balance, is_active) VALUES ('alice@example.com', 'Alice', 100.50, 1)"
        ),
        (1, 1)
    );
    let upsert = "INSERT INTO users (email, name, balance, is_active) VALUES ('alice@example.com', 'Alice Updated', 200.00, 1), ('erin@example.com', 'Erin', 3.00, 1) AS new ON DUPLICATE KEY UPDATE name = new.name, balance = new.balance";
    assert_eq!(written(&mut adapter, upsert), (3, 2));
    assert_eq!(
        rows(&mut adapter, "SELECT LAST_INSERT_ID()"),
        vec![some(&["2"])]
    );
    assert_eq!(written(&mut adapter, upsert), (0, 0));
    assert_eq!(
        rows(
            &mut adapter,
            "SELECT id, email, name, balance, is_active FROM users ORDER BY id"
        ),
        vec![
            some(&["1", "alice@example.com", "Alice Updated", "200.00", "1"]),
            some(&["2", "erin@example.com", "Erin", "3.00", "1"]),
        ]
    );
}
