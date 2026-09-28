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

fn updated_at(adapter: &mut Adapter, id: u32) -> String {
    let read = rows(
        adapter,
        &format!("SELECT `UpdatedAt` FROM `Users` WHERE `Id` = {id}"),
    );
    read[0][0].clone().unwrap()
}
