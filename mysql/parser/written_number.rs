//! Reads a written word as the number it names, for a word compared against a
//! column holding numbers.
//!
//! PHP applications send every value as a quoted word: WordPress's
//! `$wpdb->prepare('%s')` and PDO with emulated prepares write `WHERE id = '1'`.
//! MySQL reads the word as a number there. Only the words it reads as exactly
//! one number, with no warning, are read here: measured on MySQL 8.4.11, `'30'`,
//! `'+3'`, `'03'` and `'-0'` against an `INT` find the rows holding 30, 3, 3
//! and 0, and `'9007199254740993'` against a `BIGINT` finds that row and not
//! the one holding 9007199254740992, so the comparison is exact rather than
//! one between doubles. Against a `DECIMAL`, `'10.5'` and `'007.00'` are
//! exact too: `balance = '10.500000000000000001'` finds nothing.
//!
//! Every other word is left alone and stays refused: `' 30'` and `'3e1'` name
//! a number MySQL reads without a warning but this does not spell out, and
//! `'30abc'` and `'abc'` are read with warning 1292.

/// A number a word names exactly.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WrittenNumber {
    /// A whole number an `i64` holds.
    Whole(i64),
    /// A number written with a point, spelled with no `+` and no leading
    /// zeroes before the point, which a `DECIMAL` compares exactly.
    Decimal(String),
}

/// The most digits a MySQL `DECIMAL` holds, and the most of them after the
/// point.
const MOST_DECIMAL_DIGITS: usize = 65;
const MOST_DECIMAL_PLACES: usize = 30;

/// Reads a word as the number it names, or nothing when it names none exactly:
/// an optional sign, digits, and for a `DECIMAL` a point followed by digits,
/// with nothing around them.
pub fn read_written_number(word: &str) -> Option<WrittenNumber> {
    let (negative, unsigned) = match word.as_bytes().first()? {
        b'-' => (true, &word[1..]),
        b'+' => (false, &word[1..]),
        _ => (false, word),
    };
    let (whole, places) = match unsigned.split_once('.') {
        Some((whole, places)) => (whole, Some(places)),
        None => (unsigned, None),
    };
    let digits = |part: &str| !part.is_empty() && part.bytes().all(|byte| byte.is_ascii_digit());
    if !digits(whole) {
        return None;
    }
    let Some(places) = places else {
        let magnitude = whole.parse::<u64>().ok()?;
        return Some(WrittenNumber::Whole(if negative {
            0i64.checked_sub_unsigned(magnitude)?
        } else {
            i64::try_from(magnitude).ok()?
        }));
    };
    if !digits(places) {
        return None;
    }
    let whole = whole.trim_start_matches('0');
    let whole = if whole.is_empty() { "0" } else { whole };
    if places.len() > MOST_DECIMAL_PLACES || whole.len() + places.len() > MOST_DECIMAL_DIGITS {
        return None;
    }
    Some(WrittenNumber::Decimal(format!(
        "{}{whole}.{places}",
        if negative { "-" } else { "" }
    )))
}

/// The whole number MySQL stores in an integer column for a value, before
/// the column's range is checked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WholeNumber {
    Within(i128),
    /// Too far from zero for any integer column to hold.
    Past {
        negative: bool,
    },
}

/// Reads a word as the whole number MySQL stores for it in an integer column,
/// or nothing where MySQL refuses it — 1366 for a word naming no number,
/// 1265 for one with more after the number, as
/// [`whole_number_mysql_reads_from`] reads them.
pub fn whole_number_a_word_names(word: &str) -> Option<WholeNumber> {
    match whole_number_mysql_reads_from(word) {
        WordAsWholeNumber::Number { whole, cut: false } => Some(whole),
        _ => None,
    }
}

/// How MySQL reads a word written into a column of whole numbers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WordAsWholeNumber {
    /// The word starts with no number, which MySQL answers 1366.
    NoNumber,
    /// The number the word starts with, rounded, and whether anything but
    /// white space follows it, which MySQL answers 1265 once the number fits
    /// the column.
    Number { whole: WholeNumber, cut: bool },
    /// A word MySQL reads by a rule not repeated here.
    Unread,
}

/// Reads a word as MySQL reads it into a column of whole numbers.
///
/// Measured on MySQL 8.4.11 in strict mode: spaces and tabs before the number
/// are passed over, where a newline or a carriage return is 1366, and spaces,
/// tabs, newlines and carriage returns after it are passed over. Anything else
/// after it is 1265 unless the number is past the column's range, which is
/// 1264 — `'7x'`, `'1.5x'`, `'1,5'` and `'0x10'` are 1265 and `'1e19x'` is
/// 1264. A sign, a point and an exponent are read, and the number is rounded
/// half away from zero: `' 7.5 '` stores 8, `'.5'` stores 1 and `'2.5e0'`
/// stores 3. An `e` with no digits after it is passed over, `'1e'` storing 1,
/// and one followed by a bare sign that ends the word reads every digit as if
/// there were no point: `'2.5e+'` stores 25 and `'12.345e-'` stores 12345.
pub fn whole_number_mysql_reads_from(word: &str) -> WordAsWholeNumber {
    let bytes = word.as_bytes();
    let mut at = bytes
        .iter()
        .position(|byte| !matches!(byte, b' ' | b'\t'))
        .unwrap_or(bytes.len());
    let negative = match bytes.get(at) {
        Some(b'-') => {
            at += 1;
            true
        }
        Some(b'+') => {
            at += 1;
            false
        }
        _ => false,
    };
    let whole_digits = digits_at(word, &mut at);
    let places = if bytes.get(at) == Some(&b'.') {
        at += 1;
        digits_at(word, &mut at)
    } else {
        ""
    };
    if whole_digits.is_empty() && places.is_empty() {
        return WordAsWholeNumber::NoNumber;
    }
    let mut exponent: i64 = 0;
    if matches!(bytes.get(at), Some(b'e' | b'E')) {
        at += 1;
        let mut exponent_is_negative = false;
        if let Some(sign @ (b'+' | b'-')) = bytes.get(at) {
            exponent_is_negative = *sign == b'-';
            at += 1;
            if at == bytes.len() {
                return digits_read_without_their_point(negative, whole_digits, places);
            }
        }
        for digit in digits_at(word, &mut at).bytes() {
            exponent = exponent
                .saturating_mul(10)
                .saturating_add(i64::from(digit - b'0'));
        }
        if exponent_is_negative {
            exponent = -exponent;
        }
    }
    let cut = !bytes[at..]
        .iter()
        .all(|byte| matches!(byte, b' ' | b'\t' | b'\n' | b'\r'));
    WordAsWholeNumber::Number {
        whole: rounded_half_away_from_zero(negative, whole_digits, places, exponent),
        cut,
    }
}

/// The run of digits starting at `at`, moving `at` past it.
fn digits_at<'a>(word: &'a str, at: &mut usize) -> &'a str {
    let start = *at;
    while word.as_bytes().get(*at).is_some_and(u8::is_ascii_digit) {
        *at += 1;
    }
    &word[start..*at]
}

/// The digits of a number read whole, the point among them passed over,
/// which is how MySQL reads a number whose exponent is a bare sign ending the
/// word. Measured past eighteen digits MySQL answers yet another number —
/// `'1234567890.123456789e+'` stores 1234567890123456800 — which is not
/// repeated here.
fn digits_read_without_their_point(
    negative: bool,
    whole_digits: &str,
    places: &str,
) -> WordAsWholeNumber {
    let written = format!("{whole_digits}{places}");
    let digits = written.trim_start_matches('0');
    if digits.len() > 18 {
        return WordAsWholeNumber::Unread;
    }
    let magnitude: i128 = if digits.is_empty() {
        0
    } else {
        digits
            .parse()
            .expect("at most eighteen digits are a number")
    };
    WordAsWholeNumber::Number {
        whole: WholeNumber::Within(if negative { -magnitude } else { magnitude }),
        cut: false,
    }
}

/// The number written as `whole_digits.places` times ten to `exponent`,
/// rounded half away from zero.
fn rounded_half_away_from_zero(
    negative: bool,
    whole_digits: &str,
    places: &str,
    exponent: i64,
) -> WholeNumber {
    let written = format!("{whole_digits}{places}");
    let digits = written.trim_start_matches('0');
    let leading_zeroes = (written.len() - digits.len()) as i64;
    // How many of `digits` stand before the point.
    let point = (whole_digits.len() as i64)
        .saturating_add(exponent)
        .saturating_sub(leading_zeroes);
    if digits.is_empty() || point < 0 {
        return WholeNumber::Within(0);
    }
    if point > 38 {
        return WholeNumber::Past { negative };
    }
    let point = point as usize;
    let whole = &digits[..point.min(digits.len())];
    let mut magnitude: i128 = if whole.is_empty() {
        0
    } else {
        whole.parse().expect("at most 38 digits are a number")
    };
    magnitude *= 10_i128.pow((point.saturating_sub(digits.len())) as u32);
    if digits
        .as_bytes()
        .get(point)
        .is_some_and(|digit| *digit >= b'5')
    {
        magnitude += 1;
    }
    WholeNumber::Within(if negative { -magnitude } else { magnitude })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_word_is_rounded_half_away_from_zero_into_a_whole_number() {
        for (word, whole) in [
            ("2.5", 3),
            ("3.5", 4),
            ("-0.5", -1),
            ("-0.4", 0),
            (" 7.5 ", 8),
            ("  12  ", 12),
            (".5", 1),
            ("5.", 5),
            ("+5", 5),
            ("1e3", 1000),
            ("2.5e0", 3),
            ("-1.5e1", -15),
            ("0.49999999999999999999", 0),
            ("2147483647.5", 2_147_483_648),
            ("0.05", 0),
            ("1e-1", 0),
            ("15e-1", 2),
            ("007", 7),
            ("1e", 1),
        ] {
            assert_eq!(
                whole_number_a_word_names(word),
                Some(WholeNumber::Within(whole)),
                "{word:?}"
            );
        }
        assert_eq!(
            whole_number_a_word_names("1e40"),
            Some(WholeNumber::Past { negative: false })
        );
        assert_eq!(
            whole_number_a_word_names("-1e40"),
            Some(WholeNumber::Past { negative: true })
        );
        for word in ["", " ", "abc", "-", ".", "7x", "0x10", "1.2.3"] {
            assert_eq!(whole_number_a_word_names(word), None, "{word:?}");
        }
    }

    /// Every reading here was measured on MySQL 8.4.11 in strict mode, a word
    /// written into an `INT`.
    #[test]
    fn a_word_is_read_into_a_whole_number_as_mysql_reads_it() {
        let number = |whole: i128, cut: bool| WordAsWholeNumber::Number {
            whole: WholeNumber::Within(whole),
            cut,
        };
        for (word, read) in [
            (" 7", number(7, false)),
            ("7 ", number(7, false)),
            ("\t7\t", number(7, false)),
            (" \t 7 \t ", number(7, false)),
            ("7\n", number(7, false)),
            ("7\r", number(7, false)),
            (" -7", number(-7, false)),
            ("1e", number(1, false)),
            ("1e+", number(1, false)),
            ("1e-", number(1, false)),
            ("1.e3", number(1000, false)),
            ("+.5", number(1, false)),
            ("7.", number(7, false)),
            ("00007", number(7, false)),
            ("1e-400", number(0, false)),
            ("1.5e", number(2, false)),
            ("2.5e+ ", number(3, false)),
            ("2.5e+", number(25, false)),
            ("2.56e+", number(256, false)),
            ("2.5e-", number(25, false)),
            ("-2.5e+", number(-25, false)),
            ("0.5e+", number(5, false)),
            ("12.345e+", number(12345, false)),
            ("-0.4e+", number(-4, false)),
            ("7x", number(7, true)),
            ("7 x", number(7, true)),
            ("7 \n x", number(7, true)),
            ("1.5x", number(2, true)),
            ("2.5 x", number(3, true)),
            ("-.5x", number(-1, true)),
            ("-0.4x", number(0, true)),
            ("0x10", number(0, true)),
            ("1,5", number(1, true)),
            ("1_000", number(1, true)),
            ("1e3x", number(1000, true)),
            ("1e 3", number(1, true)),
            ("1 e3", number(1, true)),
            ("1ex", number(1, true)),
            ("2.5e+x", number(3, true)),
            ("7e0x", number(7, true)),
            ("7\u{0b}", number(7, true)),
            ("7\u{0c}", number(7, true)),
            ("7\0", number(7, true)),
        ] {
            assert_eq!(whole_number_mysql_reads_from(word), read, "{word:?}");
        }
        for word in [
            "", "   ", "\n7", "\r7", "\u{0b}7", "- 7", "++7", "+-7", ".", ".e3", "e3", "x7",
            "\u{ff17}",
        ] {
            assert_eq!(
                whole_number_mysql_reads_from(word),
                WordAsWholeNumber::NoNumber,
                "{word:?}"
            );
        }
        assert_eq!(
            whole_number_mysql_reads_from("1e400"),
            WordAsWholeNumber::Number {
                whole: WholeNumber::Past { negative: false },
                cut: false
            }
        );
        assert_eq!(
            whole_number_mysql_reads_from("1234567890.123456789e+"),
            WordAsWholeNumber::Unread
        );
    }

    #[test]
    fn a_word_names_a_number_only_when_it_is_spelled_as_one() {
        assert_eq!(read_written_number("30"), Some(WrittenNumber::Whole(30)));
        assert_eq!(read_written_number("+3"), Some(WrittenNumber::Whole(3)));
        assert_eq!(read_written_number("03"), Some(WrittenNumber::Whole(3)));
        assert_eq!(read_written_number("-0"), Some(WrittenNumber::Whole(0)));
        assert_eq!(
            read_written_number("-9223372036854775808"),
            Some(WrittenNumber::Whole(i64::MIN))
        );
        assert_eq!(read_written_number("9223372036854775808"), None);
        assert_eq!(
            read_written_number("007.00"),
            Some(WrittenNumber::Decimal("7.00".to_owned()))
        );
        assert_eq!(
            read_written_number("-0.5"),
            Some(WrittenNumber::Decimal("-0.5".to_owned()))
        );
        for word in [
            "", "-", "+", " 30", "30 ", "3e1", "30abc", "abc", ".5", "5.", "1.2.3", "0x1E",
        ] {
            assert_eq!(read_written_number(word), None, "{word:?}");
        }
        assert_eq!(read_written_number(&format!("1.{}", "0".repeat(31))), None);
    }
}
