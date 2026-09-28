use super::nothing_to_run::{after_line, runs_as_sql, starts_a_dash_comment};
use super::SessionSqlMode;

/// Reads whether `sql` writes a word MySQL 8.4 takes for the opening of a
/// dollar-quoted string, which its parser refuses.
///
/// The `mysql` client sends `select $$` as it connects and treats `$tag$` as a
/// quote when it splits a script only if the server answers 1064 there, as
/// MySQL does. Measured on 8.4.11: a word that begins with `$` and holds a
/// second `$` — `$$`, `$a$`, `$$a`, `$é$` — is 1064, while `$`, `$a`, `a$$`,
/// `` `$$` ``, `'$$'`, `@$$` and `x.$$` are a name, a word or a variable.
pub fn opens_a_dollar_quote(sql: &str, mode: SessionSqlMode) -> bool {
    let bytes = sql.as_bytes();
    let mut cursor = 0;
    while cursor < bytes.len() {
        let byte = bytes[cursor];
        match byte {
            b'\'' | b'"' | b'`' => cursor = after_quoted(bytes, cursor, mode),
            b'#' => cursor = after_line(bytes, cursor),
            b'-' if starts_a_dash_comment(bytes, cursor) => cursor = after_line(bytes, cursor),
            b'/' if bytes[cursor..].starts_with(b"/*") && !runs_as_sql(&bytes[cursor + 2..]) => {
                cursor = sql[cursor + 2..]
                    .find("*/")
                    .map_or(bytes.len(), |end| cursor + 2 + end + 2);
            }
            b'$' => {
                let word_end = cursor
                    + bytes[cursor..]
                        .iter()
                        .take_while(|byte| is_word_byte(**byte))
                        .count();
                if bytes[cursor + 1..word_end].contains(&b'$') {
                    return true;
                }
                cursor = word_end;
            }
            b'@' | b'.' => {
                cursor += 1;
                cursor += bytes[cursor..]
                    .iter()
                    .take_while(|byte| is_word_byte(**byte) || **byte == b'@')
                    .count();
            }
            _ if is_word_byte(byte) => {
                cursor += bytes[cursor..]
                    .iter()
                    .take_while(|byte| is_word_byte(**byte))
                    .count();
            }
            _ => cursor += 1,
        }
    }
    false
}

fn is_word_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'$' || byte >= 0x80
}

fn after_quoted(bytes: &[u8], cursor: usize, mode: SessionSqlMode) -> usize {
    let quote = bytes[cursor];
    // Backslashes escape inside a string, never inside a quoted name.
    let takes_escapes = quote == b'\'' || (quote == b'"' && !mode.ansi_quotes);
    let mut cursor = cursor + 1;
    while cursor < bytes.len() {
        let byte = bytes[cursor];
        if byte == b'\\' && takes_escapes && !mode.no_backslash_escapes {
            cursor += 2;
        } else if byte == quote {
            if bytes.get(cursor + 1) == Some(&quote) {
                cursor += 2;
            } else {
                return cursor + 1;
            }
        } else {
            cursor += 1;
        }
    }
    bytes.len()
}
