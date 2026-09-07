//! MySQL's `STR_TO_DATE`, which reads a moment out of text by a format.
//!
//! It is `DATE_FORMAT` read backwards, and what it answers depends on the
//! format rather than on the text: a format naming only day parts answers a
//! `DATE`, one naming only clock parts answers a `TIME`, and one naming both
//! answers a `DATETIME`. Everything here was measured on MySQL 8.4.11.

use crate::date_format::{MONTHS, WEEKDAYS};
use crate::temporal_value::{days_in_month, year_in_the_window};

/// What a format asks to be read, which is what says the answer's type.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FormatShape {
    Day,
    Clock,
    Moment,
}

/// Returns what a format reads, or nothing when it names a specifier this does
/// not read.
///
/// The week numbers and the weekday number are among those: measured, MySQL
/// takes them and they name no day on their own, so reading one would mean
/// answering a day it did not name.
pub fn format_reads(format: &str) -> Option<FormatShape> {
    let (mut day, mut clock) = (false, false);
    let mut rest = format.chars();
    while let Some(character) = rest.next() {
        if character != '%' {
            continue;
        }
        let specifier = rest.next()?;
        match specifier {
            'Y' | 'y' | 'm' | 'c' | 'd' | 'e' | 'D' | 'M' | 'b' | 'j' => day = true,
            'H' | 'k' | 'h' | 'I' | 'l' | 'i' | 's' | 'S' | 'p' | 'r' | 'T' => clock = true,
            // A weekday name says nothing a day needs, and MySQL takes it, so
            // it is read and thrown away.
            'W' | 'a' | '%' => {}
            _ => return None,
        }
    }
    match (day, clock) {
        (true, true) => Some(FormatShape::Moment),
        (true, false) => Some(FormatShape::Day),
        (false, true) => Some(FormatShape::Clock),
        (false, false) => None,
    }
}

/// Reads text by a format and writes back the value MySQL would store.
pub fn read_by_format(text: &str, format: &str) -> Option<String> {
    let shape = format_reads(format)?;
    let mut read = Reading::default();
    let mut rest = text;
    let mut specifiers = format.chars();
    while let Some(character) = specifiers.next() {
        // Measured: text that runs out before the format does leaves the rest
        // of the format reading nothing, so `'2026-09-06'` by
        // `'%Y-%m-%d %H:%i:%s'` is midnight on that day.
        if rest.is_empty() {
            break;
        }
        if character != '%' {
            // Measured: whitespace in the format matches any run of it, and
            // any other character has to be the one that is there.
            if character.is_ascii_whitespace() {
                rest = rest.trim_start();
                continue;
            }
            rest = rest.strip_prefix(character)?;
            continue;
        }
        let specifier = specifiers.next()?;
        rest = read.take(specifier, rest)?;
    }
    read.written(shape)
}

#[derive(Default)]
struct Reading {
    year: Option<u32>,
    month: Option<u32>,
    day: Option<u32>,
    day_of_year: Option<u32>,
    hour: Option<u32>,
    minute: Option<u32>,
    second: Option<u32>,
    afternoon: Option<bool>,
    hour_on_a_clock: bool,
}

impl Reading {
    fn take<'a>(&mut self, specifier: char, rest: &'a str) -> Option<&'a str> {
        // Measured: whitespace in front of a value is skipped, so
        // `'  2026-09-06'` reads by `'%Y-%m-%d'`.
        let rest = rest.trim_start();
        match specifier {
            'Y' => {
                let (value, digits, rest) = read_number(rest, 4)?;
                self.year = Some(if digits <= 2 {
                    u32::from(year_in_the_window(value as u16))
                } else {
                    value
                });
                Some(rest)
            }
            'y' => {
                let (value, _, rest) = read_number(rest, 2)?;
                self.year = Some(u32::from(year_in_the_window(value as u16)));
                Some(rest)
            }
            'm' | 'c' => {
                let (value, _, rest) = read_number(rest, 2)?;
                self.month = Some(value);
                Some(rest)
            }
            'd' | 'e' => {
                let (value, _, rest) = read_number(rest, 2)?;
                self.day = Some(value);
                Some(rest)
            }
            'D' => {
                let (value, _, rest) = read_number(rest, 2)?;
                self.day = Some(value);
                // The two letters after the number are its ordinal ending.
                Some(rest.get(2..).unwrap_or(""))
            }
            'j' => {
                let (value, _, rest) = read_number(rest, 3)?;
                self.day_of_year = Some(value);
                Some(rest)
            }
            'M' => self.take_name(rest, &MONTHS, |reading, index| {
                reading.month = Some(index as u32 + 1);
            }),
            'b' => self.take_short_name(rest, &MONTHS, |reading, index| {
                reading.month = Some(index as u32 + 1);
            }),
            'W' => self.take_name(rest, &WEEKDAYS, |_, _| {}),
            'a' => self.take_short_name(rest, &WEEKDAYS, |_, _| {}),
            'H' | 'k' => {
                let (value, _, rest) = read_number(rest, 2)?;
                self.hour = Some(value);
                Some(rest)
            }
            'h' | 'I' | 'l' => {
                let (value, _, rest) = read_number(rest, 2)?;
                self.hour = Some(value);
                self.hour_on_a_clock = true;
                Some(rest)
            }
            'i' => {
                let (value, _, rest) = read_number(rest, 2)?;
                self.minute = Some(value);
                Some(rest)
            }
            's' | 'S' => {
                let (value, _, rest) = read_number(rest, 2)?;
                self.second = Some(value);
                Some(rest)
            }
            'p' => {
                let afternoon = if rest.len() >= 2 && rest[..2].eq_ignore_ascii_case("AM") {
                    false
                } else if rest.len() >= 2 && rest[..2].eq_ignore_ascii_case("PM") {
                    true
                } else {
                    return None;
                };
                self.afternoon = Some(afternoon);
                Some(&rest[2..])
            }
            'r' => {
                let rest = self.take('h', rest)?;
                let rest = rest.strip_prefix(':')?;
                let rest = self.take('i', rest)?;
                let rest = rest.strip_prefix(':')?;
                let rest = self.take('s', rest)?;
                self.take('p', rest.trim_start())
            }
            'T' => {
                let rest = self.take('H', rest)?;
                let rest = rest.strip_prefix(':')?;
                let rest = self.take('i', rest)?;
                let rest = rest.strip_prefix(':')?;
                self.take('s', rest)
            }
            '%' => rest.strip_prefix('%'),
            _ => None,
        }
    }

    fn take_name<'a>(
        &mut self,
        rest: &'a str,
        names: &[&str],
        keep: impl Fn(&mut Self, usize),
    ) -> Option<&'a str> {
        for (index, name) in names.iter().enumerate() {
            if rest.len() >= name.len() && rest[..name.len()].eq_ignore_ascii_case(name) {
                keep(self, index);
                return Some(&rest[name.len()..]);
            }
        }
        None
    }

    fn take_short_name<'a>(
        &mut self,
        rest: &'a str,
        names: &[&str],
        keep: impl Fn(&mut Self, usize),
    ) -> Option<&'a str> {
        for (index, name) in names.iter().enumerate() {
            if rest.len() >= 3 && rest[..3].eq_ignore_ascii_case(&name[..3]) {
                keep(self, index);
                return Some(&rest[3..]);
            }
        }
        None
    }

    fn written(&self, shape: FormatShape) -> Option<String> {
        let hour = match (self.hour, self.hour_on_a_clock, self.afternoon) {
            (Some(hour), true, Some(true)) => (hour % 12) + 12,
            (Some(hour), true, _) => hour % 12,
            (Some(hour), false, _) => hour,
            (None, _, _) => 0,
        };
        let (minute, second) = (self.minute.unwrap_or(0), self.second.unwrap_or(0));
        if hour > 23 || minute > 59 || second > 59 {
            return None;
        }
        if shape == FormatShape::Clock {
            return Some(format!("{hour:02}:{minute:02}:{second:02}"));
        }
        let year = self.year?;
        let (year, month, day) = match (self.month, self.day, self.day_of_year) {
            (Some(month), Some(day), _) => (year, month, day),
            // Measured: a day of the year names the month and the day, and one
            // past the end of the year runs into the next — `'2026 366'` is
            // the first of January 2027.
            (None, None, Some(day_of_year)) => day_from_the_year(year, day_of_year)?,
            _ => return None,
        };
        if year > 9999 || !(1..=12).contains(&month) || day < 1 || day > days_in_month(year, month)
        {
            return None;
        }
        if shape == FormatShape::Day {
            return Some(format!("{year:04}-{month:02}-{day:02}"));
        }
        Some(format!(
            "{year:04}-{month:02}-{day:02} {hour:02}:{minute:02}:{second:02}"
        ))
    }
}

/// Reads up to `most` digits, answering the value, how many digits it read,
/// and what is left.
fn read_number(rest: &str, most: usize) -> Option<(u32, usize, &str)> {
    let digits = rest
        .bytes()
        .take(most)
        .take_while(u8::is_ascii_digit)
        .count();
    if digits == 0 {
        return None;
    }
    Some((rest[..digits].parse().ok()?, digits, &rest[digits..]))
}

fn day_from_the_year(year: u32, day_of_year: u32) -> Option<(u32, u32, u32)> {
    if day_of_year == 0 {
        return None;
    }
    let mut year = year;
    let mut left = day_of_year;
    // A count past the end of the year runs into the next one, and no further.
    for _ in 0..2 {
        for month in 1..=12 {
            let days = days_in_month(year, month);
            if left <= days {
                return Some((year, month, left));
            }
            left -= days;
        }
        year += 1;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::{format_reads, read_by_format, FormatShape};

    /// Every reading measured on MySQL 8.4.11.
    #[test]
    fn text_reads_by_the_format_the_way_mysql_reads_it() {
        for (text, format, written) in [
            ("2026-09-06", "%Y-%m-%d", "2026-09-06"),
            ("06/09/2026", "%d/%m/%Y", "2026-09-06"),
            (
                "2026-09-06 01:02:03",
                "%Y-%m-%d %H:%i:%s",
                "2026-09-06 01:02:03",
            ),
            ("Sep 6 2026", "%b %e %Y", "2026-09-06"),
            ("September 6 2026", "%M %e %Y", "2026-09-06"),
            ("Sunday, September 6th 2026", "%W, %M %D %Y", "2026-09-06"),
            ("01:02:03 AM", "%r", "01:02:03"),
            ("13:02:03", "%T", "13:02:03"),
            ("26-09-06", "%y-%m-%d", "2026-09-06"),
            ("2026-9-6", "%Y-%m-%d", "2026-09-06"),
            ("2026-09-06", "%Y-%m-%d %H:%i:%s", "2026-09-06 00:00:00"),
            ("01:02:03", "%H:%i:%s", "01:02:03"),
            // Measured: what is left over after the format is read is ignored.
            ("2026-09-06extra", "%Y-%m-%d", "2026-09-06"),
            ("249 2026", "%j %Y", "2026-09-06"),
            ("  2026-09-06", "%Y-%m-%d", "2026-09-06"),
            ("2026-09-06  ", "%Y-%m-%d", "2026-09-06"),
            ("99-12-31", "%Y-%m-%d", "1999-12-31"),
            ("69-1-1", "%y-%m-%d", "2069-01-01"),
            ("70-1-1", "%y-%m-%d", "1970-01-01"),
            ("Mon Jan 5 2026", "%a %b %e %Y", "2026-01-05"),
            ("1st Jan 2026", "%D %b %Y", "2026-01-01"),
            ("22nd Jan 2026", "%D %b %Y", "2026-01-22"),
            ("12:00:00 AM", "%r", "00:00:00"),
            ("12:00:00 PM", "%r", "12:00:00"),
            ("01:30:00 pm", "%r", "13:30:00"),
            ("7:5:3", "%H:%i:%s", "07:05:03"),
            ("5 5 5", "%H %i %s", "05:05:05"),
            (
                "2026-09-06 1:02:03 PM",
                "%Y-%m-%d %r",
                "2026-09-06 13:02:03",
            ),
            ("2024 366", "%Y %j", "2024-12-31"),
            // A day past the end of the year runs into the next one.
            ("2026 366", "%Y %j", "2027-01-01"),
        ] {
            assert_eq!(
                read_by_format(text, format).as_deref(),
                Some(written),
                "{text} by {format}"
            );
        }
    }

    #[test]
    fn text_that_does_not_read_answers_nothing() {
        for (text, format) in [
            ("not a date", "%Y-%m-%d"),
            ("2026-13-06", "%Y-%m-%d"),
            ("2026-02-30", "%Y-%m-%d"),
            ("2026-09-06", "%d/%m/%Y"),
            ("25:00:00", "%H:%i:%s"),
            ("Sep 6", "%b %e %Y"),
            ("2026 000", "%Y %j"),
            ("2026-09", "%Y-%m-%d"),
            ("2026", "%Y-%m-%d"),
            ("2026-00-01", "%Y-%m-%d"),
            ("2026-01-00", "%Y-%m-%d"),
            ("100%", "%d%%"),
            ("x2026", "x%Y"),
            ("2026x", "%Yx"),
            ("24:00:00", "%H:%i:%s"),
        ] {
            assert_eq!(read_by_format(text, format), None, "{text} by {format}");
        }
    }

    /// The format says what the answer is, not the text.
    #[test]
    fn the_format_says_what_is_read() {
        assert_eq!(format_reads("%Y-%m-%d"), Some(FormatShape::Day));
        assert_eq!(format_reads("%H:%i:%s"), Some(FormatShape::Clock));
        assert_eq!(format_reads("%r"), Some(FormatShape::Clock));
        assert_eq!(format_reads("%Y-%m-%d %H:%i:%s"), Some(FormatShape::Moment));
        // A week number names no day on its own, so a format carrying one is
        // not read at all.
        for format in ["%U", "%v %x", "%w", "%f", "%Z", "on its own", "%"] {
            assert_eq!(format_reads(format), None, "{format}");
        }
    }
}
