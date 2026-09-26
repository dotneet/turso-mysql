use chrono::{Duration, NaiveDateTime};

/// Shifts one stored-form moment by a fixed session offset in seconds.
///
/// The fractional digits are carried unchanged: chrono's calendar arithmetic
/// works on whole seconds here, so a six-digit MySQL value stays six digits.
pub fn shift_timestamp(written: &str, offset_seconds: i32) -> Option<String> {
    let (whole, fraction) = match written.split_once('.') {
        Some((whole, fraction))
            if !fraction.is_empty()
                && fraction.len() <= 6
                && fraction.bytes().all(|digit| digit.is_ascii_digit()) =>
        {
            (whole, Some(fraction))
        }
        Some(_) => return None,
        None => (written, None),
    };
    let moment = NaiveDateTime::parse_from_str(whole, "%Y-%m-%d %H:%M:%S").ok()?;
    let shifted = moment.checked_add_signed(Duration::seconds(i64::from(offset_seconds)))?;
    let mut rendered = shifted.format("%Y-%m-%d %H:%M:%S").to_string();
    if let Some(fraction) = fraction {
        rendered.push('.');
        rendered.push_str(fraction);
    }
    Some(rendered)
}

#[cfg(test)]
mod tests {
    use super::shift_timestamp;

    #[test]
    fn fixed_offset_preserves_microseconds_across_a_day_boundary() {
        assert_eq!(
            shift_timestamp("2024-01-01 03:00:00.130000", 9 * 3600).as_deref(),
            Some("2024-01-01 12:00:00.130000")
        );
        assert_eq!(
            shift_timestamp("2024-01-01 03:00:00.130000", -4 * 3600).as_deref(),
            Some("2023-12-31 23:00:00.130000")
        );
    }
}
