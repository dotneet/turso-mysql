// Copyright 2026 the Turso authors. All rights reserved. MIT license.

//! Counting the time between two moments the way MySQL's `TIMESTAMPDIFF` and
//! `DATEDIFF` count it.
//!
//! The engine has no calendar count of months, and its `unixepoch` drops the
//! fraction of a second a `DATETIME(3)` holds before subtracting — measured on
//! MySQL 8.4.11, `10:00:00.5` to `10:00:01.4` is 0 seconds, where subtracting
//! whole seconds answers 1. So both counts are worked out here instead and the
//! rendered SQL calls them.

use crate::shift_moment::days_from_civil;
use crate::temporal_value::{read_moment_to_the_microsecond, Moment};

/// How many whole `unit`s lie from `from` to `to`, negative when `to` is the
/// earlier of the two.
///
/// `None` says MySQL answers NULL: a value that names no moment, or a unit
/// this does not count in.
///
/// Measured on MySQL 8.4.11, whatever is left over is dropped, towards zero
/// either way: one second short of a day is 0 days, and 2 days and a half
/// backwards is −2. A month, a quarter and a year are counted by the calendar
/// rather than by a length — see [`months_between`].
pub fn units_between(unit: &str, from: &str, to: &str) -> Option<i64> {
    let from = read_moment_to_the_microsecond(from)?;
    let to = read_moment_to_the_microsecond(to)?;
    let months_in_each = match unit {
        "month" => Some(1),
        "quarter" => Some(3),
        "year" => Some(12),
        _ => None,
    };
    if let Some(months_in_each) = months_in_each {
        return Some(months_between(&from, &to) / months_in_each);
    }
    let microseconds_in_each: i64 = match unit {
        "microsecond" => 1,
        "second" => 1_000_000,
        "minute" => 60_000_000,
        "hour" => 3_600_000_000,
        "day" => 86_400_000_000,
        "week" => 604_800_000_000,
        _ => return None,
    };
    let between = microseconds_since_the_epoch(&to)? - microseconds_since_the_epoch(&from)?;
    Some(between / microseconds_in_each)
}

/// How many days lie from the day `earlier` falls on to the day `later` falls
/// on, which is `DATEDIFF(later, earlier)`.
///
/// Measured on MySQL 8.4.11, only the days count: `2024-01-15 10:00:00` to
/// `2024-02-15 09:59:59` is 31 days here where `TIMESTAMPDIFF` counts 30.
pub fn days_between(later: &str, earlier: &str) -> Option<i64> {
    let (later, _) = read_moment_to_the_microsecond(later)?;
    let (earlier, _) = read_moment_to_the_microsecond(earlier)?;
    Some(
        days_from_civil(later.year, later.month, later.day)?
            - days_from_civil(earlier.year, earlier.month, earlier.day)?,
    )
}

fn microseconds_since_the_epoch((moment, microseconds): &(Moment, u32)) -> Option<i64> {
    let days = days_from_civil(moment.year, moment.month, moment.day)?;
    let seconds = days * 86_400
        + i64::from(moment.hour) * 3_600
        + i64::from(moment.minute) * 60
        + i64::from(moment.second);
    Some(seconds * 1_000_000 + i64::from(*microseconds))
}

/// Whole months from one moment to the other, by MySQL's calendar rule.
///
/// Measured on MySQL 8.4.11: a month is whole once the later moment has
/// reached the same day of the month and the same time of day, so
/// `2024-01-31` to `2024-02-29` is 0 months — February has no 31st to reach —
/// and `2024-01-15 10:00:00.5` to `2024-02-15 10:00:00.4` is 0 as well. The
/// count runs the same way backwards, with the sign turned round.
fn months_between(from: &(Moment, u32), to: &(Moment, u32)) -> i64 {
    let backwards = time_order(to) < time_order(from);
    let ((earlier, earlier_fraction), (later, later_fraction)) =
        if backwards { (to, from) } else { (from, to) };
    let month_not_yet_reached =
        later.month < earlier.month || (later.month == earlier.month && later.day < earlier.day);
    let mut years = i64::from(later.year) - i64::from(earlier.year);
    if month_not_yet_reached {
        years -= 1;
    }
    let mut months = 12 * years;
    if month_not_yet_reached {
        months += 12 - (i64::from(earlier.month) - i64::from(later.month));
    } else {
        months += i64::from(later.month) - i64::from(earlier.month);
    }
    let later_time = (later.hour, later.minute, later.second, *later_fraction);
    let earlier_time = (
        earlier.hour,
        earlier.minute,
        earlier.second,
        *earlier_fraction,
    );
    if later.day < earlier.day || (later.day == earlier.day && later_time < earlier_time) {
        months -= 1;
    }
    if backwards {
        -months
    } else {
        months
    }
}

fn time_order((moment, fraction): &(Moment, u32)) -> (u32, u32, u32, u32, u32, u32, u32) {
    (
        moment.year,
        moment.month,
        moment.day,
        moment.hour,
        moment.minute,
        moment.second,
        *fraction,
    )
}

#[cfg(test)]
mod tests {
    use super::{days_between, units_between};

    /// Every count measured on MySQL 8.4.11 over `DATETIME(6)` columns.
    #[test]
    fn counts_whole_units_the_way_mysql_counts_them() {
        for (from, to, month, quarter, year, second, microsecond, day, week) in [
            (
                "2024-01-31 00:00:00",
                "2024-02-29 00:00:00",
                0,
                0,
                0,
                2505600,
                2505600000000,
                29,
                4,
            ),
            (
                "2024-01-31 00:00:00",
                "2024-03-01 00:00:00",
                1,
                0,
                0,
                2592000,
                2592000000000,
                30,
                4,
            ),
            (
                "2024-01-15 10:00:00",
                "2024-02-15 09:59:59",
                0,
                0,
                0,
                2678399,
                2678399000000,
                30,
                4,
            ),
            (
                "2024-01-15 10:00:00",
                "2024-02-15 10:00:00",
                1,
                0,
                0,
                2678400,
                2678400000000,
                31,
                4,
            ),
            (
                "2024-01-15 10:00:00.5",
                "2024-02-15 10:00:00.4",
                0,
                0,
                0,
                2678399,
                2678399900000,
                30,
                4,
            ),
            (
                "2024-03-31 00:00:00",
                "2024-02-29 00:00:00",
                -1,
                0,
                0,
                -2678400,
                -2678400000000,
                -31,
                -4,
            ),
            (
                "2024-02-29 00:00:00",
                "2025-02-28 00:00:00",
                11,
                3,
                0,
                31536000,
                31536000000000,
                365,
                52,
            ),
            (
                "2024-02-29 00:00:00",
                "2028-02-29 00:00:00",
                48,
                16,
                4,
                126230400,
                126230400000000,
                1461,
                208,
            ),
            (
                "2025-12-31 23:59:59",
                "2024-01-01 00:00:00",
                -23,
                -7,
                -1,
                -63158399,
                -63158399000000,
                -730,
                -104,
            ),
            (
                "2024-02-15 10:00:00",
                "2024-01-15 10:00:00.000001",
                0,
                0,
                0,
                -2678399,
                -2678399999999,
                -30,
                -4,
            ),
            (
                "2024-01-01 00:00:00",
                "2024-04-01 00:00:00",
                3,
                1,
                0,
                7862400,
                7862400000000,
                91,
                13,
            ),
            (
                "2024-01-01 00:00:00",
                "2024-03-31 23:59:59",
                2,
                0,
                0,
                7862399,
                7862399000000,
                90,
                12,
            ),
            (
                "2024-02-29 12:00:00",
                "2024-01-31 12:00:00",
                0,
                0,
                0,
                -2505600,
                -2505600000000,
                -29,
                -4,
            ),
            (
                "2024-01-31 00:00:00.000001",
                "2024-01-31 00:00:00",
                0,
                0,
                0,
                0,
                -1,
                0,
                0,
            ),
            (
                "2024-01-01 00:00:00.9",
                "2024-01-01 00:00:01.1",
                0,
                0,
                0,
                0,
                200000,
                0,
                0,
            ),
            (
                "2024-01-01 00:00:01.1",
                "2024-01-01 00:00:00.9",
                0,
                0,
                0,
                0,
                -200000,
                0,
                0,
            ),
        ] {
            for (unit, expected) in [
                ("month", month),
                ("quarter", quarter),
                ("year", year),
                ("second", second),
                ("microsecond", microsecond),
                ("day", day),
                ("week", week),
            ] {
                assert_eq!(
                    units_between(unit, from, to),
                    Some(expected),
                    "{unit} from {from} to {to}"
                );
            }
        }
        assert_eq!(units_between("day", "2024-02-30", "2024-03-01"), None);
        assert_eq!(units_between("fortnight", "2024-01-01", "2024-03-01"), None);
    }

    /// Measured on MySQL 8.4.11: the days alone count, whatever time either
    /// moment carries.
    #[test]
    fn counts_the_days_between_two_dates() {
        assert_eq!(
            days_between("2024-02-15 09:59:59", "2024-01-15 10:00:00"),
            Some(31)
        );
        assert_eq!(
            days_between("2024-01-01", "2025-12-31 23:59:59"),
            Some(-730)
        );
        assert_eq!(days_between("2024-01-31", "2024-01-31 23:00:00"), Some(0));
        assert_eq!(days_between("not a day", "2024-01-31"), None);
    }
}
