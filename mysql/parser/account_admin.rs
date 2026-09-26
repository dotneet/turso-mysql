//! The account SQL surface supported by the persistent account store.

use std::fmt;

use zeroize::{Zeroize, Zeroizing};

use super::{
    admin_command::{consume_admin_word, skip_admin_comments, tokenize_admin_command, AdminToken},
    MySqlDatabaseName, MySqlTableName, ParseError, SessionSqlMode,
};

/// Password text decoded from `CREATE USER ... IDENTIFIED BY`.
///
/// The SQL input remains owned by the caller. This value wipes its own decoded
/// copy when it is dropped and hides it from debug output.
pub struct AccountAdminPassword(Zeroizing<Vec<u8>>);

impl AccountAdminPassword {
    pub fn as_str(&self) -> &str {
        std::str::from_utf8(self.0.as_slice()).expect("decoded SQL password is UTF-8")
    }

    pub fn as_mut_bytes(&mut self) -> &mut [u8] {
        self.0.as_mut_slice()
    }
}

impl fmt::Debug for AccountAdminPassword {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("<redacted>")
    }
}

/// One account mutation that the current account store can represent exactly.
#[derive(Debug)]
pub enum MySqlAccountAdminCommand {
    CreateUser {
        username: String,
        password: AccountAdminPassword,
    },
    GrantTableSelect {
        username: String,
        database: MySqlDatabaseName,
        table: MySqlTableName,
    },
    RevokeTableSelect {
        username: String,
        database: MySqlDatabaseName,
        table: MySqlTableName,
    },
}

struct AccountAdminTokens(Vec<AdminToken>);

impl Drop for AccountAdminTokens {
    fn drop(&mut self) {
        for token in &mut self.0 {
            match token {
                AdminToken::Word(value)
                | AdminToken::QuotedIdentifier(value)
                | AdminToken::StringLiteral(value) => value.zeroize(),
                _ => {}
            }
        }
    }
}

/// Parses the narrow account SQL forms when the statement starts with one.
///
/// Only the host `@'%'` is accepted because account identities in the store
/// are keyed by username, without a host component. A command must be a single
/// statement, with no comments or extra clauses.
pub fn parse_optional_account_admin_command(
    sql: &str,
    mode: SessionSqlMode,
) -> Result<Option<MySqlAccountAdminCommand>, ParseError> {
    let mut tokens = AccountAdminTokens(tokenize_admin_command(sql, mode)?);
    let first = skip_admin_comments(&tokens.0, 0);
    let kind = match tokens.0.get(first) {
        Some(AdminToken::Word(word)) if word.eq_ignore_ascii_case("CREATE") => {
            let second = skip_admin_comments(&tokens.0, first + 1);
            match tokens.0.get(second) {
                Some(AdminToken::Word(word)) if word.eq_ignore_ascii_case("USER") => {
                    Some(AccountCommandKind::CreateUser)
                }
                _ => None,
            }
        }
        Some(AdminToken::Word(word)) if word.eq_ignore_ascii_case("GRANT") => {
            Some(AccountCommandKind::Grant)
        }
        Some(AdminToken::Word(word)) if word.eq_ignore_ascii_case("REVOKE") => {
            Some(AccountCommandKind::Revoke)
        }
        _ => None,
    };
    let Some(kind) = kind else {
        return Ok(None);
    };
    if tokens
        .0
        .iter()
        .any(|token| matches!(token, AdminToken::Comment))
    {
        return Err(ParseError::ExpectedAccountAdminCommand);
    }

    let mut cursor = 0;
    let command = match kind {
        AccountCommandKind::CreateUser => {
            expect_word(&tokens.0, &mut cursor, "CREATE")?;
            expect_word(&tokens.0, &mut cursor, "USER")?;
            let username = account_username(&tokens.0, &mut cursor)?;
            expect_host(&tokens.0, &mut cursor)?;
            expect_word(&tokens.0, &mut cursor, "IDENTIFIED")?;
            expect_word(&tokens.0, &mut cursor, "BY")?;
            let password = match tokens.0.get_mut(cursor) {
                Some(AdminToken::StringLiteral(value)) => {
                    AccountAdminPassword(Zeroizing::new(std::mem::take(value).into_bytes()))
                }
                _ => return Err(ParseError::ExpectedAccountAdminCommand),
            };
            cursor += 1;
            MySqlAccountAdminCommand::CreateUser { username, password }
        }
        AccountCommandKind::Grant | AccountCommandKind::Revoke => {
            let (verb, recipient_word) = match kind {
                AccountCommandKind::Grant => ("GRANT", "TO"),
                AccountCommandKind::Revoke => ("REVOKE", "FROM"),
                AccountCommandKind::CreateUser => unreachable!(),
            };
            expect_word(&tokens.0, &mut cursor, verb)?;
            expect_word(&tokens.0, &mut cursor, "SELECT")?;
            expect_word(&tokens.0, &mut cursor, "ON")?;
            let database = database_name(&tokens.0, &mut cursor)?;
            expect_token(&tokens.0, &mut cursor, AdminToken::Dot)?;
            let table = table_name(&tokens.0, &mut cursor)?;
            expect_word(&tokens.0, &mut cursor, recipient_word)?;
            let username = account_username(&tokens.0, &mut cursor)?;
            expect_host(&tokens.0, &mut cursor)?;
            match kind {
                AccountCommandKind::Grant => MySqlAccountAdminCommand::GrantTableSelect {
                    username,
                    database,
                    table,
                },
                AccountCommandKind::Revoke => MySqlAccountAdminCommand::RevokeTableSelect {
                    username,
                    database,
                    table,
                },
                AccountCommandKind::CreateUser => unreachable!(),
            }
        }
    };

    if matches!(tokens.0.get(cursor), Some(AdminToken::Semicolon)) {
        cursor += 1;
    }
    if cursor != tokens.0.len() {
        return Err(ParseError::TrailingAccountAdminCommandTokens);
    }
    Ok(Some(command))
}

#[derive(Clone, Copy)]
enum AccountCommandKind {
    CreateUser,
    Grant,
    Revoke,
}

fn expect_word(tokens: &[AdminToken], cursor: &mut usize, word: &str) -> Result<(), ParseError> {
    if consume_admin_word(tokens, cursor, word) {
        Ok(())
    } else {
        Err(ParseError::ExpectedAccountAdminCommand)
    }
}

fn expect_token(
    tokens: &[AdminToken],
    cursor: &mut usize,
    expected: AdminToken,
) -> Result<(), ParseError> {
    if tokens.get(*cursor) == Some(&expected) {
        *cursor += 1;
        Ok(())
    } else {
        Err(ParseError::ExpectedAccountAdminCommand)
    }
}

fn account_username(tokens: &[AdminToken], cursor: &mut usize) -> Result<String, ParseError> {
    let Some(AdminToken::StringLiteral(value)) = tokens.get(*cursor) else {
        return Err(ParseError::ExpectedAccountAdminCommand);
    };
    if value.is_empty() {
        return Err(ParseError::InvalidAccountUsername { reason: "empty" });
    }
    if value.len() > u8::MAX as usize {
        return Err(ParseError::InvalidAccountUsername {
            reason: "longer than 255 bytes",
        });
    }
    if value.as_bytes().contains(&0) {
        return Err(ParseError::InvalidAccountUsername { reason: "NUL byte" });
    }
    *cursor += 1;
    Ok(value.clone())
}

fn expect_host(tokens: &[AdminToken], cursor: &mut usize) -> Result<(), ParseError> {
    expect_token(tokens, cursor, AdminToken::At)?;
    match tokens.get(*cursor) {
        Some(AdminToken::StringLiteral(host)) if host == "%" => {
            *cursor += 1;
            Ok(())
        }
        _ => Err(ParseError::UnsupportedAccountHost),
    }
}

fn database_name(
    tokens: &[AdminToken],
    cursor: &mut usize,
) -> Result<MySqlDatabaseName, ParseError> {
    let name = match tokens.get(*cursor) {
        Some(AdminToken::Word(name) | AdminToken::QuotedIdentifier(name)) => name,
        _ => return Err(ParseError::ExpectedAccountAdminCommand),
    };
    *cursor += 1;
    MySqlDatabaseName::parse(name)
}

fn table_name(tokens: &[AdminToken], cursor: &mut usize) -> Result<MySqlTableName, ParseError> {
    let name = match tokens.get(*cursor) {
        Some(AdminToken::Word(name) | AdminToken::QuotedIdentifier(name)) => name,
        _ => return Err(ParseError::ExpectedAccountAdminCommand),
    };
    *cursor += 1;
    MySqlTableName::parse(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_create_user_and_redacts_password() {
        let command = parse_optional_account_admin_command(
            "CREATE USER 'Alice'@'%' IDENTIFIED BY 's''ecret';",
            SessionSqlMode::default(),
        )
        .unwrap()
        .unwrap();
        assert!(!format!("{command:?}").contains("s'ecret"));
        let MySqlAccountAdminCommand::CreateUser { username, password } = command else {
            panic!("expected CREATE USER");
        };
        assert_eq!(username, "Alice");
        assert_eq!(password.as_str(), "s'ecret");
        assert_eq!(format!("{password:?}"), "<redacted>");
    }

    #[test]
    fn preserves_multibyte_password_after_backslash() {
        let command = parse_optional_account_admin_command(
            "CREATE USER 'alice'@'%' IDENTIFIED BY '\\é'",
            SessionSqlMode::default(),
        )
        .unwrap()
        .unwrap();
        let MySqlAccountAdminCommand::CreateUser { password, .. } = command else {
            panic!("expected CREATE USER");
        };
        assert_eq!(password.as_str(), "é");
    }

    #[test]
    fn parses_grant_and_revoke_with_canonical_table_names() {
        for (sql, grant) in [
            ("GRANT SELECT ON `App`.`Records` TO 'bob'@'%'", true),
            ("REVOKE SELECT ON App.Records FROM 'bob'@'%'", false),
        ] {
            let command = parse_optional_account_admin_command(sql, SessionSqlMode::default())
                .unwrap()
                .unwrap();
            let (username, database, table) = match command {
                MySqlAccountAdminCommand::GrantTableSelect {
                    username,
                    database,
                    table,
                } if grant => (username, database, table),
                MySqlAccountAdminCommand::RevokeTableSelect {
                    username,
                    database,
                    table,
                } if !grant => (username, database, table),
                other => panic!("unexpected command: {other:?}"),
            };
            assert_eq!(username, "bob");
            assert_eq!(database.as_str(), "app");
            assert_eq!(table.as_str(), "records");
        }
    }

    #[test]
    fn rejects_hosts_extra_statements_comments_and_unsupported_rights() {
        for sql in [
            "CREATE USER 'alice'@'localhost' IDENTIFIED BY 'pw'",
            "CREATE USER 'alice' IDENTIFIED BY 'pw'",
            "CREATE USER 'alice'@'%' IDENTIFIED WITH caching_sha2_password BY 'pw'",
            "CREATE USER 'alice'@'%' IDENTIFIED BY 'pw'; SELECT 1",
            "CREATE /* comment */ USER 'alice'@'%' IDENTIFIED BY 'pw'",
            "GRANT SELECT ON app.* TO 'alice'@'%'",
            "GRANT INSERT ON app.t TO 'alice'@'%'",
            "GRANT SELECT ON app.t TO 'alice'@'localhost'",
            "GRANT SELECT ON app.t TO 'alice'@'%'; REVOKE SELECT ON app.t FROM 'alice'@'%'",
            "REVOKE SELECT ON app.t FROM 'alice'@'%' WITH GRANT OPTION",
        ] {
            assert!(
                parse_optional_account_admin_command(sql, SessionSqlMode::default()).is_err(),
                "unexpectedly accepted {sql}"
            );
        }
        assert!(parse_optional_account_admin_command(
            "CREATE TABLE t (id INT)",
            SessionSqlMode::default()
        )
        .unwrap()
        .is_none());
    }
}
