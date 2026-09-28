//! Statements Entity Framework Core 9 writes through Pomelo 9 and
//! MySqlConnector, replayed as the framework harness captured them. The
//! connector writes every value into the text of the statement, so each one
//! arrives as a `COM_QUERY`.
//!
//! Every expectation here was measured on MySQL 8.4.11.

use super::*;

type Adapter = AuthorizedDatabaseCommandAdapter<RecordingAuthorizer>;

/// The tables `dotnet ef database update` makes for the harness's blog, in
/// the words the migration writes them in, holding three users and their
/// posts.
fn adapter() -> (tempfile::TempDir, Adapter) {
    let authorizer = Arc::new(RecordingAuthorizer::default());
    let (directory, _catalog, factory) = catalog_factory(authorizer);
    let mut adapter = factory
        .build(AuthenticatedPrincipal::from_account_id_for_testing(
            AccountId::from_bytes([157; 32]),
        ))
        .unwrap();
    adapter.authorize_connection().unwrap();
    adapter.execute_init_db("reports").unwrap();
    for sql in [
        "CREATE TABLE `Tags` (\n    `Id` bigint NOT NULL AUTO_INCREMENT,\n    `Name` varchar(100) CHARACTER SET utf8mb4 NOT NULL,\n    CONSTRAINT `PK_Tags` PRIMARY KEY (`Id`)\n) CHARACTER SET=utf8mb4;\n",
        "CREATE TABLE `Users` (\n    `Id` bigint NOT NULL AUTO_INCREMENT,\n    `Email` varchar(191) CHARACTER SET utf8mb4 NOT NULL,\n    `Name` varchar(100) CHARACTER SET utf8mb4 NOT NULL,\n    `Balance` decimal(10,2) NOT NULL DEFAULT 0.0,\n    `IsActive` tinyint(1) NOT NULL DEFAULT TRUE,\n    `Profile` json NULL,\n    `CreatedAt` datetime(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6),\n    `UpdatedAt` datetime(6) NOT NULL DEFAULT CURRENT_TIMESTAMP(6) ON UPDATE CURRENT_TIMESTAMP(6),\n    CONSTRAINT `PK_Users` PRIMARY KEY (`Id`)\n) CHARACTER SET=utf8mb4;\n",
        "CREATE TABLE `Posts` (\n    `Id` bigint NOT NULL AUTO_INCREMENT,\n    `UserId` bigint NOT NULL,\n    `Title` varchar(200) CHARACTER SET utf8mb4 NOT NULL,\n    `Body` text CHARACTER SET utf8mb4 NULL,\n    `PublishedAt` datetime(6) NULL,\n    `Views` int NOT NULL,\n    CONSTRAINT `PK_Posts` PRIMARY KEY (`Id`),\n    CONSTRAINT `FK_Posts_Users_UserId` FOREIGN KEY (`UserId`) REFERENCES `Users` (`Id`) ON DELETE CASCADE\n) CHARACTER SET=utf8mb4;\n",
        "CREATE TABLE `PostTags` (\n    `PostsId` bigint NOT NULL,\n    `TagsId` bigint NOT NULL,\n    CONSTRAINT `PK_PostTags` PRIMARY KEY (`PostsId`, `TagsId`),\n    CONSTRAINT `FK_PostTags_Posts_PostsId` FOREIGN KEY (`PostsId`) REFERENCES `Posts` (`Id`) ON DELETE CASCADE,\n    CONSTRAINT `FK_PostTags_Tags_TagsId` FOREIGN KEY (`TagsId`) REFERENCES `Tags` (`Id`) ON DELETE CASCADE\n) CHARACTER SET=utf8mb4;\n",
        "CREATE INDEX `IX_PostTags_TagsId` ON `PostTags` (`TagsId`);\n",
        "CREATE INDEX `IX_Posts_UserId_PublishedAt` ON `Posts` (`UserId`, `PublishedAt`);\n",
        "CREATE UNIQUE INDEX `IX_Tags_Name` ON `Tags` (`Name`);\n",
        "CREATE UNIQUE INDEX `IX_Users_Email` ON `Users` (`Email`);\n",
        "INSERT INTO `Tags` (`Name`) VALUES ('news'), ('rust'), ('sql')",
        "INSERT INTO `Users` (`Balance`, `Email`, `Name`, `Profile`) VALUES (100.50, 'alice@example.com', 'Alice', '{\"city\": \"Tokyo\", \"tags\": [\"a\", \"b\"]}')",
        "INSERT INTO `Users` (`Balance`, `Email`, `IsActive`, `Name`, `Profile`) VALUES (20.25, 'bob@example.com', false, 'Bob', '{\"city\": \"Osaka\", \"tags\": [\"c\"]}')",
        "INSERT INTO `Users` (`Balance`, `Email`, `Name`, `Profile`) VALUES (5, 'carol@example.com', 'Carol', NULL)",
        "INSERT INTO `Posts` (`Body`, `PublishedAt`, `Title`, `UserId`, `Views`) VALUES ('Not yet', NULL, 'Draft', 1, 0)",
        "INSERT INTO `Posts` (`Body`, `PublishedAt`, `Title`, `UserId`, `Views`) VALUES (NULL, '2024-01-02 03:04:05', 'Bob writes', 2, 3)",
        "INSERT INTO `PostTags` (`PostsId`, `TagsId`) VALUES (1, 1), (1, 2), (2, 3)",
    ] {
        changed(&mut adapter, sql);
    }
    (directory, adapter)
}

fn changed(adapter: &mut Adapter, sql: &str) -> u64 {
    match adapter.execute_query(sql) {
        Ok(CommandExecutionResult::Ok(result)) => result.affected_rows,
        other => panic!("{sql}: {other:?}"),
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

fn row(values: &[&str]) -> Vec<Option<String>> {
    values
        .iter()
        .map(|value| (*value != "NULL").then(|| (*value).to_owned()))
        .collect()
}

/// `ExecuteUpdate` gives the table an alias with `AS` and names every column
/// through it, the table written with capital letters. This answered 1235:
/// the alias was written out as the table's own name, which the engine did
/// not find under the lower case it keeps the table in.
#[test]
fn execute_update_names_its_table_through_an_alias() {
    let (_directory, mut adapter) = adapter();
    assert_eq!(
        changed(
            &mut adapter,
            "UPDATE `Users` AS `u`\nSET `u`.`Balance` = `u`.`Balance` - 10.0\nWHERE `u`.`Email` = 'alice@example.com'"
        ),
        1
    );
    assert_eq!(
        changed(
            &mut adapter,
            "UPDATE `Users` AS `u`\nSET `u`.`Balance` = 0.0\nWHERE `u`.`Email` = 'bob@example.com'"
        ),
        1
    );
    assert_eq!(
        changed(
            &mut adapter,
            "UPDATE `Posts` AS `p`\nSET `p`.`Views` = `p`.`Views` + 1\nWHERE `p`.`Views` < 5"
        ),
        2
    );
    assert_eq!(
        rows(&mut adapter, "SELECT Id, Balance FROM Users ORDER BY Id"),
        [
            row(&["1", "90.50"]),
            row(&["2", "0.00"]),
            row(&["3", "5.00"])
        ]
    );
    assert_eq!(
        rows(&mut adapter, "SELECT Id, Views FROM Posts ORDER BY Id"),
        [row(&["1", "1"]), row(&["2", "4"])]
    );
    // Measured: 1054 for a column named through the table beside its alias.
    assert!(adapter
        .execute_query("UPDATE `Users` AS `u` SET `Users`.`Name` = 'x'")
        .is_err());
}

/// `SaveChanges` writes each row and reads back what the database made for
/// it in the same batch — `SELECT ... WHERE ROW_COUNT() = 1 AND Id =
/// LAST_INSERT_ID()` after an `INSERT`, `... AND Id = 2` after an `UPDATE` —
/// and takes no row as a write that did not happen. Both reads answered 1064.
#[test]
fn save_changes_reads_back_the_row_it_wrote() {
    let (_directory, mut adapter) = adapter();
    assert_eq!(
        changed(&mut adapter, "INSERT INTO `Tags` (`Name`)\nVALUES ('go')"),
        1
    );
    let read_back = result_set(
        &mut adapter,
        "SELECT `Id`\nFROM `Tags`\nWHERE ROW_COUNT() = 1 AND `Id` = LAST_INSERT_ID()",
    );
    assert_eq!(read_back.columns[0].name, "Id");
    assert_eq!(read_back.columns[0].column_type, MYSQL_TYPE_LONGLONG);
    assert_eq!(read_back.rows, [[Some(b"4".to_vec())]]);
    // The read answered rows, so the count it leaves for the next is -1.
    assert!(rows(
        &mut adapter,
        "SELECT `Id` FROM `Tags` WHERE ROW_COUNT() = 1 AND `Id` = LAST_INSERT_ID()"
    )
    .is_empty());
    assert_eq!(
        rows(
            &mut adapter,
            "SELECT `Id` FROM `Tags` WHERE -1 = ROW_COUNT() AND `Id` = LAST_INSERT_ID()"
        ),
        [row(&["4"])]
    );

    assert_eq!(
        changed(
            &mut adapter,
            "INSERT INTO `Users` (`Email`, `Name`, `Profile`)\nVALUES ('dave@example.com', 'Dave', NULL)"
        ),
        1
    );
    assert_eq!(
        rows(
            &mut adapter,
            "SELECT `Id`, `Balance`, `IsActive`\nFROM `Users`\nWHERE ROW_COUNT() = 1 AND `Id` = LAST_INSERT_ID()"
        ),
        [row(&["4", "0.00", "1"])]
    );

    assert_eq!(
        changed(
            &mut adapter,
            "UPDATE `Users` SET `Name` = 'Bob One'\nWHERE `Id` = 2"
        ),
        1
    );
    assert_eq!(
        rows(
            &mut adapter,
            "SELECT `Name`\nFROM `Users`\nWHERE ROW_COUNT() = 1 AND `Id` = 2"
        ),
        [row(&["Bob One"])]
    );
    // A stale row version changes nothing, which EF reads as a conflict.
    assert_eq!(
        changed(
            &mut adapter,
            "UPDATE `Users` SET `Name` = 'Bob Two'\nWHERE `Id` = 2 AND `Name` = 'Bob'"
        ),
        0
    );
    assert!(rows(
        &mut adapter,
        "SELECT `Name`\nFROM `Users`\nWHERE ROW_COUNT() = 1 AND `Id` = 2"
    )
    .is_empty());
    // A count compared with anything but a whole number stays refused.
    assert!(adapter
        .execute_query("SELECT `Id` FROM `Tags` WHERE ROW_COUNT() = 'a'")
        .is_err());
}

/// MySqlConnector writes every `DateTime` as `timestamp('...')`: a post's
/// `PublishedAt`, and the `UpdatedAt` row version `SaveChanges` holds an
/// `UPDATE` or a `DELETE` to. Each answered 1235.
///
/// Measured on MySQL 8.4.11, the call stores into a `DATETIME` or `TIMESTAMP`
/// column and compares with one as the word it names; written into a column
/// of another kind it does not (a `BIGINT` stores 20240102030405, a `DATE`
/// warns about the moment rounded), and a column of words compares with it as
/// a moment. Those stay refused.
#[test]
fn a_moment_written_as_a_timestamp_call_meets_a_moment_column() {
    let (_directory, mut adapter) = adapter();
    assert_eq!(
        changed(
            &mut adapter,
            "INSERT INTO `Posts` (`Body`, `PublishedAt`, `Title`, `UserId`, `Views`)\nVALUES ('First post', timestamp('2024-01-02 03:04:05.000000'), 'Hello', 1, 10)"
        ),
        1
    );
    assert_eq!(
        rows(
            &mut adapter,
            "SELECT `PublishedAt` FROM `Posts` WHERE `Title` = 'Hello'"
        ),
        [row(&["2024-01-02 03:04:05.000000"])]
    );
    assert_eq!(
        rows(
            &mut adapter,
            "SELECT `Title` FROM `Posts` WHERE `PublishedAt` = timestamp('2024-01-02 03:04:05.000000') ORDER BY `Id`"
        ),
        [row(&["Bob writes"]), row(&["Hello"])]
    );

    let version = updated_at(&mut adapter, 2);
    // The clock moves on before the row is changed, so the change moves the
    // row version.
    std::thread::sleep(std::time::Duration::from_millis(5));
    assert_eq!(
        changed(
            &mut adapter,
            &format!(
                "UPDATE `Users` SET `Name` = 'Bob One'\nWHERE `Id` = 2 AND `UpdatedAt` = timestamp('{version}')"
            )
        ),
        1
    );
    assert_eq!(
        rows(
            &mut adapter,
            "SELECT `Name` FROM `Users` WHERE ROW_COUNT() = 1 AND `Id` = 2"
        ),
        [row(&["Bob One"])]
    );
    // The version the second context read is stale now: its UPDATE changes
    // nothing, and its DELETE neither.
    assert_ne!(updated_at(&mut adapter, 2), version);
    assert_eq!(
        changed(
            &mut adapter,
            &format!("UPDATE `Users` SET `Name` = 'Bob Two'\nWHERE `Id` = 2 AND `UpdatedAt` = timestamp('{version}')")
        ),
        0
    );
    assert_eq!(
        changed(
            &mut adapter,
            &format!(
                "DELETE FROM `Users`\nWHERE `Id` = 2 AND `UpdatedAt` = timestamp('{version}')"
            )
        ),
        0
    );
    let carol = updated_at(&mut adapter, 3);
    assert_eq!(
        changed(
            &mut adapter,
            &format!("DELETE FROM `Users`\nWHERE `Id` = 3 AND `UpdatedAt` = timestamp('{carol}')")
        ),
        1
    );

    for refused in [
        "INSERT INTO `Posts` (`Body`, `PublishedAt`, `Title`, `UserId`, `Views`) VALUES (timestamp('2024-01-02 03:04:05.000000'), NULL, 'x', 1, 0)",
        "INSERT INTO `Posts` (`Body`, `PublishedAt`, `Title`, `UserId`, `Views`) VALUES (NULL, NULL, 'x', 1, timestamp('2024-01-02 03:04:05.000000'))",
        "UPDATE `Posts` SET `Title` = timestamp('2024-01-02 03:04:05.000000') WHERE `Id` = 1",
        "SELECT `Id` FROM `Posts` WHERE `Title` = timestamp('2024-01-02 03:04:05.000000')",
        // No such day: MySQL answers 1292.
        "INSERT INTO `Posts` (`PublishedAt`, `Title`, `UserId`, `Views`) VALUES (timestamp('2024-02-30 03:04:05.000000'), 'x', 1, 0)",
        // A moment with more places than the column keeps compares at full
        // precision in MySQL, and does not meet the stored one.
        "SELECT `Id` FROM `Posts` WHERE `PublishedAt` = timestamp('2024-01-02 03:04:05.0000001')",
    ] {
        assert!(adapter.execute_query(refused).is_err(), "{refused}");
    }
    assert_eq!(
        rows(&mut adapter, "SELECT COUNT(*) FROM `Posts`"),
        [row(&["3"])]
    );
}

/// A `GroupBy` totalling a `DECIMAL` writes `COALESCE(SUM(u.Balance), 0.0)`,
/// and a `HAVING` over a total `COALESCE(SUM(p.Views), 0) > 5`. Both answered
/// 1064.
///
/// Measured on MySQL 8.4.11: the fallen-back total answers the sum's own
/// shape, never null — a `NEWDECIMAL` of 34 with 2 places over a
/// `DECIMAL(10,2)` — and `0.00` over no rows. A zero with more places than
/// the sum's (`0.000`: 35 with 3) or beside a sum of whole numbers (`0.0`:
/// 35 with 1) answers another shape, and stays refused.
#[test]
fn a_total_falls_back_on_a_zero_written_with_places() {
    let (_directory, mut adapter) = adapter();
    let groups = result_set(
        &mut adapter,
        "SELECT `u`.`IsActive` AS `Active`, COUNT(*) AS `Count`, COALESCE(SUM(`u`.`Balance`), 0.0) AS `Total`, AVG(`u`.`Balance`) AS `Average`, MAX(`u`.`CreatedAt`) AS `Latest`\nFROM `Users` AS `u`\nGROUP BY `u`.`IsActive`\nHAVING COUNT(*) > 0\nORDER BY `u`.`IsActive`",
    );
    let shapes = groups
        .columns
        .iter()
        .map(|column| {
            (
                column.name.as_str(),
                column.column_type,
                column.column_length,
                column.decimals,
            )
        })
        .collect::<Vec<_>>();
    assert_eq!(
        shapes,
        [
            ("Active", MYSQL_TYPE_TINY, 1, 0),
            ("Count", MYSQL_TYPE_LONGLONG, 21, 0),
            ("Total", MYSQL_TYPE_NEWDECIMAL, 34, 2),
            ("Average", MYSQL_TYPE_NEWDECIMAL, 16, 6),
            ("Latest", MYSQL_TYPE_DATETIME, 26, 6),
        ]
    );
    assert_eq!(
        groups.columns[2].flags,
        MYSQL_NOT_NULL_FLAG | MYSQL_BINARY_FLAG | MYSQL_NUM_FLAG
    );
    let totals = rows(
        &mut adapter,
        "SELECT `u`.`IsActive`, COUNT(*), COALESCE(SUM(`u`.`Balance`), 0.0), AVG(`u`.`Balance`) FROM `Users` AS `u` GROUP BY `u`.`IsActive` ORDER BY `u`.`IsActive`",
    );
    assert_eq!(
        totals,
        [
            row(&["0", "1", "20.25", "20.250000"]),
            row(&["1", "2", "105.50", "52.750000"])
        ]
    );
    assert_eq!(
        rows(
            &mut adapter,
            "SELECT COALESCE(SUM(`u`.`Balance`), 0.0) FROM `Users` AS `u` WHERE `u`.`Id` = 0"
        ),
        [row(&["0.00"])]
    );

    assert_eq!(
        rows(
            &mut adapter,
            "SELECT `p`.`UserId`\nFROM `Posts` AS `p`\nGROUP BY `p`.`UserId`\nHAVING COALESCE(SUM(`p`.`Views`), 0) > 2"
        ),
        [row(&["2"])]
    );

    for refused in [
        "SELECT COALESCE(SUM(`u`.`Balance`), 0.000) FROM `Users` AS `u`",
        "SELECT COALESCE(SUM(`p`.`Views`), 0.0) FROM `Posts` AS `p`",
        "SELECT `p`.`UserId` FROM `Posts` AS `p` GROUP BY `p`.`UserId` HAVING COALESCE(MAX(`p`.`Title`), 0) > 2",
    ] {
        assert!(adapter.execute_query(refused).is_err(), "{refused}");
    }
}

/// `EF.Functions.Like` over a `JSON` column matches the document as text. It
/// was refused, the matched column not being one of words.
///
/// Measured on MySQL 8.4.11: the document is matched as the text MySQL
/// prints for it, under `utf8mb4_bin` — case told apart, a space after each
/// colon and comma, keys in MySQL's own order — and a NULL document matches
/// neither `LIKE` nor `NOT LIKE`.
#[test]
fn like_matches_a_json_document_as_the_text_mysql_prints() {
    let (_directory, mut adapter) = adapter();
    changed(
        &mut adapter,
        "INSERT INTO `Users` (`Balance`, `Email`, `Name`, `Profile`) VALUES (1, 'zed@example.com', 'Zed', '{\"tags\":[\"z\"],\"city\":\"Oslo\",\"a\":1.50}')",
    );
    for (pattern, count) in [
        ("'%\"a\"%'", "2"),
        ("'%TOKYO%'", "0"),
        ("'%Tokyo%'", "1"),
        ("'{\"city\": \"Tokyo\"%'", "1"),
        ("'{\"city\":\"Tokyo\"%'", "0"),
        ("'{\"a\": 1.5, \"city\": \"Oslo\"%'", "1"),
        ("'%_a_%'", "3"),
    ] {
        assert_eq!(
            rows(
                &mut adapter,
                &format!(
                    "SELECT COUNT(*)\nFROM `Users` AS `u`\nWHERE `u`.`Profile` LIKE {pattern}"
                )
            ),
            [row(&[count])],
            "{pattern}"
        );
    }
    assert_eq!(
        rows(
            &mut adapter,
            "SELECT COUNT(*) FROM `Users` AS `u` WHERE `u`.`Profile` NOT LIKE '%\"a\"%'"
        ),
        [row(&["1"])]
    );
}

/// `Database.SqlQuery<T>(...).Single()` reads the app's own statement through
/// a derived table, `SELECT s.Value FROM (<statement>) AS s LIMIT 2`. A JSON
/// reading in the statement's `WHERE` was refused there, and so was
/// `CAST(SUM(ViewCount) AS SIGNED)` anywhere.
///
/// Measured on MySQL 8.4.11: through the derived table a count is a
/// `LONGLONG` of 21, NOT NULL, and a total cast to a whole number a nullable
/// `LONGLONG` of 21, neither carrying the binary flag; on its own the cast
/// carries it.
#[test]
fn sql_query_reads_the_statement_through_a_derived_table() {
    let (_directory, mut adapter) = adapter();
    let shape = |set: &TextResultSet| {
        let column = &set.columns[0];
        (
            column.name.clone(),
            column.table.clone(),
            column.column_type,
            column.column_length,
            column.flags,
        )
    };
    let osaka = result_set(
        &mut adapter,
        "SELECT `s`.`Value`\nFROM (\n    SELECT COUNT(*) AS Value FROM Users WHERE Profile->>'$.city' = 'Osaka'\n) AS `s`\nLIMIT 2",
    );
    assert_eq!(
        shape(&osaka),
        (
            "Value".to_owned(),
            "s".to_owned(),
            MYSQL_TYPE_LONGLONG,
            21,
            MYSQL_NOT_NULL_FLAG | MYSQL_NUM_FLAG
        )
    );
    assert_eq!(osaka.rows, [[Some(b"1".to_vec())]]);

    let views = result_set(
        &mut adapter,
        "SELECT `s`.`Value`\nFROM (\n    SELECT CAST(SUM(Views) AS SIGNED) AS Value FROM Posts\n) AS `s`\nLIMIT 2",
    );
    assert_eq!(
        shape(&views),
        (
            "Value".to_owned(),
            "s".to_owned(),
            MYSQL_TYPE_LONGLONG,
            21,
            MYSQL_NUM_FLAG
        )
    );
    assert_eq!(views.rows, [[Some(b"3".to_vec())]]);
    let plain = result_set(
        &mut adapter,
        "SELECT CAST(SUM(Views) AS SIGNED) AS Value FROM Posts WHERE Id = 0",
    );
    assert_eq!(
        shape(&plain),
        (
            "Value".to_owned(),
            String::new(),
            MYSQL_TYPE_LONGLONG,
            21,
            MYSQL_BINARY_FLAG | MYSQL_NUM_FLAG
        )
    );
    assert_eq!(plain.rows, [[None]]);

    // A total with places rounds, and an average is not a whole number.
    for refused in [
        "SELECT CAST(SUM(Balance) AS SIGNED) AS Value FROM Users",
        "SELECT CAST(AVG(Views) AS SIGNED) AS Value FROM Posts",
    ] {
        assert!(adapter.execute_query(refused).is_err(), "{refused}");
    }
}

/// `OrderByDescending(u => u.Balance).Select(u => u.Name).Skip(1).Take(1)`
/// pages in a derived table and sorts again outside it. A body with an
/// `ORDER BY` and a `LIMIT` was refused.
///
/// Measured on MySQL 8.4.11, the same whatever the table holds (none, three
/// or five hundred rows): MySQL writes such a body out into a table of its
/// own, so each column names its database, the derived table, and the table
/// the body read by that table's own name — not the alias the body gave it —
/// and keeps its NOT NULL and its no-default flag but none of its keys. A
/// body grouping its rows is written out the same way.
#[test]
fn a_derived_table_pages_its_rows_before_the_statement_sorts_them() {
    let (_directory, mut adapter) = adapter();
    let second = result_set(
        &mut adapter,
        "SELECT `u0`.`Name`\nFROM (\n    SELECT `u`.`Name`, `u`.`Balance`\n    FROM `Users` AS `u`\n    ORDER BY `u`.`Balance` DESC\n    LIMIT 1 OFFSET 1\n) AS `u0`\nORDER BY `u0`.`Balance` DESC\nLIMIT 2",
    );
    assert_eq!(second.rows, [[Some(b"Bob".to_vec())]]);
    let name = &second.columns[0];
    assert_eq!(
        (
            name.schema.as_str(),
            name.table.as_str(),
            name.original_table.as_str(),
            name.column_type,
            name.column_length,
            name.flags
        ),
        (
            "reports",
            "u0",
            "users",
            MYSQL_TYPE_VAR_STRING,
            400,
            MYSQL_NOT_NULL_FLAG | MYSQL_NO_DEFAULT_VALUE_FLAG
        )
    );

    let top = result_set(
        &mut adapter,
        "SELECT `u0`.`Id`, `u0`.`Name`, `u0`.`Balance` FROM (SELECT `u`.`Id`, `u`.`Name`, `u`.`Balance` FROM `Users` AS `u` ORDER BY `u`.`Balance` DESC LIMIT 2) AS `u0` ORDER BY `u0`.`Balance`",
    );
    assert_eq!(
        top.rows
            .iter()
            .map(|row| String::from_utf8(row[1].clone().unwrap()).unwrap())
            .collect::<Vec<_>>(),
        ["Bob", "Alice"]
    );
    assert_eq!(top.columns[0].flags, MYSQL_NOT_NULL_FLAG);
    assert_eq!(top.columns[0].original_table, "users");
    assert_eq!(
        (top.columns[2].column_type, top.columns[2].flags),
        (MYSQL_TYPE_NEWDECIMAL, MYSQL_NOT_NULL_FLAG)
    );

    // A body grouping its rows under an alias names the table the same way.
    let grouped = result_set(
        &mut adapter,
        "SELECT s.UserId, s.c FROM (SELECT p.UserId, COUNT(*) AS c FROM Posts AS p GROUP BY p.UserId) AS s ORDER BY s.UserId",
    );
    assert_eq!(grouped.columns[0].original_table, "posts");
    assert_eq!(grouped.columns[0].schema, "reports");

    assert!(adapter
        .execute_query(
            "SELECT s.Name FROM (SELECT u.Name FROM Users AS u ORDER BY u.Name LIMIT ?) AS s"
        )
        .is_err());
}

/// A projection counting and summing each user's posts reads them through
/// correlated subqueries, the sum fallen back on zero inside and outside
/// its subquery: `COALESCE((SELECT COALESCE(SUM(p0.Views), 0) FROM Posts AS
/// p0 WHERE u.Id = p0.UserId), 0)`. It was refused.
///
/// Measured on MySQL 8.4.11: the count is a nullable `LONGLONG` of 21 with
/// the binary flag, the fallen-back sum a NOT NULL `NEWDECIMAL` of 33, and
/// the user's name, which the subqueries read the table of, loses its NOT
/// NULL.
#[test]
fn a_projection_sums_a_relation_through_a_fallen_back_subquery() {
    let (_directory, mut adapter) = adapter();
    let counts = result_set(
        &mut adapter,
        "SELECT `u`.`Name`, (\n    SELECT COUNT(*)\n    FROM `Posts` AS `p`\n    WHERE `u`.`Id` = `p`.`UserId`) AS `Posts`, COALESCE((\n    SELECT COALESCE(SUM(`p0`.`Views`), 0)\n    FROM `Posts` AS `p0`\n    WHERE `u`.`Id` = `p0`.`UserId`), 0) AS `Views`\nFROM `Users` AS `u`\nORDER BY `u`.`Id`",
    );
    let shapes = counts
        .columns
        .iter()
        .map(|column| {
            (
                column.name.as_str(),
                column.column_type,
                column.column_length,
                column.flags,
            )
        })
        .collect::<Vec<_>>();
    assert_eq!(
        shapes,
        [
            (
                "Name",
                MYSQL_TYPE_VAR_STRING,
                400,
                MYSQL_NO_DEFAULT_VALUE_FLAG
            ),
            (
                "Posts",
                MYSQL_TYPE_LONGLONG,
                21,
                MYSQL_BINARY_FLAG | MYSQL_NUM_FLAG
            ),
            (
                "Views",
                MYSQL_TYPE_NEWDECIMAL,
                33,
                MYSQL_NOT_NULL_FLAG | MYSQL_BINARY_FLAG | MYSQL_NUM_FLAG
            ),
        ]
    );
    assert_eq!(
        rows(
            &mut adapter,
            "SELECT `u`.`Name`, (SELECT COUNT(*) FROM `Posts` AS `p` WHERE `u`.`Id` = `p`.`UserId`) AS `Posts`, COALESCE((SELECT COALESCE(SUM(`p0`.`Views`), 0) FROM `Posts` AS `p0` WHERE `u`.`Id` = `p0`.`UserId`), 0) AS `Views` FROM `Users` AS `u` ORDER BY `u`.`Id`"
        ),
        [
            row(&["Alice", "1", "0"]),
            row(&["Bob", "1", "3"]),
            row(&["Carol", "0", "0"])
        ]
    );
}

/// `Include(u => u.Posts).ThenInclude(p => p.Tags)` reads the relations
/// through derived tables nested in each other, an inner join inside, and
/// sorts by columns of more than one table; `AsSplitQuery()` joins the inner
/// derived table beside two tables. Both were refused.
///
/// Measured on MySQL 8.4.11 over no rows, three and thousands, the shapes the
/// same each time: MySQL sorts the rows through a table of its own, so no
/// column keeps a key, a derived table's column names no database and its
/// own table by name, and a column a `LEFT JOIN` can leave missing loses its
/// NOT NULL.
#[test]
fn include_reads_relations_through_derived_tables_sorted_across_them() {
    let (_directory, mut adapter) = adapter();
    let shapes = |set: &TextResultSet| {
        set.columns
            .iter()
            .map(|column| {
                format!(
                    "{} {}.{} {} {} {} {:#x}",
                    column.name,
                    column.schema,
                    column.table,
                    column.original_table,
                    column.column_type,
                    column.column_length,
                    column.flags
                )
            })
            .collect::<Vec<_>>()
    };
    let include = result_set(
        &mut adapter,
        "SELECT `u`.`Id`, `u`.`Balance`, `u`.`CreatedAt`, `u`.`Email`, `u`.`IsActive`, `u`.`Name`, `u`.`Profile`, `u`.`UpdatedAt`, `s0`.`Id`, `s0`.`Body`, `s0`.`PublishedAt`, `s0`.`Title`, `s0`.`UserId`, `s0`.`Views`, `s0`.`PostsId`, `s0`.`TagsId`, `s0`.`Id0`, `s0`.`Name`\nFROM `Users` AS `u`\nLEFT JOIN (\n    SELECT `p`.`Id`, `p`.`Body`, `p`.`PublishedAt`, `p`.`Title`, `p`.`UserId`, `p`.`Views`, `s`.`PostsId`, `s`.`TagsId`, `s`.`Id` AS `Id0`, `s`.`Name`\n    FROM `Posts` AS `p`\n    LEFT JOIN (\n        SELECT `p0`.`PostsId`, `p0`.`TagsId`, `t`.`Id`, `t`.`Name`\n        FROM `PostTags` AS `p0`\n        INNER JOIN `Tags` AS `t` ON `p0`.`TagsId` = `t`.`Id`\n    ) AS `s` ON `p`.`Id` = `s`.`PostsId`\n) AS `s0` ON `u`.`Id` = `s0`.`UserId`\nORDER BY `u`.`Id`, `s0`.`Id`, `s0`.`PostsId`, `s0`.`TagsId`",
    );
    assert_eq!(
        shapes(&include),
        [
            "Id reports.u users 8 20 0x1",
            "Balance reports.u users 246 12 0x1",
            "CreatedAt reports.u users 12 26 0x81",
            "Email reports.u users 253 764 0x1001",
            "IsActive reports.u users 1 1 0x1",
            "Name reports.u users 253 400 0x1001",
            "Profile reports.u users 245 4294967295 0x90",
            "UpdatedAt reports.u users 12 26 0x81",
            "Id .s0 posts 8 20 0x0",
            "Body .s0 posts 252 262140 0x10",
            "PublishedAt .s0 posts 12 26 0x80",
            "Title .s0 posts 253 800 0x1000",
            "UserId .s0 posts 8 20 0x1000",
            "Views .s0 posts 3 11 0x1000",
            "PostsId .s0 posttags 8 20 0x1000",
            "TagsId .s0 posttags 8 20 0x1000",
            "Id0 .s0 tags 8 20 0x0",
            "Name .s0 tags 253 400 0x1000",
        ]
    );
    let read = |set: &TextResultSet, columns: &[usize]| {
        set.rows
            .iter()
            .map(|row| {
                columns
                    .iter()
                    .map(|at| match &row[*at] {
                        Some(value) => String::from_utf8(value.clone()).unwrap(),
                        None => "NULL".to_owned(),
                    })
                    .collect::<Vec<_>>()
                    .join(" ")
            })
            .collect::<Vec<_>>()
    };
    assert_eq!(
        read(&include, &[0, 11, 17]),
        [
            "1 Draft news",
            "1 Draft rust",
            "2 Bob writes sql",
            "3 NULL NULL"
        ]
    );

    let split = result_set(
        &mut adapter,
        "SELECT `s`.`PostsId`, `s`.`TagsId`, `s`.`Id`, `s`.`Name`, `u`.`Id`, `p`.`Id`\nFROM `Users` AS `u`\nINNER JOIN `Posts` AS `p` ON `u`.`Id` = `p`.`UserId`\nINNER JOIN (\n    SELECT `p0`.`PostsId`, `p0`.`TagsId`, `t`.`Id`, `t`.`Name`\n    FROM `PostTags` AS `p0`\n    INNER JOIN `Tags` AS `t` ON `p0`.`TagsId` = `t`.`Id`\n) AS `s` ON `p`.`Id` = `s`.`PostsId`\nORDER BY `u`.`Id`, `p`.`Id`",
    );
    assert_eq!(
        shapes(&split),
        [
            "PostsId .s posttags 8 20 0x1001",
            "TagsId .s posttags 8 20 0x1001",
            "Id .s tags 8 20 0x1",
            "Name .s tags 253 400 0x1001",
            "Id reports.u users 8 20 0x1",
            "Id reports.p posts 8 20 0x1",
        ]
    );
    assert_eq!(read(&split, &[0, 3]), ["1 news", "1 rust", "2 sql"]);

    // The split query's posts are a join of two tables alone, sorted across
    // them the same way: measured, no column keeps its keys.
    let posts = result_set(
        &mut adapter,
        "SELECT `p`.`Id`, `p`.`Body`, `p`.`PublishedAt`, `p`.`Title`, `p`.`UserId`, `p`.`Views`, `u`.`Id`\nFROM `Users` AS `u`\nINNER JOIN `Posts` AS `p` ON `u`.`Id` = `p`.`UserId`\nORDER BY `u`.`Id`, `p`.`Id`",
    );
    assert_eq!(
        shapes(&posts),
        [
            "Id reports.p posts 8 20 0x1",
            "Body reports.p posts 252 262140 0x10",
            "PublishedAt reports.p posts 12 26 0x80",
            "Title reports.p posts 253 800 0x1001",
            "UserId reports.p posts 8 20 0x1001",
            "Views reports.p posts 3 11 0x1001",
            "Id reports.u users 8 20 0x1",
        ]
    );

    // A condition may make MySQL read a table as one constant row, and an
    // order within one table leaves which table it reads first to MySQL.
    for refused in [
        "SELECT `u`.`Id`, `s`.`Name` FROM `Users` AS `u` INNER JOIN (SELECT `p0`.`PostsId`, `t`.`Name` FROM `PostTags` AS `p0` INNER JOIN `Tags` AS `t` ON `p0`.`TagsId` = `t`.`Id`) AS `s` ON `u`.`Id` = `s`.`PostsId` WHERE `u`.`Id` = 1 ORDER BY `u`.`Id`, `s`.`Name`",
        "SELECT `u`.`Id`, `s`.`Name` FROM `Users` AS `u` INNER JOIN (SELECT `p0`.`PostsId`, `t`.`Name` FROM `PostTags` AS `p0` INNER JOIN `Tags` AS `t` ON `p0`.`TagsId` = `t`.`Id`) AS `s` ON `u`.`Id` = `s`.`PostsId` ORDER BY `u`.`Id`",
        "SELECT `u`.`Id`, `s`.`Name` FROM `Users` AS `u` INNER JOIN (SELECT `p0`.`PostsId`, `t`.`Name` FROM `PostTags` AS `p0` INNER JOIN `Tags` AS `t` ON `p0`.`TagsId` = `t`.`Id`) AS `s` ON `u`.`Id` = `s`.`PostsId`",
    ] {
        assert!(adapter.execute_query(refused).is_err(), "{refused}");
    }
}

/// The app checks its connection with `SqlQuery<string>($"SELECT
/// CONCAT(VERSION(), ' ', DATABASE()) AS Value").Single()`, which EF wraps in
/// a derived table. It answered 1064.
///
/// Measured on MySQL 8.4.11: the `CONCAT` is a nullable `VAR_STRING` as long
/// as its parts together, 24 + 4 + 256 = 284, with 31 decimals, which it
/// loses through the derived table, naming it.
#[test]
fn the_connection_check_reads_the_version_and_database_through_a_derived_table() {
    let (_directory, mut adapter) = adapter();
    let checked = result_set(
        &mut adapter,
        "SELECT `s`.`Value`\nFROM (\n    SELECT CONCAT(VERSION(), ' ', DATABASE()) AS Value\n) AS `s`\nLIMIT 2",
    );
    let column = &checked.columns[0];
    assert_eq!(
        (
            column.name.as_str(),
            column.table.as_str(),
            column.column_type,
            column.column_length,
            column.decimals,
            column.flags
        ),
        ("Value", "s", MYSQL_TYPE_VAR_STRING, 284, 0, 0)
    );
    let [row] = checked.rows.as_slice() else {
        panic!("one row, answered {:?}", checked.rows);
    };
    let [Some(value)] = row.as_slice() else {
        panic!("one value, answered {row:?}");
    };
    assert!(value.ends_with(b" reports"), "{value:?}");

    let plain = result_set(
        &mut adapter,
        "SELECT CONCAT(VERSION(), ' ', DATABASE()) AS Value",
    );
    assert_eq!(
        (
            plain.columns[0].table.as_str(),
            plain.columns[0].column_length,
            plain.columns[0].decimals
        ),
        ("", 284, 31)
    );
    let version = result_set(
        &mut adapter,
        "SELECT `s`.`Value` FROM (SELECT VERSION() AS Value) AS `s` LIMIT 2",
    );
    assert_eq!(
        (
            version.columns[0].column_length,
            version.columns[0].decimals,
            version.columns[0].flags
        ),
        (24, 0, MYSQL_NOT_NULL_FLAG)
    );
}

/// The scaffold matches the columns a key lists against the table's columns
/// by name, and the catalog named the columns of `PostTags`' two-column
/// primary key and of every foreign key in lower case — `postsid`, `userid`
/// and the parent's `id` — where they were declared `PostsId`, `UserId` and
/// `Id`.
///
/// Measured on MySQL 8.4.11: `STATISTICS` and `KEY_COLUMN_USAGE` name each as
/// the column was declared, whatever case the key wrote it in.
#[test]
fn the_catalog_names_a_keys_columns_as_they_were_declared() {
    let (_directory, mut adapter) = adapter();
    for sql in [
        "CREATE TABLE kp (`Id` int NOT NULL, `Code` int NOT NULL, PRIMARY KEY (`id`, `CODE`))",
        "CREATE TABLE kc (`UserId` int NOT NULL, `UserCode` int NOT NULL, PRIMARY KEY (`userid`), KEY `ix_k` (`USERCODE`), CONSTRAINT `fk_k` FOREIGN KEY (`userID`, `usercode`) REFERENCES `kp` (`ID`, `code`))",
    ] {
        changed(&mut adapter, sql);
    }
    assert_eq!(
        rows(
            &mut adapter,
            "SELECT TABLE_NAME, INDEX_NAME, SEQ_IN_INDEX, COLUMN_NAME FROM information_schema.STATISTICS \
             WHERE TABLE_SCHEMA = 'reports' AND TABLE_NAME IN ('kp', 'kc', 'PostTags') ORDER BY 1, 2, 3"
        ),
        [
            row(&["kc", "fk_k", "1", "UserId"]),
            row(&["kc", "fk_k", "2", "UserCode"]),
            row(&["kc", "ix_k", "1", "UserCode"]),
            row(&["kc", "PRIMARY", "1", "UserId"]),
            row(&["kp", "PRIMARY", "1", "Id"]),
            row(&["kp", "PRIMARY", "2", "Code"]),
            row(&["posttags", "IX_PostTags_TagsId", "1", "TagsId"]),
            row(&["posttags", "PRIMARY", "1", "PostsId"]),
            row(&["posttags", "PRIMARY", "2", "TagsId"]),
        ]
    );
    assert_eq!(
        rows(
            &mut adapter,
            "SELECT TABLE_NAME, CONSTRAINT_NAME, ORDINAL_POSITION, COLUMN_NAME, REFERENCED_COLUMN_NAME \
             FROM information_schema.KEY_COLUMN_USAGE \
             WHERE TABLE_SCHEMA = 'reports' AND TABLE_NAME IN ('kp', 'kc', 'Posts', 'PostTags') ORDER BY 1, 2, 3"
        ),
        [
            row(&["kc", "fk_k", "1", "UserId", "Id"]),
            row(&["kc", "fk_k", "2", "UserCode", "Code"]),
            row(&["kc", "PRIMARY", "1", "UserId", "NULL"]),
            row(&["kp", "PRIMARY", "1", "Id", "NULL"]),
            row(&["kp", "PRIMARY", "2", "Code", "NULL"]),
            row(&["posts", "FK_Posts_Users_UserId", "1", "UserId", "Id"]),
            row(&["posts", "PRIMARY", "1", "Id", "NULL"]),
            row(&["posttags", "FK_PostTags_Posts_PostsId", "1", "PostsId", "Id"]),
            row(&["posttags", "FK_PostTags_Tags_TagsId", "1", "TagsId", "Id"]),
            row(&["posttags", "PRIMARY", "1", "PostsId", "NULL"]),
            row(&["posttags", "PRIMARY", "2", "TagsId", "NULL"]),
        ]
    );
}

/// `dotnet ef dbcontext scaffold` reads the database's tables, then each
/// table's primary key, other indexes and foreign keys, each read grouping the
/// catalog's rows with `GROUP_CONCAT` over `CAST`, `IFNULL` and `CONCAT_WS`,
/// the first joining `COLLATION_CHARACTER_SET_APPLICABILITY` and the last
/// reading each key's `ON DELETE` rule through a correlated subquery. The
/// tables read answered 1235 and the scaffold stopped there.
///
/// Every row and column description here was measured on a MySQL 8.4.11
/// initialized with `lower_case_table_names=1`, holding the same tables.
#[test]
fn the_scaffold_reads_the_tables_keys_indexes_and_foreign_keys() {
    let (_directory, mut adapter) = adapter();
    for sql in [
        "CREATE TABLE `__EFMigrationsHistory` (\n    `MigrationId` varchar(150) CHARACTER SET utf8mb4 NOT NULL,\n    `ProductVersion` varchar(32) CHARACTER SET utf8mb4 NOT NULL,\n    CONSTRAINT `PK___EFMigrationsHistory` PRIMARY KEY (`MigrationId`)\n) CHARACTER SET=utf8mb4;\n",
        "CREATE TABLE `Alpha` (`Id` bigint NOT NULL, `Code` int NOT NULL, PRIMARY KEY (`Id`), UNIQUE KEY `UX_Alpha_Id_Code` (`Id`, `Code`)) COMMENT 'alpha table' COLLATE utf8mb3_unicode_ci",
        "CREATE TABLE `Zeta` (`Id` bigint NOT NULL PRIMARY KEY, `AlphaId` bigint NULL, `AlphaCode` int NULL, `Other` bigint NULL, CONSTRAINT `fk_z_second` FOREIGN KEY (`Other`) REFERENCES `Alpha` (`Id`) ON DELETE SET NULL, CONSTRAINT `FK_a_first` FOREIGN KEY (`AlphaId`, `AlphaCode`) REFERENCES `Alpha` (`Id`, `Code`))",
        "CREATE INDEX `ya` ON `Zeta` (`Other`, `Id`)",
        "CREATE UNIQUE INDEX `Xc` ON `Zeta` (`AlphaCode`, `Id`)",
        "CREATE INDEX `_u` ON `Zeta` (`AlphaId`, `AlphaCode`, `Other`)",
        "CREATE VIEW `AlphaView` AS SELECT `Id` FROM `Alpha`",
    ] {
        changed(&mut adapter, sql);
    }

    let tables = result_set(
        &mut adapter,
        "SELECT\n    `t`.`TABLE_NAME`,\n    `t`.`TABLE_TYPE`,\n    IF(`t`.`TABLE_COMMENT` = 'VIEW' AND `t`.`TABLE_TYPE` = 'VIEW', '', `t`.`TABLE_COMMENT`) AS `TABLE_COMMENT`,\n    `ccsa`.`CHARACTER_SET_NAME` as `TABLE_CHARACTER_SET`,\n    `t`.`TABLE_COLLATION`\nFROM\n    `INFORMATION_SCHEMA`.`TABLES` as `t`\nLEFT JOIN\n\t`INFORMATION_SCHEMA`.`COLLATION_CHARACTER_SET_APPLICABILITY` as `ccsa` ON `ccsa`.`COLLATION_NAME` = `t`.`TABLE_COLLATION`\nWHERE\n    `TABLE_SCHEMA` = SCHEMA()\nAND\n    `TABLE_TYPE` IN ('BASE TABLE', 'VIEW');",
    );
    assert_eq!(
        text_rows(tables.rows),
        [
            row(&[
                "__efmigrationshistory",
                "BASE TABLE",
                "",
                "utf8mb4",
                "utf8mb4_0900_ai_ci"
            ]),
            row(&[
                "alpha",
                "BASE TABLE",
                "alpha table",
                "utf8mb3",
                "utf8mb3_unicode_ci"
            ]),
            row(&["alphaview", "VIEW", "", "NULL", "NULL"]),
            row(&["posts", "BASE TABLE", "", "utf8mb4", "utf8mb4_0900_ai_ci"]),
            row(&[
                "posttags",
                "BASE TABLE",
                "",
                "utf8mb4",
                "utf8mb4_0900_ai_ci"
            ]),
            row(&["records", "BASE TABLE", "", "utf8mb4", "utf8mb4_0900_ai_ci"]),
            row(&["tags", "BASE TABLE", "", "utf8mb4", "utf8mb4_0900_ai_ci"]),
            row(&["users", "BASE TABLE", "", "utf8mb4", "utf8mb4_0900_ai_ci"]),
            row(&["zeta", "BASE TABLE", "", "utf8mb4", "utf8mb4_0900_ai_ci"]),
        ]
    );
    let key = MYSQL_UNIQUE_KEY_FLAG | MYSQL_NO_DEFAULT_VALUE_FLAG | MYSQL_PART_KEY_FLAG;
    assert_eq!(
        described(&tables.columns),
        [
            (
                "TABLE_NAME",
                "TABLE_NAME",
                "t",
                "TABLES",
                "",
                MYSQL_TYPE_VAR_STRING,
                256,
                0,
                31
            ),
            (
                "TABLE_TYPE",
                "TABLE_TYPE",
                "t",
                "TABLES",
                "information_schema",
                MYSQL_TYPE_STRING,
                44,
                MYSQL_NOT_NULL_FLAG
                    | MYSQL_MULTIPLE_KEY_FLAG
                    | MYSQL_BINARY_FLAG
                    | MYSQL_ENUM_FLAG
                    | MYSQL_NO_DEFAULT_VALUE_FLAG
                    | MYSQL_PART_KEY_FLAG,
                0
            ),
            (
                "TABLE_COMMENT",
                "",
                "",
                "",
                "",
                MYSQL_TYPE_VAR_STRING,
                8192,
                0,
                31
            ),
            (
                "TABLE_CHARACTER_SET",
                "CHARACTER_SET_NAME",
                "ccsa",
                "COLLATION_CHARACTER_SET_APPLICABILITY",
                "information_schema",
                MYSQL_TYPE_VAR_STRING,
                256,
                key,
                0
            ),
            (
                "TABLE_COLLATION",
                "TABLE_COLLATION",
                "t",
                "TABLES",
                "information_schema",
                MYSQL_TYPE_VAR_STRING,
                256,
                key,
                0
            ),
        ]
    );

    let primary_key = |table: &str| {
        format!("SELECT `INDEX_NAME`,\n     GROUP_CONCAT(`COLUMN_NAME` ORDER BY `SEQ_IN_INDEX` SEPARATOR ',') AS `COLUMNS`,\n     GROUP_CONCAT(CAST(IFNULL(`SUB_PART`, 0) AS CHAR) ORDER BY `SEQ_IN_INDEX` SEPARATOR ',') AS `SUB_PARTS`\n     FROM `INFORMATION_SCHEMA`.`STATISTICS`\n     WHERE `TABLE_SCHEMA` = 'reports'\n     AND `TABLE_NAME` = '{table}'\n     AND `INDEX_NAME` = 'PRIMARY'\n     GROUP BY `INDEX_NAME`;")
    };
    for (table, columns) in [
        ("posttags", "PostsId,TagsId"),
        ("posts", "Id"),
        ("__efmigrationshistory", "MigrationId"),
        ("zeta", "Id"),
    ] {
        assert_eq!(
            rows(&mut adapter, &primary_key(table)),
            [row(&[
                "PRIMARY",
                columns,
                &vec!["0"; columns.split(',').count()].join(",")
            ])],
            "{table}"
        );
    }
    for table in ["records", "alphaview", "missing"] {
        assert!(
            rows(&mut adapter, &primary_key(table)).is_empty(),
            "{table}"
        );
    }
    let read = result_set(&mut adapter, &primary_key("PostTags"));
    assert_eq!(
        text_rows(read.rows),
        [row(&["PRIMARY", "PostsId,TagsId", "0,0"])]
    );
    assert_eq!(
        described(&read.columns),
        [
            (
                "INDEX_NAME",
                "INDEX_NAME",
                "STATISTICS",
                "STATISTICS",
                "",
                MYSQL_TYPE_VAR_STRING,
                256,
                0,
                31
            ),
            (
                "COLUMNS",
                "",
                "",
                "",
                "",
                MYSQL_TYPE_LONG_BLOB,
                36864,
                0,
                31
            ),
            (
                "SUB_PARTS",
                "",
                "",
                "",
                "",
                MYSQL_TYPE_LONG_BLOB,
                65536,
                0,
                31
            ),
        ]
    );

    let indexes = |table: &str| {
        format!("SELECT `INDEX_NAME`,\n     `NON_UNIQUE`,\n     GROUP_CONCAT(`COLUMN_NAME` ORDER BY `SEQ_IN_INDEX` SEPARATOR ',') AS `COLUMNS`,\n     GROUP_CONCAT(CAST(IFNULL(`SUB_PART`, 0) AS CHAR) ORDER BY `SEQ_IN_INDEX` SEPARATOR ',') AS `SUB_PARTS`,\n     GROUP_CONCAT(IFNULL(`COLLATION`, 'A') ORDER BY `SEQ_IN_INDEX` SEPARATOR ',') AS `COLLATION`,\n     `INDEX_TYPE`\n     FROM `INFORMATION_SCHEMA`.`STATISTICS`\n     WHERE `TABLE_SCHEMA` = 'reports'\n     AND `TABLE_NAME` = '{table}'\n     AND `INDEX_NAME` <> 'PRIMARY'\n     GROUP BY `INDEX_NAME`, `NON_UNIQUE`, `INDEX_TYPE`;")
    };
    assert_eq!(
        rows(&mut adapter, &indexes("posts")),
        [row(&[
            "IX_Posts_UserId_PublishedAt",
            "1",
            "UserId,PublishedAt",
            "0,0",
            "A,A",
            "BTREE"
        ])]
    );
    assert_eq!(
        rows(&mut adapter, &indexes("tags")),
        [row(&["IX_Tags_Name", "0", "Name", "0", "A", "BTREE"])]
    );
    assert_eq!(
        rows(&mut adapter, &indexes("alpha")),
        [row(&[
            "UX_Alpha_Id_Code",
            "0",
            "Id,Code",
            "0,0",
            "A,A",
            "BTREE"
        ])]
    );
    assert_eq!(
        rows(&mut adapter, &indexes("zeta")),
        [
            row(&[
                "_u",
                "1",
                "AlphaId,AlphaCode,Other",
                "0,0,0",
                "A,A,A",
                "BTREE"
            ]),
            row(&["Xc", "0", "AlphaCode,Id", "0,0", "A,A", "BTREE"]),
            row(&["ya", "1", "Other,Id", "0,0", "A,A", "BTREE"]),
        ]
    );
    assert!(rows(&mut adapter, &indexes("__efmigrationshistory")).is_empty());
    let read = result_set(&mut adapter, &indexes("PostTags"));
    assert_eq!(
        text_rows(read.rows),
        [row(&[
            "IX_PostTags_TagsId",
            "1",
            "TagsId",
            "0",
            "A",
            "BTREE"
        ])]
    );
    let non_unique = (
        "NON_UNIQUE",
        "NON_UNIQUE",
        "STATISTICS",
        "",
        "information_schema",
        MYSQL_TYPE_LONG,
        2,
        MYSQL_NOT_NULL_FLAG,
        0,
    );
    assert_eq!(
        described(&read.columns),
        [
            (
                "INDEX_NAME",
                "INDEX_NAME",
                "STATISTICS",
                "",
                "information_schema",
                MYSQL_TYPE_VAR_STRING,
                256,
                0,
                0
            ),
            non_unique,
            (
                "COLUMNS",
                "",
                "",
                "",
                "",
                MYSQL_TYPE_LONG_BLOB,
                36864,
                0,
                31
            ),
            (
                "SUB_PARTS",
                "",
                "",
                "",
                "",
                MYSQL_TYPE_LONG_BLOB,
                65536,
                0,
                31
            ),
            (
                "COLLATION",
                "",
                "",
                "",
                "",
                MYSQL_TYPE_LONG_BLOB,
                65536,
                0,
                31
            ),
            (
                "INDEX_TYPE",
                "INDEX_TYPE",
                "STATISTICS",
                "",
                "information_schema",
                MYSQL_TYPE_VAR_STRING,
                44,
                MYSQL_NOT_NULL_FLAG | MYSQL_BINARY_FLAG,
                0
            ),
        ]
    );
    assert_eq!(read.columns[1].character_set, MYSQL_BINARY_COLLATION);

    let foreign_keys = |table: &str| {
        format!("SELECT\n \t`CONSTRAINT_NAME`,\n \t`TABLE_NAME`,\n \t`REFERENCED_TABLE_NAME`,\n \tGROUP_CONCAT(CONCAT_WS('|', `COLUMN_NAME`, `REFERENCED_COLUMN_NAME`) ORDER BY `ORDINAL_POSITION` SEPARATOR ',') AS PAIRED_COLUMNS,\n \t(SELECT `DELETE_RULE` FROM `INFORMATION_SCHEMA`.`REFERENTIAL_CONSTRAINTS` WHERE `REFERENTIAL_CONSTRAINTS`.`CONSTRAINT_NAME` = `KEY_COLUMN_USAGE`.`CONSTRAINT_NAME` AND `REFERENTIAL_CONSTRAINTS`.`CONSTRAINT_SCHEMA` = `KEY_COLUMN_USAGE`.`CONSTRAINT_SCHEMA`) AS `DELETE_RULE`\n FROM `INFORMATION_SCHEMA`.`KEY_COLUMN_USAGE`\n WHERE `TABLE_SCHEMA` = 'reports'\n \t\tAND `TABLE_NAME` = '{table}'\n \t\tAND `CONSTRAINT_NAME` <> 'PRIMARY'\n        AND `REFERENCED_TABLE_NAME` IS NOT NULL\n        GROUP BY `CONSTRAINT_SCHEMA`,\n        `CONSTRAINT_NAME`,\n        `TABLE_NAME`,\n        `REFERENCED_TABLE_NAME`;")
    };
    assert_eq!(
        rows(&mut adapter, &foreign_keys("posts")),
        [row(&[
            "FK_Posts_Users_UserId",
            "posts",
            "users",
            "UserId|Id",
            "CASCADE"
        ])]
    );
    assert_eq!(
        rows(&mut adapter, &foreign_keys("zeta")),
        [
            row(&[
                "FK_a_first",
                "zeta",
                "alpha",
                "AlphaId|Id,AlphaCode|Code",
                "NO ACTION"
            ]),
            row(&["fk_z_second", "zeta", "alpha", "Other|Id", "SET NULL"]),
        ]
    );
    for table in ["users", "alpha", "records"] {
        assert!(
            rows(&mut adapter, &foreign_keys(table)).is_empty(),
            "{table}"
        );
    }
    let read = result_set(&mut adapter, &foreign_keys("PostTags"));
    assert_eq!(
        text_rows(read.rows),
        [
            row(&[
                "FK_PostTags_Posts_PostsId",
                "posttags",
                "posts",
                "PostsId|Id",
                "CASCADE"
            ]),
            row(&[
                "FK_PostTags_Tags_TagsId",
                "posttags",
                "tags",
                "TagsId|Id",
                "CASCADE"
            ]),
        ]
    );
    assert_eq!(
        described(&read.columns),
        [
            (
                "CONSTRAINT_NAME",
                "CONSTRAINT_NAME",
                "KEY_COLUMN_USAGE",
                "",
                "information_schema",
                MYSQL_TYPE_VAR_STRING,
                256,
                0,
                0
            ),
            (
                "TABLE_NAME",
                "TABLE_NAME",
                "KEY_COLUMN_USAGE",
                "",
                "information_schema",
                MYSQL_TYPE_VAR_STRING,
                256,
                0,
                0
            ),
            (
                "REFERENCED_TABLE_NAME",
                "REFERENCED_TABLE_NAME",
                "KEY_COLUMN_USAGE",
                "",
                "information_schema",
                MYSQL_TYPE_VAR_STRING,
                256,
                0,
                0
            ),
            (
                "PAIRED_COLUMNS",
                "",
                "",
                "",
                "",
                MYSQL_TYPE_LONG_BLOB,
                36864,
                0,
                31
            ),
            (
                "DELETE_RULE",
                "DELETE_RULE",
                "",
                "",
                "",
                MYSQL_TYPE_VAR_STRING,
                44,
                MYSQL_BINARY_FLAG,
                0
            ),
        ]
    );

    // MySqlConnector sends each as text; prepared, the same statements are
    // still refused rather than answered some other way.
    assert!(adapter.execute_stmt_prepare(&primary_key("posts")).is_err());
    assert!(adapter
        .execute_stmt_prepare(&foreign_keys("posts"))
        .is_err());
    // The widths above follow `group_concat_max_len`, so under another limit
    // the grouped reads are refused.
    changed(&mut adapter, "SET SESSION group_concat_max_len = 2048");
    assert!(adapter.execute_query(&indexes("posts")).is_err());
}

/// Each column as `(name, original name, table, original table, database,
/// type, length, flags, decimals)`.
#[allow(clippy::type_complexity)]
fn described(
    columns: &[ColumnDefinitionConfig],
) -> Vec<(&str, &str, &str, &str, &str, u8, u32, u16, u8)> {
    columns
        .iter()
        .map(|column| {
            (
                column.name.as_str(),
                column.original_name.as_str(),
                column.table.as_str(),
                column.original_table.as_str(),
                column.schema.as_str(),
                column.column_type,
                column.column_length,
                column.flags,
                column.decimals,
            )
        })
        .collect()
}

fn text_rows(rows: Vec<Vec<Option<Vec<u8>>>>) -> Vec<Vec<Option<String>>> {
    rows.into_iter()
        .map(|row| {
            row.into_iter()
                .map(|value| value.map(|value| String::from_utf8(value).unwrap()))
                .collect()
        })
        .collect()
}

fn updated_at(adapter: &mut Adapter, id: u32) -> String {
    let read = rows(
        adapter,
        &format!("SELECT `UpdatedAt` FROM `Users` WHERE `Id` = {id}"),
    );
    read[0][0].clone().unwrap()
}
