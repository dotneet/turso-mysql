//! A small hand-written tokenizer for the administrative statements.
//!
//! `CREATE DATABASE`, `USE`, `TRUNCATE TABLE` and the transaction commands are
//! not SQL that sqlparser handles the way MySQL does, so they are read straight
//! from the text instead.

use super::*;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TransactionTokenKind {
    Plain,
    /// `START TRANSACTION WITH CONSISTENT SNAPSHOT`, which sqlparser cannot
    /// parse, so the token check answers it rather than handing it on.
    ConsistentSnapshot,
    Invalid,
    Other,
}

pub(crate) fn transaction_token_kind(
    sql: &str,
    mode: SessionSqlMode,
) -> Result<TransactionTokenKind, ParseError> {
    let dialect = SessionMySqlDialect::new(mode);
    let tokens = Tokenizer::new(&dialect, sql)
        .tokenize()
        .map_err(|error| ParseError::Sqlparser(error.to_string()))?;
    let significant = tokens
        .iter()
        .filter(|token| {
            !matches!(
                token,
                Token::Whitespace(Whitespace::Space | Whitespace::Newline | Whitespace::Tab)
            )
        })
        .collect::<Vec<_>>();
    let Some(first_word) = significant
        .iter()
        .find(|token| matches!(token, Token::Word(_)))
    else {
        return Ok(TransactionTokenKind::Other);
    };
    if !["BEGIN", "START", "COMMIT", "ROLLBACK"]
        .iter()
        .any(|keyword| is_unquoted_word(first_word, keyword))
    {
        return Ok(TransactionTokenKind::Other);
    }
    let significant = significant
        .strip_suffix(&[&Token::SemiColon])
        .unwrap_or(&significant);
    let plain = matches!(
        significant,
        [token] if is_unquoted_word(token, "BEGIN")
            || is_unquoted_word(token, "COMMIT")
            || is_unquoted_word(token, "ROLLBACK")
    ) || matches!(
        significant,
        [start, transaction]
            if is_unquoted_word(start, "START")
                && is_unquoted_word(transaction, "TRANSACTION")
    ) || matches!(
        significant,
        [verb, and, chain]
            if (is_unquoted_word(verb, "COMMIT") || is_unquoted_word(verb, "ROLLBACK"))
                && is_unquoted_word(and, "AND")
                && is_unquoted_word(chain, "CHAIN")
    );
    // sqlparser 0.62.0 has no `CONSISTENT SNAPSHOT` in its AST at all, so this
    // shape is answered here and never handed on. `mysqldump
    // --single-transaction` writes it inside the versioned comment
    // `/*!40100 WITH CONSISTENT SNAPSHOT */`, which the tokenizer expands, so
    // both spellings arrive as the same five words.
    if matches!(
        significant,
        [start, transaction, with, consistent, snapshot]
            if is_unquoted_word(start, "START")
                && is_unquoted_word(transaction, "TRANSACTION")
                && is_unquoted_word(with, "WITH")
                && is_unquoted_word(consistent, "CONSISTENT")
                && is_unquoted_word(snapshot, "SNAPSHOT")
    ) {
        return Ok(TransactionTokenKind::ConsistentSnapshot);
    }
    Ok(if plain {
        TransactionTokenKind::Plain
    } else {
        TransactionTokenKind::Invalid
    })
}

/// Parses one strict MySQL database-management command.
///
/// The accepted grammar is exactly one of `CREATE DATABASE name`, `DROP
/// DATABASE name`, `USE name`, or `SHOW DATABASES`, followed by an optional
/// semicolon. Database options, `IF EXISTS` clauses, comments, qualified
/// names, and all trailing tokens are rejected. Names are checked and returned
/// in canonical ASCII-lowercase form.
pub fn parse_admin_command(
    sql: &str,
    mode: SessionSqlMode,
) -> Result<MySqlAdminCommand, ParseError> {
    parse_optional_admin_command(sql, mode)?.ok_or(ParseError::ExpectedAdminCommand)
}

/// Parses one strict database-management command when the statement belongs to
/// this parser's small administration surface.
///
/// Returns `None` for statements outside that surface, such as `SELECT`,
/// `CREATE TABLE`, and `SHOW TABLES`. Once a statement begins one of the
/// supported forms, malformed syntax remains an error rather than falling back
/// to another SQL path.
pub fn parse_optional_admin_command(
    sql: &str,
    mode: SessionSqlMode,
) -> Result<Option<MySqlAdminCommand>, ParseError> {
    let tokens = tokenize_admin_command(sql, mode)?;
    let mut cursor = skip_admin_comments(&tokens, 0);
    let had_leading_comment = cursor != 0;
    let Some(kind) = admin_statement_kind(&tokens, &mut cursor)? else {
        return Ok(None);
    };
    if had_leading_comment {
        return Err(ParseError::Unsupported {
            feature: "comments in database-management command",
        });
    }
    let command = match kind {
        AdminStatementKind::CreateDatabase => MySqlAdminCommand::CreateDatabase {
            name: consume_admin_database_name(&tokens, &mut cursor)?,
        },
        AdminStatementKind::DropDatabase => MySqlAdminCommand::DropDatabase {
            name: consume_admin_database_name(&tokens, &mut cursor)?,
        },
        AdminStatementKind::Use => MySqlAdminCommand::Use {
            name: consume_admin_database_name(&tokens, &mut cursor)?,
        },
        AdminStatementKind::ListDatabases => MySqlAdminCommand::ListDatabases,
    };

    if matches!(tokens.get(cursor), Some(AdminToken::Semicolon)) {
        cursor += 1;
    }
    if cursor != tokens.len() {
        return Err(ParseError::TrailingAdminCommandTokens);
    }
    Ok(Some(command))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum AdminToken {
    Word(String),
    QuotedIdentifier(String),
    /// The decoded contents of a `'...'` string literal.
    StringLiteral(String),
    Semicolon,
    /// The `.` that separates a database from a table.
    Dot,
    /// A `,` separating arguments or limit parameters.
    Comma,
    Comment,
    Other,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AdminStatementKind {
    CreateDatabase,
    DropDatabase,
    Use,
    ListDatabases,
}

pub(crate) fn tokenize_admin_command(
    sql: &str,
    mode: SessionSqlMode,
) -> Result<Vec<AdminToken>, ParseError> {
    let bytes = sql.as_bytes();
    let mut tokens = Vec::new();
    let mut cursor = 0;
    while cursor < bytes.len() {
        let byte = bytes[cursor];
        if byte.is_ascii_whitespace() {
            cursor += 1;
            continue;
        }
        if byte == b'#'
            || (byte == b'-' && bytes.get(cursor + 1) == Some(&b'-'))
            || (byte == b'/' && bytes.get(cursor + 1) == Some(&b'*'))
        {
            tokens.push(AdminToken::Comment);
            cursor = consume_admin_comment(bytes, cursor);
            continue;
        }
        if byte == b';' {
            tokens.push(AdminToken::Semicolon);
            cursor += 1;
            continue;
        }
        if byte == b'`' || (byte == b'"' && mode.ansi_quotes) {
            let quote = byte;
            cursor += 1;
            let mut value = String::new();
            let mut closed = false;
            while cursor < bytes.len() {
                let current = bytes[cursor];
                if current == quote {
                    if bytes.get(cursor + 1) == Some(&quote) {
                        value.push(char::from(quote));
                        cursor += 2;
                    } else {
                        cursor += 1;
                        closed = true;
                        break;
                    }
                } else {
                    value.push(char::from(current));
                    cursor += 1;
                }
            }
            if !closed {
                return Err(ParseError::Sqlparser(
                    "unterminated quoted database name".to_string(),
                ));
            }
            tokens.push(AdminToken::QuotedIdentifier(value));
            continue;
        }
        if byte == b'\'' || byte == b'"' {
            // A double quote only reaches here when ANSI_QUOTES is off, which
            // is exactly when MySQL reads it as a string literal.
            let (value, next) = consume_admin_string_literal(bytes, cursor, mode)?;
            tokens.push(AdminToken::StringLiteral(value));
            cursor = next;
            continue;
        }
        if byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'$' {
            let start = cursor;
            cursor += 1;
            while let Some(next) = bytes.get(cursor) {
                if next.is_ascii_alphanumeric() || *next == b'_' || *next == b'$' {
                    cursor += 1;
                } else {
                    break;
                }
            }
            tokens.push(AdminToken::Word(sql[start..cursor].to_string()));
            continue;
        }
        tokens.push(match bytes[cursor] {
            b'.' => AdminToken::Dot,
            b',' => AdminToken::Comma,
            _ => AdminToken::Other,
        });
        cursor += 1;
    }
    Ok(tokens)
}

/// Reads one MySQL string literal and returns its value and the byte after it.
///
/// `cursor` points at the opening quote. MySQL doubles the quote to include it,
/// and outside `NO_BACKSLASH_ESCAPES` it also takes backslash escapes. `\%` and
/// `\_` keep their backslash so that a later pattern match still sees an escape;
/// every other unlisted escape drops the backslash.
fn consume_admin_string_literal(
    bytes: &[u8],
    cursor: usize,
    mode: SessionSqlMode,
) -> Result<(String, usize), ParseError> {
    let quote = bytes[cursor];
    let mut cursor = cursor + 1;
    let mut value = Vec::new();
    while cursor < bytes.len() {
        let byte = bytes[cursor];
        if byte == quote {
            if bytes.get(cursor + 1) == Some(&quote) {
                value.push(quote);
                cursor += 2;
                continue;
            }
            return Ok((decoded_string_literal(value), cursor + 1));
        }
        if byte == b'\\' && !mode.no_backslash_escapes {
            let Some(escaped) = bytes.get(cursor + 1).copied() else {
                break;
            };
            match escaped {
                b'0' => value.push(0),
                b'b' => value.push(0x08),
                b'n' => value.push(b'\n'),
                b'r' => value.push(b'\r'),
                b't' => value.push(b'\t'),
                b'Z' => value.push(0x1a),
                b'%' | b'_' => value.extend_from_slice(&[b'\\', escaped]),
                other => value.push(other),
            }
            cursor += 2;
            continue;
        }
        value.push(byte);
        cursor += 1;
    }
    Err(ParseError::Sqlparser(
        "unterminated string literal".to_string(),
    ))
}

/// Rebuilds the literal's text from bytes copied out of a `&str`.
///
/// Every byte either came from the source string or is one this decoder wrote,
/// and the decoder only writes ASCII, so the bytes stay valid UTF-8.
fn decoded_string_literal(value: Vec<u8>) -> String {
    String::from_utf8(value).expect("string literal bytes come from a &str")
}

fn consume_admin_comment(bytes: &[u8], cursor: usize) -> usize {
    match bytes[cursor] {
        b'#' => bytes[cursor..]
            .iter()
            .position(|byte| *byte == b'\n' || *byte == b'\r')
            .map_or(bytes.len(), |offset| cursor + offset),
        b'-' => bytes[cursor + 2..]
            .iter()
            .position(|byte| *byte == b'\n' || *byte == b'\r')
            .map_or(bytes.len(), |offset| cursor + 2 + offset),
        b'/' => bytes[cursor + 2..]
            .windows(2)
            .position(|window| window == b"*/")
            .map_or(bytes.len(), |offset| cursor + 2 + offset + 2),
        _ => unreachable!("only comment starters reach this helper"),
    }
}

pub(crate) fn skip_admin_comments(tokens: &[AdminToken], mut cursor: usize) -> usize {
    while matches!(tokens.get(cursor), Some(AdminToken::Comment)) {
        cursor += 1;
    }
    cursor
}

/// Checks that nothing but what MySQL allows follows a catalog command.
///
/// Measured on MySQL 8.4.11: `SHOW TABLES;;`, `SHOW COLUMNS FROM t # x` and
/// `DESCRIBE t -- x` are all accepted, so any number of semicolons and any
/// comments among them end the statement.
pub(crate) fn admin_command_ends(tokens: &[AdminToken], mut cursor: usize) -> bool {
    loop {
        cursor = skip_admin_comments(tokens, cursor);
        match tokens.get(cursor) {
            None => return true,
            Some(AdminToken::Semicolon) => cursor += 1,
            Some(_) => return false,
        }
    }
}

fn admin_statement_kind(
    tokens: &[AdminToken],
    cursor: &mut usize,
) -> Result<Option<AdminStatementKind>, ParseError> {
    if consume_admin_word(tokens, cursor, "CREATE") {
        return admin_database_statement_kind(tokens, cursor, AdminStatementKind::CreateDatabase);
    }
    if consume_admin_word(tokens, cursor, "DROP") {
        return admin_database_statement_kind(tokens, cursor, AdminStatementKind::DropDatabase);
    }
    if consume_admin_word(tokens, cursor, "USE") {
        return Ok(Some(AdminStatementKind::Use));
    }
    if consume_admin_word(tokens, cursor, "SHOW") {
        return admin_database_statement_kind(tokens, cursor, AdminStatementKind::ListDatabases);
    }
    Ok(None)
}

fn admin_database_statement_kind(
    tokens: &[AdminToken],
    cursor: &mut usize,
    kind: AdminStatementKind,
) -> Result<Option<AdminStatementKind>, ParseError> {
    let expected = match kind {
        AdminStatementKind::CreateDatabase | AdminStatementKind::DropDatabase => "DATABASE",
        AdminStatementKind::ListDatabases => "DATABASES",
        AdminStatementKind::Use => unreachable!("USE does not have a second keyword"),
    };
    if consume_admin_word(tokens, cursor, expected) {
        return Ok(Some(kind));
    }
    match tokens.get(*cursor) {
        Some(AdminToken::Word(_)) => Ok(None),
        Some(_) | None => Err(ParseError::ExpectedAdminCommand),
    }
}

pub(crate) fn consume_admin_word(
    tokens: &[AdminToken],
    cursor: &mut usize,
    expected: &str,
) -> bool {
    let Some(AdminToken::Word(word)) = tokens.get(*cursor) else {
        return false;
    };
    if !word.eq_ignore_ascii_case(expected) {
        return false;
    }
    *cursor += 1;
    true
}

pub(crate) fn consume_admin_u64(tokens: &[AdminToken], cursor: &mut usize) -> Option<u64> {
    let AdminToken::Word(word) = tokens.get(*cursor)? else {
        return None;
    };
    let value: u64 = word.parse().ok()?;
    *cursor += 1;
    Some(value)
}

fn consume_admin_database_name(
    tokens: &[AdminToken],
    cursor: &mut usize,
) -> Result<MySqlDatabaseName, ParseError> {
    let token = tokens
        .get(*cursor)
        .ok_or(ParseError::ExpectedAdminCommand)?;
    let name = match token {
        AdminToken::Word(name) => {
            if is_admin_keyword(name) {
                return Err(ParseError::ExpectedAdminCommand);
            }
            name.as_str()
        }
        AdminToken::QuotedIdentifier(name) => name.as_str(),
        AdminToken::StringLiteral(_)
        | AdminToken::Semicolon
        | AdminToken::Dot
        | AdminToken::Comma
        | AdminToken::Comment
        | AdminToken::Other => {
            return Err(ParseError::ExpectedAdminCommand);
        }
    };
    *cursor += 1;
    MySqlDatabaseName::parse(name)
}

pub(crate) fn consume_admin_table_name(
    tokens: &[AdminToken],
    cursor: &mut usize,
) -> Result<MySqlTableName, ParseError> {
    let token = tokens
        .get(*cursor)
        .ok_or(ParseError::ExpectedAdminCommand)?;
    let name = match token {
        AdminToken::Word(name) | AdminToken::QuotedIdentifier(name) => name.as_str(),
        AdminToken::StringLiteral(_)
        | AdminToken::Semicolon
        | AdminToken::Dot
        | AdminToken::Comma
        | AdminToken::Comment
        | AdminToken::Other => {
            return Err(ParseError::ExpectedAdminCommand);
        }
    };
    let name = MySqlTableName::parse(name)?;
    *cursor += 1;
    Ok(name)
}

/// Reads `table` or `database.table`, which MySQL takes wherever it takes a
/// catalog table name.
pub(crate) fn consume_admin_qualified_table_name(
    tokens: &[AdminToken],
    cursor: &mut usize,
) -> Result<(Option<MySqlDatabaseName>, MySqlTableName), ParseError> {
    let first = consume_admin_table_name(tokens, cursor)?;
    if !matches!(tokens.get(*cursor), Some(AdminToken::Dot)) {
        return Ok((None, first));
    }
    *cursor += 1;
    let table = consume_admin_table_name(tokens, cursor)?;
    Ok((Some(MySqlDatabaseName::parse(first.as_str())?), table))
}

fn is_admin_keyword(word: &str) -> bool {
    matches!(
        word.to_ascii_uppercase().as_str(),
        "CREATE"
            | "DATABASE"
            | "DROP"
            | "USE"
            | "SHOW"
            | "DATABASES"
            | "SCHEMA"
            | "IF"
            | "NOT"
            | "EXISTS"
            | "CHARACTER"
            | "SET"
            | "COLLATE"
            | "ENCRYPTION"
            | "COMMENT"
            | "READ"
            | "ONLY"
    )
}
