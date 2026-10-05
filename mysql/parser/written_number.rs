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
/// or nothing where MySQL answers 1366.
///
/// Measured on MySQL 8.4.11 in strict mode: spaces around the number are
/// passed over, a sign, a point and an exponent are read, and the number is
/// rounded half away from zero exactly — `' 7.5 '` stores 8, `'-0.5'` stores
/// -1, `'.5'` stores 1, `'2.5e0'` stores 3 and `'1e3'` stores 1000.
pub fn whole_number_a_word_names(word: &str) -> Option<WholeNumber> {
    let word = word.trim_matches(' ');
    let (negative, unsigned) = match word.as_bytes().first() {
        Some(b'-') => (true, &word[1..]),
        Some(b'+') => (false, &word[1..]),
        _ => (false, word),
    };
    let (mantissa, exponent) = match unsigned.find(['e', 'E']) {
        Some(at) => (&unsigned[..at], unsigned[at + 1..].parse::<i64>().ok()?),
        None => (unsigned, 0),
    };
    let (whole_digits, places) = mantissa.split_once('.').unwrap_or((mantissa, ""));
    if whole_digits.is_empty() && places.is_empty()
        || !whole_digits.bytes().all(|byte| byte.is_ascii_digit())
        || !places.bytes().all(|byte| byte.is_ascii_digit())
    {
        return None;
    }
    let written = format!("{whole_digits}{places}");
    let digits = written.trim_start_matches('0');
    let leading_zeroes = i64::try_from(written.len() - digits.len()).ok()?;
    // How many of `digits` stand before the point.
    let point = i64::try_from(whole_digits.len())
        .ok()?
        .saturating_add(exponent)
        .saturating_sub(leading_zeroes);
    if digits.is_empty() || point < 0 {
        return Some(WholeNumber::Within(0));
    }
    if point > 38 {
        return Some(WholeNumber::Past { negative });
    }
    let point = usize::try_from(point).ok()?;
    let whole = &digits[..point.min(digits.len())];
    let mut magnitude: i128 = if whole.is_empty() {
        0
    } else {
        whole.parse().ok()?
    };
    magnitude *= 10_i128.pow(u32::try_from(point.saturating_sub(digits.len())).ok()?);
    if digits
        .as_bytes()
        .get(point)
        .is_some_and(|digit| *digit >= b'5')
    {
        magnitude += 1;
    }
    Some(WholeNumber::Within(if negative {
        -magnitude
    } else {
        magnitude
    }))
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
        for word in ["", " ", "abc", "-", ".", "1e", "7x", "0x10", "1.2.3"] {
            assert_eq!(whole_number_a_word_names(word), None, "{word:?}");
        }
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
