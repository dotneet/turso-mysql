//! Values a projection writes out in full: a number with a point or an
//! exponent, a hexadecimal or bit literal, and a cast or a `CONVERT` of a
//! written value.
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
    /// A cast of a written day to `DATE`.
    Day,
    /// A cast of a written moment to `DATETIME`.
    Moment,
    /// A cast of a written document to `JSON`.
    Json,
    /// A written word cast to `CHAR` or converted to utf8mb4.
    Text { characters: u32 },
}

/// Reads a written value, answering its shape and the SQL the engine answers
/// MySQL's text with.
pub(crate) fn read_written_value(expr: &Expr) -> Option<(WrittenValue, String)> {
    match expr {
        Expr::Value(value) => match &value.value {
            Value::Number(written, false) => written_number(written, false),
            Value::HexStringLiteral(digits) => written_hexadecimal(digits),
            Value::SingleQuotedByteStringLiteral(bits) => written_bits(bits),
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
        // Measured: a word naming no day answers NULL and warns, so only one
        // naming a day is taken.
        DataType::Date => {
            if null {
                return Some((WrittenValue::Day, "NULL".to_owned()));
            }
            Some((WrittenValue::Day, quoted(&normalize_date(word?)?)))
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
