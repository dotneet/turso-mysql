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

/// The affected rows and the id one write reports.
fn written(adapter: &mut Adapter, sql: &str) -> (u64, u64) {
    match adapter.execute_query(sql) {
        Ok(CommandExecutionResult::Ok(result)) => (result.affected_rows, result.last_insert_id),
        other => panic!("{sql} must answer OK, answered {other:?}"),
    }
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

fn refused(adapter: &mut Adapter, sql: &str) {
    assert!(
        matches!(
            adapter.execute_query(sql),
            Err(FrontendErrorKind::Unsupported)
        ),
        "{sql} must be refused"
    );
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

/// GORM's alter migration backfills a new column with `CONCAT('post-', id)`
/// over its `BIGINT UNSIGNED` id.
#[test]
fn gorms_backfill_writes_each_unsigned_id_out_as_a_word() {
    let (_directory, mut adapter) = adapter();
    gorm_posts(&mut adapter);
    run(&mut adapter, "ALTER TABLE `posts` ADD `slug` varchar(200)");
    assert_eq!(
        written(&mut adapter, "UPDATE posts SET slug = CONCAT('post-', id)"),
        (4, 0)
    );
    assert_eq!(
        rows(
            &mut adapter,
            "SELECT `id`, `slug` FROM `posts` ORDER BY `id`"
        ),
        (1..=4)
            .map(|id| vec![Some(id.to_string()), Some(format!("post-{id}"))])
            .collect::<Vec<_>>()
    );
    assert_eq!(
        written(
            &mut adapter,
            "UPDATE posts SET slug = CONCAT('post-', id) WHERE id = 1"
        ),
        (0, 0)
    );

    run(
        &mut adapter,
        "CREATE TABLE numbers (id int PRIMARY KEY, big bigint unsigned, whole decimal(10,0), amount decimal(10,2), n int, word varchar(64))",
    );
    run(
        &mut adapter,
        "INSERT INTO numbers (id, big, whole, amount) VALUES (1, 18446744073709551615, -3, 1.50), (2, 0, NULL, NULL)",
    );
    assert_eq!(
        written(
            &mut adapter,
            "UPDATE numbers SET word = CONCAT('b-', big, '-', whole)"
        ),
        (1, 0)
    );
    assert_eq!(
        rows(&mut adapter, "SELECT word FROM numbers ORDER BY id"),
        vec![
            vec![Some("b-18446744073709551615--3".to_owned())],
            vec![None],
        ]
    );
    // A `DECIMAL` with places is written out by a rule not checked here, a
    // number into a column of numbers is a conversion, and arithmetic over
    // the id is held apart from a plain column.
    refused(
        &mut adapter,
        "UPDATE numbers SET word = CONCAT('a-', amount)",
    );
    refused(&mut adapter, "UPDATE numbers SET n = CONCAT('1', big)");
    refused(
        &mut adapter,
        "UPDATE numbers SET word = CONCAT('a-', big + 1)",
    );
}

const TYPEORM_POSTS: &str = "CREATE TABLE `posts` (`id` bigint NOT NULL AUTO_INCREMENT, `user_id` bigint NOT NULL, `title` varchar(200) NOT NULL, `body` text NULL, `published_at` datetime NULL, `views` int NOT NULL DEFAULT '0', PRIMARY KEY (`id`)) ENGINE=InnoDB";
const TYPEORM_TAGS: &str = "CREATE TABLE `tags` (`id` bigint NOT NULL AUTO_INCREMENT, `name` varchar(100) NOT NULL, UNIQUE INDEX `IDX_d90243459a697eadb8ad56e909` (`name`), PRIMARY KEY (`id`)) ENGINE=InnoDB";
const TYPEORM_POST_TAG: &str = "CREATE TABLE `post_tag` (`post_id` bigint NOT NULL, `tag_id` bigint NOT NULL, INDEX `IDX_b5ec92f15aaa1e371f2662f681` (`post_id`), INDEX `IDX_d2fd5340bb68556fe93650fedc` (`tag_id`), PRIMARY KEY (`post_id`, `tag_id`)) ENGINE=InnoDB";

/// TypeORM loads a many-to-many relation before it deletes a row, writing the
/// id it holds as a word inside the join's `ON`, and its pagination reads the
/// page's ids back as words in an `IN` over a `LEFT JOIN`.
#[test]
fn typeorms_relation_loading_compares_a_joined_id_with_a_word() {
    let (_directory, mut adapter) = adapter();
    for sql in [TYPEORM_POSTS, TYPEORM_TAGS, TYPEORM_POST_TAG] {
        run(&mut adapter, sql);
    }
    run(
        &mut adapter,
        "INSERT INTO posts (user_id, title) VALUES (1, 'a'), (1, 'b')",
    );
    run(
        &mut adapter,
        "INSERT INTO tags (name) VALUES ('x'), ('y'), ('z'), ('w')",
    );
    run(
        &mut adapter,
        "INSERT INTO post_tag (post_id, tag_id) VALUES (1, 4), (2, 4), (1, 1)",
    );
    assert_eq!(
        rows(
            &mut adapter,
            "SELECT `Tag_posts_rid`.`post_id` AS `post_id`, `Tag_posts_rid`.`tag_id` AS `tag_id` FROM `posts` `posts` INNER JOIN `post_tag` `Tag_posts_rid` ON (`Tag_posts_rid`.`tag_id` = '4' AND `Tag_posts_rid`.`post_id` = `posts`.`id`) ORDER BY `Tag_posts_rid`.`post_id` ASC, `Tag_posts_rid`.`tag_id` ASC"
        ),
        vec![
            vec![Some("1".to_owned()), Some("4".to_owned())],
            vec![Some("2".to_owned()), Some("4".to_owned())],
        ]
    );
    assert_eq!(
        rows(
            &mut adapter,
            "SELECT `t`.`post_id` FROM `posts` `posts` INNER JOIN `post_tag` `t` ON `t`.`post_id` = `posts`.`id` WHERE `t`.`tag_id` = '04' ORDER BY `t`.`post_id`"
        ),
        vec![vec![Some("1".to_owned())], vec![Some("2".to_owned())]]
    );
    assert_eq!(
        rows(
            &mut adapter,
            "SELECT `Post`.`id` AS `Post_id`, `Post`.`title` AS `Post_title`, `pt`.`tag_id` FROM `posts` `Post` LEFT JOIN `post_tag` `pt` ON `pt`.`post_id`=`Post`.`id` WHERE `Post`.`id` IN ('1') ORDER BY `Post`.`id` ASC, `pt`.`tag_id` ASC"
        ),
        vec![
            vec![Some("1".to_owned()), Some("a".to_owned()), Some("1".to_owned())],
            vec![Some("1".to_owned()), Some("a".to_owned()), Some("4".to_owned())],
        ]
    );
    assert_eq!(
        written(&mut adapter, "DELETE FROM `post_tag` WHERE `tag_id` = '4'"),
        (2, 0)
    );
    // MySQL reads these as doubles, `'4x'` with warning 1292, by rules a
    // statement over one table refuses too.
    refused(
        &mut adapter,
        "SELECT `t`.`post_id` FROM `posts` `posts` INNER JOIN `post_tag` `t` ON `t`.`post_id` = `posts`.`id` WHERE `t`.`tag_id` = '4.0'",
    );
    refused(
        &mut adapter,
        "SELECT `t`.`post_id` FROM `posts` `posts` INNER JOIN `post_tag` `t` ON `t`.`post_id` = `posts`.`id` WHERE `t`.`tag_id` = '4x'",
    );
    // A name holding whole numbers in one table and words in another would
    // be read one way for both.
    run(
        &mut adapter,
        "CREATE TABLE labels (id int PRIMARY KEY, tag_id varchar(10))",
    );
    refused(
        &mut adapter,
        "SELECT `t`.`post_id` FROM `post_tag` `t` JOIN `labels` `l` ON `l`.`id` = `t`.`post_id` WHERE `t`.`tag_id` = '4'",
    );
}

/// GORM's association reads join on a `BIGINT UNSIGNED` id, which a word
/// naming the id finds too.
#[test]
fn a_word_naming_an_unsigned_id_finds_it_through_a_join() {
    let (_directory, mut adapter) = adapter();
    run(
        &mut adapter,
        "CREATE TABLE `tags` (`id` bigint unsigned AUTO_INCREMENT,`name` varchar(64) NOT NULL,PRIMARY KEY (`id`),UNIQUE INDEX `idx_tags_name` (`name`))",
    );
    run(
        &mut adapter,
        "CREATE TABLE `post_tags` (`post_id` bigint unsigned,`tag_id` bigint unsigned,PRIMARY KEY (`post_id`,`tag_id`))",
    );
    run(
        &mut adapter,
        "INSERT INTO tags (name) VALUES ('go'), ('sql')",
    );
    run(
        &mut adapter,
        "INSERT INTO post_tags (post_id, tag_id) VALUES (1, 1), (1, 2), (2, 2)",
    );
    assert_eq!(
        rows(
            &mut adapter,
            "SELECT `tags`.`id`,`tags`.`name` FROM `tags` JOIN `post_tags` ON `post_tags`.`tag_id` = `tags`.`id` AND `post_tags`.`post_id` = '1' ORDER BY `tags`.`id`"
        ),
        vec![
            vec![Some("1".to_owned()), Some("go".to_owned())],
            vec![Some("2".to_owned()), Some("sql".to_owned())],
        ]
    );
}
