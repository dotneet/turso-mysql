//! The test of the server's version xorm writes into its column listing.
//!
//! xorm's `GetColumns` asks whether the server is a MariaDB that quotes its
//! column defaults, by reading `VERSION()` apart inside the statement. The
//! test depends on nothing but the version, so its answer is known before the
//! statement runs, and the rest of the statement is an ordinary read of
//! `information_schema.COLUMNS`.

use super::*;
use crate::statement_reads;

/// The test, byte for byte as xorm v1.3 writes it.
const MARIADB_TEST: &str = "(INSTR(VERSION(), 'maria') > 0 && \
    (SUBSTRING_INDEX(VERSION(), '.', 1) > 10 || \
    (SUBSTRING_INDEX(VERSION(), '.', 1) = 10 && \
    (SUBSTRING_INDEX(SUBSTRING(VERSION(), 4), '.', 1) > 2 || \
    (SUBSTRING_INDEX(SUBSTRING(VERSION(), 4), '.', 1) = 2 && \
    SUBSTRING_INDEX(SUBSTRING(VERSION(), 6), '-', 1) >= 7)))))";

/// Where in `sql` xorm's MariaDB test stands, when it stands there once and
/// as a test rather than inside a quoted word.
pub fn xorm_mariadb_test_span(sql: &str, mode: SessionSqlMode) -> Option<std::ops::Range<usize>> {
    let start = sql.find(MARIADB_TEST)?;
    let end = start + MARIADB_TEST.len();
    if sql[end..].contains(MARIADB_TEST) {
        return None;
    }
    let dialect = SessionMySqlDialect::without_executable_comments(mode);
    let tokens = |text: &str| {
        statement_reads::tokens(&dialect, text).ok().map(|tokens| {
            tokens
                .into_iter()
                .filter(|token| !matches!(token, Token::Whitespace(_)))
                .collect::<Vec<_>>()
        })
    };
    let statement = tokens(sql)?;
    let test = tokens(MARIADB_TEST)?;
    // The bytes could stand inside a quoted word; the test is only there when
    // the statement's own tokens hold the test's.
    statement
        .windows(test.len())
        .any(|window| window == test.as_slice())
        .then_some(start..end)
}

/// What xorm's MariaDB test answers on a server reporting `version`.
///
/// MySQL reads `&&` as `AND`, which is false as soon as its left side is, so
/// a version naming no `maria` answers 0 whatever the rest reads. Any other
/// version is not worked out here.
pub fn xorm_mariadb_test_answer(version: &str) -> Option<i64> {
    (!version.to_lowercase().contains("maria")).then_some(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    const GET_COLUMNS: &str = "SELECT `COLUMN_NAME`, `IS_NULLABLE`, `COLUMN_DEFAULT`, `COLUMN_TYPE`, `COLUMN_KEY`, `EXTRA`, `COLUMN_COMMENT`, `CHARACTER_MAXIMUM_LENGTH`, (INSTR(VERSION(), 'maria') > 0 && (SUBSTRING_INDEX(VERSION(), '.', 1) > 10 || (SUBSTRING_INDEX(VERSION(), '.', 1) = 10 && (SUBSTRING_INDEX(SUBSTRING(VERSION(), 4), '.', 1) > 2 || (SUBSTRING_INDEX(SUBSTRING(VERSION(), 4), '.', 1) = 2 && SUBSTRING_INDEX(SUBSTRING(VERSION(), 6), '-', 1) >= 7))))) AS NEEDS_QUOTE, `COLLATION_NAME` FROM `INFORMATION_SCHEMA`.`COLUMNS` WHERE `TABLE_SCHEMA` = ? AND `TABLE_NAME` = ? ORDER BY `COLUMNS`.ORDINAL_POSITION ASC";

    #[test]
    fn finds_the_test_in_xorms_column_listing() {
        let span = xorm_mariadb_test_span(GET_COLUMNS, SessionSqlMode::default()).unwrap();
        assert!(GET_COLUMNS[span.clone()].starts_with("(INSTR(VERSION(), 'maria')"));
        assert!(GET_COLUMNS[span.end..].starts_with(" AS NEEDS_QUOTE"));
    }

    #[test]
    fn leaves_the_test_inside_a_word_or_written_twice() {
        let quoted = format!("SELECT \"{MARIADB_TEST}\"");
        assert_eq!(
            xorm_mariadb_test_span(&quoted, SessionSqlMode::default()),
            None
        );
        let twice = format!("SELECT {MARIADB_TEST}, {MARIADB_TEST}");
        assert_eq!(
            xorm_mariadb_test_span(&twice, SessionSqlMode::default()),
            None
        );
        assert_eq!(
            xorm_mariadb_test_span("SELECT VERSION()", SessionSqlMode::default()),
            None
        );
    }

    #[test]
    fn answers_zero_on_a_server_that_is_no_mariadb() {
        assert_eq!(xorm_mariadb_test_answer("8.0.36-turso"), Some(0));
        assert_eq!(xorm_mariadb_test_answer("10.11.7-MariaDB"), None);
    }
}
