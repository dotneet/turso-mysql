//! Statements Gitea v1.27.3's integration suite sends through xorm and
//! go-sql-driver/mysql, replayed as the framework harness captured them, over
//! text and prepared statements as the driver sends each one.
//!
//! Every expectation here was measured on MySQL 8.4.11.

use super::*;

type Adapter = AuthorizedDatabaseCommandAdapter<RecordingAuthorizer>;

fn adapter() -> (tempfile::TempDir, Adapter) {
    let authorizer = Arc::new(RecordingAuthorizer::default());
    let (directory, _catalog, factory) = catalog_factory(authorizer);
    let mut adapter = factory
        .build(AuthenticatedPrincipal::from_account_id_for_testing(
            AccountId::from_bytes([211; 32]),
        ))
        .unwrap();
    adapter.authorize_connection().unwrap();
    adapter.execute_init_db("REPORTS").unwrap();
    (directory, adapter)
}

/// One value as go-sql-driver/mysql binds it.
#[derive(Clone, Copy, Debug)]
enum Bound<'a> {
    /// A Go `string`, bound as `STRING`.
    Word(&'a str),
    /// A Go `int64`, bound as `LONGLONG`.
    Whole(i64),
}

/// The null bitmap, the new-parameters flag, the types and the values, as
/// go-sql-driver/mysql sends them.
fn payload(values: &[Bound<'_>]) -> Vec<u8> {
    if values.is_empty() {
        return Vec::new();
    }
    let mut payload = vec![0; values.len().div_ceil(8)];
    payload.push(1);
    for value in values {
        let code = match value {
            Bound::Word(_) => MYSQL_TYPE_STRING,
            Bound::Whole(_) => MYSQL_TYPE_LONGLONG,
        };
        payload.extend_from_slice(&[code, 0]);
    }
    for value in values {
        match value {
            Bound::Word(text) => {
                payload.push(u8::try_from(text.len()).unwrap());
                payload.extend_from_slice(text.as_bytes());
            }
            Bound::Whole(number) => payload.extend_from_slice(&number.to_le_bytes()),
        }
    }
    payload
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

/// What xorm's `GetTables` asks before it syncs a schema, and again for every
/// `DBMetas`.
const XORM_GET_TABLES: &str = "SELECT `TABLE_NAME`, `ENGINE`, `AUTO_INCREMENT`, `TABLE_COMMENT`, `TABLE_COLLATION` from `INFORMATION_SCHEMA`.`TABLES` WHERE `TABLE_SCHEMA`=? AND (`ENGINE`='MyISAM' OR `ENGINE` = 'InnoDB' OR `ENGINE` = 'TokuDB')";

/// Measured on MySQL 8.4.11: `AUTO_INCREMENT` is what `SHOW CREATE TABLE`
/// prints as `AUTO_INCREMENT=<n>` — NULL for a table that counts nothing and
/// for one that has handed out no number yet — and an unsigned `LONGLONG` of
/// 21 with the binary flag.
#[test]
fn xorms_table_listing_answers_each_counters_next_number() {
    let (_directory, mut adapter) = adapter();
    run(
        &mut adapter,
        "CREATE TABLE IF NOT EXISTS `version` (`id` BIGINT(20) PRIMARY KEY AUTO_INCREMENT NOT NULL, `version` BIGINT(20) NULL)",
    );
    run(
        &mut adapter,
        "CREATE TABLE IF NOT EXISTS `auth_token` (`id` VARCHAR(255) PRIMARY KEY NOT NULL, `token_hash` VARCHAR(255) NULL, `user_id` BIGINT(20) NULL, `expires_unix` BIGINT(20) NULL) ENGINE=InnoDB",
    );
    let listing = |adapter: &mut Adapter| {
        let result = prepared_rows(adapter, XORM_GET_TABLES, &[Bound::Word("reports")]);
        let counter = &result.columns[2];
        assert_eq!(counter.name, "AUTO_INCREMENT");
        assert_eq!(counter.column_type, MYSQL_TYPE_LONGLONG);
        assert_eq!(counter.column_length, 21);
        assert_eq!(
            counter.flags,
            MYSQL_UNSIGNED_FLAG | MYSQL_BINARY_FLAG | MYSQL_NUM_FLAG
        );
        result.rows
    };
    let text = |value: &str| BinaryResultValue::Text(value.to_owned());
    let table = |name: &str, counter: BinaryResultValue| {
        vec![
            text(name),
            text("InnoDB"),
            counter,
            BinaryResultValue::Blob(Vec::new()),
            text("utf8mb4_0900_ai_ci"),
        ]
    };
    assert_eq!(
        listing(&mut adapter),
        vec![
            table("auth_token", BinaryResultValue::Null),
            table("records", BinaryResultValue::Null),
            table("version", BinaryResultValue::Null),
        ]
    );
    run(
        &mut adapter,
        "INSERT INTO `version` (`version`) VALUES (1), (2)",
    );
    assert_eq!(
        listing(&mut adapter)[2][2],
        BinaryResultValue::UnsignedInteger(3)
    );
    let text = |value: &str| Some(value.to_owned());
    assert_eq!(
        rows(
            &mut adapter,
            "SELECT TABLE_NAME, AUTO_INCREMENT FROM information_schema.TABLES WHERE TABLE_SCHEMA = DATABASE()"
        ),
        vec![
            vec![text("auth_token"), None],
            vec![text("records"), None],
            vec![text("version"), text("3")],
        ]
    );
}

/// Gitea's `CheckCollations` asks which case-sensitive collations the server
/// has with a `WHERE` joining its tests by `OR`. Measured on MySQL 8.4.11, it
/// answers `utf8mb4_bin` and every `_as_cs` collation, the `\_` matching an
/// underscore and nothing else; this server lists the collations it has, of
/// which `utf8mb4_bin` is the one that matches.
#[test]
fn giteas_collation_check_joins_its_tests_by_or() {
    let (_directory, mut adapter) = adapter();
    assert_eq!(
        rows(
            &mut adapter,
            "SHOW COLLATION WHERE (Collation = 'utf8mb4_bin') OR (Collation LIKE '%\\_as\\_cs%')"
        ),
        vec![vec![
            Some("utf8mb4_bin".to_owned()),
            Some("utf8mb4".to_owned()),
            Some("46".to_owned()),
            Some(String::new()),
            Some("Yes".to_owned()),
            Some("1".to_owned()),
            Some("PAD SPACE".to_owned()),
        ]]
    );
    assert_eq!(
        rows(
            &mut adapter,
            "SHOW COLLATION WHERE Collation LIKE 'utf8mb4\\_0900%' OR Id = 63"
        )
        .into_iter()
        .map(|row| row[0].clone().unwrap())
        .collect::<Vec<_>>(),
        ["binary", "utf8mb4_0900_ai_ci"]
    );
    assert!(matches!(
        adapter.execute_query("SHOW COLLATION WHERE Id = 63 OR Nothing = 'x'"),
        Err(FrontendErrorKind::UnknownColumn)
    ));
}

fn create_table(adapter: &mut Adapter, table: &str) -> String {
    rows(adapter, &format!("SHOW CREATE TABLE `{table}`"))[0][1]
        .clone()
        .unwrap()
}

/// xorm writes `ROW_FORMAT=DYNAMIC` after every table Gitea makes, and
/// `ALTER TABLE t ROW_FORMAT=dynamic` over each when Gitea converts a
/// database. Measured on MySQL 8.4.11: it is printed back upper-cased after
/// the collation and before the comment, kept across a rewrite and by `LIKE`,
/// reported as `row_format=DYNAMIC` in `SHOW TABLE STATUS`, and `DEFAULT`
/// takes it away.
#[test]
fn xorms_dynamic_row_format_is_kept_and_printed_back() {
    let (_directory, mut adapter) = adapter();
    run(
        &mut adapter,
        "CREATE TABLE IF NOT EXISTS `version` (`id` BIGINT(20) PRIMARY KEY AUTO_INCREMENT NOT NULL, `version` BIGINT(20) NULL) ROW_FORMAT=DYNAMIC",
    );
    run(
        &mut adapter,
        "CREATE TABLE IF NOT EXISTS `session` (`key` CHAR(16) PRIMARY KEY NOT NULL, `data` BLOB NULL, `expiry` BIGINT(20) NULL) ENGINE=InnoDB ROW_FORMAT=DYNAMIC COMMENT='kept'",
    );
    assert_eq!(
        create_table(&mut adapter, "version"),
        "CREATE TABLE `version` (\n  `id` bigint NOT NULL AUTO_INCREMENT,\n  `version` bigint DEFAULT NULL,\n  PRIMARY KEY (`id`)\n) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_0900_ai_ci ROW_FORMAT=DYNAMIC"
    );
    run(&mut adapter, "ALTER TABLE `session` ADD COLUMN `n` INT");
    assert!(create_table(&mut adapter, "session").ends_with(
        ") ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_0900_ai_ci ROW_FORMAT=DYNAMIC COMMENT='kept'"
    ));
    let status = rows(&mut adapter, "SHOW TABLE STATUS LIKE 'session'");
    assert_eq!(status[0][16].as_deref(), Some("row_format=DYNAMIC"));
    run(&mut adapter, "CREATE TABLE `copied` LIKE `session`");
    assert!(create_table(&mut adapter, "copied").ends_with("ROW_FORMAT=DYNAMIC COMMENT='kept'"));
    run(&mut adapter, "ALTER TABLE `session` ROW_FORMAT=DEFAULT");
    assert!(create_table(&mut adapter, "session")
        .ends_with("COLLATE=utf8mb4_0900_ai_ci COMMENT='kept'"));
    run(&mut adapter, "ALTER TABLE `session` ROW_FORMAT=dynamic");
    assert!(create_table(&mut adapter, "session").ends_with("ROW_FORMAT=DYNAMIC COMMENT='kept'"));
    assert_eq!(
        rows(&mut adapter, "SHOW TABLE STATUS LIKE 'version'")[0][16].as_deref(),
        Some("row_format=DYNAMIC")
    );
    run(
        &mut adapter,
        "CREATE TABLE `plain` (`id` INT) ROW_FORMAT=DEFAULT",
    );
    assert!(create_table(&mut adapter, "plain").ends_with("COLLATE=utf8mb4_0900_ai_ci"));
    for sql in [
        "CREATE TABLE `compact` (`id` INT) ROW_FORMAT=COMPACT",
        "ALTER TABLE `session` ROW_FORMAT=COMPRESSED",
    ] {
        assert!(
            matches!(
                adapter.execute_query(sql),
                Err(FrontendErrorKind::Unsupported)
            ),
            "{sql} must be refused"
        );
    }
}

/// xorm writes `BIGINT(20) PRIMARY KEY` for every key it does not count —
/// Gitea's `action_run_index` among them. Measured on MySQL 8.4.11, the
/// width is dropped as on any other integer column and the key is printed
/// `bigint NOT NULL`.
#[test]
fn xorms_uncounted_key_takes_a_display_width() {
    let (_directory, mut adapter) = adapter();
    run(
        &mut adapter,
        "CREATE TABLE IF NOT EXISTS `action_run_index` (`group_id` BIGINT(20) PRIMARY KEY NOT NULL, `max_index` BIGINT(20) NULL) ENGINE=InnoDB ROW_FORMAT=DYNAMIC",
    );
    assert_eq!(
        create_table(&mut adapter, "action_run_index"),
        "CREATE TABLE `action_run_index` (\n  `group_id` bigint NOT NULL,\n  `max_index` bigint DEFAULT NULL,\n  PRIMARY KEY (`group_id`)\n) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_0900_ai_ci ROW_FORMAT=DYNAMIC"
    );
    run(
        &mut adapter,
        "INSERT INTO `action_run_index` (`group_id`, `max_index`) VALUES (7, 1)",
    );
    assert!(matches!(
        adapter.execute_query(
            "INSERT INTO `action_run_index` (`group_id`, `max_index`) VALUES (7, 2)"
        ),
        Err(FrontendErrorKind::ConstraintViolation)
    ));
    // An `INTEGER` key written with a width is still no rowid alias: a row
    // without one is refused rather than numbered.
    run(
        &mut adapter,
        "CREATE TABLE `keyed` (`id` INTEGER(11) PRIMARY KEY, `n` INT)",
    );
    assert!(create_table(&mut adapter, "keyed")
        .starts_with("CREATE TABLE `keyed` (\n  `id` int NOT NULL,"));
    assert!(adapter
        .execute_query("INSERT INTO `keyed` (`n`) VALUES (1)")
        .is_err());
}

/// What xorm's `GetColumns` asks for every table it syncs.
const XORM_GET_COLUMNS: &str = "SELECT `COLUMN_NAME`, `IS_NULLABLE`, `COLUMN_DEFAULT`, `COLUMN_TYPE`, `COLUMN_KEY`, `EXTRA`, `COLUMN_COMMENT`, `CHARACTER_MAXIMUM_LENGTH`, (INSTR(VERSION(), 'maria') > 0 && (SUBSTRING_INDEX(VERSION(), '.', 1) > 10 || (SUBSTRING_INDEX(VERSION(), '.', 1) = 10 && (SUBSTRING_INDEX(SUBSTRING(VERSION(), 4), '.', 1) > 2 || (SUBSTRING_INDEX(SUBSTRING(VERSION(), 4), '.', 1) = 2 && SUBSTRING_INDEX(SUBSTRING(VERSION(), 6), '-', 1) >= 7))))) AS NEEDS_QUOTE, `COLLATION_NAME` FROM `INFORMATION_SCHEMA`.`COLUMNS` WHERE `TABLE_SCHEMA` = ? AND `TABLE_NAME` = ? ORDER BY `COLUMNS`.ORDINAL_POSITION ASC";

/// xorm's column listing tests `VERSION()` for a MariaDB inside the
/// statement; on a server that is none the test answers 0 and the rest reads
/// `information_schema.COLUMNS`. Every row here is the one MySQL 8.4.11
/// answered for the same table.
#[test]
fn xorms_column_listing_answers_its_version_test() {
    let (_directory, mut adapter) = adapter();
    run(
        &mut adapter,
        "CREATE TABLE IF NOT EXISTS `ver1` (`id` BIGINT(20) PRIMARY KEY AUTO_INCREMENT NOT NULL, `version` BIGINT(20) NULL, `name` VARCHAR(255) DEFAULT '' NOT NULL COMMENT 'nm', `flag` TINYINT(1) DEFAULT false NOT NULL, `body` TEXT NULL, `created` DATETIME NULL, UNIQUE INDEX `u1` (`name`)) ENGINE=InnoDB ROW_FORMAT=DYNAMIC",
    );
    let result = prepared_rows(
        &mut adapter,
        XORM_GET_COLUMNS,
        &[Bound::Word("reports"), Bound::Word("ver1")],
    );
    let needs_quote = &result.columns[8];
    assert_eq!(needs_quote.name, "NEEDS_QUOTE");
    assert_eq!(needs_quote.column_type, MYSQL_TYPE_LONGLONG);
    let word = |value: &str| BinaryResultValue::Text(value.to_owned());
    let bytes = |value: &str| BinaryResultValue::Blob(value.as_bytes().to_vec());
    let null = BinaryResultValue::Null;
    let zero = BinaryResultValue::Integer(0);
    let row = |name: &str,
               nullable: &str,
               default: BinaryResultValue,
               declared: &str,
               key: &str,
               extra: &str,
               comment: &str,
               length: BinaryResultValue,
               collation: BinaryResultValue| {
        vec![
            word(name),
            word(nullable),
            default,
            bytes(declared),
            word(key),
            word(extra),
            bytes(comment),
            length,
            zero.clone(),
            collation,
        ]
    };
    let ai_ci = || word("utf8mb4_0900_ai_ci");
    assert_eq!(
        result.rows,
        vec![
            row(
                "id",
                "NO",
                null.clone(),
                "bigint",
                "PRI",
                "auto_increment",
                "",
                null.clone(),
                null.clone()
            ),
            row(
                "version",
                "YES",
                null.clone(),
                "bigint",
                "",
                "",
                "",
                null.clone(),
                null.clone()
            ),
            row(
                "name",
                "NO",
                bytes(""),
                "varchar(255)",
                "UNI",
                "",
                "nm",
                BinaryResultValue::Integer(255),
                ai_ci()
            ),
            row(
                "flag",
                "NO",
                bytes("0"),
                "tinyint(1)",
                "",
                "",
                "",
                null.clone(),
                null.clone()
            ),
            row(
                "body",
                "YES",
                null.clone(),
                "text",
                "",
                "",
                "",
                BinaryResultValue::Integer(65535),
                ai_ci()
            ),
            row(
                "created",
                "YES",
                null.clone(),
                "datetime",
                "",
                "",
                "",
                null.clone(),
                null
            ),
        ]
    );
}

fn first_column(adapter: &mut Adapter, sql: &str) -> Vec<String> {
    rows(adapter, sql)
        .into_iter()
        .map(|row| row[0].clone().unwrap())
        .collect()
}

/// Gitea's consistency checks find the rows whose kept count is off by
/// comparing the count column with a correlated `COUNT(*)`. A count answers
/// exactly one whole number per row, so it meets a column held to a whole
/// number the way a written one does. Every answer here was measured on
/// MySQL 8.4.11 over the same rows, a NULL count column answering no row.
#[test]
fn giteas_consistency_checks_compare_a_count_column_with_a_count() {
    let (_directory, mut adapter) = adapter();
    for sql in [
        "CREATE TABLE `repository` (`id` BIGINT(20) PRIMARY KEY AUTO_INCREMENT NOT NULL, `num_watches` INT NULL, `num_stars` INT NULL)",
        "CREATE TABLE `watch` (`id` BIGINT(20) PRIMARY KEY AUTO_INCREMENT NOT NULL, `user_id` BIGINT(20) NULL, `repo_id` BIGINT(20) NULL, `mode` SMALLINT DEFAULT 1 NOT NULL)",
        "CREATE TABLE `star` (`id` BIGINT(20) PRIMARY KEY AUTO_INCREMENT NOT NULL, `uid` BIGINT(20) NULL, `repo_id` BIGINT(20) NULL)",
        "INSERT INTO `repository` (`num_watches`, `num_stars`) VALUES (1, 0), (2, 1), (0, NULL), (3, 2)",
        "INSERT INTO `watch` (`user_id`, `repo_id`, `mode`) VALUES (1, 1, 1), (2, 2, 1), (3, 2, 2), (4, 4, 1), (5, 4, 1), (6, 4, 1)",
        "INSERT INTO `star` (`uid`, `repo_id`) VALUES (1, 2), (2, 4), (3, 4), (4, 3)",
    ] {
        run(&mut adapter, sql);
    }
    assert_eq!(
        first_column(
            &mut adapter,
            "SELECT repo.id FROM `repository` repo WHERE repo.num_watches!=(SELECT COUNT(*) FROM `watch` WHERE repo_id=repo.id AND mode<>2)"
        ),
        ["2"]
    );
    assert!(first_column(
        &mut adapter,
        "SELECT repo.id FROM `repository` repo WHERE repo.num_stars!=(SELECT COUNT(*) FROM `star` WHERE repo_id=repo.id)"
    )
    .is_empty());
    assert_eq!(
        first_column(
            &mut adapter,
            "SELECT id FROM repository WHERE num_watches = (SELECT COUNT(*) FROM watch WHERE watch.repo_id = repository.id) ORDER BY id"
        ),
        ["1", "2", "3", "4"]
    );
    assert!(first_column(
        &mut adapter,
        "SELECT id FROM repository WHERE (SELECT COUNT(*) FROM star WHERE star.repo_id = repository.id) < num_stars"
    )
    .is_empty());
    // A word meets a count by being read as a number, which is not worked
    // out here.
    run(
        &mut adapter,
        "CREATE TABLE `label` (`id` BIGINT(20) PRIMARY KEY AUTO_INCREMENT NOT NULL, `name` VARCHAR(10) NULL)",
    );
    assert!(matches!(
        adapter.execute_query(
            "SELECT id FROM label WHERE name != (SELECT COUNT(*) FROM star WHERE star.repo_id = label.id)"
        ),
        Err(FrontendErrorKind::Unsupported)
    ));
    // A `BIGINT UNSIGNED` is held in a form of its own, which a count the
    // engine answers as a plain integer does not compare with.
    run(
        &mut adapter,
        "CREATE TABLE `counted` (`id` BIGINT(20) PRIMARY KEY AUTO_INCREMENT NOT NULL, `big` BIGINT UNSIGNED NULL)",
    );
    for sql in [
        "SELECT id FROM counted WHERE big > (SELECT COUNT(*) FROM star)",
        "UPDATE counted SET big = (SELECT COUNT(*) FROM star) WHERE id = 1",
    ] {
        assert!(
            matches!(
                adapter.execute_query(sql),
                Err(FrontendErrorKind::Unsupported)
            ),
            "{sql} must be refused"
        );
    }
}

/// Gitea decides which repositories a user may see with `repository.id IN
/// (SELECT team_repo.repo_id FROM team_repo INNER JOIN team_user ON ...)`: a
/// subquery joining tables, projecting one column named through one of them.
/// That column is held to the outer one's kind the way a one-table
/// subquery's is. Every answer here was measured on MySQL 8.4.11 over the
/// same rows.
#[test]
fn giteas_access_checks_read_a_column_of_a_joined_subquery() {
    let (_directory, mut adapter) = adapter();
    for sql in [
        "CREATE TABLE `repository` (`id` BIGINT(20) PRIMARY KEY AUTO_INCREMENT NOT NULL, `owner_id` BIGINT(20) NULL, `is_private` TINYINT(1) DEFAULT 0 NOT NULL, `lower_name` VARCHAR(255) NULL)",
        "CREATE TABLE `team_repo` (`id` BIGINT(20) PRIMARY KEY AUTO_INCREMENT NOT NULL, `org_id` BIGINT(20) NULL, `team_id` BIGINT(20) NULL, `repo_id` BIGINT(20) NULL)",
        "CREATE TABLE `team_user` (`id` BIGINT(20) PRIMARY KEY AUTO_INCREMENT NOT NULL, `org_id` BIGINT(20) NULL, `team_id` BIGINT(20) NULL, `uid` BIGINT(20) NULL)",
        "CREATE TABLE `team_unit` (`id` BIGINT(20) PRIMARY KEY AUTO_INCREMENT NOT NULL, `team_id` BIGINT(20) NULL, `type` INT NULL, `access_mode` INT NULL)",
        "INSERT INTO `repository` (`owner_id`, `is_private`) VALUES (1, 0), (2, 1), (3, 1), (2, 0), (4, 1)",
        "INSERT INTO `team_repo` (`org_id`, `team_id`, `repo_id`) VALUES (3, 10, 3), (3, 11, 5), (3, 10, 5)",
        "INSERT INTO `team_user` (`org_id`, `team_id`, `uid`) VALUES (3, 10, 7), (3, 11, 8)",
        "INSERT INTO `team_unit` (`team_id`, `type`, `access_mode`) VALUES (10, 1, 2), (11, 1, 0), (11, 2, 2)",
    ] {
        run(&mut adapter, sql);
    }
    assert_eq!(
        first_column(
            &mut adapter,
            "SELECT id FROM repository WHERE repository.id IN (SELECT team_repo.repo_id FROM team_repo INNER JOIN team_user ON team_user.team_id = team_repo.team_id WHERE team_user.uid=7) OR repository.owner_id=1 ORDER BY id"
        ),
        ["1", "3", "5"]
    );
    assert_eq!(
        first_column(
            &mut adapter,
            "SELECT id FROM repository WHERE (repository.is_private=1 AND repository.id NOT IN (SELECT team_repo.repo_id FROM team_repo INNER JOIN team_user ON team_user.team_id = team_repo.team_id WHERE team_user.uid=8)) ORDER BY id"
        ),
        ["2", "3"]
    );
    assert!(first_column(
        &mut adapter,
        "SELECT id FROM repository WHERE repository.id IN (SELECT `team_repo`.repo_id FROM team_repo INNER JOIN team_user ON `team_user`.team_id = `team_repo`.team_id LEFT JOIN team_unit ON `team_unit`.team_id = `team_repo`.team_id AND `team_unit`.`type` = 1 WHERE `team_user`.uid=8 AND (`team_unit`.`access_mode`>0)) ORDER BY id"
    )
    .is_empty());
    assert_eq!(
        first_column(
            &mut adapter,
            "SELECT id FROM repository WHERE id IN (SELECT team_repo.repo_id FROM team_repo INNER JOIN team_user ON team_user.team_id = team_repo.team_id LEFT JOIN team_unit ON team_unit.team_id = team_repo.team_id AND team_unit.type = 2 WHERE team_user.uid = 8 AND team_unit.access_mode > 0) ORDER BY id"
        ),
        ["5"]
    );
    for sql in [
        // A word against a whole number is a coercion MySQL makes and this
        // does not.
        "SELECT id FROM repository WHERE lower_name IN (SELECT team_repo.repo_id FROM team_repo INNER JOIN team_user ON team_user.team_id = team_repo.team_id)",
        // Which of the joined tables an unqualified name belongs to is not
        // worked out here.
        "SELECT id FROM repository WHERE id IN (SELECT repo_id FROM team_repo INNER JOIN team_user ON team_user.team_id = team_repo.team_id)",
    ] {
        assert!(
            matches!(
                adapter.execute_query(sql),
                Err(FrontendErrorKind::Unsupported)
            ),
            "{sql} must be refused"
        );
    }
}

/// xorm writes `max(index) AS index ... ORDER BY max(index)` for Gitea's
/// latest commit statuses. Measured on MySQL 8.4.11, the name inside the
/// aggregate is the table's column even where a result column shares it:
/// `MIN(n) AS n ... ORDER BY MAX(n) DESC` orders the groups by the column's
/// largest value, the same rows in the same order as here.
#[test]
fn an_ordering_aggregate_reads_the_column_an_alias_shares_its_name_with() {
    let (_directory, mut adapter) = adapter();
    run(
        &mut adapter,
        "CREATE TABLE `commit_status` (`id` BIGINT(20) PRIMARY KEY AUTO_INCREMENT NOT NULL, `index` BIGINT(20) NULL, `repo_id` BIGINT(20) NULL, `sha` VARCHAR(64) NOT NULL, `context_hash` VARCHAR(64) NULL)",
    );
    run(
        &mut adapter,
        "INSERT INTO `commit_status` (`index`, `repo_id`, `sha`, `context_hash`) VALUES (1, 1, 'x', 'a'), (9, 1, 'x', 'a'), (5, 1, 'x', 'b'), (6, 1, 'x', 'b'), (3, 1, 'x', 'c'), (4, 1, 'y', 'c')",
    );
    assert_eq!(
        rows(
            &mut adapter,
            "SELECT min(`index`) AS `index`, context_hash FROM commit_status GROUP BY context_hash ORDER BY max(`index`) DESC"
        ),
        [
            [Some("1".to_owned()), Some("a".to_owned())],
            [Some("5".to_owned()), Some("b".to_owned())],
            [Some("3".to_owned()), Some("c".to_owned())],
        ]
    );
    assert_eq!(
        first_column(
            &mut adapter,
            "SELECT max( `index` ) as `index` FROM `commit_status` WHERE (repo_id = 1) AND (sha = 'x') GROUP BY context_hash ORDER BY max( `index` ) desc"
        ),
        ["9", "6", "3"]
    );
}

/// Gitea searches users with `LOWER(full_name) LIKE ?` beside `lower_name
/// LIKE ?`. Measured on MySQL 8.4.11, the call's answer carries the column's
/// collation, whose `LIKE` ignores case and accents and matches character by
/// character; every answer here is MySQL's over the same rows.
#[test]
fn giteas_user_search_matches_a_case_changed_column() {
    let (_directory, mut adapter) = adapter();
    run(
        &mut adapter,
        "CREATE TABLE `user` (`id` BIGINT(20) PRIMARY KEY AUTO_INCREMENT NOT NULL, `lower_name` VARCHAR(255) NOT NULL, `full_name` VARCHAR(255) NULL, `email` VARCHAR(255) NOT NULL, `type` INT NULL)",
    );
    run(
        &mut adapter,
        "INSERT INTO `user` (`lower_name`, `full_name`, `email`, `type`) VALUES ('ada', 'Ada Lovelace', 'ADA@example.com', 0), ('emile', 'ÉMILE Zola', 'emile@x.org', 0), ('bob', NULL, 'Bob@X.org', 1), ('org', 'Straße Org', 's@x.org', 0), ('under', 'a_b', 'u@x.org', 0)",
    );
    for (sql, found) in [
        (
            "SELECT id FROM `user` WHERE LOWER(full_name) LIKE '%emile%' ORDER BY id",
            &["2"][..],
        ),
        (
            "SELECT id FROM `user` WHERE LOWER(full_name) LIKE '%STRASSE%' ORDER BY id",
            &[],
        ),
        (
            "SELECT id FROM `user` WHERE UPPER(email) LIKE '%x.org' ORDER BY id",
            &["2", "3", "4", "5"],
        ),
        (
            "SELECT id FROM `user` WHERE LOWER(full_name) NOT LIKE '%a%' ORDER BY id",
            &[],
        ),
        (
            "SELECT id FROM `user` WHERE LOWER(full_name) LIKE 'a\\_b' ORDER BY id",
            &["5"],
        ),
    ] {
        assert_eq!(first_column(&mut adapter, sql), found, "{sql}");
    }
    let counted = prepared_rows(
        &mut adapter,
        "SELECT count(*) FROM `user` WHERE type IN (?) AND (lower_name LIKE ? OR LOWER(full_name) LIKE ? OR LOWER(email) LIKE ?)",
        &[
            Bound::Whole(0),
            Bound::Word("%o%"),
            Bound::Word("%o%"),
            Bound::Word("%o%"),
        ],
    );
    assert_eq!(counted.rows, [[BinaryResultValue::Integer(4)]]);
}

/// xorm joins with an unqualified name on one side of the `ON` — Gitea's
/// assignees are read with `SELECT * FROM user INNER JOIN issue_assignees ON
/// assignee_id = user.id`. Measured on MySQL 8.4.11, the name is the column
/// of whichever joined table has one, and a name two of them have is 1052.
#[test]
fn an_unqualified_name_in_a_join_is_the_column_of_the_table_that_has_it() {
    let (_directory, mut adapter) = adapter();
    for sql in [
        "CREATE TABLE `user` (`id` BIGINT(20) PRIMARY KEY AUTO_INCREMENT NOT NULL, `lower_name` VARCHAR(255) NOT NULL, `name` VARCHAR(255) NOT NULL)",
        "CREATE TABLE `issue_assignees` (`id` BIGINT(20) PRIMARY KEY AUTO_INCREMENT NOT NULL, `assignee_id` BIGINT(20) NULL, `issue_id` BIGINT(20) NULL)",
        "INSERT INTO `user` (`lower_name`, `name`) VALUES ('a', 'A'), ('b', 'B')",
        "INSERT INTO `issue_assignees` (`assignee_id`, `issue_id`) VALUES (1, 5), (2, 5), (2, 6)",
    ] {
        run(&mut adapter, sql);
    }
    let text = |value: &str| Some(value.to_owned());
    assert_eq!(
        rows(
            &mut adapter,
            "SELECT * FROM `user` INNER JOIN `issue_assignees` ON assignee_id = `user`.id WHERE (issue_assignees.issue_id = 5)"
        ),
        [
            [text("1"), text("a"), text("A"), text("1"), text("1"), text("5")],
            [text("2"), text("b"), text("B"), text("2"), text("2"), text("5")],
        ]
    );
    let assigned = prepared_rows(
        &mut adapter,
        "SELECT * FROM `user` INNER JOIN `issue_assignees` ON assignee_id = `user`.id WHERE (issue_assignees.issue_id = ?)",
        &[Bound::Whole(6)],
    );
    assert_eq!(assigned.rows.len(), 1);
    assert_eq!(assigned.rows[0][2], BinaryResultValue::Text("B".to_owned()));
    assert_eq!(
        first_column(
            &mut adapter,
            "SELECT `user`.name FROM `user` INNER JOIN `issue_assignees` ON assignee_id = `user`.id WHERE issue_id = 6"
        ),
        ["B"]
    );
    assert!(matches!(
        adapter.execute_query(
            "SELECT `user`.id FROM `user` INNER JOIN `issue_assignees` ON id = `user`.id"
        ),
        Err(FrontendErrorKind::AmbiguousColumn)
    ));
    // A word against a number is still refused, the column found in the join.
    assert!(matches!(
        adapter.execute_query(
            "SELECT `user`.name FROM `user` INNER JOIN `issue_assignees` ON assignee_id = `user`.id WHERE lower_name = 5"
        ),
        Err(FrontendErrorKind::Unsupported)
    ));
}

/// Gitea asks which teams may write to a repository with `team.id IN (SELECT
/// team_id FROM team_unit ...)` in a statement joining `team` and
/// `team_repo`. The outer column is named through one of the joined tables,
/// or without one where one of them alone has it. Every answer here was
/// measured on MySQL 8.4.11 over the same rows.
#[test]
fn a_membership_test_reads_a_column_of_a_joined_statement() {
    let (_directory, mut adapter) = adapter();
    for sql in [
        "CREATE TABLE `team` (`id` BIGINT(20) PRIMARY KEY AUTO_INCREMENT NOT NULL, `org_id` BIGINT(20) NULL, `lower_name` VARCHAR(255) NULL, `authorize` INT NULL)",
        "CREATE TABLE `team_repo` (`id` BIGINT(20) PRIMARY KEY AUTO_INCREMENT NOT NULL, `org_id` BIGINT(20) NULL, `team_id` BIGINT(20) NULL, `repo_id` BIGINT(20) NULL)",
        "CREATE TABLE `team_unit` (`id` BIGINT(20) PRIMARY KEY AUTO_INCREMENT NOT NULL, `org_id` BIGINT(20) NULL, `team_id` BIGINT(20) NULL, `type` INT NULL, `access_mode` INT NULL)",
        "INSERT INTO `team` (`org_id`, `lower_name`, `authorize`) VALUES (3, 'owners', 4), (3, 'devs', 1), (3, 'readers', 1)",
        "INSERT INTO `team_repo` (`org_id`, `team_id`, `repo_id`) VALUES (3, 1, 7), (3, 2, 7), (3, 3, 7), (3, 3, 8)",
        "INSERT INTO `team_unit` (`org_id`, `team_id`, `type`, `access_mode`) VALUES (3, 2, 1, 2), (3, 3, 1, 1)",
    ] {
        run(&mut adapter, sql);
    }
    assert_eq!(
        first_column(
            &mut adapter,
            "SELECT team.id FROM `team` INNER JOIN `team_repo` ON team_repo.team_id = team.id WHERE (team_repo.org_id = 3 AND team_repo.repo_id = 7) AND ((team.authorize >= 2) OR team.id IN (SELECT team_id FROM team_unit WHERE (team_unit.team_id = team.id) AND team_unit.type = 1 AND team_unit.access_mode >= 2)) ORDER BY team.id"
        ),
        ["1", "2"]
    );
    assert_eq!(
        first_column(
            &mut adapter,
            "SELECT team.id FROM `team` INNER JOIN `team_repo` ON team_repo.team_id = team.id WHERE team_repo.repo_id = 8 AND team_id NOT IN (SELECT team_id FROM team_unit WHERE access_mode >= 2) ORDER BY team.id"
        ),
        ["3"]
    );
    assert!(matches!(
        adapter.execute_query(
            "SELECT team.id FROM `team` INNER JOIN `team_repo` ON team_repo.team_id = team.id WHERE team.lower_name IN (SELECT team_id FROM team_unit)"
        ),
        Err(FrontendErrorKind::Unsupported)
    ));
}

/// Gitea keeps its counts with `UPDATE ... SET num_x = (SELECT COUNT(*) ...)`,
/// joining tables inside for a label's closed issues. A count answers one
/// whole number, so it is written into a column holding whole numbers; every
/// row here is the one MySQL 8.4.11 left, and a count too large for the
/// column is refused as MySQL refuses it.
#[test]
fn giteas_count_updates_write_a_count_into_a_whole_number_column() {
    let (_directory, mut adapter) = adapter();
    for sql in [
        "CREATE TABLE `repository` (`id` BIGINT(20) PRIMARY KEY AUTO_INCREMENT NOT NULL, `num_watches` INT NULL, `lower_name` VARCHAR(255) NULL, `tiny` TINYINT NULL)",
        "CREATE TABLE `watch` (`id` BIGINT(20) PRIMARY KEY AUTO_INCREMENT NOT NULL, `repo_id` BIGINT(20) NULL, `mode` SMALLINT DEFAULT 1 NOT NULL)",
        "CREATE TABLE `label` (`id` BIGINT(20) PRIMARY KEY AUTO_INCREMENT NOT NULL, `num_issues` INT NULL, `num_closed_issues` INT NULL, `updated_unix` BIGINT(20) NULL)",
        "CREATE TABLE `issue_label` (`id` BIGINT(20) PRIMARY KEY AUTO_INCREMENT NOT NULL, `issue_id` BIGINT(20) NULL, `label_id` BIGINT(20) NULL)",
        "CREATE TABLE `issue` (`id` BIGINT(20) PRIMARY KEY AUTO_INCREMENT NOT NULL, `is_closed` TINYINT(1) NULL)",
        "INSERT INTO `repository` (`num_watches`) VALUES (0), (0)",
        "INSERT INTO `watch` (`repo_id`, `mode`) VALUES (1, 1), (1, 2), (1, 1), (2, 3)",
        "INSERT INTO `label` (`num_issues`) VALUES (0), (0)",
        "INSERT INTO `issue` (`is_closed`) VALUES (0), (1), (1)",
        "INSERT INTO `issue_label` (`issue_id`, `label_id`) VALUES (1, 1), (2, 1), (3, 1), (2, 2)",
    ] {
        run(&mut adapter, sql);
    }
    let changed = prepared(
        &mut adapter,
        "UPDATE `repository` SET num_watches=(SELECT COUNT(*) FROM `watch` WHERE repo_id=? AND mode<>2) WHERE id=?",
        &[Bound::Whole(1), Bound::Whole(1)],
    );
    assert!(matches!(
        changed,
        Ok(PreparedStatementExecutionResult::Ok(CommandOkResult {
            affected_rows: 1,
            ..
        }))
    ));
    run(
        &mut adapter,
        "UPDATE `label` SET `updated_unix` = 5, `num_issues` = (SELECT count(*) FROM issue_label WHERE label_id=1), `num_closed_issues` = (SELECT count(*) FROM issue_label INNER JOIN issue ON issue_label.issue_id = issue.id WHERE issue.is_closed=1 AND issue_label.label_id=1) WHERE `id`=1",
    );
    let text = |value: &str| Some(value.to_owned());
    assert_eq!(
        rows(
            &mut adapter,
            "SELECT id, num_watches FROM repository ORDER BY id"
        ),
        [[text("1"), text("2")], [text("2"), text("0")]]
    );
    assert_eq!(
        rows(
            &mut adapter,
            "SELECT id, num_issues, num_closed_issues, updated_unix FROM label ORDER BY id"
        ),
        [
            [text("1"), text("3"), text("2"), text("5")],
            [text("2"), text("0"), None, None],
        ]
    );
    // MySQL writes a count into a column of words as its digits, which is a
    // conversion not worked out here.
    assert!(matches!(
        adapter.execute_query(
            "UPDATE `repository` SET lower_name=(SELECT COUNT(*) FROM `watch` WHERE repo_id=1) WHERE id=2"
        ),
        Err(FrontendErrorKind::Unsupported)
    ));
    run(
        &mut adapter,
        &format!(
            "INSERT INTO `watch` (`repo_id`) VALUES {}",
            vec!["(9)"; 130].join(", ")
        ),
    );
    assert!(matches!(
        adapter.execute_query(
            "UPDATE `repository` SET tiny=(SELECT COUNT(*) FROM `watch` WHERE repo_id=9) WHERE id=1"
        ),
        Err(FrontendErrorKind::OutOfRange)
    ));
}

/// Gitea nests one membership test inside another's subquery — its branch
/// listing asks `repo_id IN (SELECT id FROM repository WHERE
/// repository.owner_id NOT IN (SELECT id FROM user WHERE ...))`. The inner
/// test's column is the subquery's own, named through its table or not.
/// Every answer here was measured on MySQL 8.4.11 over the same rows.
#[test]
fn a_membership_test_inside_a_subquery_reads_the_subquerys_column() {
    let (_directory, mut adapter) = adapter();
    for sql in [
        "CREATE TABLE `branch` (`id` BIGINT(20) PRIMARY KEY AUTO_INCREMENT NOT NULL, `repo_id` BIGINT(20) NULL, `name` VARCHAR(255) NOT NULL)",
        "CREATE TABLE `repository` (`id` BIGINT(20) PRIMARY KEY AUTO_INCREMENT NOT NULL, `owner_id` BIGINT(20) NULL, `is_private` TINYINT(1) DEFAULT 0 NOT NULL)",
        "CREATE TABLE `user` (`id` BIGINT(20) PRIMARY KEY AUTO_INCREMENT NOT NULL, `visibility` INT DEFAULT 0 NOT NULL, `name` VARCHAR(255) NULL)",
        "CREATE TABLE `collaboration` (`id` BIGINT(20) PRIMARY KEY AUTO_INCREMENT NOT NULL, `repo_id` BIGINT(20) NOT NULL, `user_id` BIGINT(20) NOT NULL)",
        "INSERT INTO `user` (`visibility`) VALUES (0), (2), (0)",
        "INSERT INTO `repository` (`owner_id`, `is_private`) VALUES (1, 0), (2, 1), (3, 1)",
        "INSERT INTO `branch` (`repo_id`, `name`) VALUES (1, 'main'), (2, 'main'), (3, 'dev'), (3, 'main')",
        "INSERT INTO `collaboration` (`repo_id`, `user_id`) VALUES (3, 1)",
    ] {
        run(&mut adapter, sql);
    }
    for sql in [
        "SELECT id FROM branch WHERE repo_id IN (SELECT id FROM repository WHERE `repository`.owner_id NOT IN (SELECT id FROM `user` WHERE visibility IN (2))) ORDER BY id",
        "SELECT id FROM branch WHERE repo_id IN (SELECT id FROM repository WHERE is_private = 0 OR `repository`.id IN (SELECT repo_id FROM `collaboration` WHERE `collaboration`.user_id = 1)) ORDER BY id",
        "SELECT id FROM branch WHERE repo_id IN (SELECT id FROM repository WHERE owner_id IN (SELECT id FROM `user` WHERE visibility = 0)) ORDER BY id",
    ] {
        assert_eq!(first_column(&mut adapter, sql), ["1", "3", "4"], "{sql}");
    }
    assert!(matches!(
        adapter.execute_query(
            "SELECT id FROM branch WHERE repo_id IN (SELECT id FROM repository WHERE owner_id IN (SELECT name FROM `user`))"
        ),
        Err(FrontendErrorKind::Unsupported)
    ));
}

/// Gitea's listing of the branches a user recently pushed nests five
/// subqueries, joins inside two of them and ORs a dozen tests. Reading and
/// planning it recurses deeper than a thread's default 2 MiB stack holds,
/// which aborted the whole server; on a connection's own stack it is read
/// and answered.
#[test]
fn giteas_deepest_branch_listing_fits_a_connections_stack() {
    let answered = std::thread::Builder::new()
        .stack_size(crate::CONNECTION_THREAD_STACK_BYTES)
        .spawn(|| {
            let (_directory, mut adapter) = adapter();
            for sql in [
                "CREATE TABLE `branch` (`id` BIGINT(20) PRIMARY KEY AUTO_INCREMENT NOT NULL, `repo_id` BIGINT(20) NULL, `name` VARCHAR(255) NULL, `commit_id` VARCHAR(255) NULL, `commit_message` TEXT NULL, `pusher_id` BIGINT(20) NULL, `is_deleted` TINYINT(1) NULL, `deleted_by_id` BIGINT(20) NULL, `deleted_unix` BIGINT(20) NULL, `commit_time` BIGINT(20) NULL, `created_unix` BIGINT(20) NULL, `updated_unix` BIGINT(20) NULL)",
                "CREATE TABLE `repository` (`id` BIGINT(20) PRIMARY KEY AUTO_INCREMENT NOT NULL, `owner_id` BIGINT(20) NULL, `is_private` TINYINT(1) NULL, `is_fork` TINYINT(1) DEFAULT 0 NOT NULL, `fork_id` BIGINT(20) NULL, `is_archived` TINYINT(1) NULL)",
                "CREATE TABLE `user` (`id` BIGINT(20) PRIMARY KEY AUTO_INCREMENT NOT NULL, `visibility` INT DEFAULT 0 NOT NULL)",
                "CREATE TABLE `collaboration` (`id` BIGINT(20) PRIMARY KEY AUTO_INCREMENT NOT NULL, `repo_id` BIGINT(20) NOT NULL, `user_id` BIGINT(20) NOT NULL)",
                "CREATE TABLE `team_repo` (`id` BIGINT(20) PRIMARY KEY AUTO_INCREMENT NOT NULL, `org_id` BIGINT(20) NULL, `team_id` BIGINT(20) NULL, `repo_id` BIGINT(20) NULL)",
                "CREATE TABLE `team_user` (`id` BIGINT(20) PRIMARY KEY AUTO_INCREMENT NOT NULL, `org_id` BIGINT(20) NULL, `team_id` BIGINT(20) NULL, `uid` BIGINT(20) NULL)",
                "CREATE TABLE `team` (`id` BIGINT(20) PRIMARY KEY AUTO_INCREMENT NOT NULL, `org_id` BIGINT(20) NULL, `authorize` INT NULL)",
                "CREATE TABLE `team_unit` (`id` BIGINT(20) PRIMARY KEY AUTO_INCREMENT NOT NULL, `org_id` BIGINT(20) NULL, `team_id` BIGINT(20) NULL, `type` INT NULL, `access_mode` INT NULL)",
                "CREATE TABLE `org_user` (`id` BIGINT(20) PRIMARY KEY AUTO_INCREMENT NOT NULL, `uid` BIGINT(20) NULL, `org_id` BIGINT(20) NULL)",
            ] {
                run(&mut adapter, sql);
            }
            let whole = Bound::Whole;
            prepared_rows(
                &mut adapter,
                "SELECT `id`, `repo_id`, `name`, `commit_id`, `commit_message`, `pusher_id`, `is_deleted`, `deleted_by_id`, `deleted_unix`, `commit_time`, `created_unix`, `updated_unix` FROM `branch` WHERE is_deleted=? AND pusher_id=? AND updated_unix>=? AND repo_id IN (SELECT id FROM repository WHERE is_fork=? AND fork_id=? AND is_archived=? AND ((`repository`.is_private=? AND `repository`.owner_id NOT IN (SELECT id FROM `user` WHERE visibility IN (?))) OR `repository`.id IN (SELECT repo_id FROM `collaboration` WHERE `collaboration`.user_id=?) OR `repository`.id IN (SELECT `team_repo`.repo_id FROM team_repo INNER JOIN team_user ON `team_user`.team_id = `team_repo`.team_id INNER JOIN team ON `team`.id = `team_repo`.team_id LEFT JOIN team_unit ON `team_unit`.team_id = `team_repo`.team_id AND `team_unit`.`type` = ? WHERE `team_user`.uid=? AND (`team`.authorize>=? OR `team_unit`.`access_mode`>?)) OR `repository`.owner_id=? OR (`repository`.is_private=? AND `repository`.owner_id IN (SELECT `org_user`.org_id FROM org_user WHERE `org_user`.uid=?))) AND id=?) AND commit_id NOT IN (?) ORDER BY updated_unix DESC",
                &[
                    whole(0),
                    whole(2),
                    whole(0),
                    whole(1),
                    whole(1),
                    whole(0),
                    whole(1),
                    whole(2),
                    whole(2),
                    whole(1),
                    whole(2),
                    whole(1),
                    whole(1),
                    whole(2),
                    whole(1),
                    whole(2),
                    whole(1),
                    Bound::Word("abc"),
                ],
            )
            .rows
            .len()
        })
        .unwrap()
        .join()
        .unwrap();
    assert_eq!(answered, 0);
}

/// Gitea picks each reviewer's latest review with `id IN (SELECT max(id)
/// FROM review WHERE ... GROUP BY reviewer_id)`. `MIN` and `MAX` answer the
/// kind of the column they read, grouped or not, so the membership test holds
/// them to it. Every answer here was measured on MySQL 8.4.11 over the same
/// rows.
#[test]
fn a_membership_test_reads_the_largest_of_each_group() {
    let (_directory, mut adapter) = adapter();
    for sql in [
        "CREATE TABLE `review` (`id` BIGINT(20) PRIMARY KEY AUTO_INCREMENT NOT NULL, `type` INT NULL, `reviewer_id` BIGINT(20) NULL, `issue_id` BIGINT(20) NULL, `content` TEXT NULL, `dismissed` TINYINT(1) DEFAULT 0 NOT NULL, `created_unix` BIGINT(20) NULL, `updated_unix` BIGINT(20) NULL)",
        "INSERT INTO `review` (`type`, `reviewer_id`, `issue_id`, `created_unix`, `updated_unix`) VALUES (1, 7, 2, 10, 15), (2, 7, 2, 20, 25), (1, 8, 2, 30, 5), (4, 8, 2, 40, 45), (1, 9, 3, 50, 55), (1, 7, 3, 60, 65)",
    ] {
        run(&mut adapter, sql);
    }
    for (sql, found) in [
        (
            "SELECT `id` FROM `review` WHERE issue_id=2 AND type IN (1,2) AND `id` IN (SELECT max(id) FROM review WHERE issue_id=2 AND type IN (1,2) GROUP BY reviewer_id) ORDER BY `created_unix` ASC, `id` ASC",
            &["2", "3"][..],
        ),
        (
            "SELECT `id` FROM `review` WHERE `id` IN (SELECT max(id) as id FROM review WHERE issue_id IN (2,3) AND `type` IN (1,2,4) AND dismissed=0 GROUP BY issue_id, reviewer_id) ORDER BY review.updated_unix ASC",
            &["2", "4", "5", "6"],
        ),
        (
            "SELECT `id` FROM `review` WHERE `id` NOT IN (SELECT MIN(id) FROM review GROUP BY issue_id) ORDER BY id",
            &["2", "3", "4", "6"],
        ),
    ] {
        assert_eq!(first_column(&mut adapter, sql), found, "{sql}");
    }
    assert!(matches!(
        adapter.execute_query(
            "SELECT `id` FROM `review` WHERE content IN (SELECT MAX(id) FROM review GROUP BY issue_id)"
        ),
        Err(FrontendErrorKind::Unsupported)
    ));
}

/// Gitea's consistency check of a label's closed issues counts over a comma
/// join, `(SELECT COUNT(*) FROM issue_label, issue WHERE ...)`. A count reads
/// no column of its tables, so it may join them; the answer is MySQL
/// 8.4.11's over the same rows.
#[test]
fn a_count_compared_with_a_column_may_join_tables() {
    let (_directory, mut adapter) = adapter();
    for sql in [
        "CREATE TABLE `label` (`id` BIGINT(20) PRIMARY KEY AUTO_INCREMENT NOT NULL, `num_closed_issues` INT NULL)",
        "CREATE TABLE `issue_label` (`id` BIGINT(20) PRIMARY KEY AUTO_INCREMENT NOT NULL, `issue_id` BIGINT(20) NULL, `label_id` BIGINT(20) NULL)",
        "CREATE TABLE `issue` (`id` BIGINT(20) PRIMARY KEY AUTO_INCREMENT NOT NULL, `is_closed` TINYINT(1) NULL)",
        "INSERT INTO `label` (`num_closed_issues`) VALUES (2), (0), (1)",
        "INSERT INTO `issue` (`is_closed`) VALUES (0), (1), (1)",
        "INSERT INTO `issue_label` (`issue_id`, `label_id`) VALUES (1, 1), (2, 1), (3, 1), (2, 2), (1, 3)",
    ] {
        run(&mut adapter, sql);
    }
    assert_eq!(
        first_column(
            &mut adapter,
            "SELECT `label`.id FROM `label` WHERE `label`.num_closed_issues!=(SELECT COUNT(*) FROM `issue_label`,`issue` WHERE `issue_label`.label_id=`label`.id AND `issue_label`.issue_id=`issue`.id AND `issue`.is_closed=1) ORDER BY id"
        ),
        ["2", "3"]
    );
}

/// Gitea lists an issue's dependencies from its own repository first with
/// `ORDER BY CASE WHEN issue.repo_id = ? THEN 0 ELSE issue.repo_id END` over a
/// join, on every issue page. Each branch is a written whole number or a
/// column holding them, so the rows order as MySQL's `BIGINT` answer does;
/// every order here was measured on MySQL 8.4.11 over the same rows.
#[test]
fn an_ordering_case_over_whole_numbers_reads_joined_columns() {
    let (_directory, mut adapter) = adapter();
    for sql in [
        "CREATE TABLE `issue` (`id` BIGINT(20) PRIMARY KEY AUTO_INCREMENT NOT NULL, `repo_id` BIGINT(20) NULL, `name` VARCHAR(255) NULL, `created_unix` BIGINT(20) NULL)",
        "CREATE TABLE `repository` (`id` BIGINT(20) PRIMARY KEY AUTO_INCREMENT NOT NULL, `name` VARCHAR(255) NULL)",
        "CREATE TABLE `issue_dependency` (`id` BIGINT(20) PRIMARY KEY AUTO_INCREMENT NOT NULL, `user_id` BIGINT(20) NOT NULL, `issue_id` BIGINT(20) NOT NULL, `dependency_id` BIGINT(20) NOT NULL)",
        "INSERT INTO `repository` (`name`) VALUES ('a'), ('b'), ('c')",
        "INSERT INTO `issue` (`repo_id`, `name`, `created_unix`) VALUES (1, 'i1', 10), (3, 'i2', 20), (2, 'i3', 30), (2, 'i4', 40), (1, 'i5', 50), (3, 'i6', 60)",
        "INSERT INTO `issue_dependency` (`user_id`, `issue_id`, `dependency_id`) VALUES (1, 4, 1), (1, 4, 2), (1, 4, 3), (1, 4, 5), (1, 4, 6)",
    ] {
        run(&mut adapter, sql);
    }
    let blocked_by = prepared_rows(
        &mut adapter,
        "SELECT * FROM `issue` INNER JOIN `repository` ON repository.id = issue.repo_id INNER JOIN `issue_dependency` ON issue_dependency.dependency_id = issue.id WHERE (issue_id = ?) ORDER BY CASE WHEN issue.repo_id = ? THEN 0 ELSE issue.repo_id END, issue.created_unix DESC",
        &[Bound::Whole(4), Bound::Whole(2)],
    );
    assert_eq!(
        blocked_by
            .rows
            .iter()
            .map(|row| row[0].clone())
            .collect::<Vec<_>>(),
        [3, 5, 1, 6, 2].map(BinaryResultValue::Integer)
    );
    assert_eq!(
        first_column(
            &mut adapter,
            "SELECT issue.id FROM `issue` ORDER BY CASE WHEN issue.repo_id = 3 THEN -1 WHEN issue.repo_id = 1 THEN 7 ELSE issue.repo_id END DESC, id"
        ),
        ["1", "5", "3", "4", "2", "6"]
    );
    // A branch of words orders by a collation, which this does not read.
    assert!(matches!(
        adapter.execute_query(
            "SELECT issue.id FROM `issue` INNER JOIN `repository` ON repository.id = issue.repo_id ORDER BY CASE WHEN issue.repo_id = 1 THEN 0 ELSE issue.name END"
        ),
        Err(FrontendErrorKind::Unsupported)
    ));
}

/// Gitea lists an organisation's teams owners first with `ORDER BY CASE WHEN
/// name = ? THEN '' ELSE lower_name END`, joining `team_user`. MySQL answers
/// the `CASE` under the column's collation, a written word yielding to a
/// column's, and orders under it: measured on 8.4.11, `élan` comes before
/// `zeta`, which byte order puts the other way.
#[test]
fn an_ordering_case_over_words_orders_under_the_columns_collation() {
    let (_directory, mut adapter) = adapter();
    for sql in [
        "CREATE TABLE `team` (`id` BIGINT(20) PRIMARY KEY AUTO_INCREMENT NOT NULL, `org_id` BIGINT(20) NULL, `lower_name` VARCHAR(255) NULL, `name` VARCHAR(255) NULL, `visibility` INT DEFAULT 0 NOT NULL, `code` VARCHAR(20) COLLATE utf8mb4_bin NULL)",
        "CREATE TABLE `team_user` (`id` BIGINT(20) PRIMARY KEY AUTO_INCREMENT NOT NULL, `org_id` BIGINT(20) NULL, `team_id` BIGINT(20) NULL, `uid` BIGINT(20) NULL)",
        "INSERT INTO `team` (`org_id`, `lower_name`, `name`, `visibility`) VALUES (3, 'zeta', 'Zeta', 0), (3, 'owners', 'Owners', 0), (3, 'élan', 'Élan', 0), (3, 'alpha', 'Alpha', 1), (4, 'other', 'Other', 0)",
        "INSERT INTO `team_user` (`org_id`, `team_id`, `uid`) VALUES (3, 4, 9), (3, 1, 9)",
    ] {
        run(&mut adapter, sql);
    }
    let listed = prepared_rows(
        &mut adapter,
        "SELECT team.id FROM `team` LEFT JOIN `team_user` ON team_user.team_id = team.id AND team_user.uid = ? WHERE `team`.org_id=? AND (team_user.uid=? OR `team`.visibility IN (?)) ORDER BY CASE WHEN name=? THEN '' ELSE lower_name END LIMIT 10",
        &[
            Bound::Whole(9),
            Bound::Whole(3),
            Bound::Whole(9),
            Bound::Whole(0),
            Bound::Word("Owners"),
        ],
    );
    assert_eq!(
        listed
            .rows
            .iter()
            .map(|row| row[0].clone())
            .collect::<Vec<_>>(),
        [2, 4, 3, 1].map(BinaryResultValue::Integer)
    );
    for sql in [
        // Words beside numbers is a coercion MySQL makes by rules of its own.
        "SELECT team.id FROM `team` LEFT JOIN `team_user` ON team_user.team_id = team.id ORDER BY CASE WHEN name = 'Owners' THEN 0 ELSE lower_name END",
        // A column under another collation orders by that one.
        "SELECT team.id FROM `team` LEFT JOIN `team_user` ON team_user.team_id = team.id ORDER BY CASE WHEN name = 'Owners' THEN '' ELSE code END",
    ] {
        assert!(
            matches!(
                adapter.execute_query(sql),
                Err(FrontendErrorKind::Unsupported)
            ),
            "{sql} must be refused"
        );
    }
}

/// Gitea finds an LFS lock with `lower(path) = ?`. A value bound against a
/// call answering a word is a word, compared under the collation of the
/// column the call reads: measured on MySQL 8.4.11, `lower(path) =
/// 'DOCS/README.MD'` finds `Docs/Readme.md`. A number bound there is refused,
/// as against a column of words.
#[test]
fn a_call_answering_a_word_meets_a_bound_word() {
    let (_directory, mut adapter) = adapter();
    for sql in [
        "CREATE TABLE `lfs_lock` (`id` BIGINT(20) PRIMARY KEY AUTO_INCREMENT NOT NULL, `repo_id` BIGINT(20) NOT NULL, `owner_id` BIGINT(20) NOT NULL, `path` TEXT NULL, `created` DATETIME NULL)",
        "INSERT INTO `lfs_lock` (`repo_id`, `owner_id`, `path`) VALUES (1, 2, 'Docs/Readme.md'), (1, 2, 'src/main.go'), (2, 2, 'docs/readme.md')",
    ] {
        run(&mut adapter, sql);
    }
    let found = |adapter: &mut Adapter, path: &str| {
        prepared_rows(
            adapter,
            "SELECT `id`, `repo_id`, `owner_id`, `path`, `created` FROM `lfs_lock` WHERE (lower(path) = ?) AND `repo_id`=? LIMIT 1",
            &[Bound::Word(path), Bound::Whole(1)],
        )
        .rows
        .iter()
        .map(|row| row[0].clone())
        .collect::<Vec<_>>()
    };
    assert_eq!(
        found(&mut adapter, "DOCS/README.MD"),
        [BinaryResultValue::Integer(1)]
    );
    assert_eq!(
        found(&mut adapter, "SRC/Main.GO"),
        [BinaryResultValue::Integer(2)]
    );
    assert!(found(&mut adapter, "docs/other.md").is_empty());
    assert!(prepared(
        &mut adapter,
        "SELECT `id` FROM `lfs_lock` WHERE (lower(path) = ?) AND `repo_id`=?",
        &[Bound::Whole(5), Bound::Whole(1)],
    )
    .is_err());
}
