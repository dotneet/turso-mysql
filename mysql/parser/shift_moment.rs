// Copyright 2026 the Turso authors. All rights reserved. MIT license.

//! Shifting a moment the way MySQL's `DATE_ADD` and `DATE_SUB` do.
//!
//! The engine shifts by months the way SQLite does, which overflows a day the
//! target month has not got: measured on MySQL 8.4.11, `2026-01-31` a month on
//! is `2026-02-28`, where the engine answers `2026-03-03`. That is a different
//! day rather than a missing feature, so the arithmetic is done here instead
//! and the rendered SQL calls it.

use crate::temporal_value::{days_in_month, read_moment, Moment};

/// The unit a shift counts in, as the rendered SQL spells it.
///
/// A week and a quarter are not among them: measured on 8.4.11, a week is
/// exactly seven days and a quarter exactly three months — `2026-01-31` a
/// quarter on and three months on are both `2026-04-30` — so the renderer
/// folds each into the unit it is made of.
const YEAR: &str = "year";
const MONTH: &str = "month";
const DAY: &str = "day";
const HOUR: &str = "hour";
const MINUTE: &str = "minute";
const SECOND: &str = "second";

/// The moment `written` names, shifted by `count` of `unit`.
///
/// `None` says MySQL answers NULL: a value that names no moment, or a shift
/// that lands outside the years MySQL holds.
///
/// Measured on MySQL 8.4.11: a shift by years or months keeps the day where
/// the target month has one and takes that month's last day where it has not,
/// going forwards and backwards alike — `2026-03-31` a month back is
/// `2026-02-28`, and `2024-02-29` a year on is `2025-02-28`. Every other unit
/// is a fixed span of seconds.
///
/// A day alone stays a day when the shift is by whole days, and becomes a
/// moment at midnight when it is not — measured, a `DATE` an hour on answers
/// `2026-01-31 01:00:00`.
pub fn shifted_moment(written: &str, count: i64, unit: &str) -> Option<String> {
    let trimmed = written.trim_matches(|character: char| character.is_ascii_whitespace());
    let day_alone = trimmed.len() == 10;
    let moment = read_moment(trimmed)?;
    let months = match unit {
        YEAR => count.checked_mul(12)?,
        MONTH => count,
        _ => 0,
    };
    let seconds = match unit {
        DAY => count.checked_mul(86_400)?,
        HOUR => count.checked_mul(3_600)?,
        MINUTE => count.checked_mul(60)?,
        SECOND => count,
        _ => 0,
    };
    let whole_days = matches!(unit, YEAR | MONTH | DAY);
    if !matches!(unit, YEAR | MONTH | DAY | HOUR | MINUTE | SECOND) {
        return None;
    }
    let moment = moment_months_on(moment, months)?;
    let moment = moment_seconds_on(moment, seconds)?;
    Some(if day_alone && whole_days {
        format!("{:04}-{:02}-{:02}", moment.year, moment.month, moment.day)
    } else {
        format!(
            "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
            moment.year, moment.month, moment.day, moment.hour, moment.minute, moment.second
        )
    })
}

/// Counts a moment on by whole months, keeping the day where the month it
/// lands in has one.
fn moment_months_on(moment: Moment, months: i64) -> Option<Moment> {
    if months == 0 {
        return Some(moment);
    }
    let counted = i64::from(moment.year)
        .checked_mul(12)?
        .checked_add(i64::from(moment.month) - 1)?
        .checked_add(months)?;
    let year = u32::try_from(counted.div_euclid(12)).ok()?;
    let month = u32::try_from(counted.rem_euclid(12)).ok()? + 1;
    if year > 9999 {
        return None;
    }
    Some(Moment {
        year,
        month,
        day: moment.day.min(days_in_month(year, month)),
        ..moment
    })
}

/// Counts a moment on by a fixed span of seconds.
fn moment_seconds_on(moment: Moment, seconds: i64) -> Option<Moment> {
    if seconds == 0 {
        return Some(moment);
    }
    let from_midnight = i64::from(moment.hour) * 3_600
        + i64::from(moment.minute) * 60
        + i64::from(moment.second)
        + seconds;
    let days = days_from_civil(moment.year, moment.month, moment.day)?
        .checked_add(from_midnight.div_euclid(86_400))?;
    let left = from_midnight.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days)?;
    Some(Moment {
        year,
        month,
        day,
        hour: (left / 3_600) as u32,
        minute: (left % 3_600 / 60) as u32,
        second: (left % 60) as u32,
    })
}

/// Days from 1970-01-01 to the day named, by the shift-and-count method every
/// civil calendar conversion uses.
fn days_from_civil(year: u32, month: u32, day: u32) -> Option<i64> {
    if !(1..=12).contains(&month) || day == 0 || day > days_in_month(year, month) {
        return None;
    }
    let year = i64::from(year) - i64::from(month <= 2);
    let era = year.div_euclid(400);
    let year_of_era = year - era * 400;
    let month = i64::from(month);
    let day_of_year = (153 * (month + if month > 2 { -3 } else { 9 }) + 2) / 5 + i64::from(day) - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    Some(era * 146_097 + day_of_era - 719_468)
}

/// The day that many days from 1970-01-01, which is the reading back of
/// [`days_from_civil`].
fn civil_from_days(days: i64) -> Option<(u32, u32, u32)> {
    let days = days + 719_468;
    let era = days.div_euclid(146_097);
    let day_of_era = days - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let counted_month = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * counted_month + 2) / 5 + 1;
    let month = counted_month + if counted_month < 10 { 3 } else { -9 };
    let year = year + i64::from(month <= 2);
    if !(0..=9999).contains(&year) {
        return None;
    }
    Some((year as u32, month as u32, day as u32))
}
