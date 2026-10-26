//! The guild's time zone: a fixed UTC offset. Names are matched from a short list instead of a
//! time zone database, so only zones without daylight saving time are accepted by name.

use std::ops::RangeInclusive;

use thiserror::Error;

const HOUR: i64 = 3_600;
const MIN_OFFSET: i64 = -12 * HOUR;
const MAX_OFFSET: i64 = 14 * HOUR;

/// Zones with a fixed offset today, by name. Only zones that have no daylight saving time now.
const FIXED_ZONES: &[(&str, i64)] = &[
    ("Asia/Tokyo", 9 * HOUR),
    ("Asia/Seoul", 9 * HOUR),
    ("Asia/Shanghai", 8 * HOUR),
    ("Asia/Hong_Kong", 8 * HOUR),
    ("Asia/Taipei", 8 * HOUR),
    ("Asia/Singapore", 8 * HOUR),
    ("Asia/Bangkok", 7 * HOUR),
    ("Asia/Jakarta", 7 * HOUR),
    ("Asia/Kolkata", 5 * HOUR + 1_800),
    ("Asia/Dubai", 4 * HOUR),
];

/// Zones that do observe daylight saving time. They are refused by name, with a hint, because
/// a fixed offset is the only thing the bot can keep right.
const DAYLIGHT_SAVING_ZONES: &[&str] = &[
    "America/New_York",
    "America/Chicago",
    "America/Denver",
    "America/Los_Angeles",
    "America/Toronto",
    "America/Vancouver",
    "Europe/London",
    "Europe/Dublin",
    "Europe/Lisbon",
    "Europe/Paris",
    "Europe/Berlin",
    "Europe/Madrid",
    "Europe/Rome",
    "Europe/Amsterdam",
    "Australia/Sydney",
    "Australia/Melbourne",
    "Pacific/Auckland",
];

/// Why a time zone is refused.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum TimezoneError {
    #[error(
        "`{0}` is not a time zone; write UTC, an offset such as `UTC+9` or `+09:00`, or a fixed zone such as `Asia/Tokyo`"
    )]
    Unknown(String),
    #[error(
        "`{0}` observes daylight saving time, which is not supported; set a fixed offset such as `UTC-5` and change it when the clocks change"
    )]
    DaylightSaving(String),
    #[error("`{0}` is outside the range UTC-12:00 to UTC+14:00")]
    OutOfRange(String),
}

/// A fixed UTC offset in seconds, between UTC-12:00 and UTC+14:00.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Timezone {
    offset: i64,
}

impl Timezone {
    /// Reads a zone as the admin wrote it. Surrounding spaces are ignored. `UTC`, `GMT` and `Z`
    /// (and `UTC`/`GMT` before an offset) match in any case; zone names must be spelled as listed.
    pub fn parse(text: &str) -> Result<Self, TimezoneError> {
        let text = text.trim();
        if ["UTC", "GMT", "Z"]
            .iter()
            .any(|name| text.eq_ignore_ascii_case(name))
        {
            return Ok(Self { offset: 0 });
        }
        if let Some(rest) = utc_prefix(text) {
            return Ok(Self {
                offset: signed(rest, text)?,
            });
        }
        if text.starts_with(['+', '-']) {
            return Ok(Self {
                offset: signed(text, text)?,
            });
        }
        if let Some((_, offset)) = FIXED_ZONES.iter().find(|(name, _)| *name == text) {
            return Ok(Self { offset: *offset });
        }
        if DAYLIGHT_SAVING_ZONES.contains(&text) {
            return Err(TimezoneError::DaylightSaving(text.to_owned()));
        }
        Err(TimezoneError::Unknown(text.to_owned()))
    }

    #[must_use]
    pub fn offset_seconds(self) -> i64 {
        self.offset
    }

    /// `UTC`, or `UTC+09:00` and the like.
    #[must_use]
    pub fn label(self) -> String {
        offset_label(self.offset)
    }
}

/// The text after a `UTC` or `GMT` prefix (in any case), if there is one.
fn utc_prefix(text: &str) -> Option<&str> {
    let (head, rest) = text.split_at_checked(3)?;
    (head.eq_ignore_ascii_case("UTC") || head.eq_ignore_ascii_case("GMT")).then_some(rest)
}

/// An offset such as `+9`, `-05:00` or `+0930`, where `whole` is the text it was read from.
fn signed(body: &str, whole: &str) -> Result<i64, TimezoneError> {
    let unknown = || TimezoneError::Unknown(whole.to_owned());
    let (sign, digits) = match body.split_at_checked(1) {
        Some(("+", digits)) => (1, digits),
        Some(("-", digits)) => (-1, digits),
        _ => return Err(unknown()),
    };
    let (hours, minutes) = match digits.split_once(':') {
        Some((hours, minutes)) => (number(hours, 1..=2), number(minutes, 2..=2)),
        None if digits.len() == 4 => match digits.split_at_checked(2) {
            Some((hours, minutes)) => (number(hours, 2..=2), number(minutes, 2..=2)),
            None => (None, None),
        },
        None => (number(digits, 1..=2), Some(0)),
    };
    let (Some(hours), Some(minutes)) = (hours, minutes) else {
        return Err(unknown());
    };
    if minutes > 59 {
        return Err(unknown());
    }
    let offset = sign * (hours * HOUR + minutes * 60);
    if (MIN_OFFSET..=MAX_OFFSET).contains(&offset) {
        Ok(offset)
    } else {
        Err(TimezoneError::OutOfRange(whole.to_owned()))
    }
}

/// `digits` as a number, if it is `len` ASCII digits long.
fn number(digits: &str, len: RangeInclusive<usize>) -> Option<i64> {
    if len.contains(&digits.len()) && digits.bytes().all(|byte| byte.is_ascii_digit()) {
        digits.parse().ok()
    } else {
        None
    }
}

/// The label of an offset in seconds: `UTC`, or `UTC+09:00`.
#[must_use]
pub fn offset_label(seconds: i64) -> String {
    if seconds == 0 {
        return "UTC".to_owned();
    }
    let sign = if seconds < 0 { '-' } else { '+' };
    let minutes = seconds.abs() / 60;
    format!("UTC{sign}{:02}:{:02}", minutes / 60, minutes % 60)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn offset(text: &str) -> i64 {
        Timezone::parse(text).unwrap().offset_seconds()
    }

    #[test]
    fn every_utc_spelling_is_zero() {
        for text in [
            "UTC",
            "utc",
            "Utc",
            "GMT",
            "gmt",
            "Z",
            "z",
            "UTC+0",
            "UTC+00:00",
            "+00:00",
        ] {
            assert_eq!(offset(text), 0, "{text}");
        }
    }

    #[test]
    fn offsets_in_hours_and_minutes_are_read_in_every_form() {
        assert_eq!(offset("UTC+9"), 9 * HOUR);
        assert_eq!(offset("UTC+09:00"), 9 * HOUR);
        assert_eq!(offset("UTC-5"), -5 * HOUR);
        assert_eq!(offset("GMT+9"), 9 * HOUR);
        assert_eq!(offset("utc+9"), 9 * HOUR);
        assert_eq!(offset("UTC+05:30"), 5 * HOUR + 1_800);
        assert_eq!(offset("+09:00"), 9 * HOUR);
        assert_eq!(offset("-0500"), -5 * HOUR);
        assert_eq!(offset("+0930"), 9 * HOUR + 1_800);
        assert_eq!(offset("  UTC+9  "), 9 * HOUR);
    }

    #[test]
    fn every_fixed_zone_has_its_offset() {
        assert_eq!(offset("Asia/Tokyo"), 9 * HOUR);
        assert_eq!(offset("Asia/Seoul"), 9 * HOUR);
        assert_eq!(offset("Asia/Shanghai"), 8 * HOUR);
        assert_eq!(offset("Asia/Hong_Kong"), 8 * HOUR);
        assert_eq!(offset("Asia/Taipei"), 8 * HOUR);
        assert_eq!(offset("Asia/Singapore"), 8 * HOUR);
        assert_eq!(offset("Asia/Bangkok"), 7 * HOUR);
        assert_eq!(offset("Asia/Jakarta"), 7 * HOUR);
        assert_eq!(offset("Asia/Kolkata"), 5 * HOUR + 1_800);
        assert_eq!(offset("Asia/Dubai"), 4 * HOUR);
    }

    #[test]
    fn zone_names_are_case_sensitive() {
        assert!(matches!(
            Timezone::parse("asia/tokyo"),
            Err(TimezoneError::Unknown(_))
        ));
    }

    #[test]
    fn daylight_saving_zones_are_refused_with_a_hint() {
        let error = Timezone::parse("America/New_York").unwrap_err();
        assert_eq!(
            error,
            TimezoneError::DaylightSaving("America/New_York".to_owned())
        );
        assert!(error.to_string().contains("fixed offset"));
        assert!(matches!(
            Timezone::parse("Europe/London"),
            Err(TimezoneError::DaylightSaving(_))
        ));
    }

    #[test]
    fn offsets_outside_the_range_are_refused() {
        for text in ["UTC+15", "UTC-13", "+14:30", "-12:30", "UTC+99", "+24:00"] {
            assert!(
                matches!(Timezone::parse(text), Err(TimezoneError::OutOfRange(_))),
                "{text}"
            );
        }
        assert_eq!(offset("UTC+14"), 14 * HOUR);
        assert_eq!(offset("UTC-12"), -12 * HOUR);
    }

    #[test]
    fn garbage_is_refused() {
        for text in [
            "",
            "   ",
            "Tokyo",
            "UTC+",
            "UTC+9:5",
            "+9:99",
            "+123",
            "UTC9",
            "+ab",
            "+０９",
            "UTC+09:00:00",
        ] {
            assert!(
                matches!(Timezone::parse(text), Err(TimezoneError::Unknown(_))),
                "{text:?}"
            );
        }
    }

    #[test]
    fn labels_show_the_offset() {
        assert_eq!(Timezone::parse("Asia/Tokyo").unwrap().label(), "UTC+09:00");
        assert_eq!(Timezone::parse("UTC").unwrap().label(), "UTC");
        assert_eq!(Timezone::parse("-0500").unwrap().label(), "UTC-05:00");
        assert_eq!(
            Timezone::parse("Asia/Kolkata").unwrap().label(),
            "UTC+05:30"
        );
        assert_eq!(offset_label(-(3 * HOUR + 1_800)), "UTC-03:30");
    }
}
