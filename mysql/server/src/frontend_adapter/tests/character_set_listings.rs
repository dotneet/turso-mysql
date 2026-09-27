//! `SHOW COLLATION` and `SHOW CHARACTER SET`, which GUI tools and drivers send
//! to learn what the server has.
//!
//! Every row and shape here was measured on MySQL 8.4.11.

use super::*;

type Adapter = AuthorizedDatabaseCommandAdapter<RecordingAuthorizer>;

/// A session with no database selected, which MySQL answers both listings in.
fn adapter() -> (tempfile::TempDir, Adapter) {
    let authorizer = Arc::new(RecordingAuthorizer::default());
    let (directory, _catalog, factory) = catalog_factory(authorizer);
    let mut adapter = factory
        .build(AuthenticatedPrincipal::from_account_id_for_testing(
            AccountId::from_bytes([161; 32]),
        ))
        .unwrap();
    adapter.authorize_connection().unwrap();
    (directory, adapter)
}

fn read(adapter: &mut Adapter, sql: &str) -> TextResultSet {
    match adapter.execute_query(sql) {
        Ok(CommandExecutionResult::ResultSet(result)) => result,
        other => panic!("{sql} must return a result set, not {other:?}"),
    }
}

fn first_column(adapter: &mut Adapter, sql: &str) -> Vec<String> {
    read(adapter, sql)
        .rows
        .into_iter()
        .map(|row| String::from_utf8(row[0].clone().unwrap()).unwrap())
        .collect()
}

/// This server speaks utf8mb4, so it lists the collations a column, a table or
/// the connection may name, and the binary one every `BLOB` holds, each with
/// the row MySQL answers for it, in name order.
#[test]
fn the_collations_are_the_ones_this_server_has() {
    let (_directory, mut adapter) = adapter();
    let listed = read(&mut adapter, "SHOW COLLATION");
    assert_eq!(
        listed
            .columns
            .iter()
            .map(|column| column.name.as_str())
            .collect::<Vec<_>>(),
        [
            "Collation",
            "Charset",
            "Id",
            "Default",
            "Compiled",
            "Sortlen",
            "Pad_attribute"
        ]
    );
    let rows = listed
        .rows
        .iter()
        .map(|row| {
            row.iter()
                .map(|value| String::from_utf8(value.clone().unwrap()).unwrap())
                .collect::<Vec<_>>()
        })
        .collect::<Vec<_>>();
    assert_eq!(
        rows,
        [
            ["binary", "binary", "63", "Yes", "Yes", "1", "NO PAD"],
            [
                "utf8mb4_0900_ai_ci",
                "utf8mb4",
                "255",
                "Yes",
                "Yes",
                "0",
                "NO PAD"
            ],
            ["utf8mb4_bin", "utf8mb4", "46", "", "Yes", "1", "PAD SPACE"],
            [
                "utf8mb4_general_ci",
                "utf8mb4",
                "45",
                "",
                "Yes",
                "1",
                "PAD SPACE"
            ],
            [
                "utf8mb4_unicode_ci",
                "utf8mb4",
                "224",
                "",
                "Yes",
                "8",
                "PAD SPACE"
            ],
        ]
    );
    let id = &listed.columns[2];
    assert_eq!(
        (id.column_type, id.column_length),
        (MYSQL_TYPE_LONGLONG, 20)
    );
    assert_eq!(
        id.flags,
        MYSQL_NOT_NULL_FLAG | MYSQL_UNSIGNED_FLAG | MYSQL_NUM_FLAG
    );
    let pad = &listed.columns[6];
    assert_eq!(
        (pad.column_type, pad.column_length),
        (MYSQL_TYPE_STRING, 36)
    );
}

/// A `LIKE` names the collations or character sets to list, and a `WHERE`
/// tests their columns: measured, both match without regard to case.
#[test]
fn a_listing_keeps_what_its_filter_names() {
    let (_directory, mut adapter) = adapter();
    assert_eq!(
        first_column(&mut adapter, "SHOW COLLATION LIKE 'UTF8MB4_BIN'"),
        ["utf8mb4_bin"]
    );
    assert_eq!(
        first_column(
            &mut adapter,
            "SHOW COLLATION WHERE Charset = 'UTF8MB4' AND Collation = 'utf8mb4_bin'"
        ),
        ["utf8mb4_bin"]
    );
    assert_eq!(
        first_column(
            &mut adapter,
            "SHOW COLLATION WHERE `Default` = 'Yes' AND Charset = 'utf8mb4'"
        ),
        ["utf8mb4_0900_ai_ci"]
    );
    assert_eq!(
        first_column(&mut adapter, "SHOW COLLATION WHERE Id = 255"),
        ["utf8mb4_0900_ai_ci"]
    );
    assert_eq!(
        first_column(&mut adapter, "SHOW CHARACTER SET"),
        ["binary", "utf8mb4"]
    );
    assert_eq!(
        first_column(&mut adapter, "SHOW CHARSET LIKE 'utf8mb4'"),
        ["utf8mb4"]
    );
    assert_eq!(
        first_column(
            &mut adapter,
            "SHOW CHARACTER SET WHERE Maxlen = 4 AND Charset LIKE 'utf8%'"
        ),
        ["utf8mb4"]
    );
    let described = read(&mut adapter, "SHOW CHARACTER SET LIKE 'utf8mb4'");
    assert_eq!(
        described.rows[0]
            .iter()
            .map(|value| String::from_utf8(value.clone().unwrap()).unwrap())
            .collect::<Vec<_>>(),
        ["utf8mb4", "UTF-8 Unicode", "utf8mb4_0900_ai_ci", "4"]
    );
    assert_eq!(described.columns[2].name, "Default collation");

    // A column the listing has not got is MySQL's 1054, and a test of any
    // other shape is refused rather than read as something narrower.
    assert_eq!(
        adapter.execute_query("SHOW COLLATION WHERE Nope = 1"),
        Err(FrontendErrorKind::UnknownColumn)
    );
    assert!(adapter
        .execute_query("SHOW COLLATION WHERE Charset <> 'utf8mb4'")
        .is_err());
}
