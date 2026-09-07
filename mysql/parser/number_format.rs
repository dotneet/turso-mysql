//! MySQL's `FORMAT`, which writes a number for a person to read.
//!
//! The engine has no grouping of any kind, so the whole of it is written here:
//! the number is rounded half away from zero, its integer part is grouped in
//! threes, and it always carries exactly the digits it was asked for.

/// The widest fraction MySQL's `FORMAT` writes. Measured on 8.4.11: asking for
/// forty answers thirty.
const MAX_FORMAT_DECIMALS: u32 = 30;

/// Writes a number the way MySQL's `FORMAT` writes it.
///
/// Measured on MySQL 8.4.11: `FORMAT(1234.5678, 2)` is `1,234.57`,
/// `FORMAT(2.5, 0)` is `3` — half goes away from zero, not to the even digit —
/// and `FORMAT(-0.4, 0)` is `0` rather than `-0`.
pub fn format_number(value: f64, decimals: u32) -> String {
    let decimals = decimals.min(MAX_FORMAT_DECIMALS) as usize;
    if !value.is_finite() {
        return value.to_string();
    }
    let magnitude = value.abs();
    // Rust's shortest form is the decimal a reader would have written, which is
    // what MySQL rounds; past the point where a double holds no fraction it
    // switches to an exponent, and there no digit can be exactly a half.
    let written = format!("{magnitude}");
    let rounded = if written.contains(['e', 'E']) {
        format!("{magnitude:.decimals$}")
    } else {
        round_away_from_zero(&written, decimals)
    };
    let (whole, fraction) = match rounded.split_once('.') {
        Some((whole, fraction)) => (whole, fraction),
        None => (rounded.as_str(), ""),
    };
    let negative =
        value.is_sign_negative() && rounded.bytes().any(|digit| digit != b'0' && digit != b'.');
    let mut out = String::with_capacity(whole.len() + whole.len() / 3 + fraction.len() + 2);
    if negative {
        out.push('-');
    }
    group_in_threes(whole, &mut out);
    if !fraction.is_empty() {
        out.push('.');
        out.push_str(fraction);
    }
    out
}

/// Rounds a written decimal to `decimals` places, half away from zero.
fn round_away_from_zero(written: &str, decimals: usize) -> String {
    let (whole, fraction) = match written.split_once('.') {
        Some((whole, fraction)) => (whole, fraction),
        None => (written, ""),
    };
    if fraction.len() <= decimals {
        let mut out = whole.to_owned();
        if decimals > 0 {
            out.push('.');
            out.push_str(fraction);
            for _ in fraction.len()..decimals {
                out.push('0');
            }
        }
        return out;
    }
    let mut digits: Vec<u8> = whole
        .bytes()
        .chain(fraction.bytes().take(decimals))
        .collect();
    let rounds_up = fraction.as_bytes()[decimals] >= b'5';
    if rounds_up {
        carry_one(&mut digits);
    }
    let whole_length = digits.len() - decimals;
    let mut out = String::from_utf8(digits[..whole_length].to_vec())
        .expect("a rounded decimal holds only ASCII digits");
    if decimals > 0 {
        out.push('.');
        out.push_str(
            std::str::from_utf8(&digits[whole_length..])
                .expect("a rounded decimal holds only ASCII digits"),
        );
    }
    out
}

/// Adds one to the last digit, carrying up through the rest.
fn carry_one(digits: &mut Vec<u8>) {
    for position in (0..digits.len()).rev() {
        if digits[position] == b'9' {
            digits[position] = b'0';
            continue;
        }
        digits[position] += 1;
        return;
    }
    digits.insert(0, b'1');
}

/// Writes the whole part with a comma between every three digits.
fn group_in_threes(whole: &str, out: &mut String) {
    for (position, digit) in whole.chars().enumerate() {
        if position > 0 && (whole.len() - position) % 3 == 0 {
            out.push(',');
        }
        out.push(digit);
    }
}

/// Cuts a number off at `decimals` places, the way MySQL's `TRUNCATE` does.
///
/// Measured on MySQL 8.4.11: `TRUNCATE(1.999, 2)` is `1.99` — nothing is
/// rounded — a negative count zeroes digits left of the point, so
/// `TRUNCATE(1234.5678, -2)` is `1200`, and the sign is kept as it was.
pub fn truncate_number(value: f64, decimals: i64) -> f64 {
    if !value.is_finite() || value == 0.0 {
        return value;
    }
    let magnitude = value.abs();
    // Cutting is done on the decimal a reader would have written rather than on
    // the double behind it: 0.29 times a hundred is 28.999999999999996, and
    // cutting that answers 0.28 where MySQL answers 0.29.
    let written = format!("{magnitude}");
    if written.contains(['e', 'E']) {
        return value;
    }
    let (whole, fraction) = match written.split_once('.') {
        Some((whole, fraction)) => (whole, fraction),
        None => (written.as_str(), ""),
    };
    let cut = if decimals >= 0 {
        let kept = usize::try_from(decimals)
            .unwrap_or(usize::MAX)
            .min(fraction.len());
        if kept == 0 {
            whole.to_owned()
        } else {
            format!("{whole}.{}", &fraction[..kept])
        }
    } else {
        let zeroed = usize::try_from(-decimals).unwrap_or(usize::MAX);
        if zeroed >= whole.len() {
            "0".to_owned()
        } else {
            let kept = whole.len() - zeroed;
            format!("{}{}", &whole[..kept], "0".repeat(zeroed))
        }
    };
    let cut: f64 = cut.parse().unwrap_or(magnitude);
    if value.is_sign_negative() {
        -cut
    } else {
        cut
    }
}

#[cfg(test)]
mod tests {
    use super::{format_number, truncate_number};

    /// Every answer here measured on MySQL 8.4.11 over a utf8mb4 connection.
    #[test]
    fn format_writes_what_mysql_writes() {
        for (value, decimals, written) in [
            (1234.5678, 2, "1,234.57"),
            (1234.5678, 0, "1,235"),
            (1234567.891, 4, "1,234,567.8910"),
            (-1234.5678, 2, "-1,234.57"),
            // Half goes away from zero rather than to the even digit.
            (0.5, 0, "1"),
            (1.5, 0, "2"),
            (2.5, 0, "3"),
            (-0.5, 0, "-1"),
            // A value that rounds to nothing loses its sign.
            (-0.4, 0, "0"),
            (0.005, 2, "0.01"),
            (1234.5678, 10, "1,234.5678000000"),
            (1000.0, 2, "1,000.00"),
            (123.0, 2, "123.00"),
            (0.0, 3, "0.000"),
            (-1234567.0, 2, "-1,234,567.00"),
            (1234567890123456.0, 0, "1,234,567,890,123,456"),
            (1e17, 2, "100,000,000,000,000,000.00"),
            // Asking for forty answers thirty.
            (1234.5678, 40, "1,234.567800000000000000000000000000"),
            // Carrying up through every digit.
            (9.99, 1, "10.0"),
            (999.5, 0, "1,000"),
        ] {
            assert_eq!(
                format_number(value, decimals),
                written,
                "FORMAT({value}, {decimals})"
            );
        }
    }

    /// Every answer here measured on MySQL 8.4.11 over a utf8mb4 connection.
    #[test]
    fn truncate_cuts_where_mysql_cuts() {
        for (value, decimals, cut) in [
            (1234.5678, 2, 1234.56),
            (1234.5678, 0, 1234.0),
            (-1234.5678, 2, -1234.56),
            // Nothing is rounded on the way.
            (1.999, 2, 1.99),
            // The double behind 0.29 is under it, and cutting the double would
            // answer 0.28.
            (0.29, 2, 0.29),
            (-0.29, 2, -0.29),
            // A negative count zeroes digits left of the point.
            (1234.5678, -2, 1200.0),
            (1234.0, -2, 1200.0),
            (-1234.5678, -2, -1200.0),
            (5.0, -1, 0.0),
            // Asking for more digits than there are leaves the number alone.
            (123.0, 5, 123.0),
            (19.0, 0, 19.0),
            (0.0, 2, 0.0),
        ] {
            assert_eq!(
                truncate_number(value, decimals),
                cut,
                "TRUNCATE({value}, {decimals})"
            );
        }
    }
}
