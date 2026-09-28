//! A small hand-written tokenizer for the administrative statements.
//!
//! `CREATE DATABASE`, `USE`, `TRUNCATE TABLE` and the transaction commands are
//! not SQL that sqlparser handles the way MySQL does, so they are read straight
//! from the text instead.

use super::database_options::consume_database_options;
use super::*;
use crate::statement_reads;

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
    let tokens = statement_reads::tokens(&dialect, sql)
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
        [start, transaction, read, mode]
            if is_unquoted_word(start, "START")
                && is_unquoted_word(transaction, "TRANSACTION")
                && is_unquoted_word(read, "READ")
                && (is_unquoted_word(mode, "ONLY") || is_unquoted_word(mode, "WRITE"))
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

/// Reads one savepoint statement.
///
/// `SAVEPOINT`, `ROLLBACK TO [SAVEPOINT]` and `RELEASE SAVEPOINT` each name a
/// savepoint, so they are read here rather than through the plain transaction
/// tokens. A bare `ROLLBACK` is left alone: reading one as the other would end
/// a transaction MySQL keeps open.
pub(crate) fn savepoint_command(
    sql: &str,
    mode: SessionSqlMode,
) -> Result<Option<MySqlTransactionCommand>, ParseError> {
    let dialect = SessionMySqlDialect::new(mode);
    let tokens = statement_reads::tokens(&dialect, sql)
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
    let significant = significant
        .strip_suffix(&[&Token::SemiColon])
        .unwrap_or(&significant);
    let Some(first) = significant.first() else {
        return Ok(None);
    };
    if is_unquoted_word(first, "SAVEPOINT") {
        let name = checked_savepoint_name(&significant[1..])?;
        return Ok(Some(MySqlTransactionCommand::Savepoint(name)));
    }
    // MySQL spells this one with the keyword and nothing else: measured on
    // 8.4.11, `RELEASE s1` answers 1064.
    if is_unquoted_word(first, "RELEASE") {
        let [savepoint, rest @ ..] = &significant[1..] else {
            return Err(ParseError::ExpectedTransactionCommand);
        };
        if !is_unquoted_word(savepoint, "SAVEPOINT") {
            return Err(ParseError::ExpectedTransactionCommand);
        }
        let name = checked_savepoint_name(rest)?;
        return Ok(Some(MySqlTransactionCommand::ReleaseSavepoint(name)));
    }
    if is_unquoted_word(first, "ROLLBACK") {
        let [to, rest @ ..] = &significant[1..] else {
            return Ok(None);
        };
        if !is_unquoted_word(to, "TO") {
            return Ok(None);
        }
        // The `SAVEPOINT` keyword is optional after `TO`.
        let rest = match rest {
            [savepoint, rest @ ..] if is_unquoted_word(savepoint, "SAVEPOINT") => rest,
            rest => rest,
        };
        let name = checked_savepoint_name(rest)?;
        return Ok(Some(MySqlTransactionCommand::RollbackToSavepoint(name)));
    }
    Ok(None)
}

/// Reads the one token that names a savepoint.
fn checked_savepoint_name(tokens: &[&Token]) -> Result<String, ParseError> {
    let [Token::Word(word)] = tokens else {
        return Err(ParseError::InvalidSavepointName {
            reason: "expected one name",
        });
    };
    // A backtick is MySQL's own quoting and an unquoted word is the ordinary
    // spelling. Anything else — a string literal, a number — is not a name.
    if !matches!(word.quote_style, None | Some('`')) {
        return Err(ParseError::InvalidSavepointName {
            reason: "quoted with something other than a backtick",
        });
    }
    checked_savepoint_identifier(&word.value)
}

/// Validates and canonicalizes one savepoint name.
///
/// Measured on MySQL 8.4.11: savepoint names are matched whatever their case,
/// so `ROLLBACK TO S1` finds the savepoint `s1`, and a backquoted name may
/// hold any other character — Sequelize names a nested transaction
/// `` `09189cf0-191c-4e19-8090-a15ad6701cc0-sp-1` ``, and `` `a b.c!` `` and
/// `` `x``y` `` are taken too. The engine matches ASCII letters without regard
/// to case the same way, and the name is lowercased here so the two agree
/// about a name this frontend has already canonicalized. A letter outside
/// ASCII, whose case MySQL folds by rules not measured here, is refused, and
/// so is a space at either end.
fn checked_savepoint_identifier(name: &str) -> Result<String, ParseError> {
    if name.is_empty() {
        return Err(ParseError::InvalidSavepointName { reason: "empty" });
    }
    if name.len() > 64 {
        return Err(ParseError::InvalidSavepointName {
            reason: "longer than 64 bytes",
        });
    }
    if name.starts_with(' ') || name.ends_with(' ') {
        return Err(ParseError::InvalidSavepointName {
            reason: "space at an end",
        });
    }
    let mut canonical = String::with_capacity(name.len());
    for byte in name.bytes() {
        let byte = match byte {
            b'A'..=b'Z' => byte.to_ascii_lowercase(),
            b' '..=b'~' => byte,
            _ => {
                return Err(ParseError::InvalidSavepointName {
                    reason: "character outside printable ASCII",
                });
            }
        };
        canonical.push(char::from(byte));
    }
    Ok(canonical)
}

/// Parses one strict MySQL database-management command.
///
/// The accepted grammar is exactly one of `CREATE DATABASE [IF NOT EXISTS]
/// name [options]`, `ALTER DATABASE [name] options`, `DROP DATABASE [IF
/// EXISTS] name`, `SHOW CREATE DATABASE [IF NOT EXISTS] name`, `USE name`, or `SHOW
/// DATABASES [LIKE 'pattern']`, `SCHEMA` standing for `DATABASE` in each, followed by an
/// optional semicolon. A versioned comment MySQL runs is read as the text it
/// holds, which is how `mysqldump` writes its `CREATE DATABASE`. Database
/// options naming a character set, collation or encryption a database here
/// cannot keep, `CREATE DATABASE IF EXISTS`, other comments, qualified names, and all
/// trailing tokens are rejected. Names are checked and returned in canonical
/// ASCII-lowercase form.
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
    let tokens = tokenize_versioned_admin_command(sql, mode)?;
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
        AdminStatementKind::CreateDatabase => {
            let only_if_missing = consume_admin_word(&tokens, &mut cursor, "IF");
            if only_if_missing
                && !(consume_admin_word(&tokens, &mut cursor, "NOT")
                    && consume_admin_word(&tokens, &mut cursor, "EXISTS"))
            {
                return Err(ParseError::ExpectedAdminCommand);
            }
            let name = consume_admin_database_name(&tokens, &mut cursor)?;
            let collation = consume_database_options(&tokens, &mut cursor)?;
            MySqlAdminCommand::CreateDatabase {
                name,
                only_if_missing,
                collation: collation.unwrap_or_default(),
            }
        }
        AdminStatementKind::AlterDatabase => {
            let name = match tokens.get(cursor) {
                Some(AdminToken::Word(word)) if !starts_a_database_option(word) => {
                    Some(consume_admin_database_name(&tokens, &mut cursor)?)
                }
                Some(AdminToken::QuotedIdentifier(_)) => {
                    Some(consume_admin_database_name(&tokens, &mut cursor)?)
                }
                _ => None,
            };
            let options_start = cursor;
            let collation = consume_database_options(&tokens, &mut cursor)?;
            // Measured on MySQL 8.4.11: `READ ONLY = 1` makes every table of
            // the database refuse writes, which this server does not do.
            if consume_admin_word(&tokens, &mut cursor, "READ") {
                return Err(ParseError::Unsupported {
                    feature: "ALTER DATABASE READ ONLY",
                });
            }
            // Measured on MySQL 8.4.11: an `ALTER DATABASE` naming no option
            // at all is 1064.
            if cursor == options_start {
                return Err(ParseError::ExpectedAdminCommand);
            }
            MySqlAdminCommand::AlterDatabase { name, collation }
        }
        AdminStatementKind::DropDatabase => {
            let only_if_present = consume_admin_word(&tokens, &mut cursor, "IF");
            if only_if_present && !consume_admin_word(&tokens, &mut cursor, "EXISTS") {
                return Err(ParseError::ExpectedAdminCommand);
            }
            MySqlAdminCommand::DropDatabase {
                name: consume_admin_database_name(&tokens, &mut cursor)?,
                only_if_present,
            }
        }
        AdminStatementKind::ShowCreateDatabase => {
            let only_if_missing = consume_admin_word(&tokens, &mut cursor, "IF");
            if only_if_missing
                && !(consume_admin_word(&tokens, &mut cursor, "NOT")
                    && consume_admin_word(&tokens, &mut cursor, "EXISTS"))
            {
                return Err(ParseError::ExpectedAdminCommand);
            }
            // Measured on MySQL 8.4.11 with `lower_case_table_names=1`, the
            // rule this server follows: `SHOW CREATE DATABASE MIXEDDB` finds
            // `mixeddb` and prints the name as it was written.
            let written_name = match tokens.get(cursor) {
                Some(AdminToken::Word(name) | AdminToken::QuotedIdentifier(name)) => name.clone(),
                _ => return Err(ParseError::ExpectedAdminCommand),
            };
            MySqlAdminCommand::ShowCreateDatabase {
                name: consume_admin_database_name(&tokens, &mut cursor)?,
                written_name,
                only_if_missing,
            }
        }
        AdminStatementKind::Use => MySqlAdminCommand::Use {
            name: consume_admin_database_name(&tokens, &mut cursor)?,
        },
        AdminStatementKind::ListDatabases => {
            if consume_admin_word(&tokens, &mut cursor, "LIKE") {
                let Some(AdminToken::StringLiteral(pattern)) = tokens.get(cursor) else {
                    return Err(ParseError::ExpectedAdminCommand);
                };
                cursor += 1;
                MySqlAdminCommand::ListDatabasesLike {
                    pattern: MySqlLikePattern::new(pattern, mode),
                }
            } else {
                MySqlAdminCommand::ListDatabases
            }
        }
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
    LeftParen,
    RightParen,
    Star,
    At,
    Equals,
    Comment,
    Other,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AdminStatementKind {
    CreateDatabase,
    AlterDatabase,
    DropDatabase,
    ShowCreateDatabase,
    Use,
    ListDatabases,
}

pub(crate) fn tokenize_admin_command(
    sql: &str,
    mode: SessionSqlMode,
) -> Result<Vec<AdminToken>, ParseError> {
    tokenize_admin_text(sql, mode, VersionedComments::Kept)
}

/// Tokenizes a statement reading every versioned comment MySQL would run as
/// the text it holds.
pub(crate) fn tokenize_versioned_admin_command(
    sql: &str,
    mode: SessionSqlMode,
) -> Result<Vec<AdminToken>, ParseError> {
    tokenize_admin_text(sql, mode, VersionedComments::Expanded)
}

pub(crate) fn tokenize_lock_tables_command(
    sql: &str,
    mode: SessionSqlMode,
) -> Result<Vec<AdminToken>, ParseError> {
    tokenize_admin_text(sql, mode, VersionedComments::MysqldumpLocal)
}

/// What the tokenizer makes of a `/*!NNNNN ... */` comment, which MySQL runs
/// on any server at or past the version it names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum VersionedComments {
    /// Every comment is one [`AdminToken::Comment`].
    Kept,
    /// `/*!32311 LOCAL */`, which `mysqldump` writes into `LOCK TABLES`, is
    /// the word it holds; every other comment is kept.
    MysqldumpLocal,
    /// Every versioned comment this server's version runs is read as the text
    /// it holds.
    Expanded,
}

/// The version `/*!NNNNN ... */` is compared with: MySQL 8.4.11, the one this
/// server answers as.
const MYSQL_VERSION_ID: u32 = 80_411;

fn tokenize_admin_text(
    sql: &str,
    mode: SessionSqlMode,
    versioned_comments: VersionedComments,
) -> Result<Vec<AdminToken>, ParseError> {
    statement_reads::admin_tokens(sql, mode, versioned_comments, || {
        read_admin_text(sql, mode, versioned_comments)
    })
}

fn read_admin_text(
    sql: &str,
    mode: SessionSqlMode,
    versioned_comments: VersionedComments,
) -> Result<Vec<AdminToken>, ParseError> {
    let bytes = sql.as_bytes();
    let mut tokens = Vec::new();
    let mut cursor = 0;
    let mut inside_versioned_comment = false;
    while cursor < bytes.len() {
        let byte = bytes[cursor];
        if byte.is_ascii_whitespace() {
            cursor += 1;
            continue;
        }
        if inside_versioned_comment && sql[cursor..].starts_with("*/") {
            inside_versioned_comment = false;
            cursor += 2;
            continue;
        }
        if byte == b'#'
            || (byte == b'-' && bytes.get(cursor + 1) == Some(&b'-'))
            || (byte == b'/' && bytes.get(cursor + 1) == Some(&b'*'))
        {
            const MYSQLDUMP_LOCAL: &str = "/*!32311 LOCAL */";
            if versioned_comments == VersionedComments::MysqldumpLocal
                && sql[cursor..].starts_with(MYSQLDUMP_LOCAL)
            {
                tokens.push(AdminToken::Word("LOCAL".to_owned()));
                cursor += MYSQLDUMP_LOCAL.len();
                continue;
            }
            if versioned_comments == VersionedComments::Expanded && !inside_versioned_comment {
                if let Some(body) = versioned_comment_body(sql, cursor) {
                    inside_versioned_comment = true;
                    cursor = body;
                    continue;
                }
            }
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
            b'(' => AdminToken::LeftParen,
            b')' => AdminToken::RightParen,
            b'*' => AdminToken::Star,
            b'@' => AdminToken::At,
            b'=' => AdminToken::Equals,
            _ => AdminToken::Other,
        });
        cursor += 1;
    }
    if inside_versioned_comment {
        return Err(ParseError::Sqlparser(
            "unterminated versioned comment".to_string(),
        ));
    }
    Ok(tokens)
}

/// Returns where the text of a versioned comment starting at `cursor` begins,
/// when it is one this server's version runs.
///
/// MySQL writes the version as five digits, or none for a comment every
/// version runs.
fn versioned_comment_body(sql: &str, cursor: usize) -> Option<usize> {
    let after_mark = cursor + sql[cursor..].strip_prefix("/*!").map(|_| 3)?;
    let digits = sql[after_mark..]
        .bytes()
        .take_while(u8::is_ascii_digit)
        .count();
    match digits {
        0 => Some(after_mark),
        5 => {
            let version: u32 = sql[after_mark..after_mark + 5]
                .parse()
                .expect("five ASCII digits are a number");
            (version <= MYSQL_VERSION_ID).then_some(after_mark + 5)
        }
        _ => None,
    }
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
    let mut value = zeroize::Zeroizing::new(Vec::new());
    while cursor < bytes.len() {
        let byte = bytes[cursor];
        if byte == quote {
            if bytes.get(cursor + 1) == Some(&quote) {
                value.push(quote);
                cursor += 2;
                continue;
            }
            return Ok((
                decoded_string_literal(std::mem::take(&mut *value)),
                cursor + 1,
            ));
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
                0x80..=u8::MAX => {
                    cursor += 1;
                    continue;
                }
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
    if consume_admin_word(tokens, cursor, "ALTER") {
        return admin_database_statement_kind(tokens, cursor, AdminStatementKind::AlterDatabase);
    }
    if consume_admin_word(tokens, cursor, "DROP") {
        return admin_database_statement_kind(tokens, cursor, AdminStatementKind::DropDatabase);
    }
    if consume_admin_word(tokens, cursor, "USE") {
        return Ok(Some(AdminStatementKind::Use));
    }
    if consume_admin_word(tokens, cursor, "SHOW") {
        let after_show = *cursor;
        if consume_admin_word(tokens, cursor, "CREATE") {
            if consume_admin_word(tokens, cursor, "DATABASE")
                || consume_admin_word(tokens, cursor, "SCHEMA")
            {
                return Ok(Some(AdminStatementKind::ShowCreateDatabase));
            }
            *cursor = after_show;
            return Ok(None);
        }
        return admin_database_statement_kind(tokens, cursor, AdminStatementKind::ListDatabases);
    }
    Ok(None)
}

fn admin_database_statement_kind(
    tokens: &[AdminToken],
    cursor: &mut usize,
    kind: AdminStatementKind,
) -> Result<Option<AdminStatementKind>, ParseError> {
    let expected: &[&str] = match kind {
        AdminStatementKind::CreateDatabase
        | AdminStatementKind::AlterDatabase
        | AdminStatementKind::DropDatabase => &["DATABASE", "SCHEMA"],
        AdminStatementKind::ListDatabases => &["DATABASES"],
        AdminStatementKind::Use | AdminStatementKind::ShowCreateDatabase => {
            unreachable!("USE and SHOW CREATE read their own keywords")
        }
    };
    if expected
        .iter()
        .any(|expected| consume_admin_word(tokens, cursor, expected))
    {
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

pub(crate) fn consume_admin_database_name(
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
        _ => {
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
        _ => {
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

/// Whether an `ALTER DATABASE` naming no database goes straight on to its
/// options with this word.
fn starts_a_database_option(word: &str) -> bool {
    [
        "DEFAULT",
        "CHARACTER",
        "CHARSET",
        "COLLATE",
        "ENCRYPTION",
        "READ",
    ]
    .iter()
    .any(|option| word.eq_ignore_ascii_case(option))
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
