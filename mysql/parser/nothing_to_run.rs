/// A statement that holds nothing to run, and what MySQL answers it with.
///
/// The `mysql` client in 8.4 keeps comments by default and sends each comment
/// line of a script it reads as a statement of its own, so replaying a dump
/// sends `-- MySQL dump 10.13 ...` and `--` before any SQL. Measured on MySQL
/// 8.4.11: text that is only comments, optionally followed by semicolons,
/// answers an OK that changes nothing but the warnings, which it clears, and
/// `ROW_COUNT()`, which reads 0 after it; text with no comment at all — empty,
/// whitespace, `;` — answers 1065 `Query was empty`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NothingToRun {
    OnlyComments,
    Empty,
}

/// The version `/*!NNNNN ... */` is compared with: MySQL 8.4.11, the one this
/// server answers as.
const MYSQL_VERSION_ID: u32 = 80_411;

/// Reads whether `sql` holds nothing to run: whitespace and comments, then
/// semicolons and whitespace.
///
/// A comment after a semicolon is left to the statement path, as is a
/// versioned comment this server's version would run: measured, `; -- a` and
/// `;/* c */` are 1064 in MySQL, and `/*!40101 */` warns. A versioned comment
/// naming a later version is a comment, as `/*+ ... */` and `/*M!...*/` are.
pub fn nothing_to_run(sql: &str) -> Option<NothingToRun> {
    let bytes = sql.as_bytes();
    let mut cursor = 0;
    let mut commented = false;
    let mut semicolons = false;
    while cursor < bytes.len() {
        let byte = bytes[cursor];
        if is_mysql_space(byte) {
            cursor += 1;
        } else if byte == b';' {
            semicolons = true;
            cursor += 1;
        } else if semicolons {
            return None;
        } else if byte == b'#' || starts_a_dash_comment(bytes, cursor) {
            commented = true;
            cursor = after_line(bytes, cursor);
        } else if bytes[cursor..].starts_with(b"/*") && !runs_as_sql(&bytes[cursor + 2..]) {
            commented = true;
            let end = sql[cursor + 2..].find("*/")?;
            cursor += 2 + end + 2;
        } else {
            return None;
        }
    }
    Some(if commented {
        NothingToRun::OnlyComments
    } else {
        NothingToRun::Empty
    })
}

/// MySQL's own whitespace: the vertical tab is one, measured, where Rust's
/// ASCII whitespace leaves it out.
pub(crate) fn is_mysql_space(byte: u8) -> bool {
    matches!(byte, b' ' | b'\t' | b'\n' | 0x0b | 0x0c | b'\r')
}

/// `--` starts a comment only when a space, a control character or the end of
/// the text follows it; `--x` is two minus signs.
pub(crate) fn starts_a_dash_comment(bytes: &[u8], cursor: usize) -> bool {
    bytes[cursor..].starts_with(b"--")
        && bytes
            .get(cursor + 2)
            .is_none_or(|next| is_mysql_space(*next) || next.is_ascii_control())
}

/// Whether a `/*` comment whose text follows is one MySQL runs: `/*!` with no
/// version or with one this server's version has reached. Measured on 8.4.11,
/// the version is read from five or six digits: `/*!123456 x */` is a comment.
pub(crate) fn runs_as_sql(after_opening: &[u8]) -> bool {
    let Some(after_mark) = after_opening.strip_prefix(b"!") else {
        return false;
    };
    let digits = after_mark
        .iter()
        .take_while(|byte| byte.is_ascii_digit())
        .count();
    match digits {
        5 | 6 => {
            let version: u32 = std::str::from_utf8(&after_mark[..digits])
                .expect("ASCII digits are UTF-8")
                .parse()
                .expect("five or six ASCII digits are a number");
            version <= MYSQL_VERSION_ID
        }
        _ => true,
    }
}

pub(crate) fn after_line(bytes: &[u8], cursor: usize) -> usize {
    bytes[cursor..]
        .iter()
        .position(|byte| *byte == b'\n')
        .map_or(bytes.len(), |line_end| cursor + line_end + 1)
}
