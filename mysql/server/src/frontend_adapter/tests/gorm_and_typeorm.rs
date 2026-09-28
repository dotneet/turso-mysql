//! Statements GORM and TypeORM send, replayed as each one sent them in the
//! framework harness's third run.
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

fn result_set(adapter: &mut Adapter, sql: &str) -> TextResultSet {
    match adapter.execute_query(sql) {
        Ok(CommandExecutionResult::ResultSet(result)) => result,
        other => panic!("{sql} must return a result set, answered {other:?}"),
    }
}

fn rows(adapter: &mut Adapter, sql: &str) -> Vec<Vec<Option<String>>> {
    result_set(adapter, sql)
        .rows
        .into_iter()
        .map(|row| {
            row.into_iter()
                .map(|value| value.map(|value| String::from_utf8(value).unwrap()))
                .collect()
        })
        .collect()
}

const GORM_USERS: &str = "CREATE TABLE `users` (`id` bigint unsigned AUTO_INCREMENT,`email` varchar(191) NOT NULL,`name` varchar(100) NOT NULL,`balance` decimal(10,2) NOT NULL DEFAULT 0,`is_active` boolean NOT NULL,`profile` JSON,`created_at` datetime(3) NULL,`updated_at` datetime(3) NULL,PRIMARY KEY (`id`),UNIQUE INDEX `idx_users_email` (`email`))";
const GORM_POSTS: &str = "CREATE TABLE `posts` (`id` bigint unsigned AUTO_INCREMENT,`user_id` bigint unsigned NOT NULL,`title` varchar(200) NOT NULL,`body` text,`published_at` datetime(3) NULL,`views` bigint NOT NULL DEFAULT 0,`created_at` datetime(3) NULL,`updated_at` datetime(3) NULL,PRIMARY KEY (`id`),INDEX `idx_posts_user_id` (`user_id`),CONSTRAINT `fk_users_posts` FOREIGN KEY (`user_id`) REFERENCES `users`(`id`) ON DELETE CASCADE)";

fn gorm_posts(adapter: &mut Adapter) {
    run(adapter, GORM_USERS);
    run(adapter, GORM_POSTS);
    run(
        adapter,
        "INSERT INTO `users` (`email`,`name`,`is_active`) VALUES ('a@x', 'A', 1), ('b@x', 'B', 1), ('c@x', 'C', 0)",
    );
    run(
        adapter,
        "INSERT INTO `posts` (`user_id`,`title`) VALUES (1, 'a'), (1, 'b'), (2, 'c'), (1, 'd')",
    );
}

/// GORM's `Distinct("user_id").Count(&n)` puts the column in parentheses,
/// which count what the bare column counts and name the column as written.
#[test]
fn gorms_distinct_count_names_its_column_in_parentheses() {
    let (_directory, mut adapter) = adapter();
    gorm_posts(&mut adapter);
    for (sql, name, count) in [
        (
            "SELECT COUNT(DISTINCT(`user_id`)) FROM `posts`",
            "COUNT(DISTINCT(`user_id`))",
            "2",
        ),
        (
            "SELECT COUNT(DISTINCT(posts.user_id)) FROM posts",
            "COUNT(DISTINCT(posts.user_id))",
            "2",
        ),
        (
            "SELECT COUNT( DISTINCT ( `user_id` ) ) FROM `posts`",
            "COUNT( DISTINCT ( `user_id` ) )",
            "2",
        ),
        (
            "SELECT COUNT((user_id)) FROM posts",
            "COUNT((user_id))",
            "4",
        ),
    ] {
        let result = result_set(&mut adapter, sql);
        let [column] = result.columns.as_slice() else {
            panic!("{sql} must answer one column");
        };
        assert_eq!(column.name, name, "{sql}");
        assert_eq!(column.column_type, MYSQL_TYPE_LONGLONG, "{sql}");
        assert_eq!(column.column_length, 21, "{sql}");
        assert_eq!(column.flags, 0x8081, "{sql}: NOT_NULL BINARY NUM");
        assert_eq!(result.rows, vec![vec![Some(count.as_bytes().to_vec())]]);
    }
    assert_eq!(
        rows(
            &mut adapter,
            "SELECT COUNT(DISTINCT(`user_id`)) FROM `posts` WHERE `title` = 'none'"
        ),
        vec![vec![Some("0".to_owned())]]
    );
}
