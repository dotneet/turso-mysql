//! Statements sqlx 0.8.6 and its `sqlx` CLI send, replayed as the framework
//! harness's eighth run captured them, over text and prepared statements as
//! sqlx sends each one.
//!
//! Every expectation here was measured on MySQL 8.4.11.

use super::*;

type Adapter = AuthorizedDatabaseCommandAdapter<RecordingAuthorizer>;

const ACCOUNT: [u8; 32] = [197; 32];

fn adapter() -> (tempfile::TempDir, Adapter) {
    let authorizer = Arc::new(RecordingAuthorizer::default());
    let (directory, _catalog, factory) = catalog_factory(authorizer);
    (directory, connected(factory))
}

/// A new session over the same files, the catalog opened again from disk
/// once the session before it has let go of it.
fn reopened(directory: &tempfile::TempDir, adapter: Adapter) -> Adapter {
    drop(adapter);
    let catalog = MySqlDatabaseCatalog::open(directory.path()).unwrap();
    let factory = AuthorizedDatabaseAdapterFactory::new(
        catalog,
        binary_context(),
        Arc::new(RecordingAuthorizer::default()),
    );
    connected(factory)
}

fn connected(factory: AuthorizedDatabaseAdapterFactory<RecordingAuthorizer>) -> Adapter {
    let mut adapter = factory
        .build(AuthenticatedPrincipal::from_account_id_for_testing(
            AccountId::from_bytes(ACCOUNT),
        ))
        .unwrap();
    adapter.authorize_connection().unwrap();
    adapter.execute_init_db("REPORTS").unwrap();
    run(&mut adapter, SESSION);
    adapter
}

/// What sqlx sends first on every connection.
const SESSION: &str = "SET sql_mode=(SELECT CONCAT(@@sql_mode, ',PIPES_AS_CONCAT,NO_ENGINE_SUBSTITUTION')),time_zone='+00:00',NAMES utf8mb4 COLLATE utf8mb4_unicode_ci;";

/// The table `sqlx migrate` keeps its record in, created before every run.
const MIGRATIONS_TABLE: &str = "\nCREATE TABLE IF NOT EXISTS _sqlx_migrations (\n    version BIGINT PRIMARY KEY,\n    description TEXT NOT NULL,\n    installed_on TIMESTAMP NOT NULL DEFAULT CURRENT_TIMESTAMP,\n    success BOOLEAN NOT NULL,\n    checksum BLOB NOT NULL,\n    execution_time BIGINT NOT NULL\n);\n                ";

const RECORD_MIGRATION: &str = "\n    INSERT INTO _sqlx_migrations ( version, description, success, checksum, execution_time )\n    VALUES ( ?, ?, FALSE, ?, -1 )\n                ";

/// The app's first migration, which sqlx sends as one text query of four
/// statements.
const CREATE_BLOG: &str = "CREATE TABLE users (\n  id BIGINT UNSIGNED NOT NULL AUTO_INCREMENT PRIMARY KEY,\n  email VARCHAR(191) NOT NULL,\n  name VARCHAR(100) NOT NULL,\n  balance DECIMAL(10,2) NOT NULL DEFAULT 0.00,\n  is_active BOOLEAN NOT NULL DEFAULT TRUE,\n  profile JSON NULL,\n  avatar BLOB NULL,\n  created_at DATETIME(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),\n  updated_at TIMESTAMP(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6) ON UPDATE CURRENT_TIMESTAMP(6),\n  UNIQUE KEY users_email (email)\n);\n\nCREATE TABLE posts (\n  id BIGINT UNSIGNED NOT NULL AUTO_INCREMENT PRIMARY KEY,\n  user_id BIGINT UNSIGNED NOT NULL,\n  title VARCHAR(200) NOT NULL,\n  body TEXT NULL,\n  published_at DATETIME NULL,\n  views INT NOT NULL DEFAULT 0,\n  INDEX posts_user_published (user_id, published_at),\n  CONSTRAINT posts_user FOREIGN KEY (user_id) REFERENCES users (id) ON DELETE CASCADE\n);\n\nCREATE TABLE tags (\n  id BIGINT UNSIGNED NOT NULL AUTO_INCREMENT PRIMARY KEY,\n  name VARCHAR(100) NOT NULL,\n  UNIQUE KEY tags_name (name)\n);\n\nCREATE TABLE post_tags (\n  post_id BIGINT UNSIGNED NOT NULL,\n  tag_id BIGINT UNSIGNED NOT NULL,\n  PRIMARY KEY (post_id, tag_id),\n  CONSTRAINT post_tags_post FOREIGN KEY (post_id) REFERENCES posts (id) ON DELETE CASCADE,\n  CONSTRAINT post_tags_tag FOREIGN KEY (tag_id) REFERENCES tags (id) ON DELETE CASCADE\n);\n";

/// The app's second migration and its revert.
const ADD_SLUG: &str = "ALTER TABLE posts\n  ADD COLUMN slug VARCHAR(220) NULL AFTER title,\n  RENAME COLUMN views TO view_count,\n  MODIFY title VARCHAR(255) NOT NULL;\nCREATE UNIQUE INDEX posts_slug ON posts (slug);\n";
const DROP_SLUG: &str = "DROP INDEX posts_slug ON posts;\nALTER TABLE posts\n  DROP COLUMN slug,\n  RENAME COLUMN view_count TO views,\n  MODIFY title VARCHAR(200) NOT NULL;\n";

/// Runs a migration's statements one after another, the way the server
/// splits the one text query sqlx sends them in. None of them holds a `;`
/// inside a word.
fn migrate(adapter: &mut Adapter, script: &str) {
    for statement in script.split(';').filter(|text| !text.trim().is_empty()) {
        run(adapter, statement);
    }
}

/// One value as sqlx 0.8.6 binds it.
#[derive(Clone, Copy, Debug)]
enum Bound<'a> {
    /// `&str`, bound as `VAR_STRING`.
    Word(&'a str),
    /// `i64`, bound as `LONGLONG`.
    Whole(i64),
    /// `&[u8]`, bound as `BLOB`.
    Bytes(&'a [u8]),
}

/// The null bitmap, the new-parameters flag, the types and the values, or
/// nothing at all for a statement without parameters, as sqlx sends it.
fn payload(values: &[Bound<'_>]) -> Vec<u8> {
    if values.is_empty() {
        return Vec::new();
    }
    let mut payload = vec![0; values.len().div_ceil(8)];
    payload.push(1);
    for value in values {
        let code = match value {
            Bound::Word(_) => MYSQL_TYPE_VAR_STRING,
            Bound::Whole(_) => MYSQL_TYPE_LONGLONG,
            Bound::Bytes(_) => MYSQL_TYPE_BLOB,
        };
        payload.extend_from_slice(&[code, 0]);
    }
    for value in values {
        match value {
            Bound::Word(text) => lenenc(&mut payload, text.as_bytes()),
            Bound::Bytes(bytes) => lenenc(&mut payload, bytes),
            Bound::Whole(number) => payload.extend_from_slice(&number.to_le_bytes()),
        }
    }
    payload
}

fn lenenc(payload: &mut Vec<u8>, bytes: &[u8]) {
    if let Ok(length) = u8::try_from(bytes.len()) {
        if length < 251 {
            payload.push(length);
            payload.extend_from_slice(bytes);
            return;
        }
    }
    payload.push(0xfc);
    payload.extend_from_slice(&u16::try_from(bytes.len()).unwrap().to_le_bytes());
    payload.extend_from_slice(bytes);
}

fn prepared(
    adapter: &mut Adapter,
    sql: &str,
    values: &[Bound<'_>],
) -> Result<PreparedStatementExecutionResult, FrontendErrorKind> {
    let statement = adapter.execute_stmt_prepare(sql)?;
    let result = adapter.execute_stmt_execute(statement.statement_id, &payload(values));
    adapter.execute_stmt_close(statement.statement_id);
    result
}

fn prepared_rows(adapter: &mut Adapter, sql: &str, values: &[Bound<'_>]) -> BinaryResultSet {
    match prepared(adapter, sql, values) {
        Ok(PreparedStatementExecutionResult::ResultSet(result)) => result,
        other => panic!("{sql} must answer rows, answered {other:?}"),
    }
}

fn prepared_changes(adapter: &mut Adapter, sql: &str, values: &[Bound<'_>]) -> CommandOkResult {
    match prepared(adapter, sql, values) {
        Ok(PreparedStatementExecutionResult::Ok(ok)) => ok,
        other => panic!("{sql} must answer OK, answered {other:?}"),
    }
}

fn run(adapter: &mut Adapter, sql: &str) {
    adapter
        .execute_query(sql)
        .unwrap_or_else(|error| panic!("{sql}: {error:?}"));
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

/// `sqlx migrate run` keeps its record under a signed `BIGINT PRIMARY KEY`,
/// the version being the migration's timestamp. MySQL takes a signed key as
/// it takes an `INT` one: a negative version is a version like any other, a
/// second row with the same one is 1062 and a row without one is 1364.
#[test]
fn sqlx_keeps_its_migrations_under_a_signed_bigint_key() {
    let (directory, mut adapter) = adapter();
    run(&mut adapter, MIGRATIONS_TABLE);
    let mut adapter = reopened(&directory, adapter);
    run(&mut adapter, MIGRATIONS_TABLE);
    assert_eq!(
        rows(&mut adapter, "SHOW CREATE TABLE _sqlx_migrations")[0][1],
        Some(
            "CREATE TABLE `_sqlx_migrations` (\n  `version` bigint NOT NULL,\n  `description` text NOT NULL,\n  `installed_on` timestamp NOT NULL DEFAULT CURRENT_TIMESTAMP,\n  `success` tinyint(1) NOT NULL,\n  `checksum` blob NOT NULL,\n  `execution_time` bigint NOT NULL,\n  PRIMARY KEY (`version`)\n) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_0900_ai_ci"
                .to_owned()
        )
    );

    let checksum: Vec<u8> = (0..48).map(|byte| byte * 5).collect();
    let recorded = prepared_changes(
        &mut adapter,
        RECORD_MIGRATION,
        &[
            Bound::Whole(20260101000000),
            Bound::Word("create blog"),
            Bound::Bytes(&checksum),
        ],
    );
    assert_eq!(recorded.affected_rows, 1);
    run(
        &mut adapter,
        "INSERT INTO _sqlx_migrations ( version, description, success, checksum, execution_time ) VALUES ( -5, 'negative', FALSE, x'00', -1 )",
    );
    for (sql, refusal) in [
        (
            "INSERT INTO _sqlx_migrations ( version, description, success, checksum, execution_time ) VALUES ( -5, 'again', FALSE, x'00', -1 )",
            FrontendErrorKind::ConstraintViolation,
        ),
        (
            "INSERT INTO _sqlx_migrations ( description, success, checksum, execution_time ) VALUES ( 'no version', FALSE, x'00', -1 )",
            FrontendErrorKind::MissingRequiredDefault,
        ),
    ] {
        assert_eq!(adapter.execute_query(sql), Err(refusal), "{sql}");
    }

    let result = prepared_rows(
        &mut adapter,
        "SELECT version, checksum FROM _sqlx_migrations ORDER BY version",
        &[],
    );
    let [version, stored] = result.columns.as_slice() else {
        panic!("two columns");
    };
    assert_eq!(
        (version.column_type, version.flags, version.column_length),
        (MYSQL_TYPE_LONGLONG, 0x5003, 20),
        "NOT_NULL PRI_KEY NO_DEFAULT_VALUE PART_KEY"
    );
    assert_eq!(
        (stored.column_type, stored.flags),
        (MYSQL_TYPE_BLOB, 0x1091)
    );
    assert_eq!(
        result.rows,
        [
            vec![
                BinaryResultValue::Integer(-5),
                BinaryResultValue::Blob(vec![0])
            ],
            vec![
                BinaryResultValue::Integer(20260101000000),
                BinaryResultValue::Blob(checksum),
            ],
        ]
    );
    assert_eq!(
        rows(
            &mut adapter,
            "SELECT COLUMN_NAME, COLUMN_TYPE, COLUMN_KEY, IS_NULLABLE FROM information_schema.COLUMNS WHERE TABLE_NAME = '_sqlx_migrations' AND COLUMN_NAME = 'version'",
        ),
        [[
            Some("version".to_owned()),
            Some("bigint".to_owned()),
            Some("PRI".to_owned()),
            Some("NO".to_owned()),
        ]]
    );
}

/// sqlx will not read a column carrying the binary flag as text, and every
/// name `information_schema` holds carries it, so an app listing its tables
/// and a table's columns writes each name out with `CAST(... AS CHAR)`.
/// Measured on MySQL 8.4.11: the cast keeps the name's type and width and
/// drops every flag, and over `COLUMN_TYPE`, a `BLOB` of words, is four times
/// as wide again with only the blob flag left.
#[test]
fn sqlx_lists_tables_and_columns_with_the_names_written_out() {
    let (_directory, mut adapter) = adapter();
    run(
        &mut adapter,
        "CREATE TABLE posts (id BIGINT UNSIGNED NOT NULL AUTO_INCREMENT PRIMARY KEY, title VARCHAR(200) NOT NULL, views INT NOT NULL DEFAULT 0)",
    );
    let tables = prepared_rows(
        &mut adapter,
        "SELECT CAST(TABLE_NAME AS CHAR) FROM information_schema.TABLES WHERE TABLE_SCHEMA = DATABASE() ORDER BY TABLE_NAME",
        &[],
    );
    assert_eq!(
        shapes(&tables.columns),
        [(MYSQL_TYPE_VAR_STRING, 0, 256, 0)]
    );
    assert_eq!(
        tables.rows,
        [
            [BinaryResultValue::Text("posts".to_owned())],
            [BinaryResultValue::Text("records".to_owned())],
        ]
    );

    let columns = prepared_rows(
        &mut adapter,
        "SELECT CAST(COLUMN_NAME AS CHAR), CAST(COLUMN_TYPE AS CHAR) FROM information_schema.COLUMNS WHERE TABLE_SCHEMA = DATABASE() AND TABLE_NAME = ?",
        &[Bound::Word("posts")],
    );
    assert_eq!(
        shapes(&columns.columns),
        [
            (MYSQL_TYPE_VAR_STRING, 0, 256, 0),
            (MYSQL_TYPE_BLOB, 0x10, 268435440, 0),
        ]
    );
    let names: Vec<(String, String)> = columns
        .rows
        .into_iter()
        .map(|row| match row.as_slice() {
            [BinaryResultValue::Text(name), BinaryResultValue::Blob(declared)] => {
                (name.clone(), String::from_utf8(declared.clone()).unwrap())
            }
            other => panic!("a name and a type, answered {other:?}"),
        })
        .collect();
    assert_eq!(
        names,
        [
            ("id".to_owned(), "bigint unsigned".to_owned()),
            ("title".to_owned(), "varchar(200)".to_owned()),
            ("views".to_owned(), "int".to_owned()),
        ]
    );

    // A catalog number written out has not been measured.
    assert_eq!(
        adapter.execute_query(
            "SELECT CAST(ORDINAL_POSITION AS CHAR) FROM information_schema.COLUMNS WHERE TABLE_SCHEMA = DATABASE() AND TABLE_NAME = 'posts'"
        ),
        Err(FrontendErrorKind::Unsupported)
    );
}

fn shapes(columns: &[ColumnDefinitionConfig]) -> Vec<(u8, u16, u32, u8)> {
    columns
        .iter()
        .map(|column| {
            (
                column.column_type,
                column.flags,
                column.column_length,
                column.decimals,
            )
        })
        .collect()
}

/// The app's second migration adds a column after `title` in the same
/// `ALTER TABLE` that renames `views` and restates `title`, all inside the
/// transaction `sqlx migrate run` holds, and a table's foreign key names
/// `posts`. Measured on MySQL 8.4.11, both the migration and its revert print
/// the tables below, keep every row, and leave `post_tags` still cascading
/// from `posts`.
///
/// MySQL renames and drops before it reads a place, where this runs each
/// clause in turn, so a place naming a column the statement renames or drops
/// is refused; MySQL answers 1054 for both.
#[test]
fn sqlx_places_a_column_beside_a_rename_and_a_restatement() {
    let (_directory, mut adapter) = adapter();
    migrate(&mut adapter, CREATE_BLOG);
    for sql in [
        "INSERT INTO users (email, name) VALUES ('bob@example.com', 'Bob')",
        "INSERT INTO posts (user_id, title, body, views) VALUES (1, 'Hello', 'First post', 11), (1, 'Draft', NULL, 4)",
        "INSERT INTO tags (name) VALUES ('news')",
        "INSERT INTO post_tags VALUES (1, 1), (2, 1)",
        "BEGIN",
    ] {
        run(&mut adapter, sql);
    }
    migrate(&mut adapter, ADD_SLUG);
    run(&mut adapter, "COMMIT");
    assert_eq!(
        rows(&mut adapter, "SHOW CREATE TABLE posts")[0][1].as_deref(),
        Some(
            "CREATE TABLE `posts` (\n  `id` bigint unsigned NOT NULL AUTO_INCREMENT,\n  `user_id` bigint unsigned NOT NULL,\n  `title` varchar(255) NOT NULL,\n  `slug` varchar(220) DEFAULT NULL,\n  `body` text,\n  `published_at` datetime DEFAULT NULL,\n  `view_count` int NOT NULL DEFAULT '0',\n  PRIMARY KEY (`id`),\n  UNIQUE KEY `posts_slug` (`slug`),\n  KEY `posts_user_published` (`user_id`,`published_at`),\n  CONSTRAINT `posts_user` FOREIGN KEY (`user_id`) REFERENCES `users` (`id`) ON DELETE CASCADE\n) ENGINE=InnoDB AUTO_INCREMENT=3 DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_0900_ai_ci"
        )
    );
    run(&mut adapter, "UPDATE posts SET slug = CONCAT('post-', id)");
    assert_eq!(
        rows(
            &mut adapter,
            "SELECT id, title, slug, body, view_count FROM posts ORDER BY id"
        ),
        [
            [
                Some("1".to_owned()),
                Some("Hello".to_owned()),
                Some("post-1".to_owned()),
                Some("First post".to_owned()),
                Some("11".to_owned()),
            ],
            [
                Some("2".to_owned()),
                Some("Draft".to_owned()),
                Some("post-2".to_owned()),
                None,
                Some("4".to_owned()),
            ],
        ]
    );
    assert!(matches!(
        adapter.execute_query("UPDATE posts SET slug = 'post-1' WHERE id = 2"),
        Err(FrontendErrorKind::ConstraintViolation)
    ));

    run(&mut adapter, "BEGIN");
    migrate(&mut adapter, DROP_SLUG);
    run(&mut adapter, "COMMIT");
    assert_eq!(
        rows(&mut adapter, "SHOW CREATE TABLE posts")[0][1].as_deref(),
        Some(
            "CREATE TABLE `posts` (\n  `id` bigint unsigned NOT NULL AUTO_INCREMENT,\n  `user_id` bigint unsigned NOT NULL,\n  `title` varchar(200) NOT NULL,\n  `body` text,\n  `published_at` datetime DEFAULT NULL,\n  `views` int NOT NULL DEFAULT '0',\n  PRIMARY KEY (`id`),\n  KEY `posts_user_published` (`user_id`,`published_at`),\n  CONSTRAINT `posts_user` FOREIGN KEY (`user_id`) REFERENCES `users` (`id`) ON DELETE CASCADE\n) ENGINE=InnoDB AUTO_INCREMENT=3 DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_0900_ai_ci"
        )
    );
    run(&mut adapter, "DELETE FROM posts WHERE id = 1");
    assert_eq!(
        rows(&mut adapter, "SELECT post_id FROM post_tags"),
        [[Some("2".to_owned())]]
    );

    for sql in [
        "ALTER TABLE posts ADD COLUMN a INT AFTER title, RENAME COLUMN title TO headline",
        "ALTER TABLE posts ADD COLUMN a INT AFTER body, DROP COLUMN body",
    ] {
        assert_eq!(
            adapter.execute_query(sql),
            Err(FrontendErrorKind::Unsupported),
            "{sql}"
        );
    }
    assert_eq!(
        adapter.execute_query(
            "ALTER TABLE posts ADD COLUMN a INT AFTER nope, RENAME COLUMN views TO v"
        ),
        Err(FrontendErrorKind::UnknownColumn)
    );
    assert_eq!(
        rows(&mut adapter, "SELECT COLUMN_NAME FROM information_schema.COLUMNS WHERE TABLE_NAME = 'posts' ORDER BY ORDINAL_POSITION"),
        [
            [Some("id".to_owned())],
            [Some("user_id".to_owned())],
            [Some("title".to_owned())],
            [Some("body".to_owned())],
            [Some("published_at".to_owned())],
            [Some("views".to_owned())],
        ]
    );
}

/// sqlx's `raw_sql` sends `UPDATE posts SET body = CONCAT(COALESCE(body,
/// ''), '!') WHERE title = 'Hello'` beside two reads in one text query,
/// appending to a column that may hold nothing. Measured on MySQL 8.4.11:
/// `First post` becomes `First post!` and a NULL becomes `!`, and an `IFNULL`
/// beside a column and a written `', '` joins all three.
#[test]
fn sqlx_appends_to_a_column_that_may_hold_nothing() {
    let (_directory, mut adapter) = adapter();
    for sql in [
        "CREATE TABLE posts (id BIGINT UNSIGNED NOT NULL AUTO_INCREMENT PRIMARY KEY, title VARCHAR(200) NOT NULL, body TEXT NULL, views INT NOT NULL DEFAULT 0)",
        "INSERT INTO posts (title, body) VALUES ('Hello', 'First post'), ('Hello', NULL), ('Other', 'x')",
        "UPDATE posts SET body = CONCAT(COALESCE(body, ''), '!') WHERE title = 'Hello'",
        "UPDATE posts SET body = CONCAT(IFNULL(body, 'none'), ', ', title) WHERE id = 3",
    ] {
        run(&mut adapter, sql);
    }
    assert_eq!(
        rows(&mut adapter, "SELECT body FROM posts ORDER BY id"),
        [
            [Some("First post!".to_owned())],
            [Some("!".to_owned())],
            [Some("x, Other".to_owned())],
        ]
    );
    assert_eq!(
        rows(
            &mut adapter,
            " SELECT COUNT(*) AS n FROM posts WHERE body LIKE '%!'"
        ),
        [[Some("2".to_owned())]]
    );

    // A number written out as a word, a word written into a number and a
    // column the same `SET` has already written each follow rules of their
    // own, which this does not repeat.
    for sql in [
        "UPDATE posts SET body = CONCAT(COALESCE(views, ''), '!')",
        "UPDATE posts SET views = CONCAT(COALESCE(body, ''), '!')",
        "UPDATE posts SET title = 'a', body = CONCAT(COALESCE(title, ''), '!')",
    ] {
        assert_eq!(
            adapter.execute_query(sql),
            Err(FrontendErrorKind::Unsupported),
            "{sql}"
        );
    }
}

/// sqlx's transaction step copies a user's id beside a bound title, `INSERT
/// INTO posts (user_id, title) SELECT id, ? FROM users WHERE email = ?`, out
/// of a table holding a `DECIMAL` balance it does not read. Measured on MySQL
/// 8.4.11: one row, the next post id, and the balance left alone.
#[test]
fn sqlx_copies_an_id_beside_a_bound_title_out_of_a_table_holding_a_decimal() {
    let (_directory, mut adapter) = adapter();
    migrate(&mut adapter, CREATE_BLOG);
    for sql in [
        "INSERT INTO users (email, name, balance) VALUES ('alice@example.com', 'Alice', 100.50), ('bob@example.com', 'Bob', 20.25)",
        "INSERT INTO posts (user_id, title) VALUES (1, 'Hello'), (1, 'Draft'), (2, 'Bob writes')",
        "BEGIN",
    ] {
        run(&mut adapter, sql);
    }
    prepared_changes(
        &mut adapter,
        "UPDATE users SET balance = balance + 10 WHERE email = ?",
        &[Bound::Word("bob@example.com")],
    );
    let copied = prepared_changes(
        &mut adapter,
        "INSERT INTO posts (user_id, title) SELECT id, ? FROM users WHERE email = ?",
        &[
            Bound::Word("In a transaction"),
            Bound::Word("bob@example.com"),
        ],
    );
    assert_eq!((copied.affected_rows, copied.last_insert_id), (1, 4));
    run(&mut adapter, "COMMIT");
    assert_eq!(
        rows(
            &mut adapter,
            "SELECT p.id, p.user_id, p.title, u.balance FROM posts p JOIN users u ON u.id = p.user_id WHERE p.id = 4"
        ),
        [[
            Some("4".to_owned()),
            Some("2".to_owned()),
            Some("In a transaction".to_owned()),
            Some("30.25".to_owned()),
        ]]
    );

    // A `?` written into the `DECIMAL` itself goes through the copy path's
    // own rule for one, which reads a `DECIMAL` only out of another.
    assert_eq!(
        prepared(
            &mut adapter,
            "INSERT INTO users (email, name, balance) SELECT 'carol@example.com', name, ? FROM users WHERE id = ?",
            &[Bound::Word("5.00"), Bound::Whole(1)],
        ),
        Err(FrontendErrorKind::Unsupported)
    );
}

/// `sqlx database reset` connects with no database selected, asks whether
/// its database is there with a prepared `select exists(...)`, drops it,
/// asks again on a new connection and creates it. Measured on MySQL 8.4.11
/// through sqlx 0.8.6: a NOT NULL `LONGLONG` of 1 with the binary flag, named
/// after the call as written, 1 while the database is there and 0 once it is
/// gone.
#[test]
fn sqlx_asks_whether_its_database_is_there_before_selecting_one() {
    const EXISTS: &str =
        "select exists(SELECT 1 from INFORMATION_SCHEMA.SCHEMATA WHERE SCHEMA_NAME = ?)";
    let authorizer = Arc::new(RecordingAuthorizer::default());
    let (_directory, _catalog, factory) = catalog_factory(authorizer);
    let mut adapter = factory
        .build(AuthenticatedPrincipal::from_account_id_for_testing(
            AccountId::from_bytes(ACCOUNT),
        ))
        .unwrap();
    adapter.authorize_connection().unwrap();
    run(&mut adapter, SESSION);

    let is_there = |adapter: &mut Adapter| {
        let result = prepared_rows(adapter, EXISTS, &[Bound::Word("reports")]);
        assert_eq!(
            result
                .columns
                .iter()
                .map(|column| (
                    column.name.as_str(),
                    column.column_type,
                    column.flags,
                    column.column_length
                ))
                .collect::<Vec<_>>(),
            [(
                "exists(SELECT 1 from INFORMATION_SCHEMA.SCHEMATA WHERE SCHEMA_NAME = ?)",
                MYSQL_TYPE_LONGLONG,
                0x81,
                1
            )]
        );
        result.rows
    };
    assert_eq!(is_there(&mut adapter), [[BinaryResultValue::Integer(1)]]);
    run(&mut adapter, "DROP DATABASE IF EXISTS `reports`");
    assert_eq!(is_there(&mut adapter), [[BinaryResultValue::Integer(0)]]);
    run(&mut adapter, "CREATE DATABASE `reports`");
    assert_eq!(is_there(&mut adapter), [[BinaryResultValue::Integer(1)]]);

    let written = result_columns_and_rows(
        &mut adapter,
        "select exists(SELECT 1 from INFORMATION_SCHEMA.SCHEMATA WHERE SCHEMA_NAME = 'nope');",
    );
    assert_eq!(
        written,
        (
            vec![
                "exists(SELECT 1 from INFORMATION_SCHEMA.SCHEMATA WHERE SCHEMA_NAME = 'nope')"
                    .to_owned()
            ],
            vec![vec![Some("0".to_owned())]]
        )
    );

    // Whether a name differing only in case names the database turns on how
    // MySQL compares the catalog's names, which has not been measured.
    assert_eq!(
        prepared(&mut adapter, EXISTS, &[Bound::Word("REPORTS")]),
        Err(FrontendErrorKind::Unsupported)
    );
}

fn result_columns_and_rows(
    adapter: &mut Adapter,
    sql: &str,
) -> (Vec<String>, Vec<Vec<Option<String>>>) {
    let Ok(CommandExecutionResult::ResultSet(result)) = adapter.execute_query(sql) else {
        panic!("{sql} must answer rows");
    };
    (
        result
            .columns
            .into_iter()
            .map(|column| column.name)
            .collect(),
        result
            .rows
            .into_iter()
            .map(|row| {
                row.into_iter()
                    .map(|value| value.map(|value| String::from_utf8(value).unwrap()))
                    .collect()
            })
            .collect(),
    )
}
