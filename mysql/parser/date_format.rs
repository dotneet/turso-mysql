//! MySQL's `DATE_FORMAT`, written out from the moment a column holds.
//!
//! The engine's `strftime` answers a few of MySQL's specifiers and none of the
//! rest — no month or weekday name, no twelve-hour clock, no week number — so
//! the whole of it is written here instead. Everything was measured on MySQL
//! 8.4.11.

use crate::temporal_value::{days_in_month, read_moment, Moment};

/// Writes a moment out in the format given, or answers nothing when the value
/// names no moment or the format names a specifier this does not write.
pub fn format_moment(written: &str, format: &str) -> Option<String> {
    let moment = read_moment(written)?;
    let mut out = String::with_capacity(format.len() * 2);
    let mut rest = format.chars();
    while let Some(character) = rest.next() {
        if character != '%' {
            out.push(character);
            continue;
        }
        let Some(specifier) = rest.next() else {
            // Measured: a trailing per-cent writes nothing at all.
            break;
        };
        write_specifier(specifier, &moment, &mut out)?;
    }
    Some(out)
}

/// How many characters MySQL reserves in a result column for one specifier.
///
/// Measured on 8.4.11 by reading the column width back one specifier at a
/// time: a `%H` reserves seven because a time may run past a day, a month or
/// weekday name reserves sixty-four, and a literal character reserves one.
pub fn format_width(format: &str) -> u32 {
    let mut width = 0;
    let mut rest = format.chars();
    while let Some(character) = rest.next() {
        if character != '%' {
            width += 1;
            continue;
        }
        let Some(specifier) = rest.next() else {
            break;
        };
        width += match specifier {
            'M' | 'W' => 64,
            'a' | 'b' => 32,
            'r' => 11,
            'T' => 8,
            'H' | 'k' => 7,
            'f' => 6,
            'Y' | 'X' | 'x' | 'D' => 4,
            'j' => 3,
            'w' | '%' => 1,
            'y' | 'm' | 'c' | 'd' | 'e' | 'h' | 'I' | 'l' | 'i' | 's' | 'S' | 'p' | 'U' | 'u'
            | 'V' | 'v' => 2,
            // An unknown specifier writes its own letter.
            _ => 1,
        };
    }
    width
}

pub(crate) const WEEKDAYS: [&str; 7] = [
    "Monday",
    "Tuesday",
    "Wednesday",
    "Thursday",
    "Friday",
    "Saturday",
    "Sunday",
];
pub(crate) const MONTHS: [&str; 12] = [
    "January",
    "February",
    "March",
    "April",
    "May",
    "June",
    "July",
    "August",
    "September",
    "October",
    "November",
    "December",
];

fn write_specifier(specifier: char, moment: &Moment, out: &mut String) -> Option<()> {
    let hour_on_a_clock = match moment.hour % 12 {
        0 => 12,
        hour => hour,
    };
    match specifier {
        'Y' => out.push_str(&format!("{:04}", moment.year)),
        'y' => out.push_str(&format!("{:02}", moment.year % 100)),
        'm' => out.push_str(&format!("{:02}", moment.month)),
        'c' => out.push_str(&moment.month.to_string()),
        'd' => out.push_str(&format!("{:02}", moment.day)),
        'e' => out.push_str(&moment.day.to_string()),
        'D' => out.push_str(&format!("{}{}", moment.day, day_suffix(moment.day))),
        'H' => out.push_str(&format!("{:02}", moment.hour)),
        'k' => out.push_str(&moment.hour.to_string()),
        'h' | 'I' => out.push_str(&format!("{hour_on_a_clock:02}")),
        'l' => out.push_str(&hour_on_a_clock.to_string()),
        'i' => out.push_str(&format!("{:02}", moment.minute)),
        's' | 'S' => out.push_str(&format!("{:02}", moment.second)),
        'p' => out.push_str(if moment.hour < 12 { "AM" } else { "PM" }),
        'r' => out.push_str(&format!(
            "{hour_on_a_clock:02}:{:02}:{:02} {}",
            moment.minute,
            moment.second,
            if moment.hour < 12 { "AM" } else { "PM" }
        )),
        'T' => out.push_str(&format!(
            "{:02}:{:02}:{:02}",
            moment.hour, moment.minute, moment.second
        )),
        'j' => out.push_str(&format!("{:03}", day_of_year(moment))),
        'W' => out.push_str(WEEKDAYS[weekday(moment, false) as usize]),
        'a' => out.push_str(&WEEKDAYS[weekday(moment, false) as usize][..3]),
        'M' => out.push_str(MONTHS[moment.month as usize - 1]),
        'b' => out.push_str(&MONTHS[moment.month as usize - 1][..3]),
        'w' => out.push_str(&weekday(moment, true).to_string()),
        // This holds whole seconds, so the fraction is always zero.
        'f' => out.push_str("000000"),
        'U' => out.push_str(&format!("{:02}", week(moment, 0).0)),
        'u' => out.push_str(&format!("{:02}", week(moment, 1).0)),
        'V' => out.push_str(&format!("{:02}", week(moment, 2).0)),
        'v' => out.push_str(&format!("{:02}", week(moment, 3).0)),
        'X' => out.push_str(&format!("{:04}", week(moment, 2).1)),
        'x' => out.push_str(&format!("{:04}", week(moment, 3).1)),
        '%' => out.push('%'),
        // Measured: an unknown specifier writes its own letter.
        other => out.push(other),
    }
    Some(())
}

/// Measured: `1st`, `2nd`, `3rd`, `4th`, and the teens are all `th`.
fn day_suffix(day: u32) -> &'static str {
    if (11..=13).contains(&(day % 100)) {
        return "th";
    }
    match day % 10 {
        1 => "st",
        2 => "nd",
        3 => "rd",
        _ => "th",
    }
}

fn day_of_year(moment: &Moment) -> u32 {
    (day_number(moment.year, moment.month, moment.day) - day_number(moment.year, 1, 1) + 1) as u32
}

/// The weekday, counting from Monday unless asked to count from Sunday, which
/// is what `%w` counts from.
fn weekday(moment: &Moment, from_sunday: bool) -> i64 {
    weekday_of(
        day_number(moment.year, moment.month, moment.day),
        from_sunday,
    )
}

fn weekday_of(day_number: i64, from_sunday: bool) -> i64 {
    (day_number + 5 + i64::from(from_sunday)) % 7
}

/// Days since the year zero, counting the way MySQL counts them.
fn day_number(year: u32, month: u32, day: u32) -> i64 {
    if year == 0 && month == 0 {
        return 0;
    }
    let mut whole_years = i64::from(year);
    let mut days = 365 * whole_years + 31 * (i64::from(month) - 1) + i64::from(day);
    if month <= 2 {
        whole_years -= 1;
    } else {
        days -= (i64::from(month) * 4 + 23) / 10;
    }
    days + whole_years / 4 - ((whole_years / 100 + 1) * 3) / 4
}

fn days_in_year(year: u32) -> i64 {
    if days_in_month(year, 2) == 29 {
        366
    } else {
        365
    }
}

/// The week number and the year it belongs to, in one of MySQL's four
/// countings.
///
/// The four are `%U`, `%u`, `%V` and `%v`, and they differ in whether a week
/// starts on Sunday or Monday and in whether the first week of a year is
/// numbered from zero or from one. `%X` and `%x` are the years of the last
/// two. This is MySQL's own reckoning, kept as it is because nothing simpler
/// answers the corners: measured, the first of January 2026 is week 00 of
/// 2026 by `%U` and week 52 of 2025 by `%V`.
fn week(moment: &Moment, mode: u32) -> (u32, u32) {
    let mode = if mode & 1 == 0 {
        (mode & 7) ^ 4
    } else {
        mode & 7
    };
    let monday_first = mode & 1 != 0;
    let mut week_is_a_year_of_its_own = mode & 2 != 0;
    let first_weekday = mode & 4 != 0;

    let day = day_number(moment.year, moment.month, moment.day);
    let mut first_day = day_number(moment.year, 1, 1);
    let mut year = moment.year;
    let mut weekday = weekday_of(first_day, !monday_first);

    if moment.month == 1 && i64::from(moment.day) <= 7 - weekday {
        if !week_is_a_year_of_its_own
            && ((first_weekday && weekday != 0) || (!first_weekday && weekday >= 4))
        {
            return (0, year);
        }
        week_is_a_year_of_its_own = true;
        year -= 1;
        let days = days_in_year(year);
        first_day -= days;
        weekday = (weekday + 53 * 7 - days) % 7;
    }

    let days = if (first_weekday && weekday != 0) || (!first_weekday && weekday >= 4) {
        day - (first_day + (7 - weekday))
    } else {
        day - (first_day - weekday)
    };

    if week_is_a_year_of_its_own && days >= 52 * 7 {
        let weekday = (weekday + days_in_year(year)) % 7;
        if (!first_weekday && weekday < 4) || (first_weekday && weekday == 0) {
            return (1, year + 1);
        }
    }
    ((days / 7 + 1) as u32, year)
}

#[cfg(test)]
mod tests {
    use super::{format_moment, format_width};

    /// Every reading measured on MySQL 8.4.11.
    #[test]
    fn a_moment_writes_out_the_way_mysql_writes_it() {
        let moment = "2026-09-06 01:02:03";
        for (format, written) in [
            ("%Y", "2026"),
            ("%y", "26"),
            ("%m", "09"),
            ("%c", "9"),
            ("%d", "06"),
            ("%e", "6"),
            ("%D", "6th"),
            ("%H", "01"),
            ("%k", "1"),
            ("%h", "01"),
            ("%I", "01"),
            ("%l", "1"),
            ("%i", "02"),
            ("%s", "03"),
            ("%S", "03"),
            ("%p", "AM"),
            ("%r", "01:02:03 AM"),
            ("%T", "01:02:03"),
            ("%j", "249"),
            ("%W", "Sunday"),
            ("%a", "Sun"),
            ("%M", "September"),
            ("%b", "Sep"),
            ("%w", "0"),
            ("%f", "000000"),
            ("%%", "%"),
            // An unknown specifier writes its own letter.
            ("%Z", "Z"),
            ("%Y-%m-%d", "2026-09-06"),
            ("%Y-%m-%d %H:%i:%s", "2026-09-06 01:02:03"),
            ("%d/%m/%Y", "06/09/2026"),
            ("on %Y", "on 2026"),
            ("%Y%m%d", "20260906"),
        ] {
            assert_eq!(
                format_moment(moment, format).as_deref(),
                Some(written),
                "{format}"
            );
        }
    }

    #[test]
    fn the_clock_reads_twelve_at_midnight_and_at_noon() {
        assert_eq!(
            format_moment("2026-01-01 00:00:00", "%h %I %l %p %r").as_deref(),
            Some("12 12 12 AM 12:00:00 AM")
        );
        assert_eq!(
            format_moment("2024-02-29 12:00:00", "%h %l %p").as_deref(),
            Some("12 12 PM")
        );
        assert_eq!(
            format_moment("2026-01-23 15:00:00", "%h %l %p").as_deref(),
            Some("03 3 PM")
        );
    }

    #[test]
    fn the_day_of_the_month_carries_the_suffix_mysql_writes() {
        for (day, written) in [
            ("2026-09-01", "1st"),
            ("2026-09-02", "2nd"),
            ("2026-09-03", "3rd"),
            ("2026-09-04", "4th"),
            ("2026-09-11", "11th"),
            ("2026-09-12", "12th"),
            ("2026-09-13", "13th"),
            ("2026-09-21", "21st"),
            ("2026-09-22", "22nd"),
            ("2026-09-23", "23rd"),
            ("2026-09-30", "30th"),
        ] {
            assert_eq!(format_moment(day, "%D").as_deref(), Some(written), "{day}");
        }
    }

    /// The four week countings differ at the turn of a year, which is where
    /// they were measured.
    #[test]
    fn a_week_is_numbered_four_ways_and_they_part_at_a_new_year() {
        for (moment, written) in [
            ("2026-09-06 01:02:03", "36 36 36 36 2026 2026"),
            ("2026-01-01 00:00:00", "00 01 52 01 2025 2026"),
            ("2026-12-31 23:59:59", "52 53 52 53 2026 2026"),
            ("2024-02-29 12:00:00", "08 09 08 09 2024 2024"),
            ("2001-01-02 11:11:11", "00 01 53 01 2000 2001"),
            ("2026-11-02 09:08:07", "44 45 44 45 2026 2026"),
        ] {
            assert_eq!(
                format_moment(moment, "%U %u %V %v %X %x").as_deref(),
                Some(written),
                "{moment}"
            );
        }
    }

    #[test]
    fn a_day_alone_writes_a_time_of_midnight() {
        assert_eq!(
            format_moment("2026-09-06", "%Y-%m-%d %H:%i:%s").as_deref(),
            Some("2026-09-06 00:00:00")
        );
        assert_eq!(format_moment("not a moment", "%Y"), None);
    }

    /// Measured by reading a result column's width back one specifier at a
    /// time, in characters before utf8mb4's four bytes are counted.
    #[test]
    fn a_format_reserves_the_width_mysql_reserves() {
        for (format, width) in [
            ("%Y", 4),
            ("%y", 2),
            ("%H", 7),
            ("%k", 7),
            ("%r", 11),
            ("%T", 8),
            ("%W", 64),
            ("%M", 64),
            ("%a", 32),
            ("%b", 32),
            ("%D", 4),
            ("%j", 3),
            ("%w", 1),
            ("%f", 6),
            ("%%", 1),
            ("%Z", 1),
            ("x", 1),
            ("%Y-%m-%d", 10),
            ("%Y-%m-%d %H:%i:%s", 24),
        ] {
            assert_eq!(format_width(format), width, "{format}");
        }
    }
}
