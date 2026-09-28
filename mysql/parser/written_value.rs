//! Values a projection writes out in full: a word in quotes, a number with a
//! point or an exponent, a hexadecimal or bit literal, a cast or a `CONVERT`
//! of a written value, and a few calls over written values alone —
//! `HEX(255)`, `CHAR(65)`, `FIELD('b', 'a', 'b')`, `TRUNCATE(1.567, 2)`.
//!
//! Each is worked out here, before the statement runs, into the text MySQL
//! answers, because the engine would answer a different one — it prints `0.10`
//! as `0.1`, has no bit literal, and keeps a JSON document as it was written.
//! The shape each answers is fixed by how it is written, so it travels as a
//! [`WrittenValue`]. Everything here was measured on MySQL 8.4.11.

use crate::json_value::normalize_json;
use crate::temporal_value::{normalize_date, normalize_datetime};
use sqlparser::ast::{CastKind, DataType, ExactNumberInfo, Expr, UnaryOperator, Value};

/// The most digits a MySQL `DECIMAL` holds. A written number with more is
/// read as a `DOUBLE` instead.
const DECIMAL_MOST_DIGITS: u32 = 65;
/// The most places after the point a MySQL `DECIMAL` holds.
const DECIMAL_MOST_PLACES: u32 = 30;

/// The shape of a value written out in a projection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WrittenValue {
    /// A word written in quotes. Measured: never null, four bytes to each
    /// character, `'abc'` a `VAR_STRING` of 12.
    Word { characters: u32 },
    /// A `NEWDECIMAL`: a number written with a point, or a cast to `DECIMAL`.
    /// A cast of `NULL` is the one that can be null.
    Decimal {
        precision: u32,
        scale: u32,
        not_null: bool,
    },
    /// A `DOUBLE` written with an exponent, as wide as it was written.
    Double { length: u32 },
    /// A binary string written in hexadecimal or in bits, as many bytes long
    /// as it holds. Measured: the hexadecimal spellings carry the unsigned
    /// flag and the bit spelling does not.
    Bytes { length: u32, unsigned: bool },
    /// A cast of a written day to `DATE`, or a day `MAKEDATE` or
    /// `FROM_DAYS` works out. Measured: `FROM_DAYS` is the one reported NOT
    /// NULL.
    Day { not_null: bool },
    /// A time of day `MAKETIME` works out.
    Time,
    /// A cast of a written moment to `DATETIME`.
    Moment,
    /// A cast of a written document to `JSON`.
    Json,
    /// A written word cast to `CHAR` or converted to utf8mb4, and the text
    /// `HEX`, `BIN`, `OCT` and `ELT` answer over written values.
    Text { characters: u32 },
    /// The binary string `CHAR` answers, four bytes reserved for each number
    /// it was given.
    BinaryWord { length: u32 },
    /// A whole number `ASCII`, `ORD`, `FIELD` or `TRUNCATE` answers over
    /// written values, which cannot be null.
    WholeNumber { length: u32 },
}

/// Reads a written value, answering its shape and the SQL the engine answers
/// MySQL's text with.
pub(crate) fn read_written_value(expr: &Expr) -> Option<(WrittenValue, String)> {
    match expr {
        Expr::Value(value) => match &value.value {
            Value::Number(written, false) => written_number(written, false),
            Value::HexStringLiteral(digits) => written_hexadecimal(digits),
            Value::SingleQuotedByteStringLiteral(bits) => written_bits(bits),
            Value::SingleQuotedString(word) | Value::DoubleQuotedString(word) => Some((
                WrittenValue::Word {
                    characters: u32::try_from(word.chars().count()).ok()?,
                },
                format!("'{}'", word.replace('\'', "''")),
            )),
            _ => None,
        },
        Expr::UnaryOp {
            op: op @ (UnaryOperator::Minus | UnaryOperator::Plus),
            expr: inner,
        } => {
            let Expr::Value(value) = inner.as_ref() else {
                return None;
            };
            let Value::Number(written, false) = &value.value else {
                return None;
            };
            written_number(written, *op == UnaryOperator::Minus)
        }
        Expr::Cast {
            kind: CastKind::Cast,
            expr: cast,
            data_type,
            format: None,
            array: false,
        } => written_cast(cast, data_type),
        Expr::Function(function) => written_call(function),
        Expr::Convert {
            is_try: false,
            expr: converted,
            data_type,
            charset,
            target_before_value: false,
            styles,
        } if styles.is_empty() => match (data_type, charset) {
            (Some(data_type), None) => written_cast(converted, data_type),
            (None, Some(charset)) if names_utf8mb4(charset) => {
                written_cast(converted, &DataType::Char(None))
            }
            _ => None,
        },
        _ => None,
    }
}

/// Reads a string of bytes written out — `X'616263'`, `0x616263`,
/// `b'01100001'`, `_binary 'abc'` or `_binary X'616263'` — as the blob the
/// engine writes for the same bytes, or `None` for any other value.
///
/// Measured on MySQL 8.4.11, each of these is a binary string wherever it is
/// written as a string, and a word after `_binary` is the bytes it is written
/// in. A hexadecimal literal with an odd count of digits is refused: MySQL
/// answers 1064 for `X'ABC'` and reads `0xABC` as `0x0ABC`, and the two reach
/// this spelled the same.
pub(crate) fn written_byte_string(expr: &Expr) -> Option<Option<String>> {
    let hexadecimal = |digits: &str| written_hexadecimal(digits).map(|(_, sql)| sql);
    match expr {
        Expr::Value(value) => match &value.value {
            Value::HexStringLiteral(digits) => Some(hexadecimal(digits)),
            Value::SingleQuotedByteStringLiteral(bits) => {
                Some(written_bits(bits).map(|(_, sql)| sql))
            }
            _ => None,
        },
        Expr::Prefixed { prefix, value } if prefix.value.eq_ignore_ascii_case("_binary") => {
            let Expr::Value(value) = value.as_ref() else {
                return Some(None);
            };
            Some(match &value.value {
                Value::HexStringLiteral(digits) => hexadecimal(digits),
                Value::SingleQuotedString(word) | Value::DoubleQuotedString(word) => Some(format!(
                    "x'{}'",
                    word.bytes()
                        .map(|byte| format!("{byte:02X}"))
                        .collect::<String>()
                )),
                _ => None,
            })
        }
        _ => None,
    }
}

/// The name MySQL gives the column of a word written in quotes with no alias.
///
/// Measured on MySQL 8.4.11: the word as it reads once its escapes are
/// worked out — `'it''s'` is named `it's` and `'a\nb'` carries its line
/// break — with the spaces and control characters before it left out, `' '`
/// being named nothing at all, cut at a NUL, each character outside the
/// Basic Multilingual Plane written `?`, and cut to 255 bytes without
/// splitting a character. MySQL keeps a column's name in utf8mb3, which has
/// no room for those characters, and in 255 bytes.
pub(crate) fn word_column_name(word: &str) -> String {
    let word = word.trim_start_matches(|character: char| {
        character.is_ascii() && !character.is_ascii_graphic()
    });
    let word = word.split('\0').next().unwrap_or_default();
    let mut name: String = word
        .chars()
        .map(|character| {
            if character.len_utf8() == 4 {
                '?'
            } else {
                character
            }
        })
        .collect();
    let mut kept = name.len().min(MOST_NAME_BYTES);
    while !name.is_char_boundary(kept) {
        kept -= 1;
    }
    name.truncate(kept);
    name
}

/// The most bytes MySQL keeps of a column's name.
const MOST_NAME_BYTES: usize = 255;

/// Reads a call over written values alone that this works out in full.
///
/// Measured on MySQL 8.4.11: `HEX(n)` writes a whole number's 64 bits in
/// hexadecimal, `HEX(-1)` being `FFFFFFFFFFFFFFFF`, as a `VAR_STRING` of 64,
/// and a word's bytes as eight times its characters; `BIN` and `OCT` write the
/// same bits in their radix as one of 260; `CHAR(65, 66)` answers the bytes of
/// each number as a binary string of four bytes to the number; `ASCII` and
/// `ORD` read the first byte and the first character as a `LONGLONG` of 3 and
/// of 21; `FIELD` finds a word among the ones after it without regard to case
/// as one of 3, and `ELT` reads one out by its place; `TRUNCATE` cuts a number
/// to the places it names. None of them reads a column, so none can be null
/// apart from `ELT` past its last word.
fn written_call(function: &sqlparser::ast::Function) -> Option<(WrittenValue, String)> {
    let [sqlparser::ast::ObjectNamePart::Identifier(name)] = function.name.0.as_slice() else {
        return None;
    };
    if name.quote_style.is_some()
        || function.over.is_some()
        || function.filter.is_some()
        || function.null_treatment.is_some()
        || !function.within_group.is_empty()
        || function.uses_odbc_syntax
        || function.parameters != sqlparser::ast::FunctionArguments::None
    {
        return None;
    }
    let sqlparser::ast::FunctionArguments::List(arguments) = &function.args else {
        return None;
    };
    if arguments.duplicate_treatment.is_some() || !arguments.clauses.is_empty() {
        return None;
    }
    let values = arguments
        .args
        .iter()
        .map(|argument| match argument {
            sqlparser::ast::FunctionArg::Unnamed(sqlparser::ast::FunctionArgExpr::Expr(expr)) => {
                Some(expr)
            }
            _ => None,
        })
        .collect::<Option<Vec<_>>>()?;
    let named = |candidate: &str| name.value.eq_ignore_ascii_case(candidate);
    if named("HEX") || named("BIN") || named("OCT") {
        let [value] = values.as_slice() else {
            return None;
        };
        if let (true, Some(word)) = (named("HEX"), written_word(value)) {
            let hexadecimal = word
                .bytes()
                .map(|byte| format!("{byte:02X}"))
                .collect::<String>();
            let characters = u32::try_from(word.chars().count()).ok()?.checked_mul(8)?;
            return Some((WrittenValue::Text { characters }, quoted(&hexadecimal)));
        }
        let bits = written_whole_number_bits(value)?;
        let (written, characters) = if named("HEX") {
            (format!("{bits:X}"), 16)
        } else if named("BIN") {
            (format!("{bits:b}"), 65)
        } else {
            (format!("{bits:o}"), 65)
        };
        return Some((WrittenValue::Text { characters }, quoted(&written)));
    }
    if named("CHAR") {
        if values.is_empty() {
            return None;
        }
        let mut bytes = Vec::new();
        for value in &values {
            let number = u32::try_from(crate::translate::direct_signed_integer(value)?).ok()?;
            let written = number.to_be_bytes();
            let first = written
                .iter()
                .position(|byte| *byte != 0)
                .unwrap_or(written.len() - 1);
            bytes.extend_from_slice(&written[first..]);
        }
        let hexadecimal = bytes
            .iter()
            .map(|byte| format!("{byte:02X}"))
            .collect::<String>();
        let length = u32::try_from(values.len()).ok()?.checked_mul(4)?;
        return Some((
            WrittenValue::BinaryWord { length },
            format!("x'{hexadecimal}'"),
        ));
    }
    if named("ASCII") || named("ORD") {
        let [value] = values.as_slice() else {
            return None;
        };
        let word = written_word(value)?;
        let (answer, length) = if named("ASCII") {
            (crate::first_byte(word.as_bytes()), 3)
        } else {
            (crate::first_character_code(word), 21)
        };
        return Some((WrittenValue::WholeNumber { length }, answer.to_string()));
    }
    if named("FIELD") {
        let [looked_for, choices @ ..] = values.as_slice() else {
            return None;
        };
        if choices.is_empty() {
            return None;
        }
        // Only words written in ASCII: they compare without regard to case
        // under the connection's collation, which for ASCII is the case fold
        // alone, where a letter outside it has accents and weights of its own.
        let looked_for = written_word(looked_for).filter(|word| word.is_ascii())?;
        let mut found = 0;
        for (place, choice) in choices.iter().enumerate() {
            let choice = written_word(choice).filter(|word| word.is_ascii())?;
            if found == 0 && choice.eq_ignore_ascii_case(looked_for) {
                found = place + 1;
            }
        }
        return Some((WrittenValue::WholeNumber { length: 3 }, found.to_string()));
    }
    if named("ELT") {
        let [place, choices @ ..] = values.as_slice() else {
            return None;
        };
        let place = crate::translate::direct_signed_integer(place)?;
        let choices = choices
            .iter()
            .map(|choice| written_word(choice))
            .collect::<Option<Vec<_>>>()?;
        if choices.is_empty() {
            return None;
        }
        let characters = choices
            .iter()
            .map(|choice| choice.chars().count())
            .max()
            .and_then(|widest| u32::try_from(widest).ok())?;
        let chosen = usize::try_from(place)
            .ok()
            .and_then(|place| place.checked_sub(1))
            .and_then(|at| choices.get(at));
        return Some((
            WrittenValue::Text { characters },
            chosen.map_or_else(|| "NULL".to_owned(), |chosen| quoted(chosen)),
        ));
    }
    if named("TRUNCATE") {
        let [number, places] = values.as_slice() else {
            return None;
        };
        let places = crate::translate::direct_signed_integer(places)?;
        return written_truncation(number, places);
    }
    written_calendar_call(&name.value, &values)
}

/// Reads `MAKEDATE`, `FROM_DAYS`, `MAKETIME`, `PERIOD_DIFF` and `GET_FORMAT`
/// over written values. Measured on MySQL 8.4.11: `MAKEDATE(2026, 32)` is
/// 2026-02-01, a year under seventy read in this century and one under a
/// hundred in the last; `FROM_DAYS(739000)` is 2023-04-25; `MAKETIME(-1, 2, 3)`
/// is `-01:02:03`; `PERIOD_DIFF(7001, 6912)` is -1199, a two-digit year read
/// the way `MAKEDATE` reads one; and `GET_FORMAT(DATE, 'ISO')` is `%Y-%m-%d`.
/// A value MySQL answers NULL for, a zero day, or an error — `PERIOD_DIFF(0, 0)`
/// is 1210 — is refused.
fn written_calendar_call(name: &str, values: &[&Expr]) -> Option<(WrittenValue, String)> {
    let named = |candidate: &str| name.eq_ignore_ascii_case(candidate);
    let number = |expr: &Expr| crate::translate::direct_signed_integer(expr);
    if named("MAKEDATE") {
        let [year, day] = values else {
            return None;
        };
        let year = u32::try_from(number(year)?).ok()?;
        let year = match year {
            0..=69 => year + 2000,
            70..=99 => year + 1900,
            _ => year,
        };
        let day = u32::try_from(number(day)?).ok()?;
        let written = crate::date_format::day_of_year_written_out(year, day)?;
        return Some((WrittenValue::Day { not_null: false }, quoted(&written)));
    }
    if named("FROM_DAYS") {
        let [days] = values else {
            return None;
        };
        let written = crate::date_format::day_counted_from_the_year_zero(number(days)?)?;
        return Some((WrittenValue::Day { not_null: true }, quoted(&written)));
    }
    if named("MAKETIME") {
        let [hours, minutes, seconds] = values else {
            return None;
        };
        let (hours, minutes, seconds) = (number(hours)?, number(minutes)?, number(seconds)?);
        if hours.abs() > 838 || !(0..=59).contains(&minutes) || !(0..=59).contains(&seconds) {
            return None;
        }
        let sign = if hours < 0 { "-" } else { "" };
        let written = format!("{sign}{:02}:{minutes:02}:{seconds:02}", hours.abs());
        return Some((WrittenValue::Time, quoted(&written)));
    }
    if named("PERIOD_DIFF") {
        let [later, earlier] = values else {
            return None;
        };
        let months = |period: &Expr| -> Option<i64> {
            let period = number(period)?;
            let (year, month) = (period / 100, period % 100);
            if period <= 0 || !(1..=12).contains(&month) {
                return None;
            }
            let year = match year {
                0..=69 => year + 2000,
                70..=99 => year + 1900,
                _ => year,
            };
            Some(year * 12 + month)
        };
        let difference = months(later)? - months(earlier)?;
        return Some((
            WrittenValue::WholeNumber { length: 21 },
            difference.to_string(),
        ));
    }
    if named("GET_FORMAT") {
        let [Expr::Identifier(kind), standard] = values else {
            return None;
        };
        if kind.quote_style.is_some() {
            return None;
        }
        let standard = written_word(standard)?.to_ascii_uppercase();
        let kind = kind.value.to_ascii_uppercase();
        let format = match (kind.as_str(), standard.as_str()) {
            ("DATE", "USA") => "%m.%d.%Y",
            ("DATE", "JIS" | "ISO") => "%Y-%m-%d",
            ("DATE", "EUR") => "%d.%m.%Y",
            ("DATE", "INTERNAL") => "%Y%m%d",
            ("DATETIME" | "TIMESTAMP", "USA" | "EUR") => "%Y-%m-%d %H.%i.%s",
            ("DATETIME" | "TIMESTAMP", "JIS" | "ISO") => "%Y-%m-%d %H:%i:%s",
            ("DATETIME" | "TIMESTAMP", "INTERNAL") => "%Y%m%d%H%i%s",
            ("TIME", "USA") => "%h:%i:%s %p",
            ("TIME", "JIS" | "ISO") => "%H:%i:%s",
            ("TIME", "EUR") => "%H.%i.%s",
            ("TIME", "INTERNAL") => "%H%i%s",
            _ => return None,
        };
        return Some((WrittenValue::Text { characters: 17 }, quoted(format)));
    }
    None
}

/// Reads a written word, which is what most of these calls read.
fn written_word(expr: &Expr) -> Option<&str> {
    let Expr::Value(value) = expr else {
        return None;
    };
    match &value.value {
        Value::SingleQuotedString(word) | Value::DoubleQuotedString(word) => Some(word),
        _ => None,
    }
}

/// The 64 bits of a written whole number, a negative one as its two's
/// complement, which is how MySQL reads one for `HEX`, `BIN` and `OCT`.
fn written_whole_number_bits(expr: &Expr) -> Option<u64> {
    if let Some(number) = crate::translate::direct_signed_integer(expr) {
        return Some(number as u64);
    }
    let Expr::Value(value) = expr else {
        return None;
    };
    let Value::Number(digits, false) = &value.value else {
        return None;
    };
    digits.parse::<u64>().ok()
}

/// `TRUNCATE` over a written number. Measured on MySQL 8.4.11: over a whole
/// number it answers a `LONGLONG` of 21, `TRUNCATE(1567, -2)` being 1500, and
/// over a number with a point a `NEWDECIMAL` whose places are the ones asked
/// for held to the ones written — `TRUNCATE(1.567, 2)` is 1.56 with a length
/// of 5 and `TRUNCATE(1.5, 3)` 1.5 with one of 4. A count left of the point
/// over a number with a point is not taken.
fn written_truncation(number: &Expr, places: i64) -> Option<(WrittenValue, String)> {
    if let Some(whole) = crate::translate::direct_signed_integer(number) {
        let cut = match u32::try_from(-places) {
            Ok(digits) if places < 0 => {
                let unit = 10i64.checked_pow(digits).unwrap_or(i64::MAX);
                whole / unit * unit
            }
            _ => whole,
        };
        return Some((WrittenValue::WholeNumber { length: 21 }, cut.to_string()));
    }
    let places = u32::try_from(places).ok()?;
    let decimal = written_decimal(number)?;
    let (whole, fraction) = (decimal.whole.clone(), decimal.fraction.clone());
    if (whole.len() > 1 && whole.starts_with('0')) || fraction.is_empty() {
        return None;
    }
    let scale = places.min(u32::try_from(fraction.len()).ok()?);
    let cut = Decimal {
        negative: decimal.negative,
        whole: whole.clone(),
        fraction: fraction[..scale as usize].to_owned(),
    };
    // Measured: a zero written before the point counts as a digit and an
    // empty whole part as none — `TRUNCATE(0.567, 2)` reports 5 and
    // `TRUNCATE(.567, 2)` 4.
    let precision = u32::try_from(whole.len()).ok()? + scale;
    if precision == 0 || precision > DECIMAL_MOST_DIGITS {
        return None;
    }
    Some((
        WrittenValue::Decimal {
            precision,
            scale,
            not_null: true,
        },
        quoted(&cut.written()),
    ))
}

/// `CONVERT(x USING utf8mb4)` names the one character set this server speaks.
/// Another one would change the collation the answer carries.
fn names_utf8mb4(charset: &sqlparser::ast::ObjectName) -> bool {
    matches!(charset.0.as_slice(), [sqlparser::ast::ObjectNamePart::Identifier(name)]
        if name.quote_style.is_none() && name.value.eq_ignore_ascii_case("utf8mb4"))
}

/// Reads a number written with a point, which MySQL reads as a `DECIMAL`, or
/// with an exponent, which it reads as a `DOUBLE`. A whole number is read
/// elsewhere.
fn written_number(written: &str, negative: bool) -> Option<(WrittenValue, String)> {
    if written.contains(['e', 'E']) {
        return written_double(written, negative);
    }
    let (whole, fraction) = written.split_once('.')?;
    // Measured: MySQL counts a zero written before the point as a digit and
    // an empty whole part as none — `0.5` is two digits wide and `.5` one —
    // and counts leading zeroes by a rule of its own, `007.5` three digits
    // wide and `000.5` two, which is not taken.
    if (whole.len() > 1 && whole.starts_with('0'))
        || !whole
            .bytes()
            .chain(fraction.bytes())
            .all(|byte| byte.is_ascii_digit())
        || (whole.is_empty() && fraction.is_empty())
    {
        return None;
    }
    let precision = u32::try_from(whole.len() + fraction.len()).ok()?;
    let scale = u32::try_from(fraction.len()).ok()?;
    if precision > DECIMAL_MOST_DIGITS || scale > DECIMAL_MOST_PLACES {
        return None;
    }
    let shape = WrittenValue::Decimal {
        precision,
        scale,
        not_null: true,
    };
    let digits = Decimal {
        negative,
        whole: whole.to_owned(),
        fraction: fraction.to_owned(),
    };
    Some((shape, quoted(&digits.written())))
}

/// Measured: a `DOUBLE` written out answers as many characters as it was
/// written with, and a negative one the 23 every other `DOUBLE` answers.
fn written_double(written: &str, negative: bool) -> Option<(WrittenValue, String)> {
    let value = written.parse::<f64>().ok()?;
    // MySQL refuses a number past the largest double, and writes a negative
    // zero by a rule this has not measured.
    if !value.is_finite() || (negative && value == 0.0) {
        return None;
    }
    let length = if negative {
        23
    } else {
        u32::try_from(written.len()).ok()?
    };
    let value = if negative { -value } else { value };
    Some((WrittenValue::Double { length }, format!("{value:e}")))
}

/// Reads `0x41` and `X'41'`, which sqlparser does not tell apart and MySQL
/// answers alike. An odd count of digits is refused: `X'4'` is a syntax error
/// in MySQL where `0x4` is one byte.
fn written_hexadecimal(digits: &str) -> Option<(WrittenValue, String)> {
    if digits.len() % 2 != 0 || !digits.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return None;
    }
    let length = u32::try_from(digits.len() / 2).ok()?;
    Some((
        WrittenValue::Bytes {
            length,
            unsigned: true,
        },
        format!("x'{digits}'"),
    ))
}

/// Reads `b'101'`, whose bits are read as bytes from the right, the leftmost
/// byte taking whatever bits are left over.
fn written_bits(bits: &str) -> Option<(WrittenValue, String)> {
    if !bits.bytes().all(|byte| byte == b'0' || byte == b'1') {
        return None;
    }
    let padded = format!("{}{bits}", "0".repeat((8 - bits.len() % 8) % 8));
    let mut digits = String::with_capacity(padded.len() / 4);
    for byte in padded.as_bytes().chunks(8) {
        let byte = std::str::from_utf8(byte).expect("bits are ASCII");
        let byte = u8::from_str_radix(byte, 2).expect("eight bits make a byte");
        digits.push_str(&format!("{byte:02X}"));
    }
    let length = u32::try_from(padded.len() / 8).ok()?;
    Some((
        WrittenValue::Bytes {
            length,
            unsigned: false,
        },
        format!("x'{digits}'"),
    ))
}

/// Reads a cast of a written value to a type whose answer is worked out here.
fn written_cast(cast: &Expr, data_type: &DataType) -> Option<(WrittenValue, String)> {
    let null = matches!(cast, Expr::Value(value) if matches!(value.value, Value::Null));
    let word = match cast {
        Expr::Value(value) => match &value.value {
            Value::SingleQuotedString(word) | Value::DoubleQuotedString(word) => Some(word),
            _ => None,
        },
        _ => None,
    };
    match data_type {
        DataType::Decimal(size) => {
            let (precision, scale) = decimal_size(size)?;
            let shape = WrittenValue::Decimal {
                precision,
                scale,
                not_null: !null,
            };
            if null {
                return Some((shape, "NULL".to_owned()));
            }
            let cast = written_decimal(cast)?.rounded_to(scale);
            // MySQL holds a number too wide for the type to the widest one it
            // takes and warns, which is refused here.
            if cast.whole_digits() > precision - scale {
                return None;
            }
            Some((shape, quoted(&cast.written())))
        }
        // Measured on MySQL 8.4.11: a NOT NULL `LONGLONG` of 21 holding the
        // number, whichever of the two spellings. Only a whole number that
        // fits is taken: past the range MySQL answers another number without
        // a warning — `CAST(18446744073709551615 AS SIGNED)` is -1 — a
        // fraction is rounded, and a word that is no whole number warns.
        // `CAST(NULL AS SIGNED)`, nullable, is not taken either.
        DataType::Signed | DataType::SignedInteger => {
            let cast = written_decimal(cast)?;
            if !cast.fraction.is_empty() {
                return None;
            }
            let written = if cast.negative {
                format!("-{}", cast.whole)
            } else {
                cast.whole
            };
            let number = written.parse::<i64>().ok()?;
            Some((WrittenValue::WholeNumber { length: 21 }, number.to_string()))
        }
        // Measured: a word naming no day answers NULL and warns, so only one
        // naming a day is taken.
        DataType::Date => {
            if null {
                return Some((WrittenValue::Day { not_null: false }, "NULL".to_owned()));
            }
            Some((
                WrittenValue::Day { not_null: false },
                quoted(&normalize_date(word?)?),
            ))
        }
        DataType::Datetime(None) => {
            if null {
                return Some((WrittenValue::Moment, "NULL".to_owned()));
            }
            Some((WrittenValue::Moment, quoted(&normalize_datetime(word?)?)))
        }
        // Measured: a word that is no document is refused with 3141, which
        // this refuses too rather than answering it.
        DataType::JSON => {
            if null {
                return Some((WrittenValue::Json, "NULL".to_owned()));
            }
            let document = normalize_json(word?).ok()?;
            Some((WrittenValue::Json, quoted(&document)))
        }
        DataType::Char(None) | DataType::Character(None) => {
            if null {
                return Some((WrittenValue::Text { characters: 0 }, "NULL".to_owned()));
            }
            let word = word?;
            let characters = u32::try_from(word.chars().count()).ok()?;
            Some((WrittenValue::Text { characters }, quoted(word)))
        }
        _ => None,
    }
}

/// The digits and places a `DECIMAL` cast names. Measured: `DECIMAL` alone
/// is `DECIMAL(10,0)`, and `DECIMAL(p)` is `DECIMAL(p,0)`.
fn decimal_size(size: &ExactNumberInfo) -> Option<(u32, u32)> {
    let (precision, scale) = match size {
        ExactNumberInfo::None => (10, 0),
        ExactNumberInfo::Precision(precision) => (u32::try_from(*precision).ok()?, 0),
        ExactNumberInfo::PrecisionAndScale(precision, scale) => {
            (u32::try_from(*precision).ok()?, u32::try_from(*scale).ok()?)
        }
    };
    if precision == 0
        || precision > DECIMAL_MOST_DIGITS
        || scale > DECIMAL_MOST_PLACES
        || scale > precision
    {
        return None;
    }
    Some((precision, scale))
}

/// Reads a written whole number or a number written with a point, with its
/// sign.
fn written_decimal(expr: &Expr) -> Option<Decimal> {
    let (negative, written) = match expr {
        Expr::UnaryOp {
            op: op @ (UnaryOperator::Minus | UnaryOperator::Plus),
            expr: inner,
        } => (*op == UnaryOperator::Minus, inner.as_ref()),
        other => (false, other),
    };
    let Expr::Value(value) = written else {
        return None;
    };
    let Value::Number(written, false) = &value.value else {
        return None;
    };
    let (whole, fraction) = written.split_once('.').unwrap_or((written, ""));
    if !whole
        .bytes()
        .chain(fraction.bytes())
        .all(|byte| byte.is_ascii_digit())
        || (whole.is_empty() && fraction.is_empty())
    {
        return None;
    }
    Some(Decimal {
        negative,
        whole: whole.to_owned(),
        fraction: fraction.to_owned(),
    })
}

/// An exact number held as the digits it was written with.
struct Decimal {
    negative: bool,
    whole: String,
    fraction: String,
}

impl Decimal {
    /// Rounds to a count of places, half away from zero, the way MySQL
    /// rounds a `DECIMAL`, padding with zeroes where there are fewer.
    fn rounded_to(self, scale: u32) -> Decimal {
        let scale = scale as usize;
        let mut digits: Vec<u8> = self
            .whole
            .bytes()
            .chain(self.fraction.bytes())
            .chain(std::iter::repeat(b'0'))
            .take(self.whole.len() + scale.max(self.fraction.len()))
            .collect();
        let kept = self.whole.len() + scale;
        let rounds_up = digits.get(kept).is_some_and(|digit| *digit >= b'5');
        digits.truncate(kept);
        if rounds_up {
            let mut at = digits.len();
            loop {
                if at == 0 {
                    digits.insert(0, b'1');
                    break;
                }
                at -= 1;
                if digits[at] == b'9' {
                    digits[at] = b'0';
                } else {
                    digits[at] += 1;
                    break;
                }
            }
        }
        let split = digits.len() - scale;
        let (whole, fraction) = digits.split_at(split);
        Decimal {
            negative: self.negative,
            whole: String::from_utf8(whole.to_vec()).expect("digits are ASCII"),
            fraction: String::from_utf8(fraction.to_vec()).expect("digits are ASCII"),
        }
    }

    fn whole_digits(&self) -> u32 {
        self.whole.trim_start_matches('0').len() as u32
    }

    /// Writes the number the way MySQL writes a `DECIMAL`: one zero before a
    /// point with nothing ahead of it, no leading zeroes past that, and no
    /// sign on a zero.
    fn written(&self) -> String {
        let whole = self.whole.trim_start_matches('0');
        let whole = if whole.is_empty() { "0" } else { whole };
        let zero = whole == "0" && self.fraction.bytes().all(|digit| digit == b'0');
        let sign = if self.negative && !zero { "-" } else { "" };
        if self.fraction.is_empty() {
            format!("{sign}{whole}")
        } else {
            format!("{sign}{whole}.{}", self.fraction)
        }
    }
}

fn quoted(text: &str) -> String {
    format!("'{}'", text.replace('\'', "''"))
}
