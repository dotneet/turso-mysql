//! Which statements a read-only session or transaction refuses.

use crate::admin_command::{tokenize_versioned_admin_command, AdminToken};
use crate::SessionSqlMode;

/// What a statement may change, which decides whose access mode it answers
/// to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StatementWrites {
    /// Nothing: it runs in a read-only session or transaction.
    Nothing,
    /// Rows or temporary tables, inside the transaction it runs in, so the
    /// transaction's access mode decides, or the session's outside one.
    InTheTransaction,
    /// The schema or the accounts, after ending the transaction it runs in,
    /// so the session's access mode decides.
    AfterCommitting,
    /// A statement this does not know, refused if either mode is read-only
    /// rather than let through.
    Unknown,
}

/// Reads what a statement may change.
///
/// Measured on MySQL 8.4.11. Under `SET SESSION TRANSACTION READ ONLY`, every
/// `INSERT`, `UPDATE`, `DELETE` and `REPLACE`, every DDL statement,
/// `CREATE TEMPORARY TABLE`, `CREATE USER`, `OPTIMIZE TABLE`, `EXPLAIN UPDATE`,
/// a `WITH` over an `UPDATE`, `LOCK TABLES ... WRITE` and
/// `SELECT ... FOR UPDATE` answer 1792, while `SELECT` — with `FOR SHARE`,
/// `LOCK IN SHARE MODE` or `INTO @name` too — a `WITH` over a `SELECT`,
/// `TABLE`, `VALUES`, `SHOW`, `DESCRIBE`, `EXPLAIN SELECT`, `SET`, `USE`, `DO`,
/// `CHECK TABLE`, `FLUSH TABLES`, `LOCK TABLES ... READ`, `SAVEPOINT` and the
/// transaction statements run. Inside a `START TRANSACTION READ ONLY` of a
/// read-write session, DDL, `CREATE USER`, `OPTIMIZE TABLE`,
/// `ANALYZE TABLE` and `LOCK TABLES ... WRITE` run, each ending the
/// transaction first, while `CREATE TEMPORARY TABLE`, `DROP TEMPORARY TABLE`
/// and `SELECT ... FOR UPDATE` answer 1792.
pub fn what_a_statement_writes(sql: &str, mode: SessionSqlMode) -> StatementWrites {
    let Ok(tokens) = tokenize_versioned_admin_command(sql, mode) else {
        return StatementWrites::Unknown;
    };
    let mut tokens = tokens
        .into_iter()
        .filter(|token| !matches!(token, AdminToken::Comment))
        .collect::<Vec<_>>();
    while tokens.last() == Some(&AdminToken::Semicolon) {
        tokens.pop();
    }
    if tokens.contains(&AdminToken::Semicolon) {
        return StatementWrites::Unknown;
    }
    let Some(first) = tokens.iter().find_map(|token| match token {
        AdminToken::LeftParen => None,
        AdminToken::Word(word) => Some(Some(word.to_ascii_uppercase())),
        _ => Some(None),
    }) else {
        return StatementWrites::Nothing;
    };
    let Some(first) = first else {
        return StatementWrites::Unknown;
    };
    let writes_rows_when = |writes: bool| {
        if writes {
            StatementWrites::InTheTransaction
        } else {
            StatementWrites::Nothing
        }
    };
    match first.as_str() {
        "SELECT" | "TABLE" | "VALUES" => writes_rows_when(locks_rows_to_update(&tokens)),
        "WITH" | "EXPLAIN" | "DESCRIBE" | "DESC" => {
            writes_rows_when(writes_at_the_top(&tokens[1..]) || locks_rows_to_update(&tokens))
        }
        "INSERT" | "UPDATE" | "DELETE" | "REPLACE" => StatementWrites::InTheTransaction,
        "CREATE" | "DROP" if word_at(&tokens, 1).as_deref() == Some("TEMPORARY") => {
            StatementWrites::InTheTransaction
        }
        "CREATE" | "ALTER" | "DROP" | "TRUNCATE" | "RENAME" | "OPTIMIZE" | "ANALYZE" | "GRANT"
        | "REVOKE" => StatementWrites::AfterCommitting,
        "LOCK" if has_word(&tokens, "WRITE") => StatementWrites::AfterCommitting,
        "SET" if has_word(&tokens, "PASSWORD") => StatementWrites::Unknown,
        "FLUSH" if !matches!(word_at(&tokens, 1).as_deref(), Some("TABLE" | "TABLES")) => {
            StatementWrites::Unknown
        }
        "SHOW" | "SET" | "USE" | "DO" | "HELP" | "CHECK" | "FLUSH" | "LOCK" | "UNLOCK"
        | "BEGIN" | "START" | "COMMIT" | "ROLLBACK" | "SAVEPOINT" | "RELEASE" => {
            StatementWrites::Nothing
        }
        _ => StatementWrites::Unknown,
    }
}

fn locks_rows_to_update(tokens: &[AdminToken]) -> bool {
    tokens.windows(2).any(|pair| {
        matches!(
            pair,
            [AdminToken::Word(first), AdminToken::Word(second)]
                if first.eq_ignore_ascii_case("FOR") && second.eq_ignore_ascii_case("UPDATE")
        )
    })
}

/// Whether a word that begins a write stands outside every parenthesis, as
/// the `UPDATE` after a `WITH` or an `EXPLAIN` does.
fn writes_at_the_top(tokens: &[AdminToken]) -> bool {
    let mut depth = 0usize;
    for token in tokens {
        match token {
            AdminToken::LeftParen => depth += 1,
            AdminToken::RightParen => depth = depth.saturating_sub(1),
            AdminToken::Word(word) if depth == 0 => {
                if ["INSERT", "UPDATE", "DELETE", "REPLACE"]
                    .iter()
                    .any(|write| word.eq_ignore_ascii_case(write))
                {
                    return true;
                }
            }
            _ => {}
        }
    }
    false
}

fn has_word(tokens: &[AdminToken], wanted: &str) -> bool {
    tokens
        .iter()
        .any(|token| matches!(token, AdminToken::Word(word) if word.eq_ignore_ascii_case(wanted)))
}

fn word_at(tokens: &[AdminToken], index: usize) -> Option<String> {
    match tokens.get(index) {
        Some(AdminToken::Word(word)) => Some(word.to_ascii_uppercase()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_what_each_statement_writes_as_mysql_does() {
        let mode = SessionSqlMode::default();
        let cases: &[(StatementWrites, &[&str])] = &[
            (
                StatementWrites::Nothing,
                &[
                    "SELECT * FROM t",
                    "select * from t for share",
                    "SELECT * FROM t LOCK IN SHARE MODE",
                    "SELECT 1 INTO @a",
                    "(SELECT 1) UNION (SELECT 2)",
                    "WITH c AS (SELECT 1) SELECT * FROM c",
                    "TABLE t",
                    "VALUES ROW(1)",
                    "SHOW TABLES",
                    "DESCRIBE t",
                    "EXPLAIN SELECT * FROM t",
                    "SET @x = 1",
                    "SET SESSION TRANSACTION READ WRITE",
                    "USE probe",
                    "DO 1",
                    "CHECK TABLE t",
                    "FLUSH TABLES",
                    "LOCK TABLES t READ",
                    "UNLOCK TABLES",
                    "BEGIN",
                    "START TRANSACTION READ WRITE",
                    "COMMIT",
                    "ROLLBACK",
                    "SAVEPOINT s",
                    "RELEASE SAVEPOINT s",
                    "SELECT `update` FROM t;",
                    "/* a comment */ SELECT 1",
                    "",
                ],
            ),
            (
                StatementWrites::InTheTransaction,
                &[
                    "INSERT INTO t VALUES (1)",
                    "update t set v = 1",
                    "DELETE FROM t",
                    "REPLACE INTO t VALUES (1)",
                    "CREATE TEMPORARY TABLE tt (id INT)",
                    "DROP TEMPORARY TABLE tt",
                    "EXPLAIN UPDATE t SET v = 1",
                    "WITH c AS (SELECT 1 AS a) UPDATE t SET v = 1 WHERE id IN (SELECT a FROM c)",
                    "SELECT * FROM t FOR UPDATE",
                    "select * from t where id = 1 for update skip locked",
                ],
            ),
            (
                StatementWrites::AfterCommitting,
                &[
                    "CREATE TABLE u (id INT)",
                    "ALTER TABLE t ADD COLUMN w INT",
                    "DROP TABLE t",
                    "TRUNCATE TABLE t",
                    "RENAME TABLE t TO u",
                    "CREATE INDEX i ON t (v)",
                    "CREATE DATABASE d",
                    "CREATE USER u IDENTIFIED BY 'x'",
                    "OPTIMIZE TABLE t",
                    "ANALYZE TABLE t",
                    "LOCK TABLES t WRITE",
                    "/*!40000 ALTER TABLE t DISABLE KEYS */",
                ],
            ),
            (
                StatementWrites::Unknown,
                &[
                    "SET PASSWORD = 'x'",
                    "FLUSH PRIVILEGES",
                    "SELECT 1; DELETE FROM t",
                    "CALL p()",
                    "LOAD DATA INFILE 'x' INTO TABLE t",
                ],
            ),
        ];
        for (expected, statements) in cases {
            for sql in *statements {
                assert_eq!(what_a_statement_writes(sql, mode), *expected, "{sql}");
            }
        }
    }
}
