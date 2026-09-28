//! What Prisma's schema engine and client send, as prisma-engines 7.1 (the
//! engines of Prisma 6.19) writes it through `mysql_async`.
//!
//! Every expected answer was measured on MySQL 8.4.11.

use super::*;

type Adapter = AuthorizedDatabaseCommandAdapter<RecordingAuthorizer>;

fn catalog() -> (
    tempfile::TempDir,
    Arc<MySqlDatabaseCatalog>,
    Arc<RecordingAuthorizer>,
) {
    let authorizer = Arc::new(RecordingAuthorizer::default());
    let (directory, catalog, _factory) = catalog_factory(Arc::clone(&authorizer));
    catalog.create("prisma").unwrap();
    catalog.create("prisma_shadow").unwrap();
    (directory, catalog, authorizer)
}

/// A session connected with `database` selected, as a Prisma URL names it.
fn session(
    catalog: &Arc<MySqlDatabaseCatalog>,
    authorizer: &Arc<RecordingAuthorizer>,
    database: &str,
) -> Adapter {
    let factory = AuthorizedDatabaseAdapterFactory::new(
        Arc::clone(catalog),
        binary_context(),
        Arc::clone(authorizer),
    );
    let mut adapter = factory
        .build(AuthenticatedPrincipal::from_account_id_for_testing(
            AccountId::from_bytes([167; 32]),
        ))
        .unwrap();
    adapter.authorize_connection().unwrap();
    adapter.execute_init_db(database).unwrap();
    adapter
}

fn run(adapter: &mut Adapter, sql: &str) {
    match adapter.execute_query(sql) {
        Ok(_) => {}
        Err(error) => panic!("{sql}: {error:?} {:?}", adapter.take_error_message()),
    }
}

fn words(adapter: &mut Adapter, sql: &str) -> Vec<Option<String>> {
    let Ok(CommandExecutionResult::ResultSet(result)) = adapter.execute_query(sql) else {
        panic!("{sql} must return a result set");
    };
    result
        .rows
        .into_iter()
        .map(|row| {
            row[0]
                .clone()
                .map(|value| String::from_utf8(value).unwrap())
        })
        .collect()
}

fn refusal(adapter: &mut Adapter, sql: &str) -> (FrontendErrorKind, String) {
    let error = adapter
        .execute_query(sql)
        .expect_err("the statement must be refused");
    let message = adapter
        .take_error_message()
        .map(|message| String::from_utf8(message).unwrap())
        .unwrap_or_default();
    (error, message)
}

/// `prisma migrate reset` drops and makes again the database the app's own
/// pooled connections still have selected. MySQL drops it at once: an idle
/// session holds nothing up, keeps the name, answers 1049 for a table of it,
/// and reads the new database once there is one.
#[test]
fn migrate_reset_drops_a_database_the_pool_still_has_selected() {
    let (_directory, catalog, authorizer) = catalog();
    let mut pooled = session(&catalog, &authorizer, "prisma");
    run(
        &mut pooled,
        "CREATE TABLE `users` (`id` BIGINT NOT NULL AUTO_INCREMENT, PRIMARY KEY (`id`))",
    );
    run(&mut pooled, "INSERT INTO `users` () VALUES ()");
    let prepared = pooled
        .execute_stmt_prepare("SELECT `prisma`.`users`.`id` FROM `prisma`.`users`")
        .unwrap();

    let mut reset = session(&catalog, &authorizer, "prisma");
    run(&mut reset, "DROP DATABASE `prisma`");
    assert_eq!(words(&mut reset, "SELECT DATABASE()"), [None]);

    assert_eq!(
        words(&mut pooled, "SELECT DATABASE()"),
        [Some("prisma".to_owned())]
    );
    assert_eq!(
        refusal(&mut pooled, "SELECT `id` FROM `users`").0,
        FrontendErrorKind::UnknownDatabase
    );
    assert_eq!(
        pooled.execute_stmt_execute(prepared.statement_id, &[]),
        Err(FrontendErrorKind::UnknownDatabase)
    );
    assert_eq!(
        pooled.take_error_message(),
        Some(b"Unknown database 'prisma'".to_vec())
    );

    run(&mut reset, "CREATE DATABASE `prisma`");
    run(&mut reset, "USE `prisma`");
    run(
        &mut reset,
        "CREATE TABLE `users` (`id` BIGINT NOT NULL AUTO_INCREMENT, PRIMARY KEY (`id`))",
    );
    assert_eq!(
        words(&mut pooled, "SELECT COUNT(*) FROM `users`"),
        [Some("0".to_owned())]
    );
}

/// The shadow database is dropped by the session connected to it, which is
/// then in no database until it selects the one it makes again.
#[test]
fn migrate_dev_drops_the_shadow_database_it_is_in() {
    let (_directory, catalog, authorizer) = catalog();
    let mut shadow = session(&catalog, &authorizer, "prisma_shadow");
    run(
        &mut shadow,
        "CREATE TABLE `users` (`id` INT NOT NULL, PRIMARY KEY (`id`))",
    );
    run(&mut shadow, "DROP DATABASE `prisma_shadow`");
    assert_eq!(words(&mut shadow, "SELECT DATABASE()"), [None]);
    assert_eq!(
        refusal(&mut shadow, "SELECT 1 FROM `users`").0,
        FrontendErrorKind::NoDatabaseSelected
    );
    run(&mut shadow, "CREATE DATABASE `prisma_shadow`");
    run(&mut shadow, "USE `prisma_shadow`");
    run(
        &mut shadow,
        "CREATE TABLE `users` (`id` INT NOT NULL, PRIMARY KEY (`id`))",
    );
    assert_eq!(
        words(&mut shadow, "SHOW TABLES"),
        [Some("users".to_owned())]
    );
}

/// `DROP DATABASE` commits the session's transaction first, whichever
/// database it names, and a database that is not there answers 1008.
#[test]
fn drop_database_commits_first_and_names_a_database_that_is_not_there() {
    let (_directory, catalog, authorizer) = catalog();
    let mut adapter = session(&catalog, &authorizer, "prisma");
    run(
        &mut adapter,
        "CREATE TABLE `kept` (`id` INT NOT NULL, PRIMARY KEY (`id`))",
    );
    run(&mut adapter, "BEGIN");
    run(&mut adapter, "INSERT INTO `kept` VALUES (1)");
    run(&mut adapter, "DROP DATABASE `prisma_shadow`");
    run(&mut adapter, "ROLLBACK");
    assert_eq!(
        words(&mut adapter, "SELECT COUNT(*) FROM `kept`"),
        [Some("1".to_owned())]
    );

    assert_eq!(
        refusal(&mut adapter, "DROP DATABASE `prisma_shadow`"),
        (
            FrontendErrorKind::NoDatabaseToDrop,
            "Can't drop database 'prisma_shadow'; database doesn't exist".to_owned()
        )
    );
}

/// A transaction another session holds open on the database holds the drop
/// up until it ends, and its rows are the last the database had.
#[test]
fn drop_database_waits_for_a_transaction_on_it() {
    let (_directory, catalog, authorizer) = catalog();
    let mut holder = session(&catalog, &authorizer, "prisma");
    run(
        &mut holder,
        "CREATE TABLE `kept` (`id` INT NOT NULL, PRIMARY KEY (`id`))",
    );
    run(&mut holder, "BEGIN");
    run(&mut holder, "INSERT INTO `kept` VALUES (1)");

    let dropper = {
        let mut dropper = session(&catalog, &authorizer, "prisma_shadow");
        std::thread::spawn(move || dropper.execute_query("DROP DATABASE `prisma`").map(|_| ()))
    };
    std::thread::sleep(std::time::Duration::from_millis(200));
    assert!(!dropper.is_finished());
    assert_eq!(
        words(&mut holder, "SELECT COUNT(*) FROM `kept`"),
        [Some("1".to_owned())]
    );
    run(&mut holder, "COMMIT");
    assert_eq!(dropper.join().unwrap(), Ok(()));

    assert_eq!(
        refusal(&mut holder, "SELECT COUNT(*) FROM `kept`").0,
        FrontendErrorKind::UnknownDatabase
    );
    assert_eq!(
        words(&mut holder, "SELECT DATABASE()"),
        [Some("prisma".to_owned())]
    );
}

/// The schema `migrate deploy` makes from the app's first migration, beside a
/// table with a `CHECK` and one with the column types the describer reads
/// defaults and enums of.
const MIGRATED: &[&str] = &[
    "CREATE TABLE _prisma_migrations (\n    id                      VARCHAR(36) PRIMARY KEY NOT NULL,\n    checksum                VARCHAR(64) NOT NULL,\n    finished_at             DATETIME(3),\n    migration_name          VARCHAR(255) NOT NULL,\n    logs                    TEXT,\n    rolled_back_at          DATETIME(3),\n    started_at              DATETIME(3) NOT NULL DEFAULT CURRENT_TIMESTAMP(3),\n    applied_steps_count     INTEGER UNSIGNED NOT NULL DEFAULT 0\n) DEFAULT CHARACTER SET utf8mb4 COLLATE utf8mb4_unicode_ci;\n",
    "-- CreateTable\nCREATE TABLE `users` (\n    `id` BIGINT NOT NULL AUTO_INCREMENT,\n    `email` VARCHAR(191) NOT NULL,\n    `name` VARCHAR(100) NOT NULL,\n    `balance` DECIMAL(10, 2) NOT NULL DEFAULT 0,\n    `is_active` BOOLEAN NOT NULL DEFAULT true,\n    `profile` JSON NULL,\n    `created_at` DATETIME(3) NOT NULL DEFAULT CURRENT_TIMESTAMP(3),\n    `updated_at` DATETIME(3) NOT NULL,\n\n    UNIQUE INDEX `users_email_key`(`email`),\n    PRIMARY KEY (`id`)\n) DEFAULT CHARACTER SET utf8mb4 COLLATE utf8mb4_unicode_ci",
    "-- CreateTable\nCREATE TABLE `posts` (\n    `id` BIGINT NOT NULL AUTO_INCREMENT,\n    `user_id` BIGINT NOT NULL,\n    `title` VARCHAR(200) NOT NULL,\n    `body` TEXT NULL,\n    `published_at` DATETIME(3) NULL,\n    `views` INTEGER NOT NULL DEFAULT 0,\n\n    INDEX `posts_user_id_idx`(`user_id`),\n    PRIMARY KEY (`id`)\n) DEFAULT CHARACTER SET utf8mb4 COLLATE utf8mb4_unicode_ci",
    "-- CreateTable\nCREATE TABLE `tags` (\n    `id` BIGINT NOT NULL AUTO_INCREMENT,\n    `name` VARCHAR(100) NOT NULL,\n\n    UNIQUE INDEX `tags_name_key`(`name`),\n    PRIMARY KEY (`id`)\n) DEFAULT CHARACTER SET utf8mb4 COLLATE utf8mb4_unicode_ci",
    "-- CreateTable\nCREATE TABLE `post_tag` (\n    `post_id` BIGINT NOT NULL,\n    `tag_id` BIGINT NOT NULL,\n\n    INDEX `post_tag_tag_id_idx`(`tag_id`),\n    PRIMARY KEY (`post_id`, `tag_id`)\n) DEFAULT CHARACTER SET utf8mb4 COLLATE utf8mb4_unicode_ci",
    "-- AddForeignKey\nALTER TABLE `posts` ADD CONSTRAINT `posts_user_id_fkey` FOREIGN KEY (`user_id`) REFERENCES `users`(`id`) ON DELETE CASCADE ON UPDATE CASCADE",
    "-- AddForeignKey\nALTER TABLE `post_tag` ADD CONSTRAINT `post_tag_post_id_fkey` FOREIGN KEY (`post_id`) REFERENCES `posts`(`id`) ON DELETE CASCADE ON UPDATE CASCADE",
    "-- AddForeignKey\nALTER TABLE `post_tag` ADD CONSTRAINT `post_tag_tag_id_fkey` FOREIGN KEY (`tag_id`) REFERENCES `tags`(`id`) ON DELETE CASCADE ON UPDATE CASCADE",
    "CREATE TABLE checked (id INT PRIMARY KEY, n INT CHECK (n > 0))",
    "CREATE TABLE enumed (id INT PRIMARY KEY, s VARCHAR(10) DEFAULT 'it''s', e ENUM('a','b') DEFAULT 'a', f DOUBLE DEFAULT 1.5, t TIMESTAMP NULL DEFAULT NULL, u TIMESTAMP(6) DEFAULT CURRENT_TIMESTAMP(6) ON UPDATE CURRENT_TIMESTAMP(6)) COMMENT 'enums here'",
];

fn migrated() -> (tempfile::TempDir, Adapter) {
    let (directory, catalog, authorizer) = catalog();
    let mut adapter = session(&catalog, &authorizer, "prisma");
    for sql in MIGRATED {
        run(&mut adapter, sql);
    }
    (directory, adapter)
}

/// Prepares and executes `sql` with each `?` bound to a word, as
/// `mysql_async` binds Prisma's schema name, and checks the answer crosses
/// the wire.
fn described(adapter: &mut Adapter, sql: &str, bound: &[&str]) -> BinaryResultSet {
    let statement = adapter
        .execute_stmt_prepare(sql)
        .unwrap_or_else(|error| panic!("prepare {sql}: {error:?}"));
    assert_eq!(statement.parameters.len(), bound.len(), "{sql}");
    let mut payload = Vec::new();
    if !bound.is_empty() {
        payload.extend(std::iter::repeat_n(0, bound.len().div_ceil(8)));
        payload.push(1);
        for _ in bound {
            payload.extend_from_slice(&[MYSQL_TYPE_VAR_STRING, 0]);
        }
        for word in bound {
            payload.push(u8::try_from(word.len()).unwrap());
            payload.extend_from_slice(word.as_bytes());
        }
    }
    let PreparedStatementExecutionResult::ResultSet(result) = adapter
        .execute_stmt_execute(statement.statement_id, &payload)
        .unwrap_or_else(|error| panic!("execute {sql}: {error:?}"))
    else {
        panic!("{sql} must answer rows");
    };
    crate::dispatcher::encode_binary_result_set(
        PacketCodec::new(16_777_215).unwrap(),
        0,
        result.clone(),
    )
    .unwrap();
    assert_eq!(result.columns, statement.columns, "{sql}");
    adapter.execute_stmt_close(statement.statement_id);
    result
}

fn shown(result: &BinaryResultSet) -> Vec<String> {
    result
        .rows
        .iter()
        .map(|row| {
            row.iter()
                .map(|value| match value {
                    BinaryResultValue::Null => "NULL".to_owned(),
                    BinaryResultValue::Integer(value) => value.to_string(),
                    BinaryResultValue::UnsignedInteger(value) => value.to_string(),
                    BinaryResultValue::Text(value) => value.clone(),
                    BinaryResultValue::Blob(value) => String::from_utf8(value.clone()).unwrap(),
                    other => panic!("unexpected {other:?}"),
                })
                .collect::<Vec<_>>()
                .join("|")
        })
        .collect()
}

/// Each column's name, type, character set, length and flags.
fn described_columns(result: &BinaryResultSet) -> Vec<(String, u8, u16, u32, u16)> {
    result
        .columns
        .iter()
        .map(|column| {
            (
                column.name.clone(),
                column.column_type,
                column.character_set,
                column.column_length,
                column.flags,
            )
        })
        .collect()
}

const TABLE_NAMES: &str = "\n            SELECT DISTINCT\n              BINARY table_info.table_name AS table_name,\n              table_info.create_options AS create_options,\n              table_info.table_comment AS table_comment\n            FROM information_schema.tables AS table_info\n            JOIN information_schema.columns AS column_info\n                ON BINARY column_info.table_name = BINARY table_info.table_name\n            WHERE\n                table_info.table_schema = ?\n                AND column_info.table_schema = ?\n                -- Exclude views.\n                AND table_info.table_type = 'BASE TABLE'\n            ORDER BY BINARY table_info.table_name";

/// `migrate deploy` and the describer list the tables first, comparing and
/// ordering their names by their bytes.
#[test]
fn the_describer_lists_the_tables_by_their_bytes() {
    let (_directory, mut adapter) = migrated();
    let listed = described(
        &mut adapter,
        "\n                SELECT DISTINCT BINARY table_info.table_name AS table_name\n                FROM information_schema.tables AS table_info\n                JOIN information_schema.columns AS column_info\n                    ON BINARY column_info.table_name = BINARY table_info.table_name\n                WHERE\n                    table_info.table_schema = ?\n                    AND column_info.table_schema = ?\n                    -- Exclude views.\n                    AND table_info.table_type = 'BASE TABLE'\n                ORDER BY BINARY table_info.table_name\n            ",
        &["prisma", "prisma"],
    );
    assert_eq!(
        shown(&listed),
        [
            "_prisma_migrations",
            "checked",
            "enumed",
            "post_tag",
            "posts",
            "tags",
            "users"
        ]
    );
    assert_eq!(
        described_columns(&listed),
        [(
            "table_name".to_owned(),
            MYSQL_TYPE_VAR_STRING,
            63,
            192,
            MYSQL_BINARY_FLAG
        )]
    );

    let listed = described(&mut adapter, TABLE_NAMES, &["prisma", "prisma"]);
    assert_eq!(
        shown(&listed),
        [
            "_prisma_migrations||",
            "checked||",
            "enumed||enums here",
            "post_tag||",
            "posts||",
            "tags||",
            "users||"
        ]
    );
    assert_eq!(
        described_columns(&listed),
        [
            (
                "table_name".to_owned(),
                MYSQL_TYPE_VAR_STRING,
                63,
                192,
                MYSQL_BINARY_FLAG
            ),
            (
                "create_options".to_owned(),
                MYSQL_TYPE_VAR_STRING,
                45,
                1024,
                0
            ),
            (
                "table_comment".to_owned(),
                MYSQL_TYPE_BLOB,
                45,
                24_576,
                MYSQL_BLOB_FLAG
            ),
        ]
    );
    // Another database's name finds none of this one's tables.
    assert!(
        described(&mut adapter, TABLE_NAMES, &["prisma_shadow", "prisma"])
            .rows
            .is_empty()
    );
}

/// The describer's `CHECK` read, its type spelled in lower case.
#[test]
fn the_describer_reads_the_check_constraints() {
    let (_directory, mut adapter) = migrated();
    let checks = described(
        &mut adapter,
        "SELECT\n\ttc.table_schema AS namespace,\n\ttc.table_name AS table_name,\n\ttc.constraint_name AS constraint_name,\n\tLOWER(tc.constraint_type) AS constraint_type,\n\tcc.check_clause AS constraint_definition\nFROM INFORMATION_SCHEMA.TABLE_CONSTRAINTS tc\nLEFT JOIN INFORMATION_SCHEMA.CHECK_CONSTRAINTS cc\n\tON cc.constraint_schema = tc.table_schema\n\tAND cc.constraint_name = tc.constraint_name\nWHERE constraint_type = 'CHECK'\nORDER BY namespace, table_name, constraint_type, constraint_name;\n",
        &[],
    );
    assert_eq!(
        shown(&checks),
        ["prisma|checked|checked_chk_1|check|(`n` > 0)"]
    );
    let key = MYSQL_NOT_NULL_FLAG | MYSQL_BINARY_FLAG | MYSQL_NO_DEFAULT_VALUE_FLAG;
    assert_eq!(
        described_columns(&checks),
        [
            ("namespace".to_owned(), MYSQL_TYPE_VAR_STRING, 45, 256, key),
            ("table_name".to_owned(), MYSQL_TYPE_VAR_STRING, 45, 256, key),
            (
                "constraint_name".to_owned(),
                MYSQL_TYPE_VAR_STRING,
                45,
                256,
                0
            ),
            (
                "constraint_type".to_owned(),
                MYSQL_TYPE_VAR_STRING,
                45,
                44,
                MYSQL_BINARY_FLAG
            ),
            (
                "constraint_definition".to_owned(),
                MYSQL_TYPE_BLOB,
                45,
                u32::MAX,
                MYSQL_BLOB_FLAG | MYSQL_BINARY_FLAG | MYSQL_NO_DEFAULT_VALUE_FLAG
            ),
        ]
    );
}

/// Every column with its type, precision, default and `EXTRA`, a generated
/// default marked `DEFAULT_GENERATED`, an empty comment answered as NULL.
#[test]
fn the_describer_reads_every_column() {
    let (_directory, mut adapter) = migrated();
    let columns = described(
        &mut adapter,
        "\n            SELECT\n                column_name column_name,\n                data_type data_type,\n                column_type full_data_type,\n                character_maximum_length character_maximum_length,\n                numeric_precision numeric_precision,\n                numeric_scale numeric_scale,\n                datetime_precision datetime_precision,\n                column_default column_default,\n                is_nullable is_nullable,\n                extra extra,\n                table_name table_name,\n                IF(column_comment = '', NULL, column_comment) AS column_comment\n            FROM information_schema.columns\n            WHERE table_schema = ?\n            ORDER BY ordinal_position\n        ",
        &["prisma"],
    );
    let shown = shown(&columns);
    // MySQL orders by the column's place alone, so only each table's own
    // columns keep an order to compare.
    let of = |table: &str| {
        let written = format!("|{table}|NULL");
        shown
            .iter()
            .filter_map(|row| row.strip_suffix(&written))
            .map(str::to_owned)
            .collect::<Vec<_>>()
    };
    assert_eq!(
        of("_prisma_migrations"),
        [
            "id|varchar|varchar(36)|36|NULL|NULL|NULL|NULL|NO|",
            "checksum|varchar|varchar(64)|64|NULL|NULL|NULL|NULL|NO|",
            "finished_at|datetime|datetime(3)|NULL|NULL|NULL|3|NULL|YES|",
            "migration_name|varchar|varchar(255)|255|NULL|NULL|NULL|NULL|NO|",
            "logs|text|text|65535|NULL|NULL|NULL|NULL|YES|",
            "rolled_back_at|datetime|datetime(3)|NULL|NULL|NULL|3|NULL|YES|",
            "started_at|datetime|datetime(3)|NULL|NULL|NULL|3|CURRENT_TIMESTAMP(3)|NO|DEFAULT_GENERATED",
            "applied_steps_count|int|int unsigned|NULL|10|0|NULL|0|NO|",
        ]
    );
    assert_eq!(
        of("users"),
        [
            "id|bigint|bigint|NULL|19|0|NULL|NULL|NO|auto_increment",
            "email|varchar|varchar(191)|191|NULL|NULL|NULL|NULL|NO|",
            "name|varchar|varchar(100)|100|NULL|NULL|NULL|NULL|NO|",
            "balance|decimal|decimal(10,2)|NULL|10|2|NULL|0.00|NO|",
            "is_active|tinyint|tinyint(1)|NULL|3|0|NULL|1|NO|",
            "profile|json|json|NULL|NULL|NULL|NULL|NULL|YES|",
            "created_at|datetime|datetime(3)|NULL|NULL|NULL|3|CURRENT_TIMESTAMP(3)|NO|DEFAULT_GENERATED",
            "updated_at|datetime|datetime(3)|NULL|NULL|NULL|3|NULL|NO|",
        ]
    );
    assert_eq!(
        of("enumed"),
        [
            "id|int|int|NULL|10|0|NULL|NULL|NO|",
            "s|varchar|varchar(10)|10|NULL|NULL|NULL|it's|YES|",
            "e|enum|enum('a','b')|1|NULL|NULL|NULL|a|YES|",
            "f|double|double|NULL|22|NULL|NULL|1.5|YES|",
            "t|timestamp|timestamp|NULL|NULL|NULL|0|NULL|YES|",
            "u|timestamp|timestamp(6)|NULL|NULL|NULL|6|CURRENT_TIMESTAMP(6)|YES|DEFAULT_GENERATED on update CURRENT_TIMESTAMP(6)",
        ]
    );
    assert_eq!(shown.len(), 8 + 8 + 6 + 6 + 2 + 2 + 2);
    let key = MYSQL_NOT_NULL_FLAG | MYSQL_BINARY_FLAG | MYSQL_NO_DEFAULT_VALUE_FLAG;
    let blob = MYSQL_BLOB_FLAG | MYSQL_BINARY_FLAG;
    assert_eq!(
        described_columns(&columns),
        [
            ("column_name".to_owned(), MYSQL_TYPE_VAR_STRING, 45, 256, 0),
            (
                "data_type".to_owned(),
                MYSQL_TYPE_BLOB,
                45,
                201_326_580,
                blob
            ),
            (
                "full_data_type".to_owned(),
                MYSQL_TYPE_BLOB,
                45,
                67_108_860,
                blob | MYSQL_NOT_NULL_FLAG | MYSQL_NO_DEFAULT_VALUE_FLAG
            ),
            (
                "character_maximum_length".to_owned(),
                MYSQL_TYPE_LONGLONG,
                63,
                21,
                MYSQL_NUM_FLAG
            ),
            (
                "numeric_precision".to_owned(),
                MYSQL_TYPE_LONGLONG,
                63,
                10,
                MYSQL_UNSIGNED_FLAG | MYSQL_NUM_FLAG
            ),
            (
                "numeric_scale".to_owned(),
                MYSQL_TYPE_LONGLONG,
                63,
                10,
                MYSQL_UNSIGNED_FLAG | MYSQL_NUM_FLAG
            ),
            (
                "datetime_precision".to_owned(),
                MYSQL_TYPE_LONG,
                63,
                10,
                MYSQL_UNSIGNED_FLAG | MYSQL_NUM_FLAG
            ),
            (
                "column_default".to_owned(),
                MYSQL_TYPE_BLOB,
                45,
                262_140,
                blob
            ),
            (
                "is_nullable".to_owned(),
                MYSQL_TYPE_VAR_STRING,
                45,
                12,
                MYSQL_NOT_NULL_FLAG
            ),
            ("extra".to_owned(), MYSQL_TYPE_VAR_STRING, 45, 1024, 0),
            ("table_name".to_owned(), MYSQL_TYPE_VAR_STRING, 45, 256, key),
            (
                "column_comment".to_owned(),
                MYSQL_TYPE_BLOB,
                45,
                24_576,
                blob
            ),
        ]
    );
}

/// Every foreign key's columns and actions, joined and ordered on the names'
/// bytes, and every index's columns.
#[test]
fn the_describer_reads_the_foreign_keys_and_indexes() {
    let (_directory, mut adapter) = migrated();
    let keys = described(
        &mut adapter,
        "\n            SELECT\n                kcu.constraint_name constraint_name,\n                kcu.column_name column_name,\n                kcu.referenced_table_name referenced_table_name,\n                kcu.referenced_column_name referenced_column_name,\n                kcu.ordinal_position ordinal_position,\n                kcu.table_name table_name,\n                rc.delete_rule delete_rule,\n                rc.update_rule update_rule\n            FROM information_schema.key_column_usage AS kcu\n            INNER JOIN information_schema.referential_constraints AS rc ON\n                BINARY kcu.constraint_name = BINARY rc.constraint_name\n            WHERE\n                BINARY kcu.table_schema = ?\n                AND BINARY rc.constraint_schema = ?\n                AND kcu.referenced_column_name IS NOT NULL\n\n            ORDER BY\n                BINARY kcu.table_schema,\n                BINARY kcu.table_name,\n                BINARY kcu.constraint_name,\n                kcu.ordinal_position\n        ",
        &["prisma", "prisma"],
    );
    assert_eq!(
        shown(&keys),
        [
            "post_tag_post_id_fkey|post_id|posts|id|1|post_tag|CASCADE|CASCADE",
            "post_tag_tag_id_fkey|tag_id|tags|id|1|post_tag|CASCADE|CASCADE",
            "posts_user_id_fkey|user_id|users|id|1|posts|CASCADE|CASCADE",
        ]
    );
    let key = MYSQL_NOT_NULL_FLAG | MYSQL_BINARY_FLAG | MYSQL_NO_DEFAULT_VALUE_FLAG;
    assert_eq!(
        described_columns(&keys),
        [
            (
                "constraint_name".to_owned(),
                MYSQL_TYPE_VAR_STRING,
                45,
                256,
                0
            ),
            ("column_name".to_owned(), MYSQL_TYPE_VAR_STRING, 45, 256, 0),
            (
                "referenced_table_name".to_owned(),
                MYSQL_TYPE_VAR_STRING,
                45,
                256,
                MYSQL_BINARY_FLAG
            ),
            (
                "referenced_column_name".to_owned(),
                MYSQL_TYPE_VAR_STRING,
                45,
                256,
                0
            ),
            (
                "ordinal_position".to_owned(),
                MYSQL_TYPE_LONG,
                63,
                10,
                MYSQL_NOT_NULL_FLAG | MYSQL_UNSIGNED_FLAG | MYSQL_NUM_FLAG
            ),
            ("table_name".to_owned(), MYSQL_TYPE_VAR_STRING, 45, 256, key),
            (
                "delete_rule".to_owned(),
                MYSQL_TYPE_STRING,
                45,
                44,
                key | MYSQL_ENUM_FLAG
            ),
            (
                "update_rule".to_owned(),
                MYSQL_TYPE_STRING,
                45,
                44,
                key | MYSQL_ENUM_FLAG
            ),
        ]
    );
    // The two names are compared as written: MySQL's `BINARY` finds nothing
    // for a name spelled in another case.
    assert!(described(
        &mut adapter,
        "\n            SELECT\n                kcu.constraint_name constraint_name,\n                kcu.column_name column_name,\n                kcu.referenced_table_name referenced_table_name,\n                kcu.referenced_column_name referenced_column_name,\n                kcu.ordinal_position ordinal_position,\n                kcu.table_name table_name,\n                rc.delete_rule delete_rule,\n                rc.update_rule update_rule\n            FROM information_schema.key_column_usage AS kcu\n            INNER JOIN information_schema.referential_constraints AS rc ON\n                BINARY kcu.constraint_name = BINARY rc.constraint_name\n            WHERE\n                BINARY kcu.table_schema = ?\n                AND BINARY rc.constraint_schema = ?\n                AND kcu.referenced_column_name IS NOT NULL\n\n            ORDER BY\n                BINARY kcu.table_schema,\n                BINARY kcu.table_name,\n                BINARY kcu.constraint_name,\n                kcu.ordinal_position\n        ",
        &["PRISMA", "prisma"],
    )
    .rows
    .is_empty());

    let indexes = described(
        &mut adapter,
        "SELECT\n    table_name AS table_name,\n    index_name AS index_name,\n    column_name AS column_name,\n    sub_part AS partial,\n    seq_in_index AS seq_in_index,\n    collation AS column_order,\n    non_unique AS non_unique,\n    index_type AS index_type\nFROM information_schema.statistics\nWHERE table_schema = ?\nORDER BY BINARY table_name, BINARY index_name, seq_in_index\n",
        &["prisma"],
    );
    assert_eq!(
        shown(&indexes),
        [
            "_prisma_migrations|PRIMARY|id|NULL|1|A|0|BTREE",
            "checked|PRIMARY|id|NULL|1|A|0|BTREE",
            "enumed|PRIMARY|id|NULL|1|A|0|BTREE",
            "post_tag|PRIMARY|post_id|NULL|1|A|0|BTREE",
            "post_tag|PRIMARY|tag_id|NULL|2|A|0|BTREE",
            "post_tag|post_tag_tag_id_idx|tag_id|NULL|1|A|1|BTREE",
            "posts|PRIMARY|id|NULL|1|A|0|BTREE",
            "posts|posts_user_id_idx|user_id|NULL|1|A|1|BTREE",
            "tags|PRIMARY|id|NULL|1|A|0|BTREE",
            "tags|tags_name_key|name|NULL|1|A|0|BTREE",
            "users|PRIMARY|id|NULL|1|A|0|BTREE",
            "users|users_email_key|email|NULL|1|A|0|BTREE",
        ]
    );
    assert_eq!(
        described_columns(&indexes),
        [
            ("table_name".to_owned(), MYSQL_TYPE_VAR_STRING, 45, 256, key),
            ("index_name".to_owned(), MYSQL_TYPE_VAR_STRING, 45, 256, 0),
            ("column_name".to_owned(), MYSQL_TYPE_VAR_STRING, 45, 256, 0),
            (
                "partial".to_owned(),
                MYSQL_TYPE_LONGLONG,
                63,
                21,
                MYSQL_NUM_FLAG
            ),
            (
                "seq_in_index".to_owned(),
                MYSQL_TYPE_LONG,
                63,
                10,
                MYSQL_NOT_NULL_FLAG
                    | MYSQL_UNSIGNED_FLAG
                    | MYSQL_NO_DEFAULT_VALUE_FLAG
                    | MYSQL_NUM_FLAG
            ),
            ("column_order".to_owned(), MYSQL_TYPE_VAR_STRING, 45, 4, 0),
            (
                "non_unique".to_owned(),
                MYSQL_TYPE_LONG,
                63,
                2,
                MYSQL_NOT_NULL_FLAG | MYSQL_NUM_FLAG
            ),
            (
                "index_type".to_owned(),
                MYSQL_TYPE_VAR_STRING,
                45,
                44,
                MYSQL_NOT_NULL_FLAG | MYSQL_BINARY_FLAG
            ),
        ]
    );
}
