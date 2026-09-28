//! The one `information_schema.COLUMN_STATISTICS` query `mysqldump` sends for
//! every table it dumps.

use super::{
    admin_command::AdminToken, admin_command_ends, consume_admin_word, skip_admin_comments,
    tokenize_admin_command, ParseError, SessionSqlMode,
};

/// `mysqldump` 8.4's question whether a table has histograms, written exactly
/// as it writes it:
///
/// ```sql
/// SELECT COLUMN_NAME, JSON_EXTRACT(HISTOGRAM, '$."number-of-buckets-specified"')
/// FROM information_schema.COLUMN_STATISTICS
/// WHERE SCHEMA_NAME = 'db' AND TABLE_NAME = 'table'
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MySqlHistogramQuery {
    reading_column_name: String,
}

impl MySqlHistogramQuery {
    /// The name MySQL gives the second column: the call as written.
    pub fn reading_column_name(&self) -> &str {
        &self.reading_column_name
    }
}

/// The path `mysqldump` reads out of each histogram.
const BUCKETS_PATH: &str = "$.\"number-of-buckets-specified\"";

/// Parses `mysqldump`'s histogram query, or returns `None` for any other
/// statement, `COLUMN_STATISTICS` read any other way included.
pub fn parse_optional_histogram_query(
    sql: &str,
    mode: SessionSqlMode,
) -> Result<Option<MySqlHistogramQuery>, ParseError> {
    let upper = sql.to_ascii_uppercase();
    if !upper.contains("COLUMN_STATISTICS") {
        return Ok(None);
    }
    let tokens = tokenize_admin_command(sql, mode)?;
    let mut cursor = skip_admin_comments(&tokens, 0);
    let words = |cursor: &mut usize, expected: &[&str]| {
        expected
            .iter()
            .all(|word| consume_admin_word(&tokens, cursor, word))
    };
    let token = |cursor: &mut usize, expected: AdminToken| {
        let found = tokens.get(*cursor) == Some(&expected);
        if found {
            *cursor += 1;
        }
        found
    };
    let string = |cursor: &mut usize| {
        let Some(AdminToken::StringLiteral(value)) = tokens.get(*cursor) else {
            return None;
        };
        *cursor += 1;
        Some(value.clone())
    };
    let shaped = words(&mut cursor, &["SELECT", "COLUMN_NAME"])
        && token(&mut cursor, AdminToken::Comma)
        && words(&mut cursor, &["JSON_EXTRACT"])
        && token(&mut cursor, AdminToken::LeftParen)
        && words(&mut cursor, &["HISTOGRAM"])
        && token(&mut cursor, AdminToken::Comma)
        && string(&mut cursor).as_deref() == Some(BUCKETS_PATH)
        && token(&mut cursor, AdminToken::RightParen)
        && words(&mut cursor, &["FROM", "information_schema"])
        && token(&mut cursor, AdminToken::Dot)
        && words(&mut cursor, &["COLUMN_STATISTICS", "WHERE", "SCHEMA_NAME"])
        && token(&mut cursor, AdminToken::Equals)
        && string(&mut cursor).is_some()
        && words(&mut cursor, &["AND", "TABLE_NAME"])
        && token(&mut cursor, AdminToken::Equals)
        && string(&mut cursor).is_some()
        && admin_command_ends(&tokens, cursor);
    if !shaped {
        return Ok(None);
    }
    let start = upper
        .find("JSON_EXTRACT")
        .expect("the shape names JSON_EXTRACT");
    let end = start
        + sql[start..]
            .find(')')
            .expect("the shape closes JSON_EXTRACT with nothing holding a `)` before it");
    Ok(Some(MySqlHistogramQuery {
        reading_column_name: sql[start..=end].to_owned(),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    const MYSQLDUMP_SENDS: &str = "SELECT COLUMN_NAME,                       JSON_EXTRACT(HISTOGRAM, '$.\"number-of-buckets-specified\"')                FROM information_schema.COLUMN_STATISTICS                WHERE SCHEMA_NAME = 'probe' AND TABLE_NAME = 'child'";

    #[test]
    fn reads_what_mysqldump_sends() {
        let query = parse_optional_histogram_query(MYSQLDUMP_SENDS, SessionSqlMode::default())
            .unwrap()
            .unwrap();
        assert_eq!(
            query.reading_column_name(),
            "JSON_EXTRACT(HISTOGRAM, '$.\"number-of-buckets-specified\"')"
        );
    }

    #[test]
    fn leaves_every_other_statement_to_its_own_parser() {
        for sql in [
            "SELECT * FROM information_schema.COLUMN_STATISTICS",
            "SELECT COLUMN_NAME FROM information_schema.COLUMN_STATISTICS WHERE SCHEMA_NAME = 'probe' AND TABLE_NAME = 'child'",
            "SELECT COLUMN_NAME, JSON_EXTRACT(HISTOGRAM, '$.buckets') FROM information_schema.COLUMN_STATISTICS WHERE SCHEMA_NAME = 'probe' AND TABLE_NAME = 'child'",
            "SELECT 1",
        ] {
            assert_eq!(
                parse_optional_histogram_query(sql, SessionSqlMode::default()).unwrap(),
                None,
                "{sql}"
            );
        }
    }
}
