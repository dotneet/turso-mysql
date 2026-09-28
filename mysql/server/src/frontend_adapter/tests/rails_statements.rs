//! The statements Active Record writes for ordinary reads and saves, as its
//! MySQL adapter spells them: every boolean as `TRUE` or `FALSE`, and every
//! column an `UPDATE` saves qualified by its table.

use super::*;

fn adapter() -> (
    tempfile::TempDir,
    AuthorizedDatabaseCommandAdapter<RecordingAuthorizer>,
) {
    let authorizer = Arc::new(RecordingAuthorizer::default());
    let (directory, _catalog, factory) = catalog_factory(authorizer);
    let mut adapter = factory
        .build(AuthenticatedPrincipal::from_account_id_for_testing(
            AccountId::from_bytes([131; 32]),
        ))
        .unwrap();
    adapter.authorize_connection().unwrap();
    adapter.execute_init_db("REPORTS").unwrap();
    for sql in [
        "CREATE TABLE `users` (`id` bigint NOT NULL AUTO_INCREMENT PRIMARY KEY, `name` varchar(255), `admin` tinyint(1) DEFAULT 0 NOT NULL, `created_at` datetime(6) NOT NULL, `updated_at` datetime(6) NOT NULL)",
        "INSERT INTO `users` (`name`, `admin`, `created_at`, `updated_at`) VALUES ('a', TRUE, '2026-09-27 00:00:00', '2026-09-27 00:00:00')",
        "INSERT INTO `users` (`name`, `admin`, `created_at`, `updated_at`) VALUES ('b', FALSE, '2026-09-27 00:00:00', '2026-09-27 00:00:00')",
    ] {
        adapter
            .execute_query(sql)
            .unwrap_or_else(|error| panic!("{sql}: {error:?}"));
    }
    (directory, adapter)
}

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

fn affected(adapter: &mut AuthorizedDatabaseCommandAdapter<RecordingAuthorizer>, sql: &str) -> u64 {
    match adapter.execute_query(sql) {
        Ok(CommandExecutionResult::Ok(result)) => result.affected_rows,
        other => panic!("{sql}: {other:?}"),
    }
}

/// MySQL's `TRUE` and `FALSE` are the integers 1 and 0.
#[test]
fn a_boolean_is_compared_as_the_integer_it_is() {
    let (_directory, mut adapter) = adapter();
    assert_eq!(
        names(
            &mut adapter,
            "SELECT `users`.`name` FROM `users` WHERE `users`.`admin` = FALSE"
        ),
        ["b"]
    );
    assert_eq!(
        names(
            &mut adapter,
            "SELECT `users`.`name` FROM `users` WHERE `users`.`admin` IN (TRUE, FALSE) ORDER BY `users`.`id` ASC"
        ),
        ["a", "b"]
    );
    assert_eq!(
        affected(
            &mut adapter,
            "UPDATE `users` SET `users`.`admin` = TRUE WHERE `users`.`admin` = FALSE"
        ),
        1
    );
    assert_eq!(
        affected(
            &mut adapter,
            "DELETE FROM `users` WHERE `users`.`admin` = TRUE AND `users`.`name` = 'b'"
        ),
        1
    );
    // A word against a boolean is the coercion every word against a number is.
    assert_eq!(
        adapter.execute_query("SELECT `users`.`id` FROM `users` WHERE `users`.`name` = TRUE"),
        Err(FrontendErrorKind::Unsupported)
    );
}

/// `record.save` on a loaded record.
#[test]
fn a_saved_column_qualified_by_its_table_is_that_column() {
    let (_directory, mut adapter) = adapter();
    assert_eq!(
        affected(
            &mut adapter,
            "UPDATE `users` SET `users`.`name` = 'c', `users`.`updated_at` = '2026-09-27 00:00:01.000000' WHERE `users`.`id` = 1"
        ),
        1
    );
    assert_eq!(
        names(
            &mut adapter,
            "SELECT `users`.`name` FROM `users` WHERE `users`.`id` = 1 LIMIT 1"
        ),
        ["c"]
    );
    // Another table's name is not a column of this one. Measured on MySQL
    // 8.4.11: `Unknown column 'posts.name' in 'field list'`.
    assert_eq!(
        adapter.execute_query("UPDATE `users` SET `posts`.`name` = 'd' WHERE `users`.`id` = 1"),
        Err(FrontendErrorKind::UnknownColumn)
    );
    assert_eq!(
        adapter.take_error_message(),
        Some(b"Unknown column 'posts.name' in 'field list'".to_vec())
    );
}
