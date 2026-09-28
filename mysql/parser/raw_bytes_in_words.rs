//! Statement text that is not UTF-8 because a `_binary '...'` word holds raw
//! bytes, which is how `mysqldump` writes a column of bytes by default.
//!
//! Measured on MySQL 8.4.11 under `character_set_client = utf8mb4`: bytes
//! that are not UTF-8 inside `_binary '...'` are the word's own bytes; inside
//! a word without the introducer they are kept with warning 1300 and refused
//! with 1366 by a column of words; in a comment they are ignored. Only the
//! first is read here. Every such word is written again as `_binary X'...'`,
//! which MySQL reads as the same binary string — measured, `_binary X'41'`
//! into an `INT` answers 1366 as `_binary 'A'` does, where `X'41'` alone is
//! the number 65 — so everything after this reads UTF-8 text.

/// Writes each `_binary '...'` word of `text` again in hexadecimal, and
/// answers the text that makes, or `None` when it is still not UTF-8 — a byte
/// that is not UTF-8 outside such a word — or when a word is followed by
/// another written straight after it, which MySQL joins to it.
///
/// A word's escapes are read as MySQL reads them, byte by byte: measured,
/// `_binary'\%\_\q\b\t\"'` is `5C 25 5C 5F 71 08 09 22`, and a backslash
/// before any byte it does not name is dropped, as `mysqldump` relies on when
/// it writes a backslash before a byte that would begin a character.
pub fn raw_bytes_in_words_as_hexadecimal(
    text: &[u8],
    no_backslash_escapes: bool,
) -> Option<String> {
    let mut written = Vec::with_capacity(text.len());
    let mut at = 0;
    while at < text.len() {
        let byte = text[at];
        match byte {
            b'\'' | b'"' | b'`' => {
                let end = end_of_quoted(text, at, no_backslash_escapes && byte != b'`')?;
                written.extend_from_slice(&text[at..end]);
                at = end;
            }
            b'#' => at = copy_through_line_end(text, at, &mut written),
            b'-' if text.get(at + 1) == Some(&b'-')
                && text
                    .get(at + 2)
                    .is_some_and(|next| next.is_ascii_whitespace() || next.is_ascii_control()) =>
            {
                at = copy_through_line_end(text, at, &mut written);
            }
            // A versioned comment, `/*!40101 ... */`, holds a statement's
            // text, so it is read as text; its closing `*/` is copied where it
            // stands.
            b'/' if text.get(at + 1) == Some(&b'*') && text.get(at + 2) != Some(&b'!') => {
                let end = text[at + 2..]
                    .windows(2)
                    .position(|pair| pair == b"*/")
                    .map(|found| at + 2 + found + 2)?;
                written.extend_from_slice(&text[at..end]);
                at = end;
            }
            b'_' if at == 0 || !is_part_of_a_name(text[at - 1]) => {
                let name_end = at
                    + text[at..]
                        .iter()
                        .take_while(|byte| is_part_of_a_name(**byte))
                        .count();
                let quote = name_end
                    + text[name_end..]
                        .iter()
                        .take_while(|byte| byte.is_ascii_whitespace())
                        .count();
                if !text[at..name_end].eq_ignore_ascii_case(b"_binary")
                    || text.get(quote) != Some(&b'\'')
                {
                    written.extend_from_slice(&text[at..name_end]);
                    at = name_end;
                    continue;
                }
                let (bytes, end) = word_bytes(text, quote, no_backslash_escapes)?;
                let next = end
                    + text[end..]
                        .iter()
                        .take_while(|byte| byte.is_ascii_whitespace())
                        .count();
                if matches!(text.get(next), Some(b'\'' | b'"')) {
                    return None;
                }
                written.extend_from_slice(b"_binary X'");
                for byte in bytes {
                    written.extend_from_slice(format!("{byte:02X}").as_bytes());
                }
                written.push(b'\'');
                at = end;
            }
            _ => {
                written.push(byte);
                at += 1;
            }
        }
    }
    String::from_utf8(written).ok()
}

/// Whether a byte can be part of an unquoted name: a letter, a digit, `_`,
/// `$`, or any byte of a character outside ASCII.
fn is_part_of_a_name(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'$' || byte >= 0x80
}

fn copy_through_line_end(text: &[u8], at: usize, written: &mut Vec<u8>) -> usize {
    let end = text[at..]
        .iter()
        .position(|byte| *byte == b'\n')
        .map_or(text.len(), |found| at + found + 1);
    written.extend_from_slice(&text[at..end]);
    end
}

/// The place just past the quote closing the quoted text opened at `start`.
fn end_of_quoted(text: &[u8], start: usize, backslash_escapes: bool) -> Option<usize> {
    let quote = text[start];
    let mut at = start + 1;
    while at < text.len() {
        match text[at] {
            // Under ANSI_QUOTES, which this does not know of, text in double
            // quotes is a name, where a backslash escapes nothing; which of
            // the two it is decides where the text ends, so it is not read.
            b'\\' if quote == b'"' => return None,
            b'\\' if backslash_escapes => at += 2,
            byte if byte == quote && text.get(at + 1) == Some(&quote) => at += 2,
            byte if byte == quote => return Some(at + 1),
            _ => at += 1,
        }
    }
    None
}

/// The bytes of the single-quoted word opened at `start`, and the place just
/// past its closing quote.
fn word_bytes(text: &[u8], start: usize, no_backslash_escapes: bool) -> Option<(Vec<u8>, usize)> {
    let mut bytes = Vec::new();
    let mut at = start + 1;
    while at < text.len() {
        match text[at] {
            b'\\' if !no_backslash_escapes => {
                let escaped = *text.get(at + 1)?;
                match escaped {
                    b'0' => bytes.push(0),
                    b'b' => bytes.push(0x08),
                    b'n' => bytes.push(b'\n'),
                    b'r' => bytes.push(b'\r'),
                    b't' => bytes.push(b'\t'),
                    b'Z' => bytes.push(0x1A),
                    b'%' | b'_' => bytes.extend_from_slice(&[b'\\', escaped]),
                    other => bytes.push(other),
                }
                at += 2;
            }
            b'\'' if text.get(at + 1) == Some(&b'\'') => {
                bytes.push(b'\'');
                at += 2;
            }
            b'\'' => return Some((bytes, at + 1)),
            byte => {
                bytes.push(byte);
                at += 1;
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_binary_word_holding_raw_bytes_is_written_in_hexadecimal() {
        assert_eq!(
            raw_bytes_in_words_as_hexadecimal(
                b"INSERT INTO t VALUES (1,_binary '\\0\xff\\\xc3''x\\\\\\'\\n\\%',NULL)",
                false
            )
            .as_deref(),
            Some("INSERT INTO t VALUES (1,_binary X'00FFC327785C270A5C25',NULL)")
        );
        assert_eq!(
            raw_bytes_in_words_as_hexadecimal(b"SELECT _BINARY\n'\xfe', 'a\xc3\xa9'", false)
                .as_deref(),
            Some("SELECT _binary X'FE', 'a\u{e9}'")
        );
    }

    #[test]
    fn without_backslash_escapes_a_backslash_is_a_byte() {
        assert_eq!(
            raw_bytes_in_words_as_hexadecimal(b"SELECT _binary'\\\xff'", true).as_deref(),
            Some("SELECT _binary X'5CFF'")
        );
    }

    #[test]
    fn a_word_is_read_only_where_it_is_code() {
        assert_eq!(
            raw_bytes_in_words_as_hexadecimal(
                b"SELECT '_binary ''x', `_binary` /* _binary 'y' */, /*!40101 _binary'\xfe' */",
                false
            )
            .as_deref(),
            Some("SELECT '_binary ''x', `_binary` /* _binary 'y' */, /*!40101 _binary X'FE' */")
        );
        assert_eq!(
            raw_bytes_in_words_as_hexadecimal(b"SELECT x_binary '\xff'", false),
            None
        );
    }

    #[test]
    fn a_raw_byte_anywhere_else_is_not_read() {
        for text in [
            &b"SELECT '\xff'"[..],
            b"SELECT 1 -- \xff\n",
            b"SELECT `\xff`",
            b"SELECT _binary \"\xff\"",
            b"SELECT _binary '\xff' 'a'",
            b"SELECT _binary '\xff",
            b"SELECT \"a\\\"\", _binary '\xff'",
        ] {
            assert_eq!(
                raw_bytes_in_words_as_hexadecimal(text, false),
                None,
                "{text:?}"
            );
        }
    }
}
