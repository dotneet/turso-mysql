//! What reading a statement's text gives, kept while the server answers it.
//!
//! The server reads one statement many times on its way to running it: each
//! recognizer of a kind of statement tokenizes it to see whether it is that
//! kind, and the checked parsers read it again for each thing they check. A
//! read costs time in proportion to the text — in a debug build about 50 ms to
//! tokenize 0.6 MB and as much again to parse it — so replaying a mysqldump,
//! whose `INSERT`s run to a MiB each, spent most of its time reading.
//!
//! While a [`KeptReads`] is held, a read of a text this thread already read
//! the same way is answered from that read, which follows from the text and
//! the way it is read alone. The reads are let go of with the last guard, so a
//! long statement's tokens and syntax trees are not held once it is answered.

use std::cell::{Cell, RefCell};
use std::marker::PhantomData;
use std::rc::Rc;

use sqlparser::ast::Statement;
use sqlparser::dialect::MySqlDialect;
use sqlparser::tokenizer::{Location, Token, TokenWithSpan, Tokenizer, TokenizerError};
use turso_parser::{ast::Cmd as TursoCmd, parser::Parser as TursoParser};

use crate::admin_command::{AdminToken, VersionedComments};
use crate::{CheckedAutoIncrementInsert, ParseError, SessionMySqlDialect, SessionSqlMode};

/// Keeps what reading each text gives until it is dropped.
///
/// Guards may be nested; the reads are let go of when the outermost one is
/// dropped. A guard belongs to the thread that took it.
#[must_use = "reads are kept only while the guard is held"]
pub struct KeptReads {
    one_thread: PhantomData<*const ()>,
}

/// Starts keeping what reading each text gives, for as long as the guard lives.
pub fn keep_reads() -> KeptReads {
    KEEPERS.with(|keepers| keepers.set(keepers.get() + 1));
    KeptReads {
        one_thread: PhantomData,
    }
}

impl Drop for KeptReads {
    fn drop(&mut self) {
        let keepers = KEEPERS.with(|keepers| {
            keepers.set(keepers.get() - 1);
            keepers.get()
        });
        if keepers == 0 {
            TOKENS.with(|kept| kept.borrow_mut().clear());
            PLAIN_TOKENS.with(|kept| kept.borrow_mut().clear());
            STATEMENTS.with(|kept| kept.borrow_mut().clear());
            ENGINE_STATEMENTS.with(|kept| kept.borrow_mut().clear());
            ADMIN_TOKENS.with(|kept| kept.borrow_mut().clear());
            COUNTED_INSERTS.with(|kept| kept.borrow_mut().clear());
        }
    }
}

/// How many bytes of text this thread has read, by the kind of reading.
///
/// Only a read that was not answered from a kept one counts. Dividing by a
/// statement's length tells how many times it was read whole.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct BytesRead {
    /// Read by sqlparser's tokenizer.
    pub tokenized: usize,
    /// Read into a sqlparser syntax tree.
    pub parsed: usize,
    /// Read by the engine's own parser.
    pub parsed_by_the_engine: usize,
    /// Read by the tokenizer of administrative commands.
    pub tokenized_as_a_command: usize,
    /// Checked as a counted `INSERT`, which renders it and has the engine
    /// read what was rendered.
    pub checked_as_a_counted_insert: usize,
}

/// What this thread has read so far.
pub fn bytes_read() -> BytesRead {
    BYTES_READ.with(Cell::get)
}

/// The tokens of `sql` as sqlparser reads them under `dialect`.
pub(crate) fn tokens_with_location(
    dialect: &SessionMySqlDialect,
    sql: &str,
) -> Result<Vec<TokenWithSpan>, TokenizerError> {
    with_tokens(TokenDialect::Session(*dialect), sql, |tokens| {
        tokens.map(<[TokenWithSpan]>::to_vec)
    })
}

/// The tokens of `sql` as sqlparser reads them under `dialect`, without the
/// places they were written at.
pub(crate) fn tokens(
    dialect: &SessionMySqlDialect,
    sql: &str,
) -> Result<Rc<Vec<Token>>, TokenizerError> {
    plain_tokens(TokenDialect::Session(*dialect), sql)
}

/// The tokens of `sql` as sqlparser's own MySQL dialect reads them.
pub(crate) fn tokens_of_plain_mysql(sql: &str) -> Result<Rc<Vec<Token>>, TokenizerError> {
    plain_tokens(TokenDialect::PlainMySql, sql)
}

fn plain_tokens(dialect: TokenDialect, sql: &str) -> Result<Rc<Vec<Token>>, TokenizerError> {
    answer(&PLAIN_TOKENS, dialect, sql, || {
        with_tokens(dialect, sql, |tokens| match tokens {
            Ok(tokens) => Ok(Rc::new(
                tokens.iter().map(|token| token.token.clone()).collect(),
            )),
            Err(error) => Err((error.message, error.location)),
        })
    })
    .map_err(|(message, location)| TokenizerError { message, location })
}

/// The one statement `read` makes of `sql`, answered from the last time this
/// thread read the same text in the same mode while reads are kept.
pub(crate) fn statement(
    sql: &str,
    mode: SessionSqlMode,
    read: impl FnOnce() -> Result<Statement, ParseError>,
) -> Rc<Result<Statement, ParseError>> {
    answer(&STATEMENTS, mode, sql, || {
        count(|bytes| bytes.parsed += sql.len());
        Rc::new(read())
    })
}

/// What the engine's parser makes of `sql`: its first command, and whether
/// another follows it. The second is read only when the first was read.
#[derive(Debug, Clone)]
pub(crate) struct EngineReading {
    pub(crate) first: Result<Option<TursoCmd>, String>,
    pub(crate) another_follows: Result<bool, String>,
}

pub(crate) fn engine_reading(sql: &str) -> EngineReading {
    answer(&ENGINE_STATEMENTS, (), sql, || {
        count(|bytes| bytes.parsed_by_the_engine += sql.len());
        let mut parser = TursoParser::new(sql.as_bytes());
        let first = parser.next_cmd().map_err(|error| error.to_string());
        let another_follows = match &first {
            Ok(_) => parser
                .next_cmd()
                .map(|next| next.is_some())
                .map_err(|error| error.to_string()),
            Err(_) => Ok(false),
        };
        EngineReading {
            first,
            another_follows,
        }
    })
}

pub(crate) fn admin_tokens(
    sql: &str,
    mode: SessionSqlMode,
    versioned_comments: VersionedComments,
    read: impl FnOnce() -> Result<Vec<AdminToken>, ParseError>,
) -> Result<Rc<Vec<AdminToken>>, ParseError> {
    let read_alike_by_every_setting = !sql.contains("/*!");
    let kept_as = if read_alike_by_every_setting {
        VersionedComments::Kept
    } else {
        versioned_comments
    };
    answer(&ADMIN_TOKENS, (mode, kept_as), sql, || {
        count(|bytes| bytes.tokenized_as_a_command += sql.len());
        read().map(Rc::new)
    })
}

/// Which values a counted `INSERT` was checked to take.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CountedValues {
    /// Only values written into the statement.
    Written,
    /// Written values and bare `?` placeholders.
    Bound,
}

/// A counted `INSERT` checked by `read`, which the frontend asks for up to
/// three times a statement: to raise the counter past the ids it writes, to
/// write rows naming their own ids, and to number the rest. Checking renders
/// the whole statement and has the engine read it, so the answer is kept.
pub(crate) fn counted_insert(
    sql: &str,
    mode: SessionSqlMode,
    values: CountedValues,
    read: impl FnOnce() -> Result<CheckedAutoIncrementInsert, ParseError>,
) -> Result<CheckedAutoIncrementInsert, ParseError> {
    answer(&COUNTED_INSERTS, (mode, values), sql, || {
        count(|bytes| bytes.checked_as_a_counted_insert += sql.len());
        read()
    })
}

/// How sqlparser was told to read a text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TokenDialect {
    Session(SessionMySqlDialect),
    PlainMySql,
}

/// A tokenizer's answer, shared by the dialect settings that read a text the
/// same way. A failure is kept in parts because sqlparser's error cannot be
/// copied.
type KeptTokens = Rc<Result<Vec<TokenWithSpan>, (String, Location)>>;

fn with_tokens<T>(
    dialect: TokenDialect,
    sql: &str,
    use_tokens: impl FnOnce(Result<&[TokenWithSpan], TokenizerError>) -> T,
) -> T {
    let read = || -> KeptTokens {
        count(|bytes| bytes.tokenized += sql.len());
        let tokens = match &dialect {
            TokenDialect::Session(dialect) => Tokenizer::new(dialect, sql).tokenize_with_location(),
            TokenDialect::PlainMySql => {
                Tokenizer::new(&MySqlDialect {}, sql).tokenize_with_location()
            }
        };
        Rc::new(tokens.map_err(|error| (error.message, error.location)))
    };
    let lend = |tokens: &KeptTokens| {
        use_tokens(match tokens.as_ref() {
            Ok(tokens) => Ok(tokens.as_slice()),
            Err((message, location)) => Err(TokenizerError {
                message: message.clone(),
                location: *location,
            }),
        })
    };
    if !keeping() {
        return lend(&read());
    }
    let kept_for = |dialect: TokenDialect| {
        TOKENS.with(|kept| {
            kept.borrow()
                .iter()
                .find(|(key, text, _)| *key == dialect && text == sql)
                .map(|(_, _, tokens)| Rc::clone(tokens))
        })
    };
    let tokens = match kept_for(dialect) {
        Some(tokens) => tokens,
        None => {
            let tokens = the_other_setting(dialect)
                .filter(|_| !crate::mentions_ignoring_case(sql, "/*!"))
                .and_then(kept_for)
                .unwrap_or_else(read);
            TOKENS.with(|kept| remember(&mut kept.borrow_mut(), dialect, sql, Rc::clone(&tokens)));
            tokens
        }
    };
    lend(&tokens)
}

/// The session's other setting for `/*!NNNNN ... */` comments.
///
/// The two differ only in whether such a comment is read as the text it
/// holds, and sqlparser consults the setting nowhere but at a comment opening
/// `/*!`, so a text with no `/*!` in it reads as the same tokens, a failure
/// included, under both.
fn the_other_setting(dialect: TokenDialect) -> Option<TokenDialect> {
    match dialect {
        TokenDialect::Session(session) => Some(TokenDialect::Session(SessionMySqlDialect {
            expand_executable_comments: !session.expand_executable_comments,
            ..session
        })),
        TokenDialect::PlainMySql => None,
    }
}

/// How many texts of each kind of reading are kept. A statement is read
/// under several dialects, and the frontend rewrites some statements — an
/// `INSERT` with no column list is read again with the list written out — and
/// reads short texts of its own along the way, such as a table's definition.
/// When there is no room, the shortest text is let go of, since reading it
/// again costs the least.
const KEPT_TEXTS: usize = 8;

type Kept<K, V> = RefCell<Vec<(K, String, V)>>;
type KeptPlainTokens = Result<Rc<Vec<Token>>, (String, Location)>;
type KeptAdminTokens = Result<Rc<Vec<AdminToken>>, ParseError>;

thread_local! {
    static KEEPERS: Cell<usize> = const { Cell::new(0) };
    static BYTES_READ: Cell<BytesRead> = const {
        Cell::new(BytesRead {
            tokenized: 0,
            parsed: 0,
            parsed_by_the_engine: 0,
            tokenized_as_a_command: 0,
            checked_as_a_counted_insert: 0,
        })
    };
    static TOKENS: Kept<TokenDialect, KeptTokens> = const { RefCell::new(Vec::new()) };
    static PLAIN_TOKENS: Kept<TokenDialect, KeptPlainTokens> = const { RefCell::new(Vec::new()) };
    static STATEMENTS: Kept<SessionSqlMode, Rc<Result<Statement, ParseError>>> =
        const { RefCell::new(Vec::new()) };
    static ENGINE_STATEMENTS: Kept<(), EngineReading> = const { RefCell::new(Vec::new()) };
    static COUNTED_INSERTS: Kept<(SessionSqlMode, CountedValues), Result<CheckedAutoIncrementInsert, ParseError>> =
        const { RefCell::new(Vec::new()) };
    static ADMIN_TOKENS: Kept<(SessionSqlMode, VersionedComments), KeptAdminTokens> =
        const { RefCell::new(Vec::new()) };
}

fn keeping() -> bool {
    KEEPERS.with(Cell::get) > 0
}

fn count(add: impl FnOnce(&mut BytesRead)) {
    BYTES_READ.with(|bytes_read| {
        let mut bytes = bytes_read.get();
        add(&mut bytes);
        bytes_read.set(bytes);
    });
}

/// The kept answer for `sql` read the way `key` says, or what `read` answers,
/// kept while reads are kept.
///
/// `read` runs with nothing borrowed, since it may read through another kind
/// of kept reading.
fn answer<K: Copy + PartialEq + 'static, V: Clone + 'static>(
    kept: &'static std::thread::LocalKey<Kept<K, V>>,
    key: K,
    sql: &str,
    read: impl FnOnce() -> V,
) -> V {
    if !keeping() {
        return read();
    }
    if let Some(answer) = kept.with(|kept| {
        kept.borrow()
            .iter()
            .find(|(kept_key, text, _)| *kept_key == key && text == sql)
            .map(|(_, _, answer)| answer.clone())
    }) {
        return answer;
    }
    let answer = read();
    kept.with(|kept| remember(&mut kept.borrow_mut(), key, sql, answer.clone()));
    answer
}

fn remember<K, V>(kept: &mut Vec<(K, String, V)>, key: K, sql: &str, answer: V) {
    if kept.len() == KEPT_TEXTS {
        let shortest = (0..kept.len())
            .min_by_key(|&at| kept[at].1.len())
            .expect("a full list holds a text");
        kept.remove(shortest);
    }
    kept.push((key, sql.to_owned(), answer));
}
