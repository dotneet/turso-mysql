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
