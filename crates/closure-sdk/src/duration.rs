//! Durations and points in time for moderation: what a user types (`1h30m`) and how Discord
//! shows a moment.

use std::time::Duration;

use closure_core::locale::{Args, Locale};
use closure_core::module::EventContext;
use closure_core::schedule::MAX_DELAY;
use thiserror::Error;

/// Longer than any digits a moderation duration can reasonably have; more is `TooLarge`.
const MAX_DIGITS: usize = 10;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum DurationError {
    #[error("the duration is empty")]
    Empty,
    #[error("the duration is not of the form 1h30m")]
    Invalid,
    #[error("the duration is longer than ten years")]
    TooLarge,
}

/// Parses `1h30m`, `1h 30m`, `2d`, `1w2d` and the like: numbers with one unit each, the units
/// `s m h d w` in lowercase. A total of zero is refused, since it would moderate nothing.
pub fn parse(text: &str) -> Result<Duration, DurationError> {
    let mut rest = text.trim();
    if rest.is_empty() {
        return Err(DurationError::Empty);
    }
    let mut total: u64 = 0;
    while !rest.is_empty() {
        let digits = rest.bytes().take_while(u8::is_ascii_digit).count();
        if digits > MAX_DIGITS {
            return Err(DurationError::TooLarge);
        }
        let (number, after) = rest.split_at(digits);
        let value: u64 = number.parse().map_err(|_| DurationError::Invalid)?;
        let unit = after.chars().next().ok_or(DurationError::Invalid)?;
        let seconds = match unit {
            's' => 1,
            'm' => 60,
            'h' => 3_600,
            'd' => 86_400,
            'w' => 604_800,
            _ => return Err(DurationError::Invalid),
        };
        let seconds = value.checked_mul(seconds).ok_or(DurationError::TooLarge)?;
        total = total.checked_add(seconds).ok_or(DurationError::TooLarge)?;
        rest = after[unit.len_utf8()..].trim_start();
    }
    match total {
        0 => Err(DurationError::Invalid),
        total if total > MAX_DELAY.as_secs() => Err(DurationError::TooLarge),
        total => Ok(Duration::from_secs(total)),
    }
}

/// How Discord shows a moment (`<t:…:R>` is "in 2 hours").
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TimeStyle {
    ShortTime,
    LongTime,
    ShortDate,
    LongDate,
    ShortDateTime,
    LongDateTime,
    Relative,
}

impl TimeStyle {
    fn code(self) -> char {
        match self {
            Self::ShortTime => 't',
            Self::LongTime => 'T',
            Self::ShortDate => 'd',
            Self::LongDate => 'D',
            Self::ShortDateTime => 'f',
            Self::LongDateTime => 'F',
            Self::Relative => 'R',
        }
    }
}

/// The Discord markup of a moment, in seconds since the Unix epoch.
#[must_use]
pub fn timestamp(unix_seconds: i64, style: TimeStyle) -> String {
    format!("<t:{unix_seconds}:{}>", style.code())
}

/// When Discord made the object with this snowflake id, in Unix seconds: the id carries the time.
#[must_use]
pub fn snowflake_time(id: u64) -> i64 {
    const DISCORD_EPOCH_MS: u64 = 1_420_070_400_000;
    i64::try_from(((id >> 22) + DISCORD_EPOCH_MS) / 1_000).unwrap_or(i64::MAX)
}

/// The Discord markup of the moment `duration` after `now`, in seconds since the Unix epoch. A wait or
/// a punishment shows when it ends this way, in each reader's own time zone.
#[must_use]
pub fn ends_at(now: i64, duration: Duration, style: TimeStyle) -> String {
    let end = now.saturating_add(i64::try_from(duration.as_secs()).unwrap_or(i64::MAX));
    timestamp(end, style)
}

/// A length of time in `locale`, in its two largest units that are not zero. Weeks are counted in
/// days. Used for what is said to a member or to the log, in the server's language.
#[must_use]
pub fn format_in(ctx: &EventContext, locale: Locale, duration: Duration) -> String {
    const UNITS: [(u64, &str); 4] = [
        (86_400, "sdk-unit-day"),
        (3_600, "sdk-unit-hour"),
        (60, "sdk-unit-minute"),
        (1, "sdk-unit-second"),
    ];
    let mut seconds = duration.as_secs();
    let mut parts = Vec::new();
    for (size, key) in UNITS {
        let count = seconds / size;
        seconds %= size;
        if count > 0 && parts.len() < 2 {
            let count = i64::try_from(count).unwrap_or(i64::MAX);
            parts.push(ctx.t_core(locale, key, &Args::new().with("n", count)));
        }
    }
    if parts.is_empty() {
        return ctx.t_core(locale, "sdk-duration-none", &Args::new());
    }
    parts.join(" ")
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use closure_core::schedule::MAX_DELAY;

    use super::{DurationError, TimeStyle, ends_at, parse, snowflake_time, timestamp};

    #[test]
    fn parses_the_usual_forms() {
        for (text, seconds) in [
            ("10m", 600),
            ("1h30m", 5400),
            ("1h 30m", 5400),
            ("2d", 172_800),
            ("1w2d", 9 * 86_400),
            ("90s", 90),
        ] {
            assert_eq!(parse(text), Ok(Duration::from_secs(seconds)), "{text:?}");
        }
    }

    #[test]
    fn rejects_what_is_not_a_duration() {
        use DurationError::{Empty, Invalid, TooLarge};
        for (text, expected) in [
            ("", Empty),
            (" ", Empty),
            ("10", Invalid),
            ("m", Invalid),
            ("10x", Invalid),
            ("-5m", Invalid),
            ("1.5h", Invalid),
            ("10M", Invalid),
            ("１０ｍ", Invalid),
            ("0m", Invalid),
            ("0s", Invalid),
            ("99999999999w", TooLarge),
            ("3651d", TooLarge),
        ] {
            assert_eq!(parse(text), Err(expected), "{text:?}");
        }
    }

    #[test]
    fn ten_years_is_the_longest_allowed() {
        assert_eq!(parse("3650d"), Ok(MAX_DELAY));
        assert_eq!(parse("3651d"), Err(DurationError::TooLarge));
    }

    #[test]
    fn a_span_from_now_is_the_moment_it_ends_at() {
        assert_eq!(
            ends_at(
                1_700_000_000,
                Duration::from_secs(90),
                TimeStyle::ShortDateTime
            ),
            "<t:1700000090:f>"
        );
    }

    #[test]
    fn a_snowflake_says_when_its_object_was_made() {
        // Discord's snowflake: milliseconds since its epoch, then the worker and sequence bits.
        let id = ((1_600_000_000_000 - 1_420_070_400_000) << 22) | 1;

        assert_eq!(snowflake_time(id), 1_600_000_000);
    }

    #[test]
    fn timestamps_have_one_code_per_style() {
        for (style, code) in [
            (TimeStyle::ShortTime, 't'),
            (TimeStyle::LongTime, 'T'),
            (TimeStyle::ShortDate, 'd'),
            (TimeStyle::LongDate, 'D'),
            (TimeStyle::ShortDateTime, 'f'),
            (TimeStyle::LongDateTime, 'F'),
            (TimeStyle::Relative, 'R'),
        ] {
            assert_eq!(
                timestamp(1_700_000_000, style),
                format!("<t:1700000000:{code}>")
            );
        }
    }
}
