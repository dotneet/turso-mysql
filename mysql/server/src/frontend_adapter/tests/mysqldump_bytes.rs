//! Restoring what `mysqldump` writes for columns of bytes, and answering what
//! it reads to dump them again.
//!
//! `mysqldump_bytes_default.sql` and `mysqldump_bytes_hex_blob.sql` are real
//! dumps, taken with `mysqldump --single-transaction --routines --triggers
//! --events --databases bindata` from MySQL 8.4.11, the second with
//! `--hex-blob` besides, of a table holding a `BLOB`, a `TINYBLOB`, a
//! `MEDIUMBLOB`, a `LONGBLOB`, a `VARBINARY(300)` under a unique key and a
//! `BINARY(8)`, and a table keyed by a `BINARY(16)`. Their rows hold every
//! byte from 0x00 to 0xFF, the empty value, NULL, bytes that begin a
//! character without finishing it, and text. The first writes each value as
//! `_binary '...'` holding its raw bytes, which are not UTF-8; the second as
//! `0x...`. The rows were made by:
//!
//! ```sql
//! INSERT INTO blobs VALUES (1, <0x00..0xFF>, <0xFF..0x01>, <0x00..0xFF>, <0x00..0xFF>, <0x00..0xFF>, X'00FF27225C0A0D1A');
//! INSERT INTO blobs VALUES (2, '', '', '', '', '', '');
//! INSERT INTO blobs VALUES (3, NULL, NULL, NULL, NULL, NULL, NULL);
//! INSERT INTO blobs VALUES (4, 'héllo', X'00', 'text', X'C3', 'ab  ', 'ab');
//! INSERT INTO blobs VALUES (5, X'FF', X'80', X'E282', X'F09F9880', X'00', X'0000000000000000');
//! INSERT INTO tokens VALUES (X'000102030405060708090A0B0C0D0E0F', 'counting'), (X'FFFEFDFCFBFAF9F8F7F6F5F4F3F2F1F0', 'high'), (X'275C0A0D1A2200000000000000000000', 'escapes');
//! ```
//!
//! MySQL filled `'ab'` and `''` out to eight bytes in the `BINARY(8)`, so the
//! dumps carry them eight bytes wide. Every expectation here was measured
//! against the same server.

use super::*;

type Adapter = AuthorizedDatabaseCommandAdapter<RecordingAuthorizer>;

const DEFAULT_DUMP: &[u8] = include_bytes!("mysqldump_bytes_default.sql");
const HEX_BLOB_DUMP: &[u8] = include_bytes!("mysqldump_bytes_hex_blob.sql");

fn restoring_session() -> (tempfile::TempDir, Adapter) {
    let authorizer = Arc::new(RecordingAuthorizer::with_schema_creator("dump_owner"));
    let (directory, _catalog, factory) = catalog_factory(authorizer);
    let mut adapter = factory
        .build(AuthenticatedPrincipal::from_account_id_for_testing(
            AccountId::from_bytes([0x63; 32]),
        ))
        .unwrap();
    adapter.authorize_connection().unwrap();
    (directory, adapter)
}

/// Sends a dump the way the `mysql` client does — each comment line on its
/// own, a statement ending at the line that ends with `;` — and reads each
/// statement that is not UTF-8 the way the command reader reads it.
fn replay(adapter: &mut Adapter, dump: &[u8]) {
    let mut refused = Vec::new();
    let mut pending = Vec::new();
    for line in dump.split(|byte| *byte == b'\n') {
        if pending.is_empty() {
            if line.iter().all(u8::is_ascii_whitespace) {
                continue;
            }
            if line.starts_with(b"--") {
                send(adapter, line, &mut refused);
                continue;
            }
        } else {
            pending.push(b'\n');
        }
        pending.extend_from_slice(line);
        if pending.ends_with(b";") {
            pending.pop();
            send(adapter, &pending, &mut refused);
            pending.clear();
        }
    }
    assert!(pending.is_empty(), "the dump ends inside a statement");
    assert!(refused.is_empty(), "{}", refused.join("\n"));
}

fn send(adapter: &mut Adapter, statement: &[u8], refused: &mut Vec<String>) {
    let sql = match std::str::from_utf8(statement) {
        Ok(sql) => sql.to_owned(),
        Err(_) => turso_mysql_parser::raw_bytes_in_words_as_hexadecimal(
            statement,
            adapter.no_backslash_escapes(),
        )
        .unwrap_or_else(|| panic!("the reader refused {}", String::from_utf8_lossy(statement))),
    };
    let answered = adapter.execute_query(&sql).and_then(|_| {
        if let Some(database) = sql.strip_prefix("USE ") {
            adapter.execute_query("SELECT DATABASE()")?;
            adapter.execute_init_db(database.trim_matches('`'))?;
        }
        Ok(())
    });
    if let Err(error) = answered {
        refused.push(format!(
            "{error:?}: {}",
            sql.chars().take(200).collect::<String>()
        ));
    }
}

fn result(adapter: &mut Adapter, sql: &str) -> TextResultSet {
    match adapter.execute_query(sql) {
        Ok(CommandExecutionResult::ResultSet(result)) => result,
        other => panic!("{sql} must return a result set, answered {other:?}"),
    }
}

fn every_byte() -> Vec<u8> {
    (0..=255).collect()
}

/// What `SELECT * FROM blobs` answers, as MySQL stored the rows.
fn the_rows_mysql_holds() -> Vec<Vec<Option<Vec<u8>>>> {
    let value = |bytes: &[u8]| Some(bytes.to_vec());
    vec![
        vec![
            value(b"1"),
            Some(every_byte()),
            Some((1..=255).rev().collect()),
            Some(every_byte()),
            Some(every_byte()),
            Some(every_byte()),
            value(&[0x00, 0xFF, 0x27, 0x22, 0x5C, 0x0A, 0x0D, 0x1A]),
        ],
        vec![
            value(b"2"),
            value(b""),
            value(b""),
            value(b""),
            value(b""),
            value(b""),
            value(&[0; 8]),
        ],
        vec![value(b"3"), None, None, None, None, None, None],
        vec![
            value(b"4"),
            value("héllo".as_bytes()),
            value(&[0x00]),
            value(b"text"),
            value(&[0xC3]),
            value(b"ab  "),
            value(b"ab\0\0\0\0\0\0"),
        ],
        vec![
            value(b"5"),
            value(&[0xFF]),
            value(&[0x80]),
            value(&[0xE2, 0x82]),
            value(&[0xF0, 0x9F, 0x98, 0x80]),
            value(&[0x00]),
            value(&[0; 8]),
        ],
    ]
}

/// Both dumps restore every byte, and the restored tables answer what
/// `mysqldump` reads to dump them again — the same `CREATE TABLE`, and the
/// rows in columns described in the binary collation, which is what makes it
/// write each value as `_binary '...'`, or `0x...` under `--hex-blob` — so a
/// second dump taken from this server writes what the first one did.
#[test]
fn a_dump_of_bytes_restores_every_byte_and_dumps_again_alike() {
    for dump in [DEFAULT_DUMP, HEX_BLOB_DUMP] {
        let (_directory, mut adapter) = restoring_session();
        replay(&mut adapter, dump);
        let rows = result(
            &mut adapter,
            "SELECT /*!40001 SQL_NO_CACHE */ * FROM `blobs`",
        );
        assert_eq!(
            rows.columns[1..]
                .iter()
                .map(|column| (
                    column.column_type,
                    column.column_length,
                    column.flags,
                    column.character_set
                ))
                .collect::<Vec<_>>(),
            [
                (MYSQL_TYPE_BLOB, 65_535, 144, 63),
                (MYSQL_TYPE_BLOB, 255, 144, 63),
                (MYSQL_TYPE_BLOB, 16_777_215, 144, 63),
                (MYSQL_TYPE_BLOB, 4_294_967_295, 144, 63),
                (MYSQL_TYPE_VAR_STRING, 300, 16516, 63),
                (MYSQL_TYPE_STRING, 8, 128, 63),
            ]
        );
        assert_eq!(rows.rows, the_rows_mysql_holds());
        let tokens = result(
            &mut adapter,
            "SELECT /*!40001 SQL_NO_CACHE */ * FROM `tokens`",
        );
        assert_eq!(
            (
                tokens.columns[0].column_type,
                tokens.columns[0].column_length,
                tokens.columns[0].flags,
                tokens.columns[0].character_set
            ),
            (MYSQL_TYPE_STRING, 16, 20611, 63)
        );
        assert_eq!(
            tokens.rows,
            [
                ((0..16).collect::<Vec<u8>>(), "counting"),
                (
                    [&[0x27, 0x5C, 0x0A, 0x0D, 0x1A, 0x22][..], &[0; 10]].concat(),
                    "escapes"
                ),
                ((0xF0..=0xFF).rev().collect(), "high"),
            ]
            .map(|(id, label)| vec![Some(id), Some(label.as_bytes().to_vec())])
        );
        for table in ["blobs", "tokens"] {
            let printed = result(&mut adapter, &format!("SHOW CREATE TABLE `{table}`")).rows[0][1]
                .clone()
                .unwrap();
            let dumped = the_dumped_create_table(dump, table);
            assert_eq!(String::from_utf8(printed).unwrap(), dumped, "{table}");
        }
    }
}

/// The `CREATE TABLE` a dump wrote for a table, without its `;`.
fn the_dumped_create_table(dump: &[u8], table: &str) -> String {
    let text = String::from_utf8_lossy(dump);
    let start = text
        .find(&format!("CREATE TABLE `{table}` ("))
        .expect("the dump creates the table");
    let end = start + text[start..].find(";\n").expect("the statement ends");
    text[start..end].to_owned()
}

/// A key over bytes finds a row by the bytes it was dumped with, and holds a
/// second row with the same bytes off with 1062.
#[test]
fn a_restored_key_over_bytes_finds_its_rows() {
    let (_directory, mut adapter) = restoring_session();
    replay(&mut adapter, DEFAULT_DUMP);
    assert_eq!(
        result(
            &mut adapter,
            "SELECT label FROM tokens WHERE id = X'FFFEFDFCFBFAF9F8F7F6F5F4F3F2F1F0'"
        )
        .rows,
        [[Some(b"high".to_vec())]]
    );
    assert_eq!(
        result(&mut adapter, "SELECT id FROM blobs WHERE vb = X'00'").rows,
        [[Some(b"5".to_vec())]]
    );
    assert_eq!(
        adapter.execute_query("INSERT INTO blobs (id, vb) VALUES (6, '')"),
        Err(FrontendErrorKind::ConstraintViolation)
    );
}
