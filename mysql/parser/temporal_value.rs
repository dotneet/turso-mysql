//! MySQL's canonical form for a `DATE`, a `DATETIME`, a `TIME` and a `YEAR`.
//!
//! MySQL takes a wide surface of spellings for each of these and stores one
//! form, so `'2026-9-6'`, `'20260906'` and `'26/9/6'` all become
//! `2026-09-06`. Everything here was measured on MySQL 8.4.11 under its
//! shipped `sql_mode`, which is strict and refuses a zero month or day.

/// The day a `DATE` column stores.
///
/// A time written after the day is read and checked and then dropped, which
/// is what MySQL does: measured, `'2026-09-06 25:00:00'` is refused for the
/// hour it names even though no hour is kept.
pub fn normalize_date(written: &str) -> Option<String> {
    let moment = read_moment(written)?;
    Some(format!(
        "{:04}-{:02}-{:02}",
        moment.year, moment.month, moment.day
    ))
}

/// The moment a `DATETIME` column stores.
pub fn normalize_datetime(written: &str) -> Option<String> {
    let moment = read_moment(written)?;
    Some(format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
        moment.year, moment.month, moment.day, moment.hour, moment.minute, moment.second
    ))
}

/// The span a `TIME` column stores.
///
/// A `TIME` is not a moment: measured, it runs from `-838:59:59` to
/// `838:59:59`, so it holds more than a day and it holds a sign.
pub fn normalize_time(written: &str) -> Option<String> {
    let span = read_span(written)?;
    let sign = if span.negative { "-" } else { "" };
    Some(format!(
        "{sign}{:02}:{:02}:{:02}",
        span.hours, span.minutes, span.seconds
    ))
}

/// The year a `YEAR` column stores, written as text.
///
/// Measured: text is read as a number and a number under a hundred names a
/// year in the window MySQL keeps — `'0'` and `'00'` are 2000, `'69'` is 2069
/// and `'70'` is 1970 — which is why this differs from a year written as a
/// number, where the zero is the zero year.
pub fn normalize_year(written: &str) -> Option<u16> {
    let written = written.trim_matches(|character: char| character.is_ascii_whitespace());
    if written.is_empty() || !written.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    let number = written.parse::<i64>().ok()?;
    if (0..=99).contains(&number) {
        return Some(year_in_the_window(number as u16));
    }
    year_from_number(number)
}

/// The year a `YEAR` column stores for a year written as a number.
///
/// Measured: the zero is the zero year here, where text `'0'` is 2000.
pub fn year_from_number(number: i64) -> Option<u16> {
    match number {
        0 => Some(0),
        1..=99 => Some(year_in_the_window(number as u16)),
        1901..=2155 => Some(number as u16),
        _ => None,
    }
}

/// Measured: a year under seventy is this century's and the rest are the last
/// one's, so 69 is 2069 and 70 is 1970.
fn year_in_the_window(number: u16) -> u16 {
    if number < 70 {
        2000 + number
    } else {
        1900 + number
    }
}

/// The most a `TIME` holds, measured on MySQL 8.4.11.
const WIDEST_SPAN_HOURS: u32 = 838;
const WIDEST_SPAN_SECONDS: u32 = WIDEST_SPAN_HOURS * 3600 + 59 * 60 + 59;

struct Moment {
    year: u32,
    month: u32,
    day: u32,
    hour: u32,
    minute: u32,
    second: u32,
}

/// Reads any spelling of a day, with or without a time after it.
///
/// MySQL reads one of two ways, and which one it picks turns on the character
/// right after the first run of digits. A run that ends the value or is
/// followed by a point is read as if the whole value had been written without
/// separators: the run is cut into a year and then two digits at a time, and
/// the year is four digits only when the run is four, eight or fourteen long.
/// So `'0.1.1'` is the year 2000 where `'0-1-1'` is the year 0, and
/// `'3311309'` is 2033-11-30 with an hour of 9 left over.
fn read_moment(written: &str) -> Option<Moment> {
    let written = written.trim_matches(|character: char| character.is_ascii_whitespace());
    let head = &written[..digits_in_front(written)];
    if head.is_empty() {
        return None;
    }
    let mut fields = Vec::new();
    let mut rest = &written[head.len()..];
    let packed_shape = rest.is_empty() || rest.starts_with('.');
    if packed_shape {
        let year_digits = if matches!(head.len(), 4 | 8) || head.len() >= 14 {
            4
        } else {
            2
        };
        let mut at = 0;
        let mut width = year_digits;
        while at < head.len() && fields.len() < 6 {
            let take = width.min(head.len() - at);
            fields.push(head[at..at + take].parse::<u32>().ok()?);
            at += take;
            width = 2;
        }
        if at < head.len() {
            return None;
        }
        if year_digits == 2 {
            fields[0] = u32::from(year_in_the_window(fields[0] as u16));
        }
    } else {
        fields.push(head.parse::<u32>().ok()?);
        if head.len() == 2 {
            fields[0] = u32::from(year_in_the_window(fields[0] as u16));
        }
    }
    let fraction = read_remaining_fields(&mut fields, &mut rest)?;
    if fields.len() < 3 {
        return None;
    }
    let mut moment = Moment {
        year: fields[0],
        month: fields[1],
        day: fields[2],
        hour: fields.get(3).copied().unwrap_or(0),
        minute: fields.get(4).copied().unwrap_or(0),
        second: fields.get(5).copied().unwrap_or(0),
    };
    if !names_a_real_moment(&moment) {
        return None;
    }
    if rounds_up(fraction) {
        moment = moment_one_second_later(moment)?;
    }
    Some(moment)
}

/// Reads the numbers left after the first one, and then the fraction of a
/// second that may follow the last of them.
///
/// Measured: MySQL takes any punctuation between two numbers — `'2026/09/06'`,
/// `'2026.09.06'` and `'2026*09*06'` all name the same day — and a space or a
/// `T` between the day and the time. What it does not take is a letter or a
/// digit left over at the end, so `'2026-9-6extra'` names no day.
fn read_remaining_fields<'a>(fields: &mut Vec<u32>, rest: &mut &'a str) -> Option<&'a str> {
    while fields.len() < 6 {
        let separator = rest.len() - trim_separators(rest, fields.len() == 3).len();
        if separator == 0 {
            break;
        }
        let after = &rest[separator..];
        let digits = digits_in_front(after);
        if digits == 0 {
            break;
        }
        fields.push(after[..digits].parse::<u32>().ok()?);
        *rest = &after[digits..];
    }
    let fraction = match rest.strip_prefix('.') {
        Some(fraction) if fraction.bytes().all(|byte| byte.is_ascii_digit()) => {
            *rest = "";
            fraction
        }
        _ => "",
    };
    rest.bytes()
        .all(|byte| !byte.is_ascii_alphanumeric())
        .then_some(fraction)
}

fn digits_in_front(text: &str) -> usize {
    text.len()
        - text
            .trim_start_matches(|character: char| character.is_ascii_digit())
            .len()
}

/// Skips what stands between two numbers: any punctuation, plus the space or
/// the `T` that MySQL takes between a day and the time after it — and only
/// there, which is why `'2026 09 06'` names no day.
fn trim_separators(rest: &str, before_the_time: bool) -> &str {
    if !before_the_time {
        return rest.trim_start_matches(|character: char| {
            !character.is_ascii_alphanumeric() && !character.is_ascii_whitespace()
        });
    }
    let rest = match rest.as_bytes().first() {
        Some(b'T' | b't') => &rest[1..],
        _ => rest,
    };
    rest.trim_start_matches(|character: char| !character.is_ascii_alphanumeric())
}

struct Span {
    negative: bool,
    hours: u32,
    minutes: u32,
    seconds: u32,
}

/// Reads any spelling of a span of time.
///
/// Measured, a `TIME` reads unlike a `DATETIME`: only a colon separates its
/// fields, so `'12.34.56'` is refused where the same text after a day is
/// taken. Without a colon the digits are read from the right — `'5'` is five
/// seconds and `'12345'` is `01:23:45` — and a number with a space and then a
/// digit after it is a count of days, where what follows is hours, so
/// `'2 1:1:1'` is `49:01:01` and `'0 437'` is `437:00:00`.
fn read_span(written: &str) -> Option<Span> {
    let written = written.trim_matches(|character: char| character.is_ascii_whitespace());
    let (negative, unsigned) = match written.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, written),
    };
    let joined;
    let (found_day, days, rest) = match split_days(unsigned)? {
        Days::Counted(days, rest) => (true, days, rest),
        Days::None(rest) => (false, 0, rest),
        Days::SpaceToSkip(text) => {
            joined = text;
            (false, 0, joined.as_str())
        }
    };
    let (body, fraction) = split_fraction(rest)?;
    let seconds = if found_day {
        days.checked_mul(86_400)?
            .checked_add(colon_seconds(body, false)?)?
    } else if body.contains(':') {
        colon_seconds(body, true)?
    } else {
        packed_seconds(body)?
    };
    let seconds = if rounds_up(fraction) {
        seconds.checked_add(1)?
    } else {
        seconds
    };
    // Measured: a fraction past the widest span is refused rather than rounded
    // down into it, so `'838:59:59.4'` names no span.
    let past_the_widest = seconds == WIDEST_SPAN_SECONDS
        && fraction.bytes().any(|digit| digit != b'0')
        && !rounds_up(fraction);
    if seconds > WIDEST_SPAN_SECONDS || past_the_widest {
        return None;
    }
    Some(Span {
        negative: negative && seconds != 0,
        hours: seconds / 3600,
        minutes: seconds % 3600 / 60,
        seconds: seconds % 60,
    })
}

/// What stands in front of the hours.
enum Days<'a> {
    /// A number, a space and then a digit: a count of days, and the rest.
    Counted(u32, &'a str),
    /// No count of days.
    None(&'a str),
    /// A number, a space and then no digit. Measured, the space is simply
    /// skipped there, so `'1 :2:3'` is `01:02:03`.
    SpaceToSkip(String),
}

fn split_days(unsigned: &str) -> Option<Days<'_>> {
    let digits = digits_in_front(unsigned);
    if digits == 0 {
        return Some(Days::None(unsigned));
    }
    let Some(after) = unsigned[digits..].strip_prefix(' ') else {
        return Some(Days::None(unsigned));
    };
    let hour_digits = digits_in_front(after);
    if hour_digits == 0 {
        // Measured: the space is skipped only in front of a colon, so
        // `'1 :2:3'` is `01:02:03` and `'1  2'` names no span.
        let after = after.trim_start_matches(' ');
        if !after.starts_with(':') {
            return None;
        }
        return Some(Days::SpaceToSkip(format!("{}{after}", &unsigned[..digits])));
    }
    // Measured: one digit of hours after a count of days is taken only when a
    // colon or a point follows it, so `'1 9'` names no span and `'1 9.9'` does.
    if hour_digits == 1 && !matches!(after.as_bytes().get(1), Some(b':' | b'.')) {
        return None;
    }
    Some(Days::Counted(unsigned[..digits].parse().ok()?, after))
}

/// Reads `H[:M[:S]]`, where the hours are unbounded and the rest are not.
fn colon_seconds(body: &str, empty_hour_is_zero: bool) -> Option<u32> {
    let mut fields = body.split(':');
    // Measured: a missing leading field is a zero — `':12'` is twelve minutes
    // — where any later empty field is refused, so `'12:'` names no span.
    let written = fields.next()?;
    let hours = if written.is_empty() && empty_hour_is_zero {
        0
    } else {
        read_span_field(written, u32::MAX)?
    };
    let minutes = match fields.next() {
        Some(field) => read_span_field(field, 59)?,
        None => 0,
    };
    let seconds = match fields.next() {
        Some(field) => read_span_field(field, 59)?,
        None => 0,
    };
    if fields.next().is_some() {
        return None;
    }
    hours.checked_mul(3600)?.checked_add(minutes * 60 + seconds)
}

fn read_span_field(field: &str, most: u32) -> Option<u32> {
    if field.is_empty() || !field.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    let value = field.parse::<u32>().ok()?;
    (value <= most).then_some(value)
}

/// Reads digits alone, from the right: the last two are seconds, the two
/// before them minutes, and whatever is left the hours.
fn packed_seconds(body: &str) -> Option<u32> {
    if body.is_empty() {
        return Some(0);
    }
    if !body.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    let digits = format!("{body:0>6}");
    let split = digits.len() - 4;
    let hours = digits[..split].parse::<u32>().ok()?;
    let minutes = digits[split..split + 2].parse::<u32>().ok()?;
    let seconds = digits[split + 2..].parse::<u32>().ok()?;
    if minutes > 59 || seconds > 59 {
        return None;
    }
    hours.checked_mul(3600)?.checked_add(minutes * 60 + seconds)
}

/// Splits a trailing fraction of a second from the rest, refusing a second
/// point. The fraction comes back without its point and may be empty.
fn split_fraction(written: &str) -> Option<(&str, &str)> {
    let Some(point) = written.find('.') else {
        return Some((written, ""));
    };
    let (body, fraction) = written.split_at(point);
    let fraction = &fraction[1..];
    if !fraction.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    Some((body, fraction))
}

/// Measured: a fraction of half a second or more adds a second, and less
/// drops, so `'12:34:56.5'` is `12:34:57` and `.4` is `12:34:56`.
fn rounds_up(fraction: &str) -> bool {
    fraction
        .as_bytes()
        .first()
        .is_some_and(|byte| *byte >= b'5')
}

/// Measured under MySQL's shipped `sql_mode`, which refuses a zero month or a
/// zero day and takes the year zero written with separators.
fn names_a_real_moment(moment: &Moment) -> bool {
    moment.year <= 9999
        && (1..=12).contains(&moment.month)
        && moment.day >= 1
        && moment.day <= days_in_month(moment.year, moment.month)
        && moment.hour <= 23
        && moment.minute <= 59
        && moment.second <= 59
}

fn days_in_month(year: u32, month: u32) -> u32 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        // Measured: the year zero is not a leap year here, so `'0-02-29'`
        // names no day.
        2 if year != 0 && year % 4 == 0 && (year % 100 != 0 || year % 400 == 0) => 29,
        2 => 28,
        _ => 0,
    }
}

/// Carries a rounded-up fraction through the minute, the hour, the day, the
/// month and the year. Measured: `'2026-12-31 23:59:59.6'` is 2027.
fn moment_one_second_later(mut moment: Moment) -> Option<Moment> {
    moment.second += 1;
    if moment.second <= 59 {
        return Some(moment);
    }
    moment.second = 0;
    moment.minute += 1;
    if moment.minute <= 59 {
        return Some(moment);
    }
    moment.minute = 0;
    moment.hour += 1;
    if moment.hour <= 23 {
        return Some(moment);
    }
    moment.hour = 0;
    moment.day += 1;
    if moment.day <= days_in_month(moment.year, moment.month) {
        return Some(moment);
    }
    moment.day = 1;
    moment.month += 1;
    if moment.month <= 12 {
        return Some(moment);
    }
    moment.month = 1;
    moment.year += 1;
    (moment.year <= 9999).then_some(moment)
}

#[cfg(test)]
mod tests {
    use super::{normalize_date, normalize_datetime, normalize_time, normalize_year};

    /// Every reading below was measured on MySQL 8.4.11.
    #[test]
    fn a_day_is_written_one_way_however_it_was_spelled() {
        for written in [
            "2026-09-06",
            "2026-9-6",
            "20260906",
            "260906",
            "26-9-6",
            "2026/09/06",
            "2026.09.06",
            "2026*09*06",
            "2026:09:06",
            "2026--09--06",
            "2026-09-06 01:02:03",
            "2026-09-06T01:02:03",
            "  2026-09-06  ",
            "2026-09-06;",
        ] {
            assert_eq!(
                normalize_date(written).as_deref(),
                Some("2026-09-06"),
                "{written}"
            );
        }
        assert_eq!(normalize_date("9999-12-31").as_deref(), Some("9999-12-31"));
        assert_eq!(normalize_date("2024-02-29").as_deref(), Some("2024-02-29"));
        // A year of one or two digits reads differently: two are the window
        // MySQL keeps and one is the year itself.
        assert_eq!(normalize_date("99-1-1").as_deref(), Some("1999-01-01"));
        assert_eq!(normalize_date("69-1-1").as_deref(), Some("2069-01-01"));
        assert_eq!(normalize_date("70-1-1").as_deref(), Some("1970-01-01"));
        assert_eq!(normalize_date("1-1-1").as_deref(), Some("0001-01-01"));
        assert_eq!(normalize_date("0-1-1").as_deref(), Some("0000-01-01"));
        assert_eq!(normalize_date("0069-1-1").as_deref(), Some("0069-01-01"));
    }

    /// A point right after the year makes MySQL read the value as if it had
    /// been written without separators, where a year is four digits only when
    /// it is written with four, eight or fourteen.
    #[test]
    fn a_point_after_the_year_changes_what_the_year_is() {
        assert_eq!(normalize_date("0.1.1").as_deref(), Some("2000-01-01"));
        assert_eq!(normalize_date("0/1/1").as_deref(), Some("0000-01-01"));
        assert_eq!(normalize_date("5.1.1").as_deref(), Some("2005-01-01"));
        assert_eq!(normalize_date("5-1-1").as_deref(), Some("0005-01-01"));
        assert_eq!(normalize_date("1999.1.1").as_deref(), Some("1999-01-01"));
        // The same reading cuts a run of digits into fields of two, so seven
        // digits are a day with an hour left over, and `063.6.5` is the sixth
        // of March: the run holds the year `06` and the month `3`.
        assert_eq!(normalize_date("3311309").as_deref(), Some("2033-11-30"));
        assert_eq!(normalize_date("063.6.5").as_deref(), Some("2006-03-06"));
        assert_eq!(
            normalize_datetime("3311309").as_deref(),
            Some("2033-11-30 09:00:00")
        );
    }

    /// Measured: the year zero is not a leap year here, where the usual rule
    /// would make it one.
    #[test]
    fn the_year_zero_has_no_twenty_ninth_of_february() {
        assert_eq!(normalize_date("0-02-29"), None);
        assert_eq!(normalize_date("2000-02-29").as_deref(), Some("2000-02-29"));
    }

    #[test]
    fn a_day_mysql_refuses_is_refused() {
        for written in [
            "0000-00-00",
            "2026-13-01",
            "2026-02-30",
            "2023-02-29",
            "2026-0-1",
            "2026-1-0",
            "10000-1-1",
            "2026-9-6extra",
            "20260906extra",
            "2026 09 06",
            "123456789",
            "1234567",
            "12345",
            "2026-09-06 25:00:00",
            "",
        ] {
            assert_eq!(normalize_date(written), None, "{written}");
        }
    }

    #[test]
    fn a_moment_keeps_the_time_the_day_carries() {
        for (written, stored) in [
            ("2026-09-06 01:02:03", "2026-09-06 01:02:03"),
            ("2026-9-6 1:2:3", "2026-09-06 01:02:03"),
            ("20260906010203", "2026-09-06 01:02:03"),
            ("260906010203", "2026-09-06 01:02:03"),
            ("2026-09-06", "2026-09-06 00:00:00"),
            ("2026-09-06T01:02:03", "2026-09-06 01:02:03"),
            ("2026-09-06 01.02.03", "2026-09-06 01:02:03"),
            ("2026*09*06*01*02*03", "2026-09-06 01:02:03"),
            ("2026-09-06  01:02:03", "2026-09-06 01:02:03"),
            ("2026-09-06 1:2", "2026-09-06 01:02:00"),
            ("2026-09-06 1", "2026-09-06 01:00:00"),
        ] {
            assert_eq!(
                normalize_datetime(written).as_deref(),
                Some(stored),
                "{written}"
            );
        }
        for written in [
            "20260906 010203",
            "2026-09-06 010203",
            "0000-00-00 00:00:00",
            "2026-09-06 24:00:00",
        ] {
            assert_eq!(normalize_datetime(written), None, "{written}");
        }
    }

    /// A fraction of half a second or more adds a second, and it carries.
    #[test]
    fn a_fraction_of_a_second_rounds_the_moment() {
        for (written, stored) in [
            ("2026-09-06 01:02:03.7", "2026-09-06 01:02:04"),
            ("2026-09-06 01:02:03.4", "2026-09-06 01:02:03"),
            ("2026-09-06 23:59:59.5", "2026-09-07 00:00:00"),
            ("2026-12-31 23:59:59.6", "2027-01-01 00:00:00"),
            ("20260906010203.7", "2026-09-06 01:02:04"),
        ] {
            assert_eq!(
                normalize_datetime(written).as_deref(),
                Some(stored),
                "{written}"
            );
        }
    }

    #[test]
    fn a_span_is_written_one_way_however_it_was_spelled() {
        for (written, stored) in [
            ("12:34:56", "12:34:56"),
            ("12:34", "12:34:00"),
            ("1:2:3", "01:02:03"),
            ("123456", "12:34:56"),
            ("12345", "01:23:45"),
            ("5", "00:00:05"),
            ("0", "00:00:00"),
            ("-5", "-00:00:05"),
            ("-1:00", "-01:00:00"),
            ("-01:00:00", "-01:00:00"),
            ("  12:34:56  ", "12:34:56"),
            (":12", "00:12:00"),
            (".5", "00:00:01"),
            ("0.5", "00:00:01"),
            ("0.4", "00:00:00"),
            ("1.5", "00:00:02"),
            ("12:34:56.7", "12:34:57"),
            ("838:59:59", "838:59:59"),
            ("-838:59:59", "-838:59:59"),
            ("-0:0:1", "-00:00:01"),
            // A number, a space and a digit is a count of days, and what
            // follows it is hours however many digits it has.
            ("2 1:1:1", "49:01:01"),
            ("0 1:1:1", "01:01:01"),
            ("1 12", "36:00:00"),
            ("6 93", "237:00:00"),
            ("0 437", "437:00:00"),
            ("1 9.9", "33:00:01"),
            ("34 22:00:00", "838:00:00"),
            // A number, a space and no digit is just the space being skipped.
            ("1 :2:3", "01:02:03"),
            // Digits alone are read from the right however many there are.
            ("6905954", "690:59:54"),
            ("8385959", "838:59:59"),
            ("0000000", "00:00:00"),
        ] {
            assert_eq!(
                normalize_time(written).as_deref(),
                Some(stored),
                "{written}"
            );
        }
    }

    #[test]
    fn a_span_mysql_refuses_is_refused() {
        for written in [
            "839:00:00",
            "838:59:59.4",
            "838:59:59.6",
            "-838:59:59.6",
            "9999",
            "1234567",
            "12:60:00",
            "12:00:60",
            "99:99",
            "1:2:3:4",
            "12:",
            "12.34.56",
            "12:34:56extra",
            // One digit of hours after a count of days is taken only when a
            // colon or a point follows it.
            "1 2",
            "1 9",
            "0 9",
            "1  2",
            "35 00",
            "8390000",
            "690595",
        ] {
            assert_eq!(normalize_time(written), None, "{written}");
        }
    }

    #[test]
    fn a_year_written_as_text_reads_a_short_one_as_this_century_or_the_last() {
        for (written, stored) in [
            ("2026", 2026),
            ("0", 2000),
            ("00", 2000),
            ("69", 2069),
            ("70", 1970),
            ("0069", 2069),
            ("  70", 1970),
            ("70 ", 1970),
            ("1901", 1901),
            ("2155", 2155),
        ] {
            assert_eq!(normalize_year(written), Some(stored), "{written}");
        }
        for written in ["1900", "2156", "", "seven", "-1"] {
            assert_eq!(normalize_year(written), None, "{written}");
        }
    }

    #[test]
    fn a_year_written_as_a_number_keeps_its_zero() {
        for (number, stored) in [(2026, 2026), (70, 1970), (69, 2069), (0, 0), (1901, 1901)] {
            assert_eq!(super::year_from_number(number), Some(stored), "{number}");
        }
        for number in [1900, 2156, -1, 100] {
            assert_eq!(super::year_from_number(number), None, "{number}");
        }
    }

    #[test]
    fn what_mysql_normalized_once_it_leaves_alone() {
        assert_eq!(normalize_date("2026-09-06").as_deref(), Some("2026-09-06"));
        assert_eq!(
            normalize_datetime("2026-09-06 01:02:03").as_deref(),
            Some("2026-09-06 01:02:03")
        );
        assert_eq!(normalize_time("12:34:56").as_deref(), Some("12:34:56"));
        assert_eq!(normalize_time("-838:59:59").as_deref(), Some("-838:59:59"));
    }
}
