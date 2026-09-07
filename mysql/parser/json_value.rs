//! MySQL's canonical form for a JSON document.
//!
//! MySQL does not store the text it was given. It parses the document and
//! prints it back from what it parsed, so object keys come out sorted, a key
//! written twice keeps only the value written last, spacing is fixed at one
//! space after every `:` and `,`, and a number is printed from the value it
//! parsed to. Everything here was measured on MySQL 8.4.11, whose parser is
//! rapidjson — the refusals below carry rapidjson's own wording and the byte
//! offset it reports, because that is the text MySQL hands the client.

use super::like_pattern::MySqlLikePattern;

/// Why MySQL would refuse a document.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum JsonError {
    /// What rapidjson said, at the byte offset it said it. MySQL prints both
    /// in errors 3140 and 3141.
    Text {
        message: &'static str,
        position: usize,
    },
    /// MySQL reports this one on its own, without an offset.
    TooDeep,
}

/// Rewrites a JSON document the way MySQL stores it.
pub fn normalize_json(text: &str) -> Result<String, JsonError> {
    let mut reader = JsonReader { text, at: 0 };
    reader.skip_blanks();
    if reader.at_end() {
        return Err(reader.fail("The document is empty."));
    }
    let value = reader.read_value(0)?;
    reader.skip_blanks();
    if !reader.at_end() {
        return Err(reader.fail("The document root must not be followed by other values."));
    }
    let mut written = String::with_capacity(text.len());
    write_value(&value, &mut written);
    Ok(written)
}

/// The word MySQL calls a document's kind, or nothing when the text is not a
/// document.
///
/// Measured on MySQL 8.4.11: `OBJECT`, `ARRAY`, `STRING`, `INTEGER`,
/// `UNSIGNED INTEGER` for a whole number past a signed one, `DOUBLE`,
/// `BOOLEAN`, and `NULL` for the JSON null — which is a word, not the absence
/// of an answer.
pub fn json_type(text: &str) -> Option<&'static str> {
    Some(match read_document(text)? {
        JsonValue::Null => "NULL",
        JsonValue::Boolean(_) => "BOOLEAN",
        JsonValue::Signed(_) => "INTEGER",
        JsonValue::Unsigned(_) => "UNSIGNED INTEGER",
        JsonValue::Double(_) => "DOUBLE",
        JsonValue::Text(_) => "STRING",
        JsonValue::Array(_) => "ARRAY",
        JsonValue::Object(_) => "OBJECT",
    })
}

/// How many members an object holds, how many elements an array holds, and one
/// for anything else. Measured: only the top level is counted, so
/// `[[1,2],[3]]` is two.
pub fn json_length(text: &str) -> Option<u64> {
    Some(match read_document(text)? {
        JsonValue::Array(elements) => elements.len() as u64,
        JsonValue::Object(members) => members.len() as u64,
        _ => 1,
    })
}

/// The keys an object holds, as a document of their own, or nothing when the
/// text is not an object — measured, `JSON_KEYS` of an array answers no value
/// at all rather than an empty one.
pub fn json_keys(text: &str) -> Option<String> {
    let JsonValue::Object(members) = read_document(text)? else {
        return None;
    };
    let keys = members
        .into_iter()
        .map(|(name, _)| JsonValue::Text(name))
        .collect();
    let mut written = String::new();
    write_value(&JsonValue::Array(keys), &mut written);
    Some(written)
}

/// Writes text as a JSON string, escaping it the way a document does.
pub fn json_quote(text: &str) -> String {
    let mut written = String::new();
    write_text(text, &mut written);
    written
}

/// Answers whether one document holds another, the way `JSON_CONTAINS` does.
///
/// Measured on MySQL 8.4.11, and the rule MySQL's own documentation gives: a
/// candidate array is held by a target array when every one of its elements is
/// held by some element of the target; a candidate that is not an array is
/// held by a target array when some element holds it; a candidate object is
/// held by a target object when every one of its members is there by name and
/// its value is held; and anything else is held only by something equal to it.
/// Nothing is answered for text that is not a document.
pub fn json_contains(target: &str, candidate: &str) -> Option<bool> {
    let target = read_document(target)?;
    let candidate = read_document(candidate)?;
    Some(holds(&target, &candidate))
}

fn holds(target: &JsonValue, candidate: &JsonValue) -> bool {
    match (target, candidate) {
        (JsonValue::Array(elements), JsonValue::Array(wanted)) => wanted
            .iter()
            .all(|want| elements.iter().any(|element| holds(element, want))),
        (JsonValue::Array(elements), _) => elements.iter().any(|element| holds(element, candidate)),
        (JsonValue::Object(members), JsonValue::Object(wanted)) => {
            wanted.iter().all(|(name, want)| {
                members
                    .iter()
                    .any(|(held, value)| held == name && holds(value, want))
            })
        }
        _ => same_value(target, candidate),
    }
}

/// Answers whether two documents are the same value.
///
/// Measured: `JSON_CONTAINS('1', '1.0')` is 1, so two numbers are the same when
/// they count the same rather than when they were written the same.
fn same_value(left: &JsonValue, right: &JsonValue) -> bool {
    if let (Some(left), Some(right)) = (as_number(left), as_number(right)) {
        return left == right;
    }
    match (left, right) {
        (JsonValue::Null, JsonValue::Null) => true,
        (JsonValue::Boolean(left), JsonValue::Boolean(right)) => left == right,
        (JsonValue::Text(left), JsonValue::Text(right)) => left == right,
        (JsonValue::Array(left), JsonValue::Array(right)) => {
            left.len() == right.len()
                && left
                    .iter()
                    .zip(right.iter())
                    .all(|(left, right)| same_value(left, right))
        }
        (JsonValue::Object(left), JsonValue::Object(right)) => {
            left.len() == right.len()
                && left
                    .iter()
                    .zip(right.iter())
                    .all(|(left, right)| left.0 == right.0 && same_value(&left.1, &right.1))
        }
        _ => false,
    }
}

fn as_number(value: &JsonValue) -> Option<f64> {
    match value {
        JsonValue::Signed(signed) => Some(*signed as f64),
        JsonValue::Unsigned(unsigned) => Some(*unsigned as f64),
        JsonValue::Double(double) => Some(*double),
        _ => None,
    }
}

/// Answers whether two documents share anything, the way `JSON_OVERLAPS` does.
///
/// Measured on MySQL 8.4.11: two arrays share an element, two objects share a
/// member — the same key with the same value — an array and anything else share
/// when the other is an element, and two of anything else share when they are
/// equal. An array and an object share nothing, and sharing is equality rather
/// than containment: `[[1,2]]` and `[1]` answer 0 where `JSON_CONTAINS` of the
/// same two answers 1.
pub fn json_overlaps(left: &str, right: &str) -> Option<bool> {
    let left = read_document(left)?;
    let right = read_document(right)?;
    Some(overlaps(&left, &right))
}

fn overlaps(left: &JsonValue, right: &JsonValue) -> bool {
    match (left, right) {
        (JsonValue::Array(left), JsonValue::Array(right)) => left
            .iter()
            .any(|element| right.iter().any(|other| same_value(element, other))),
        (JsonValue::Array(elements), other) | (other, JsonValue::Array(elements)) => {
            !matches!(other, JsonValue::Object(_))
                && elements.iter().any(|element| same_value(element, other))
        }
        (JsonValue::Object(left), JsonValue::Object(right)) => left.iter().any(|(name, value)| {
            right
                .iter()
                .any(|(other, held)| other == name && same_value(value, held))
        }),
        _ => same_value(left, right),
    }
}

/// Merges one document into another the way `JSON_MERGE_PATCH` does.
///
/// Two objects merge member by member, a member patched with the JSON null is
/// taken out, and anything that is not an object replaces what it is merged
/// into. Measured on MySQL 8.4.11: `JSON_MERGE_PATCH('{"a":1}', '{"a":null}')`
/// is `{}` and `JSON_MERGE_PATCH('[1,2]', '[3]')` is `[3]`.
pub fn json_merge_patch(target: &str, patch: &str) -> Option<String> {
    let target = read_document(target)?;
    let patch = read_document(patch)?;
    let mut written = String::new();
    write_value(&patched(target, patch), &mut written);
    Some(written)
}

fn patched(target: JsonValue, patch: JsonValue) -> JsonValue {
    let JsonValue::Object(members) = patch else {
        return patch;
    };
    let mut kept = match target {
        JsonValue::Object(kept) => kept,
        _ => Vec::new(),
    };
    for (name, value) in members {
        let at = kept.iter().position(|(held, _)| *held == name);
        if matches!(value, JsonValue::Null) {
            if let Some(at) = at {
                kept.remove(at);
            }
            continue;
        }
        match at {
            Some(at) => {
                let held = std::mem::replace(&mut kept[at].1, JsonValue::Null);
                kept[at].1 = patched(held, value);
            }
            None => kept.push((name, value)),
        }
    }
    JsonValue::Object(sorted_members(kept))
}

/// Merges one document into another the way `JSON_MERGE_PRESERVE` does.
///
/// Nothing is replaced: two arrays join end to end, two objects merge with a
/// key held by both becoming an array of what each held, and anything that is
/// not an array becomes one to join with. Measured on MySQL 8.4.11:
/// `JSON_MERGE_PRESERVE('{"a":1}', '[2]')` is `[{"a": 1}, 2]` and
/// `JSON_MERGE_PRESERVE('1', '2')` is `[1, 2]`.
pub fn json_merge_preserve(left: &str, right: &str) -> Option<String> {
    let left = read_document(left)?;
    let right = read_document(right)?;
    let mut written = String::new();
    write_value(&preserved(left, right), &mut written);
    Some(written)
}

fn preserved(left: JsonValue, right: JsonValue) -> JsonValue {
    let (left, right) = match (left, right) {
        (JsonValue::Object(mut kept), JsonValue::Object(members)) => {
            for (name, value) in members {
                match kept.iter().position(|(held, _)| *held == name) {
                    Some(at) => {
                        let held = std::mem::replace(&mut kept[at].1, JsonValue::Null);
                        kept[at].1 = preserved(held, value);
                    }
                    None => kept.push((name, value)),
                }
            }
            return JsonValue::Object(sorted_members(kept));
        }
        pair => pair,
    };
    let mut joined = match left {
        JsonValue::Array(elements) => elements,
        other => std::vec![other],
    };
    match right {
        JsonValue::Array(elements) => joined.extend(elements),
        other => joined.push(other),
    }
    JsonValue::Array(joined)
}

/// Finds the paths to the strings a pattern matches, the way `JSON_SEARCH` does.
///
/// Measured on MySQL 8.4.11: only strings are looked at, so a number is never
/// found; the match tells one case of a letter from the other, so `X` is not
/// found by `x`; `one` answers the first path as a JSON string and `all`
/// answers an array of them, except that a single match answers the one string
/// on its own; and nothing found answers no value at all.
pub fn json_search(
    document: &str,
    every: bool,
    pattern: &str,
    escape: Option<char>,
) -> Option<String> {
    let document = read_document(document)?;
    let pattern = MySqlLikePattern::with_escape(pattern, escape);
    let mut found = Vec::new();
    search_value(&document, &pattern, every, "$".to_owned(), &mut found);
    let mut written = String::new();
    match found.len() {
        0 => return None,
        1 => write_value(&JsonValue::Text(found.remove(0)), &mut written),
        _ => write_value(
            &JsonValue::Array(found.into_iter().map(JsonValue::Text).collect()),
            &mut written,
        ),
    }
    Some(written)
}

fn search_value(
    value: &JsonValue,
    pattern: &MySqlLikePattern,
    every: bool,
    path: String,
    found: &mut Vec<String>,
) {
    if !every && !found.is_empty() {
        return;
    }
    match value {
        JsonValue::Text(text) => {
            if pattern.matches_keeping_case(text) {
                found.push(path);
            }
        }
        JsonValue::Array(elements) => {
            for (index, element) in elements.iter().enumerate() {
                search_value(element, pattern, every, format!("{path}[{index}]"), found);
            }
        }
        JsonValue::Object(members) => {
            for (name, member) in members {
                search_value(member, pattern, every, path_with_key(&path, name), found);
            }
        }
        _ => {}
    }
}

/// Writes the path to one member of an object.
///
/// Measured on MySQL 8.4.11: a key is written bare when it reads as a name and
/// in quotes when it does not, so `{"my key": "x"}` is found at `$."my key"`.
fn path_with_key(path: &str, name: &str) -> String {
    let reads_as_a_name = !name.is_empty()
        && !name.starts_with(|character: char| character.is_ascii_digit())
        && name.chars().all(|character| {
            character.is_ascii_alphanumeric() || character == '_' || character == '$'
        });
    if reads_as_a_name {
        return format!("{path}.{name}");
    }
    let mut quoted = String::new();
    write_text(name, &mut quoted);
    format!("{path}.{quoted}")
}

fn read_document(text: &str) -> Option<JsonValue> {
    let mut reader = JsonReader { text, at: 0 };
    reader.skip_blanks();
    let value = reader.read_value(0).ok()?;
    reader.skip_blanks();
    reader.at_end().then_some(value)
}

/// A document MySQL has parsed. Its three numeric shapes are MySQL's own: a
/// literal written without a point or an exponent keeps every digit it had if
/// it fits a 64-bit integer, and becomes a double when it does not.
enum JsonValue {
    Null,
    Boolean(bool),
    Signed(i64),
    Unsigned(u64),
    Double(f64),
    Text(String),
    Array(Vec<JsonValue>),
    Object(Vec<(String, JsonValue)>),
}

/// MySQL stops at 100 nested arrays and objects.
const DEEPEST_NESTING: usize = 100;

struct JsonReader<'a> {
    text: &'a str,
    at: usize,
}

impl JsonReader<'_> {
    fn read_value(&mut self, depth: usize) -> Result<JsonValue, JsonError> {
        match self.peek() {
            Some(b'{') => self.read_object(depth),
            Some(b'[') => self.read_array(depth),
            Some(b'"') => Ok(JsonValue::Text(self.read_text()?)),
            Some(b't') => self.read_word("true").map(|()| JsonValue::Boolean(true)),
            Some(b'f') => self.read_word("false").map(|()| JsonValue::Boolean(false)),
            Some(b'n') => self.read_word("null").map(|()| JsonValue::Null),
            Some(b'-' | b'0'..=b'9') => self.read_number(),
            _ => Err(self.fail("Invalid value.")),
        }
    }

    fn read_object(&mut self, depth: usize) -> Result<JsonValue, JsonError> {
        if depth == DEEPEST_NESTING {
            return Err(JsonError::TooDeep);
        }
        self.at += 1;
        let mut members: Vec<(String, JsonValue)> = Vec::new();
        loop {
            self.skip_blanks();
            if members.is_empty() && self.peek() == Some(b'}') {
                self.at += 1;
                break;
            }
            if self.peek() != Some(b'"') {
                return Err(self.fail("Missing a name for object member."));
            }
            let name = self.read_text()?;
            self.skip_blanks();
            if self.peek() != Some(b':') {
                return Err(self.fail("Missing a colon after a name of object member."));
            }
            self.at += 1;
            self.skip_blanks();
            members.push((name, self.read_value(depth + 1)?));
            self.skip_blanks();
            match self.peek() {
                Some(b',') => self.at += 1,
                Some(b'}') => {
                    self.at += 1;
                    break;
                }
                _ => return Err(self.fail("Missing a comma or '}' after an object member.")),
            }
        }
        Ok(JsonValue::Object(sorted_members(members)))
    }

    fn read_array(&mut self, depth: usize) -> Result<JsonValue, JsonError> {
        if depth == DEEPEST_NESTING {
            return Err(JsonError::TooDeep);
        }
        self.at += 1;
        let mut elements = Vec::new();
        loop {
            self.skip_blanks();
            if elements.is_empty() && self.peek() == Some(b']') {
                self.at += 1;
                break;
            }
            elements.push(self.read_value(depth + 1)?);
            self.skip_blanks();
            match self.peek() {
                Some(b',') => self.at += 1,
                Some(b']') => {
                    self.at += 1;
                    break;
                }
                _ => return Err(self.fail("Missing a comma or ']' after an array element.")),
            }
        }
        Ok(JsonValue::Array(elements))
    }

    /// Reads a quoted string, turning every escape into the character it names.
    fn read_text(&mut self) -> Result<String, JsonError> {
        self.at += 1;
        let mut read = String::new();
        loop {
            let Some(byte) = self.peek() else {
                return Err(self.fail("Missing a closing quotation mark in string."));
            };
            match byte {
                b'"' => {
                    self.at += 1;
                    return Ok(read);
                }
                // Measured: a raw byte below a space is refused, and 0x7f is not.
                0x00..=0x1f => return Err(self.fail("Invalid encoding in string.")),
                b'\\' => read.push(self.read_escape()?),
                _ => {
                    let character = self.text[self.at..]
                        .chars()
                        .next()
                        .expect("the byte at hand starts a character");
                    self.at += character.len_utf8();
                    read.push(character);
                }
            }
        }
    }

    /// Reads one backslash escape. Every refusal here reports the backslash.
    fn read_escape(&mut self) -> Result<char, JsonError> {
        let backslash = self.at;
        self.at += 1;
        let named = match self.peek() {
            Some(b'"') => '"',
            Some(b'\\') => '\\',
            Some(b'/') => '/',
            Some(b'b') => '\u{8}',
            Some(b'f') => '\u{c}',
            Some(b'n') => '\n',
            Some(b'r') => '\r',
            Some(b't') => '\t',
            Some(b'u') => {
                self.at = backslash;
                return self.read_unicode_escape();
            }
            _ => {
                self.at = backslash;
                return Err(self.fail("Invalid escape character in string."));
            }
        };
        self.at += 1;
        Ok(named)
    }

    /// Reads `\uXXXX`, joining a surrogate pair into the character it names.
    fn read_unicode_escape(&mut self) -> Result<char, JsonError> {
        let backslash = self.at;
        let leading = self.read_four_hex_digits(backslash)?;
        if let Some(named) = char::from_u32(u32::from(leading)) {
            return Ok(named);
        }
        let broken_pair = |reader: &mut Self| {
            reader.at = backslash;
            reader.fail("The surrogate pair in string is invalid.")
        };
        if !(0xd800..=0xdbff).contains(&leading)
            || self.peek() != Some(b'\\')
            || self.text.as_bytes().get(self.at + 1) != Some(&b'u')
        {
            return Err(broken_pair(self));
        }
        let trailing = self.read_four_hex_digits(backslash)?;
        if !(0xdc00..=0xdfff).contains(&trailing) {
            return Err(broken_pair(self));
        }
        let joined =
            0x10000 + ((u32::from(leading) - 0xd800) << 10) + (u32::from(trailing) - 0xdc00);
        Ok(char::from_u32(joined).expect("a joined surrogate pair names a character"))
    }

    fn read_four_hex_digits(&mut self, backslash: usize) -> Result<u16, JsonError> {
        self.at += 2;
        let Some(digits) = self.text.as_bytes().get(self.at..self.at + 4) else {
            self.at = backslash;
            return Err(self.fail("Incorrect hex digit after \\u escape in string."));
        };
        let mut read = 0u16;
        for digit in digits {
            let Some(value) = (*digit as char).to_digit(16) else {
                self.at = backslash;
                return Err(self.fail("Incorrect hex digit after \\u escape in string."));
            };
            read = read * 16 + value as u16;
        }
        self.at += 4;
        Ok(read)
    }

    /// Reads `true`, `false` or `null`, reporting the byte that did not match.
    fn read_word(&mut self, word: &str) -> Result<(), JsonError> {
        for expected in word.bytes() {
            if self.peek() != Some(expected) {
                return Err(self.fail("Invalid value."));
            }
            self.at += 1;
        }
        Ok(())
    }

    fn read_number(&mut self) -> Result<JsonValue, JsonError> {
        let start = self.at;
        if self.peek() == Some(b'-') {
            self.at += 1;
        }
        match self.peek() {
            // A leading zero ends the number, which leaves the digits after it
            // for whoever asked, and they answer for the junk.
            Some(b'0') => self.at += 1,
            Some(b'1'..=b'9') => {
                while matches!(self.peek(), Some(b'0'..=b'9')) {
                    self.at += 1;
                }
            }
            _ => return Err(self.fail("Invalid value.")),
        }
        let mut whole = true;
        if self.peek() == Some(b'.') {
            whole = false;
            self.at += 1;
            if !matches!(self.peek(), Some(b'0'..=b'9')) {
                return Err(self.fail("Miss fraction part in number."));
            }
            while matches!(self.peek(), Some(b'0'..=b'9')) {
                self.at += 1;
            }
        }
        if matches!(self.peek(), Some(b'e' | b'E')) {
            whole = false;
            self.at += 1;
            if matches!(self.peek(), Some(b'+' | b'-')) {
                self.at += 1;
            }
            if !matches!(self.peek(), Some(b'0'..=b'9')) {
                return Err(self.fail("Miss exponent in number."));
            }
            while matches!(self.peek(), Some(b'0'..=b'9')) {
                self.at += 1;
            }
        }
        let written = &self.text[start..self.at];
        if whole {
            if let Ok(signed) = written.parse::<i64>() {
                return Ok(JsonValue::Signed(signed));
            }
            if let Ok(unsigned) = written.parse::<u64>() {
                return Ok(JsonValue::Unsigned(unsigned));
            }
        }
        let read = written
            .parse::<f64>()
            .expect("the digits just read spell a double");
        if read.is_infinite() {
            self.at = start;
            return Err(self.fail("Number too big to be stored in double."));
        }
        Ok(JsonValue::Double(read))
    }

    fn skip_blanks(&mut self) {
        while matches!(self.peek(), Some(b' ' | b'\t' | b'\n' | b'\r')) {
            self.at += 1;
        }
    }

    fn peek(&self) -> Option<u8> {
        self.text.as_bytes().get(self.at).copied()
    }

    fn at_end(&self) -> bool {
        self.at >= self.text.len()
    }

    fn fail(&self, message: &'static str) -> JsonError {
        JsonError::Text {
            message,
            position: self.at,
        }
    }
}

/// Puts an object's members in the order MySQL keeps them: shorter keys first,
/// and keys of the same length by their bytes. A key written twice keeps the
/// value written last.
fn sorted_members(mut members: Vec<(String, JsonValue)>) -> Vec<(String, JsonValue)> {
    members.sort_by(|left, right| {
        left.0
            .len()
            .cmp(&right.0.len())
            .then_with(|| left.0.cmp(&right.0))
    });
    let mut kept: Vec<(String, JsonValue)> = Vec::with_capacity(members.len());
    for member in members {
        match kept.last_mut() {
            // The sort is stable, so a run of one key is still in the order the
            // members were written, and the last of the run is the one to keep.
            Some(last) if last.0 == member.0 => *last = member,
            _ => kept.push(member),
        }
    }
    kept
}

fn write_value(value: &JsonValue, out: &mut String) {
    match value {
        JsonValue::Null => out.push_str("null"),
        JsonValue::Boolean(true) => out.push_str("true"),
        JsonValue::Boolean(false) => out.push_str("false"),
        JsonValue::Signed(signed) => out.push_str(&signed.to_string()),
        JsonValue::Unsigned(unsigned) => out.push_str(&unsigned.to_string()),
        JsonValue::Double(double) => write_double(*double, out),
        JsonValue::Text(text) => write_text(text, out),
        JsonValue::Array(elements) => {
            out.push('[');
            for (index, element) in elements.iter().enumerate() {
                if index > 0 {
                    out.push_str(", ");
                }
                write_value(element, out);
            }
            out.push(']');
        }
        JsonValue::Object(members) => {
            out.push('{');
            for (index, (name, member)) in members.iter().enumerate() {
                if index > 0 {
                    out.push_str(", ");
                }
                write_text(name, out);
                out.push_str(": ");
                write_value(member, out);
            }
            out.push('}');
        }
    }
}

/// Writes a string the way MySQL prints one: the seven named escapes, every
/// other byte below a space as `\u00xx`, and everything else as it stands —
/// `/` is not escaped, and neither is 0x7f or anything above it.
fn write_text(text: &str, out: &mut String) {
    out.push('"');
    for character in text.chars() {
        match character {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            other if other < ' ' => out.push_str(&format!("\\u{:04x}", other as u32)),
            other => out.push(other),
        }
    }
    out.push('"');
}

/// Writes a double the way MySQL prints one.
///
/// Rust writes the shortest decimal that reads back as the same double, which
/// is what MySQL's dtoa gives it too. What is left is where MySQL puts the
/// decimal point. Measured on 8.4.11: an exponent is written when the point
/// sits more than 14 places to the left of the digits, or more than 15 places
/// to the right of them with no digit left over to write after it — so 1e14
/// prints as 100000000000000.0, 1e15 as 1e15, and 1234567890123456.8, whose
/// point is just as far right, prints in full. A double printed without a
/// point or an exponent gets `.0`, which is what tells it from an integer.
fn write_double(value: f64, out: &mut String) {
    let printed = format!("{value:e}");
    let (mantissa, exponent) = printed
        .split_once('e')
        .expect("Rust writes an exponent in this format");
    let exponent: i32 = exponent.parse().expect("Rust writes a whole exponent");
    let (sign, mantissa) = match mantissa.strip_prefix('-') {
        Some(rest) => ("-", rest),
        None => ("", mantissa),
    };
    let digits: String = mantissa
        .chars()
        .filter(|character| *character != '.')
        .collect();
    let count = digits.len() as i32;
    // The value is 0.<digits> times ten to the point.
    let point = exponent + 1;
    out.push_str(sign);
    if point < -14 || (point > 15 && point >= count) {
        out.push_str(&digits[..1]);
        if count > 1 {
            out.push('.');
            out.push_str(&digits[1..]);
        }
        out.push('e');
        out.push_str(&(point - 1).to_string());
    } else if point <= 0 {
        out.push_str("0.");
        for _ in 0..-point {
            out.push('0');
        }
        out.push_str(&digits);
    } else if point >= count {
        out.push_str(&digits);
        for _ in 0..point - count {
            out.push('0');
        }
        out.push_str(".0");
    } else {
        out.push_str(&digits[..point as usize]);
        out.push('.');
        out.push_str(&digits[point as usize..]);
    }
}

#[cfg(test)]
mod tests {
    use super::{
        json_contains, json_merge_patch, json_merge_preserve, json_overlaps, json_search,
        normalize_json, JsonError,
    };

    fn normalized(text: &str) -> String {
        normalize_json(text).expect("the document is one MySQL takes")
    }

    /// Every answer here measured on MySQL 8.4.11 over a utf8mb4 connection.
    #[test]
    fn contains_holds_what_mysql_holds() {
        let document = r#"{"a": 1, "b": [1, 2, 3], "s": "x", "n": null, "o": {"k": 1}}"#;
        for (target, candidate, held) in [
            (document, "1", false),
            (document, r#"{"a": 1}"#, true),
            (document, r#"{"a": 1, "s": "x"}"#, true),
            (document, r#"{"a": 2}"#, false),
            // An array holds a candidate array when every one of its elements
            // is held by some element of the target.
            ("[1,2,3]", "[1,3]", true),
            ("[1,2,3]", "[1,4]", false),
            ("[1,2,3]", "2", true),
            ("[[1,2]]", "[1]", true),
            ("[1,2,3]", "[]", true),
            // Anything else is held only by something equal to it, and two
            // numbers are equal when they count the same.
            ("1", "1", true),
            ("1", "1.0", true),
            ("1", "2", false),
            (r#""x""#, r#""x""#, true),
            (r#""x""#, r#""y""#, false),
            ("null", "null", true),
            ("true", "true", true),
            ("true", "false", false),
            (r#"{"k": 1}"#, r#"{"k": 1}"#, true),
            (r#"{"k": 1}"#, r#"{"k": 2}"#, false),
        ] {
            assert_eq!(
                json_contains(target, candidate),
                Some(held),
                "JSON_CONTAINS({target}, {candidate})"
            );
        }

        // Text that is not a document is answered with nothing at all.
        assert_eq!(json_contains("{", "1"), None);
        assert_eq!(json_contains("1", "{"), None);
    }

    /// Every answer here measured on MySQL 8.4.11 over a utf8mb4 connection.
    #[test]
    fn search_finds_the_paths_mysql_finds() {
        let document = r#"{"a": "x", "b": ["y", "x"], "o": {"k": "x"}, "n": 1, "t": "xyz"}"#;
        for (every, pattern, found) in [
            (false, "x", Some(r#""$.a""#)),
            (true, "x", Some(r#"["$.a", "$.b[1]", "$.o.k"]"#)),
            (false, "z", None),
            (true, "z", None),
            (true, "x%", Some(r#"["$.a", "$.b[1]", "$.o.k", "$.t"]"#)),
            (false, "x%", Some(r#""$.a""#)),
            (
                true,
                "%",
                Some(r#"["$.a", "$.b[0]", "$.b[1]", "$.o.k", "$.t"]"#),
            ),
            // Only strings are looked at, so the number is never found.
            (false, "1", None),
        ] {
            assert_eq!(
                json_search(document, every, pattern, Some('\\')).as_deref(),
                found,
                "JSON_SEARCH(doc, {every}, {pattern})"
            );
        }

        // A single match answers the one path on its own rather than an array.
        assert_eq!(
            json_search(r#"["a","b"]"#, true, "a", Some('\\')).as_deref(),
            Some(r#""$[0]""#)
        );
        // A document that is one string of its own is found at the root.
        assert_eq!(
            json_search(r#""a""#, false, "a", Some('\\')).as_deref(),
            Some(r#""$""#)
        );
        // The match tells one case of a letter from the other.
        assert_eq!(json_search(r#"{"a":"X"}"#, false, "x", Some('\\')), None);
        // A key that does not read as a name is written in quotes.
        assert_eq!(
            json_search(r#"{"my key":"x","a b":"x"}"#, true, "x", Some('\\')).as_deref(),
            Some(r#"["$.\"a b\"", "$.\"my key\""]"#)
        );
        // A key that starts with a dollar still reads as a name.
        assert_eq!(
            json_search(r#"{"$k":"x"}"#, false, "x", Some('\\')).as_deref(),
            Some(r#""$.$k""#)
        );
        // An element inside an element keeps both of its places.
        assert_eq!(
            json_search(r#"{"a":[["x"]]}"#, false, "x", Some('\\')).as_deref(),
            Some(r#""$.a[0][0]""#)
        );
        // The escape character is the one it was given.
        assert_eq!(
            json_search(r#"{"a":"x_y"}"#, false, "x!_y", Some('!')).as_deref(),
            Some(r#""$.a""#)
        );
    }

    /// Every answer here measured on MySQL 8.4.11 over a utf8mb4 connection.
    #[test]
    fn overlaps_shares_what_mysql_shares() {
        for (left, right, shared) in [
            ("[1,2,3]", "[3,4]", true),
            ("[1,2,3]", "[4,5]", false),
            ("[1,2,3]", "2", true),
            ("1", "1", true),
            ("1", "2", false),
            (r#"{"a":1,"b":2}"#, r#"{"a":1,"c":3}"#, true),
            (r#"{"a":1}"#, r#"{"a":2}"#, false),
            // An array and an object share nothing.
            ("[1,2]", r#"{"a":1}"#, false),
            // Sharing is equality rather than containment, where
            // JSON_CONTAINS of the same two answers 1.
            ("[[1,2]]", "[1]", false),
        ] {
            assert_eq!(
                json_overlaps(left, right),
                Some(shared),
                "JSON_OVERLAPS({left}, {right})"
            );
        }
    }

    /// Every answer here measured on MySQL 8.4.11 over a utf8mb4 connection.
    #[test]
    fn the_merges_join_what_mysql_joins() {
        for (target, patch, merged) in [
            (
                r#"{"a":1,"b":2}"#,
                r#"{"b":3,"c":4}"#,
                r#"{"a": 1, "b": 3, "c": 4}"#,
            ),
            // A member patched with the JSON null is taken out.
            (r#"{"a":1}"#, r#"{"a":null}"#, "{}"),
            (r#"{"a":{"b":1}}"#, r#"{"a":{"b":null}}"#, r#"{"a": {}}"#),
            // Anything that is not an object replaces what it is merged into.
            ("[1,2]", "[3]", "[3]"),
            ("1", "2", "2"),
            (r#"{"a":1}"#, "2", "2"),
            (
                r#"{"a":{"x":1,"y":2}}"#,
                r#"{"a":{"y":3}}"#,
                r#"{"a": {"x": 1, "y": 3}}"#,
            ),
        ] {
            assert_eq!(
                json_merge_patch(target, patch).as_deref(),
                Some(merged),
                "JSON_MERGE_PATCH({target}, {patch})"
            );
        }

        for (left, right, merged) in [
            (
                r#"{"a":1,"b":2}"#,
                r#"{"b":3,"c":4}"#,
                r#"{"a": 1, "b": [2, 3], "c": 4}"#,
            ),
            ("[1,2]", "[3]", "[1, 2, 3]"),
            ("1", "2", "[1, 2]"),
            (r#"{"a":1}"#, "[2]", r#"[{"a": 1}, 2]"#),
            ("[1]", r#"{"a":1}"#, r#"[1, {"a": 1}]"#),
            ("1", "[2,3]", "[1, 2, 3]"),
            (r#"{"a":[1]}"#, r#"{"a":2}"#, r#"{"a": [1, 2]}"#),
        ] {
            assert_eq!(
                json_merge_preserve(left, right).as_deref(),
                Some(merged),
                "JSON_MERGE_PRESERVE({left}, {right})"
            );
        }

        // Folding two at a time answers what MySQL answers for three.
        let first = json_merge_preserve(r#"{"a":1}"#, r#"{"a":2}"#).unwrap();
        assert_eq!(
            json_merge_preserve(&first, r#"{"a":3}"#).as_deref(),
            Some(r#"{"a": [1, 2, 3]}"#)
        );
    }

    fn refusal(text: &str) -> JsonError {
        normalize_json(text).expect_err("the document is one MySQL refuses")
    }

    fn refusal_text(text: &str) -> (&'static str, usize) {
        match refusal(text) {
            JsonError::Text { message, position } => (message, position),
            JsonError::TooDeep => panic!("the document is not too deep"),
        }
    }

    /// Every reading below was measured on MySQL 8.4.11.
    #[test]
    fn object_keys_come_out_shortest_first_and_then_by_their_bytes() {
        assert_eq!(
            normalized(r#"{"bb":1,"a":2,"ccc":3,"b":4,"A":5,"_":6}"#),
            r#"{"A": 5, "_": 6, "a": 2, "b": 4, "bb": 1, "ccc": 3}"#
        );
        assert_eq!(normalized(r#"{"":1,"a":2}"#), r#"{"": 1, "a": 2}"#);
        assert_eq!(normalized(r#"{"日":1,"あ":2}"#), r#"{"あ": 2, "日": 1}"#);
        assert_eq!(
            normalized(r#"{"n":[1,2,{"z":1,"y":2}]}"#),
            r#"{"n": [1, 2, {"y": 2, "z": 1}]}"#
        );
    }

    #[test]
    fn a_key_written_twice_keeps_the_value_written_last() {
        assert_eq!(normalized(r#"{"a":1,"a":2}"#), r#"{"a": 2}"#);
        assert_eq!(normalized(r#"{"a":1,"b":9,"a":2}"#), r#"{"a": 2, "b": 9}"#);
    }

    #[test]
    fn spacing_is_one_space_after_a_colon_and_a_comma() {
        assert_eq!(normalized("[1,  2,3]"), "[1, 2, 3]");
        assert_eq!(normalized("[ ]"), "[]");
        assert_eq!(normalized("{ }"), "{}");
        assert_eq!(
            normalized("  [[1,[2]],{\"a\":[]}] "),
            "[[1, [2]], {\"a\": []}]"
        );
    }

    #[test]
    fn a_whole_number_keeps_every_digit_that_fits_a_64_bit_integer() {
        assert_eq!(normalized("42"), "42");
        assert_eq!(normalized("-0"), "0");
        assert_eq!(normalized("9223372036854775807"), "9223372036854775807");
        assert_eq!(normalized("-9223372036854775808"), "-9223372036854775808");
        assert_eq!(normalized("18446744073709551615"), "18446744073709551615");
        assert_eq!(normalized("18446744073709551616"), "1.8446744073709552e19");
    }

    #[test]
    fn a_number_written_with_a_point_or_an_exponent_prints_as_a_double() {
        assert_eq!(normalized("{\"a\":1.0}"), "{\"a\": 1.0}");
        assert_eq!(normalized("{\"a\":1E2}"), "{\"a\": 100.0}");
        assert_eq!(normalized("{\"a\":1e0}"), "{\"a\": 1.0}");
        assert_eq!(normalized("{\"a\":1e+5}"), "{\"a\": 100000.0}");
        assert_eq!(normalized("{\"a\":-0.0}"), "{\"a\": -0.0}");
        assert_eq!(normalized("{\"a\":0.1}"), "{\"a\": 0.1}");
        assert_eq!(
            normalized("{\"a\":3.0000000000000004}"),
            "{\"a\": 3.0000000000000004}"
        );
    }

    /// The point is written in place until it would need more than fourteen
    /// zeros on the left or fifteen on the right.
    #[test]
    fn a_double_takes_an_exponent_only_once_the_point_is_far_enough_away() {
        assert_eq!(normalized("1e14"), "100000000000000.0");
        assert_eq!(normalized("1e15"), "1e15");
        assert_eq!(normalized("1.5e15"), "1.5e15");
        assert_eq!(normalized("1.23e14"), "123000000000000.0");
        assert_eq!(normalized("1e300"), "1e300");
        assert_eq!(
            normalized("1.7976931348623157e308"),
            "1.7976931348623157e308"
        );
        assert_eq!(normalized("1e-4"), "0.0001");
        assert_eq!(normalized("1e-15"), "0.000000000000001");
        assert_eq!(normalized("1e-16"), "1e-16");
        assert_eq!(normalized("5e-324"), "5e-324");
        assert_eq!(normalized("-1e15"), "-1e15");
        assert_eq!(normalized("-1e-16"), "-1e-16");
        // The point sits sixteen places right, but there is a digit left over
        // to write after it, so MySQL writes the number in full.
        assert_eq!(normalized("1000000000000000.4"), "1000000000000000.4");
        // Nothing is left over here, so it takes the exponent.
        assert_eq!(normalized("1.234567890123456e15"), "1.234567890123456e15");
        assert_eq!(normalized("2.5e-5"), "0.000025");
        assert_eq!(normalized("7.25e17"), "7.25e17");
        assert_eq!(normalized("6.02e23"), "6.02e23");
        assert_eq!(normalized("-3.75"), "-3.75");
    }

    /// MySQL reads a JSON number with rapidjson's fast path, which lands on
    /// the double next to the right one often enough to see: measured on
    /// 8.4.11, MySQL answers `1000000000000000.1` with `1e15` and `1e-30`
    /// with `9.999999999999999e-31`. Rust reads both correctly, and nothing
    /// here reproduces the miss.
    #[test]
    fn a_number_is_read_more_accurately_than_mysql_reads_it() {
        assert_eq!(normalized("1000000000000000.1"), "1000000000000000.1");
        assert_eq!(normalized("1e-30"), "1e-30");
    }

    #[test]
    fn a_string_keeps_the_seven_named_escapes_and_writes_the_rest_as_it_stands() {
        assert_eq!(normalized(r#""a\tb""#), r#""a\tb""#);
        assert_eq!(normalized(r#""a\nb""#), r#""a\nb""#);
        assert_eq!(normalized(r#""a\rb""#), r#""a\rb""#);
        assert_eq!(normalized(r#""a\bb""#), r#""a\bb""#);
        assert_eq!(normalized(r#""a\fb""#), r#""a\fb""#);
        assert_eq!(normalized(r#""a\"b""#), r#""a\"b""#);
        assert_eq!(normalized(r#""a\\b""#), r#""a\\b""#);
        // A forward slash may be escaped on the way in and never is on the way
        // out, and 0x7f is left alone while everything below a space is not.
        assert_eq!(normalized(r#""a\/b""#), r#""a/b""#);
        assert_eq!(normalized("\"a\u{7f}b\""), "\"a\u{7f}b\"");
        assert_eq!(normalized(r#""a\u0001\u001fb""#), r#""a\u0001\u001fb""#);
        // An escape naming a character above a space is written as itself.
        assert_eq!(normalized(r#""éA""#), r#""éA""#);
        assert_eq!(normalized(r#""😀""#), "\"😀\"");
    }

    #[test]
    fn a_bare_scalar_is_a_document_of_its_own() {
        assert_eq!(normalized(r#""plain""#), r#""plain""#);
        assert_eq!(normalized("true"), "true");
        assert_eq!(normalized("false"), "false");
        assert_eq!(normalized("null"), "null");
    }

    #[test]
    fn a_document_mysql_refuses_carries_the_words_and_the_offset_mysql_reports() {
        assert_eq!(refusal_text(""), ("The document is empty.", 0));
        assert_eq!(refusal_text("  "), ("The document is empty.", 2));
        assert_eq!(
            refusal_text("{oops}"),
            ("Missing a name for object member.", 1)
        );
        assert_eq!(refusal_text("{"), ("Missing a name for object member.", 1));
        assert_eq!(
            refusal_text("{\"a\":1,,}"),
            ("Missing a name for object member.", 7)
        );
        assert_eq!(refusal_text("{\"a\":}"), ("Invalid value.", 5));
        assert_eq!(refusal_text("[1,]"), ("Invalid value.", 3));
        assert_eq!(refusal_text("["), ("Invalid value.", 1));
        assert_eq!(refusal_text(".5"), ("Invalid value.", 0));
        assert_eq!(refusal_text("+1"), ("Invalid value.", 0));
        assert_eq!(refusal_text("NaN"), ("Invalid value.", 0));
        assert_eq!(refusal_text("'single'"), ("Invalid value.", 0));
        assert_eq!(refusal_text("nul"), ("Invalid value.", 3));
        assert_eq!(refusal_text("{\"a\":tru}"), ("Invalid value.", 8));
        assert_eq!(refusal_text("{\"a\":-}"), ("Invalid value.", 6));
        assert_eq!(
            refusal_text("{\"a\":1} x"),
            ("The document root must not be followed by other values.", 8)
        );
        assert_eq!(
            refusal_text("01"),
            ("The document root must not be followed by other values.", 1)
        );
        assert_eq!(
            refusal_text("{\"a\" 1}"),
            ("Missing a colon after a name of object member.", 5)
        );
        assert_eq!(
            refusal_text("{\"a\""),
            ("Missing a colon after a name of object member.", 4)
        );
        assert_eq!(
            refusal_text("{\"a\":1 \"b\":2}"),
            ("Missing a comma or '}' after an object member.", 7)
        );
        assert_eq!(
            refusal_text("{\"a\":00}"),
            ("Missing a comma or '}' after an object member.", 6)
        );
        assert_eq!(
            refusal_text("[1 2]"),
            ("Missing a comma or ']' after an array element.", 3)
        );
        assert_eq!(
            refusal_text("[1,2"),
            ("Missing a comma or ']' after an array element.", 4)
        );
        assert_eq!(
            refusal_text("\"unterminated"),
            ("Missing a closing quotation mark in string.", 13)
        );
        assert_eq!(
            refusal_text(r#""ab\x""#),
            ("Invalid escape character in string.", 3)
        );
        assert_eq!(
            refusal_text(r#"{"k":"ab\x"}"#),
            ("Invalid escape character in string.", 8)
        );
        assert_eq!(
            refusal_text(r#""ab\uZZ""#),
            ("Incorrect hex digit after \\u escape in string.", 3)
        );
        assert_eq!(
            refusal_text(r#""\u00""#),
            ("Incorrect hex digit after \\u escape in string.", 1)
        );
        assert_eq!(
            refusal_text(r#""\ud800""#),
            ("The surrogate pair in string is invalid.", 1)
        );
        assert_eq!(
            refusal_text(r#""\udc00""#),
            ("The surrogate pair in string is invalid.", 1)
        );
        assert_eq!(
            refusal_text("{\"a\":1.}"),
            ("Miss fraction part in number.", 7)
        );
        assert_eq!(
            refusal_text("{\"a\":1.5e}"),
            ("Miss exponent in number.", 9)
        );
        assert_eq!(
            refusal_text("{\"a\":1e400}"),
            ("Number too big to be stored in double.", 5)
        );
        assert_eq!(
            refusal_text("{\"a\":\"x\u{1f}y\"}"),
            ("Invalid encoding in string.", 7)
        );
    }

    /// Measured: a hundred nested arrays are taken and a hundred and one are
    /// not, and MySQL reports that one without an offset.
    #[test]
    fn a_document_nested_deeper_than_a_hundred_is_refused() {
        let taken = format!("{}1{}", "[".repeat(100), "]".repeat(100));
        assert!(normalize_json(&taken).is_ok());
        let refused = format!("{}1{}", "[".repeat(101), "]".repeat(101));
        assert_eq!(refusal(&refused), JsonError::TooDeep);
    }

    /// Every reading measured on MySQL 8.4.11.
    #[test]
    fn a_document_answers_the_kind_mysql_names_it() {
        use super::json_type;
        for (document, kind) in [
            (r#"{"a": 1}"#, "OBJECT"),
            ("[1, 2]", "ARRAY"),
            (r#""x""#, "STRING"),
            ("42", "INTEGER"),
            ("-1", "INTEGER"),
            ("18446744073709551615", "UNSIGNED INTEGER"),
            ("1.5", "DOUBLE"),
            ("true", "BOOLEAN"),
            ("false", "BOOLEAN"),
            // The JSON null is a word, not the absence of an answer.
            ("null", "NULL"),
        ] {
            assert_eq!(json_type(document), Some(kind), "{document}");
        }
        assert_eq!(json_type("not a document"), None);
    }

    #[test]
    fn a_document_answers_how_many_it_holds_at_the_top() {
        use super::json_length;
        for (document, length) in [
            (r#"{"a": 1, "b": 2}"#, 2),
            ("[1, 2, 3]", 3),
            ("[[1,2],[3]]", 2),
            (r#"{"a":{"b":1,"c":2}}"#, 1),
            ("{}", 0),
            ("[]", 0),
            (r#""x""#, 1),
            ("42", 1),
            ("null", 1),
        ] {
            assert_eq!(json_length(document), Some(length), "{document}");
        }
        assert_eq!(json_length("not a document"), None);
    }

    #[test]
    fn an_object_answers_its_keys_and_nothing_else_does() {
        use super::json_keys;
        assert_eq!(
            json_keys(r#"{"bb":1,"a":2,"ccc":3,"b":4}"#).as_deref(),
            Some(r#"["a", "b", "bb", "ccc"]"#)
        );
        assert_eq!(json_keys("{}").as_deref(), Some("[]"));
        assert_eq!(json_keys("[1, 2]"), None);
        assert_eq!(json_keys(r#""x""#), None);
        assert_eq!(json_keys("not a document"), None);
    }

    #[test]
    fn text_written_as_a_json_string_is_escaped_the_way_a_document_is() {
        use super::json_quote;
        assert_eq!(json_quote("x"), r#""x""#);
        assert_eq!(json_quote("a\tb"), r#""a\tb""#);
        assert_eq!(json_quote("a\"b"), r#""a\"b""#);
        assert_eq!(json_quote("a\\b"), r#""a\\b""#);
        assert_eq!(json_quote("a/b"), r#""a/b""#);
        assert_eq!(json_quote("é"), "\"é\"");
        assert_eq!(json_quote("\u{1}"), r#""\u0001""#);
    }

    #[test]
    fn what_mysql_normalized_once_it_leaves_alone() {
        for document in [
            r#"{"A": 5, "_": 6, "a": 2, "bb": 1}"#,
            r#"[1, 2, {"y": 2, "z": 1}]"#,
            r#"{"a": 1.0, "b": 1e15, "c": 0.000000000000001}"#,
            r#"{"list": [true, false, null, 1, "x"]}"#,
            r#"{"deep": {"deeper": {"deepest": [1.5, 20000000000.0]}}}"#,
            r#""a\tb""#,
            "true",
            "null",
        ] {
            assert_eq!(normalized(document), document, "{document}");
        }
    }
}
