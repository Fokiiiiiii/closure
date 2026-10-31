//! Times a user types for a punishment: a length (`1h30m`, see [`crate::duration`]) or a moment
//! (`2026-11-01 18:00`, `18時`, `明日 9:00`, a Discord timestamp), read against the clock when it
//! is resolved. A moment without a zone is read in the server's time zone when it has one, else
//! at UTC+09:00 on a Japanese server and at UTC on any other.

use std::ops::RangeInclusive;
use std::time::Duration;

use closure_core::event::InteractionRef;
use closure_core::locale::Locale;
use closure_core::schedule::MAX_DELAY;
use closure_core::timezone::{Timezone, offset_label};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::duration::{self, DurationError};
use crate::{Cx, UserError};

const DAY: i64 = 86_400;
const JST: i64 = 9 * 3_600;
/// A leap year, so that a day that only some years have (Feb 29) is accepted without a year.
const LEAP_YEAR: i64 = 2_000;

/// Why a span does not resolve.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum SpanError {
    #[error("the time is not understood")]
    NotUnderstood,
    #[error("the time is in the past")]
    Past,
    #[error("the time is more than ten years away")]
    TooFar,
}

/// A time typed for a punishment, kept as typed until it is resolved.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Span(String);

impl Span {
    /// Checks that `text` has one of the forms of a span. The clock is not read here, so a
    /// moment in the past passes this check and fails [`Self::resolve_at`].
    pub fn parse(text: &str) -> Result<Self, SpanError> {
        read(text)?;
        Ok(Self(text.to_owned()))
    }

    /// A length of time as a span, for a template's duration.
    #[must_use]
    pub fn from_duration(duration: Duration) -> Self {
        Self(format!("{}s", duration.as_secs()))
    }

    /// Whether the span is a moment, such as a date or a time of day, rather than a length.
    #[must_use]
    pub fn is_moment(&self) -> bool {
        matches!(read(&self.0), Ok(Parsed::Epoch(_) | Parsed::Moment(_)))
    }

    /// The length of time from `now_unix` to the span's moment. A moment without a zone is read
    /// in `default_offset` seconds from UTC.
    pub fn resolve_at(&self, now_unix: i64, default_offset: i64) -> Result<Duration, SpanError> {
        match read(&self.0)? {
            Parsed::Length(length) => Ok(length),
            Parsed::Epoch(at) => until(at, now_unix),
            Parsed::Moment(moment) => {
                let offset = moment.zone.unwrap_or(default_offset);
                until(moment.instant(now_unix, offset)?, now_unix)
            }
        }
    }

    /// [`Self::resolve_at`] at the command's clock, with the server's offset. The error names the
    /// offset that was read.
    pub fn resolve<I: InteractionRef + 'static>(&self, cx: &Cx<I>) -> Result<Duration, UserError> {
        let default = cx
            .ctx()
            .policy()
            .timezone
            .map_or_else(|| default_offset(cx.ctx().locale), Timezone::offset_seconds);
        self.resolve_at(cx.now(), default).map_err(|error| {
            let offset = match read(&self.0) {
                Ok(Parsed::Moment(moment)) => moment.zone.unwrap_or(default),
                _ => default,
            };
            error.into_user(offset)
        })
    }
}

impl SpanError {
    fn into_user(self, offset: i64) -> UserError {
        match self {
            Self::NotUnderstood => UserError::sdk("span-not-understood"),
            Self::Past => UserError::sdk("span-past").with("zone", offset_label(offset)),
            Self::TooFar => UserError::sdk("span-too-far").with("zone", offset_label(offset)),
        }
    }
}

/// The offset in seconds of a server's language, for a server with no time zone set: Japanese is
/// UTC+09:00, the others UTC.
fn default_offset(locale: Locale) -> i64 {
    if matches!(locale, Locale::Ja) { JST } else { 0 }
}

/// What a span stands for, as read from its text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Parsed {
    Length(Duration),
    /// A Discord timestamp: Unix seconds, which carry their own zone.
    Epoch(i64),
    Moment(Moment),
}

/// A moment in some day, at a time of that day. A zone that was typed overrides the default.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Moment {
    when: When,
    /// Seconds after local midnight.
    clock: i64,
    zone: Option<i64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum When {
    /// A calendar date; without a year, the next time it comes.
    Date {
        year: Option<i64>,
        month: i64,
        day: i64,
    },
    /// Days after today, in the offset of the moment.
    Days(i64),
    /// Today if the time still lies ahead, else tomorrow.
    Next,
}

impl Moment {
    /// The moment as Unix seconds, read in the zone `offset` seconds from UTC.
    fn instant(self, now: i64, offset: i64) -> Result<i64, SpanError> {
        let today = (now + offset).div_euclid(DAY);
        let on = |days: i64| days * DAY + self.clock - offset;
        match self.when {
            When::Days(days) => Ok(on(today + days)),
            When::Next => {
                let first = on(today);
                Ok(if first > now { first } else { on(today + 1) })
            }
            When::Date {
                year: Some(year),
                month,
                day,
            } => Ok(on(days_from_civil(year, month, day))),
            When::Date {
                year: None,
                month,
                day,
            } => {
                let (this_year, _, _) = civil_from_days(today);
                (this_year..=this_year + 8)
                    .filter(|year| day <= days_in_month(*year, month))
                    .map(|year| on(days_from_civil(year, month, day)))
                    .find(|&at| at > now)
                    .ok_or(SpanError::TooFar)
            }
        }
    }
}

/// The distance from `now` to `at`, which must be ahead and within [`MAX_DELAY`].
fn until(at: i64, now: i64) -> Result<Duration, SpanError> {
    if at <= now {
        return Err(SpanError::Past);
    }
    let distance = Duration::from_secs(at.abs_diff(now));
    if distance > MAX_DELAY {
        Err(SpanError::TooFar)
    } else {
        Ok(distance)
    }
}

/// Reads a span's text: a length, a Discord timestamp, or a moment, with an optional zone.
fn read(text: &str) -> Result<Parsed, SpanError> {
    let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let text = text.to_lowercase();
    if let Some(at) = epoch(&text) {
        return Ok(Parsed::Epoch(at));
    }
    match duration::parse(&text) {
        Ok(length) => return Ok(Parsed::Length(length)),
        Err(DurationError::TooLarge) => return Err(SpanError::TooFar),
        Err(_) => {}
    }
    if let Some(moment) = moment(&text) {
        return Ok(Parsed::Moment(moment));
    }
    split_zone(&text)
        .map(Parsed::Moment)
        .ok_or(SpanError::NotUnderstood)
}

/// A Discord timestamp, `<t:1767225600>` or `<t:1767225600:R>` (any style letter).
fn epoch(text: &str) -> Option<i64> {
    let inner = text.strip_prefix("<t:")?.strip_suffix('>')?;
    let (seconds, style) = match inner.split_once(':') {
        Some((seconds, style)) => (seconds, Some(style)),
        None => (inner, None),
    };
    if style
        .is_some_and(|style| style.len() != 1 || !style.bytes().all(|b| b.is_ascii_alphabetic()))
    {
        return None;
    }
    digits_of(seconds, 1..=19)
}

/// The moment of `text` with a zone at its end (`18:00 JST`, `18:00+0900`, `18:00-05:00`).
fn split_zone(text: &str) -> Option<Moment> {
    text.char_indices().skip(1).find_map(|(index, _)| {
        let zone = zone_of(&text[index..])?;
        let mut moment = moment(text[..index].trim_end())?;
        moment.zone = Some(zone);
        Some(moment)
    })
}

/// A zone as the end of a moment: `z`, `utc`, `jst`, or a sign with hours and optional minutes.
fn zone_of(text: &str) -> Option<i64> {
    match text {
        "z" | "utc" => return Some(0),
        "jst" => return Some(JST),
        _ => {}
    }
    let sign = match text.as_bytes().first()? {
        b'+' => 1,
        b'-' => -1,
        _ => return None,
    };
    let body = &text[1..];
    let (hours, minutes) = match body.split_once(':') {
        Some((hours, minutes)) => (hours, Some(minutes)),
        // Slice only ASCII: byte 2 of a four-byte body may fall inside a character.
        None if body.len() == 4 && body.is_ascii() => (&body[..2], Some(&body[2..])),
        None => (body, None),
    };
    let hours = digits_of(hours, 1..=2)?;
    let minutes = match minutes {
        Some(minutes) => digits_of(minutes, 2..=2)?,
        None => 0,
    };
    (hours <= 14 && minutes <= 59).then_some(sign * (hours * 3_600 + minutes * 60))
}

/// A moment without a zone, as the whole of `text`.
fn moment(text: &str) -> Option<Moment> {
    let mut cursor = Cursor(text);
    let (when, clock) = if let Some(days) = word(&mut cursor) {
        cursor.skip_space();
        (When::Days(days), time_of(&mut cursor)?)
    } else {
        let digits = cursor.leading_digits();
        let following = text.get(digits..).and_then(|rest| rest.chars().next());
        match (digits, following) {
            (4, Some('-' | '/' | '年')) => {
                let when = full_date(&mut cursor)?;
                cursor.skip_space();
                cursor.eat("t");
                cursor.skip_space();
                let clock = if cursor.is_done() {
                    0
                } else {
                    time_of(&mut cursor)?
                };
                (when, clock)
            }
            (1 | 2, Some('/' | '月')) => {
                let when = yearless_date(&mut cursor)?;
                cursor.skip_space();
                (when, time_of(&mut cursor)?)
            }
            _ => (When::Next, time_of(&mut cursor)?),
        }
    };
    cursor.is_done().then_some(Moment {
        when,
        clock,
        zone: None,
    })
}

/// `今日`, `明日`, `明後日`, `today`, `tomorrow`: how many days after today.
fn word(cursor: &mut Cursor) -> Option<i64> {
    [
        ("明後日", 2),
        ("明日", 1),
        ("今日", 0),
        ("tomorrow", 1),
        ("today", 0),
    ]
    .into_iter()
    .find_map(|(word, days)| cursor.eat(word).then_some(days))
}

/// `2026-11-01`, `2026/11/01`, `2026年11月1日`, with the year in 2000 to 2200 and a real day.
fn full_date(cursor: &mut Cursor) -> Option<When> {
    let year = exact(cursor, 4)?;
    let (month, day) = if cursor.eat("年") {
        let month = number(cursor)?;
        if !cursor.eat("月") {
            return None;
        }
        let day = number(cursor)?;
        if !cursor.eat("日") {
            return None;
        }
        (month, day)
    } else if cursor.eat("-") || cursor.eat("/") {
        let month = number(cursor)?;
        if !(cursor.eat("-") || cursor.eat("/")) {
            return None;
        }
        (month, number(cursor)?)
    } else {
        return None;
    };
    ((2_000..=2_200).contains(&year) && is_day(year, month, day)).then_some(When::Date {
        year: Some(year),
        month,
        day,
    })
}

/// `11/01` or `11月1日`, which takes the next year it comes in.
fn yearless_date(cursor: &mut Cursor) -> Option<When> {
    let month = number(cursor)?;
    let day = if cursor.eat("/") {
        number(cursor)?
    } else if cursor.eat("月") {
        let day = number(cursor)?;
        if !cursor.eat("日") {
            return None;
        }
        day
    } else {
        return None;
    };
    is_day(LEAP_YEAR, month, day).then_some(When::Date {
        year: None,
        month,
        day,
    })
}

/// `18:00`, `9:05:30`, `18時`, `18時30分`, `18時30分15秒`, as seconds after midnight.
fn time_of(cursor: &mut Cursor) -> Option<i64> {
    let hour = number(cursor)?;
    let (minute, second) = if cursor.eat(":") {
        let minute = exact(cursor, 2)?;
        let second = if cursor.eat(":") {
            exact(cursor, 2)?
        } else {
            0
        };
        (minute, second)
    } else if cursor.eat("時") {
        let minute = unit_part(cursor, "分")?;
        let second = unit_part(cursor, "秒")?;
        (minute, second)
    } else {
        return None;
    };
    (hour <= 23 && minute <= 59 && second <= 59).then_some(hour * 3_600 + minute * 60 + second)
}

/// A minute or second of a Japanese time, which may be left out.
fn unit_part(cursor: &mut Cursor, unit: &str) -> Option<i64> {
    if cursor.leading_digits() == 0 {
        return Some(0);
    }
    let value = exact(cursor, 2)?;
    cursor.eat(unit).then_some(value)
}

fn is_day(year: i64, month: i64, day: i64) -> bool {
    (1..=12).contains(&month) && (1..=days_in_month(year, month)).contains(&day)
}

fn digits_of(text: &str, len: RangeInclusive<usize>) -> Option<i64> {
    if !len.contains(&text.len()) || !text.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    text.parse().ok()
}

/// A number of one or two digits.
fn number(cursor: &mut Cursor) -> Option<i64> {
    cursor.digits(2).map(|(value, _)| value)
}

/// A number of exactly `len` digits.
fn exact(cursor: &mut Cursor, len: usize) -> Option<i64> {
    let (value, found) = cursor.digits(len)?;
    (found == len).then_some(value)
}

/// The rest of a span's text, read from its front.
#[derive(Clone, Copy)]
struct Cursor<'a>(&'a str);

impl Cursor<'_> {
    fn eat(&mut self, literal: &str) -> bool {
        match self.0.strip_prefix(literal) {
            Some(rest) => {
                self.0 = rest;
                true
            }
            None => false,
        }
    }

    /// Takes up to `max` digits, and how many it took; none when there is no digit.
    fn digits(&mut self, max: usize) -> Option<(i64, usize)> {
        let len = self
            .0
            .bytes()
            .take(max)
            .take_while(u8::is_ascii_digit)
            .count();
        if len == 0 {
            return None;
        }
        let (digits, rest) = self.0.split_at(len);
        self.0 = rest;
        Some((digits.parse().ok()?, len))
    }

    fn leading_digits(&self) -> usize {
        self.0.bytes().take_while(u8::is_ascii_digit).count()
    }

    fn skip_space(&mut self) {
        self.0 = self.0.trim_start();
    }

    fn is_done(&self) -> bool {
        self.0.is_empty()
    }
}

/// Days since 1970-01-01 of a date in the proleptic Gregorian calendar (H. Hinnant's algorithm).
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let year_of_era = year - era * 400;
    let month_from_march = (month + 9) % 12;
    let day_of_year = (153 * month_from_march + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

/// The date (year, month, day) of a day count since 1970-01-01, the inverse of
/// [`days_from_civil`].
fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let days = days + 719_468;
    let era = days.div_euclid(146_097);
    let day_of_era = days - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_from_march = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_from_march + 2) / 5 + 1;
    let month = if month_from_march < 10 {
        month_from_march + 3
    } else {
        month_from_march - 9
    };
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    (year, month, day)
}

fn days_in_month(year: i64, month: i64) -> i64 {
    match month {
        2 if year % 4 == 0 && (year % 100 != 0 || year % 400 == 0) => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use std::time::Duration;

    use closure_core::schedule::MAX_DELAY;

    use super::{
        DAY, JST, Span, SpanError, civil_from_days, days_from_civil, days_in_month, offset_label,
    };

    /// 2026-10-26 12:00 UTC, a Monday.
    const NOW: i64 = 20_752 * DAY + 12 * 3_600;

    fn at(text: &str, offset: i64) -> Result<Duration, SpanError> {
        Span::parse(text).and_then(|span| span.resolve_at(NOW, offset))
    }

    fn secs(text: &str, offset: i64) -> u64 {
        at(text, offset).expect(text).as_secs()
    }

    #[test]
    fn calendar_matches_known_dates() {
        assert_eq!(days_from_civil(1970, 1, 1), 0);
        assert_eq!(days_from_civil(2000, 2, 29), 11_016);
        assert_eq!(days_from_civil(2026, 1, 1) * DAY, 1_767_225_600);
        assert_eq!(civil_from_days(20_752), (2026, 10, 26));
        assert_eq!(civil_from_days(-1), (1969, 12, 31));
    }

    #[test]
    fn calendar_knows_the_leap_days() {
        assert_eq!(days_in_month(2000, 2), 29, "2000 is a leap year");
        assert_eq!(days_in_month(2100, 2), 28, "2100 is not");
        assert_eq!(days_in_month(2024, 2), 29);
        assert_eq!(days_in_month(2026, 2), 28);
        assert_eq!(
            days_from_civil(2100, 3, 1) - days_from_civil(2100, 2, 28),
            1,
            "no Feb 29 in 2100"
        );
        assert_eq!(
            days_from_civil(2000, 3, 1) - days_from_civil(2000, 2, 28),
            2,
            "Feb 29 in 2000"
        );
    }

    #[test]
    fn civil_days_round_trip() {
        for days in (-800_000..800_000).step_by(9_973) {
            let (year, month, day) = civil_from_days(days);
            assert_eq!(
                days_from_civil(year, month, day),
                days,
                "{year}-{month}-{day}"
            );
        }
    }

    #[test]
    fn zones_are_labelled_for_errors() {
        assert_eq!(offset_label(0), "UTC");
        assert_eq!(offset_label(JST), "UTC+09:00");
        assert_eq!(offset_label(-5 * 3_600), "UTC-05:00");
    }

    #[test]
    fn relative_lengths_are_still_lengths() {
        assert_eq!(at("1h30m", 0), Ok(Duration::from_mins(90)));
        assert_eq!(at("1h 30m", JST), Ok(Duration::from_mins(90)));
        assert_eq!(at("2d", 0), Ok(Duration::from_hours(48)));
        assert_eq!(at("3650d", 0), Ok(MAX_DELAY));
        assert_eq!(at("3651d", 0), Err(SpanError::TooFar));
    }

    #[test]
    fn accepted_dates_and_times_resolve_in_the_default_zone() {
        // A full date with a time, in UTC and in JST (09:00 UTC is 18:00 JST).
        assert_eq!(secs("2026-11-01 18:00", 0), 540_000);
        assert_eq!(secs("2026-11-01 18:00", JST), 507_600);
        assert_eq!(secs("2026/11/01 18:00:30", 0), 540_030);
        assert_eq!(secs("2026-11-01T18:00", 0), 540_000);
        assert_eq!(
            secs("2026-11-01", JST),
            442_800,
            "00:00 JST, 31 Oct 15:00 UTC"
        );
        assert_eq!(secs("2026-11-01", 0), 475_200);
        assert_eq!(secs("2026年11月1日 18時30分", JST), 509_400);
        assert_eq!(secs("2026年11月1日18時", JST), 507_600);
    }

    #[test]
    fn yearless_forms_take_the_next_time_they_come() {
        assert_eq!(secs("18:00", 0), 21_600, "later today");
        assert_eq!(secs("09:00", 0), 75_600, "passed today, so tomorrow");
        assert_eq!(secs("10/27 09:00", 0), 75_600);
        assert_eq!(secs("11/01 18:00", JST), 507_600);
        assert_eq!(secs("11月1日 18:00", JST), 507_600);
        assert_eq!(
            secs("02/29 12:00", 0),
            42_422_400,
            "the next leap day, 2028"
        );
        assert_eq!(
            secs("10/26 11:00", 0),
            31_532_400,
            "the day passed, so next year"
        );
        assert_eq!(
            secs("18時", JST),
            75_600,
            "passed at 21:00 JST, so tomorrow"
        );
    }

    #[test]
    fn words_and_japanese_days_are_fixed_dates() {
        assert_eq!(secs("今日 18:00", 0), 21_600);
        assert_eq!(secs("today 18:00", 0), 21_600);
        assert_eq!(secs("明日 18:00", 0), 108_000);
        assert_eq!(secs("tomorrow 18:00", 0), 108_000);
        assert_eq!(secs("明後日 9時", JST), 129_600);
        assert_eq!(secs("TODAY 18:00", 0), 21_600, "case does not matter");
    }

    #[test]
    fn an_explicit_zone_wins_over_the_default() {
        assert_eq!(secs("2026-11-01 18:00 UTC", JST), 540_000);
        assert_eq!(secs("2026-11-01 18:00 z", JST), 540_000);
        assert_eq!(secs("2026-11-01 18:00 jst", 0), 507_600);
        assert_eq!(secs("2026-11-01 18:00+9", 0), 507_600);
        assert_eq!(secs("2026-11-01 18:00+0900", 0), 507_600);
        assert_eq!(secs("2026-11-01 18:00 +09:00", 0), 507_600);
        assert_eq!(secs("2026-11-01 18:00-05:00", 0), 558_000);
    }

    #[test]
    fn a_discord_timestamp_is_read_as_it_is() {
        let future = NOW + 3_600;
        assert_eq!(secs(&format!("<t:{future}>"), JST), 3_600);
        assert_eq!(secs(&format!("<t:{future}:R>"), 0), 3_600);
        assert_eq!(secs(&format!("<t:{future}:F>"), 0), 3_600);
        assert_eq!(at("<t:1767225600>", 0), Err(SpanError::Past));
    }

    #[test]
    fn a_negative_offset_is_read_across_midnight_and_the_year_end() {
        let new_year_eve_23h_utc = days_from_civil(2026, 12, 31) * DAY + 23 * 3_600;
        let typed = Span::parse("2027-01-01 00:30 -05:00").expect("a form");
        assert_eq!(
            typed.resolve_at(new_year_eve_23h_utc, 0),
            Ok(Duration::from_mins(6 * 60 + 30)),
            "00:30 at -05:00 is 05:30 UTC"
        );
        // 02:00 UTC is still 21:00 of the day before at -05:00.
        let two_utc = days_from_civil(2026, 10, 26) * DAY + 2 * 3_600;
        let typed = Span::parse("21:30 -05:00").expect("a form");
        assert_eq!(
            typed.resolve_at(two_utc, 0),
            Ok(Duration::from_mins(30)),
            "21:30 at -05:00 is 02:30 UTC the same day"
        );
    }

    #[test]
    fn what_is_not_a_time_is_refused_as_not_understood() {
        for text in [
            "",
            "banana",
            "18",
            "2026-13-01 10:00",
            "2026-02-30 10:00",
            "2100-02-29 10:00",
            "2026-10-27 24:00",
            "18:60",
            "18:0",
            "1999-01-01 00:00",
            "2026-11-01 18:00 +15:00",
            "2026-11-01 18:00 pst",
            "<t:1767225600:RR>",
        ] {
            assert_eq!(Span::parse(text), Err(SpanError::NotUnderstood), "{text:?}");
        }
    }

    #[test]
    fn leap_day_is_accepted_when_its_year_has_one() {
        assert_eq!(secs("2028-02-29 10:00", 0), 42_422_400 - 2 * 3_600);
    }

    #[test]
    fn a_moment_that_has_passed_is_refused_when_resolved() {
        assert_eq!(at("2026-10-01 00:00", 0), Err(SpanError::Past));
        assert_eq!(
            at("today 09:00", 0),
            Err(SpanError::Past),
            "09:00 today has passed"
        );
        assert_eq!(
            at("today 18:00 JST", 0),
            Err(SpanError::Past),
            "18:00 JST is 09:00 UTC"
        );
    }

    #[test]
    fn a_moment_more_than_ten_years_away_is_refused() {
        assert_eq!(at("2036-11-01 00:00", 0), Err(SpanError::TooFar));
        assert!(at("2036-09-01 00:00", 0).is_ok());
    }

    #[test]
    fn a_span_knows_whether_it_is_a_moment() {
        assert!(Span::parse("18:00").expect("a form").is_moment());
        assert!(Span::parse("2026-11-01").expect("a form").is_moment());
        assert!(!Span::parse("1h30m").expect("a form").is_moment());
        assert!(!Span::from_duration(Duration::from_secs(60)).is_moment());
    }

    #[test]
    fn a_length_from_a_duration_resolves_to_itself() {
        let span = Span::from_duration(Duration::from_mins(90));
        assert_eq!(span.resolve_at(NOW, 0), Ok(Duration::from_mins(90)));
    }

    /// Every string of up to eight bytes made of multi-byte and combining pieces.
    fn hostile_tails() -> Vec<String> {
        const PIECES: [&str; 6] = ["a", "日", "😀", "\u{301}", "é", "ß"];
        let mut all = vec![String::new()];
        let mut frontier = vec![String::new()];
        while let Some(text) = frontier.pop() {
            for piece in PIECES {
                if text.len() + piece.len() <= 8 {
                    let next = text.clone() + piece;
                    all.push(next.clone());
                    frontier.push(next);
                }
            }
        }
        all
    }

    #[test]
    fn a_zone_with_non_ascii_text_is_refused_without_a_panic() {
        assert_eq!(
            Span::parse("18:00+a日"),
            Err(SpanError::NotUnderstood),
            "byte 2 falls inside 日"
        );
        for prefix in ["18:00", "2026-11-01 18:00", "today 09:00", ""] {
            for sign in ["+", "-"] {
                for tail in hostile_tails() {
                    let text = format!("{prefix}{sign}{tail}");
                    assert_eq!(
                        Span::parse(&text),
                        Err(SpanError::NotUnderstood),
                        "{text:?}"
                    );
                }
            }
        }
    }
}
