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
        };
        payload.extend_from_slice(&[code, 0]);
    }
    for value in values {
        match value {
            Bound::Word(text) => {
                payload.push(u8::try_from(text.len()).unwrap());
                payload.extend_from_slice(text.as_bytes());
            }
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
