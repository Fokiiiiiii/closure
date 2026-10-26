//! A `Span` as a command option, run against the SDK's pretend Discord.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::time::{SystemTime, UNIX_EPOCH};

use closure_core::event::CommandOptionValue;
use closure_core::settings::GuildPolicy;
use closure_core::timezone::Timezone;
use closure_sdk::testing::{Harness, Invocation};
use closure_sdk::{Command, CommandArgs, CommandCx, Locale, ModuleBuilder, Span, UserError};

const HOUR: i64 = 3_600;

#[derive(CommandArgs)]
struct Until {
    /// When it ends
    until: Span,
}

/// Answers with the seconds from now until the moment the option names.
async fn until(cx: CommandCx, args: Until) -> Result<(), UserError> {
    let length = args.until.resolve(&cx)?;
    cx.reply(length.as_secs().to_string()).await
}

fn harness() -> Harness {
    let builder = ModuleBuilder::new("span-test", "Span test", "0.1.0")
        .expect("a module id")
        .command(Command::<Until>::new("until", "Test").handler(until));
    Harness::new(builder).expect("a harness")
}

fn now() -> i64 {
    i64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("after 1970")
            .as_secs(),
    )
    .expect("fits")
}

fn is_leap(year: i64) -> bool {
    year % 4 == 0 && (year % 100 != 0 || year % 400 == 0)
}

/// `unix` written as a wall clock `YYYY-MM-DD HH:MM` at `offset` seconds from UTC.
fn wall_clock(unix: i64, offset: i64) -> String {
    let local = unix + offset;
    let mut days = local.div_euclid(86_400);
    let seconds = local.rem_euclid(86_400);
    let mut year = 1970;
    loop {
        let length = if is_leap(year) { 366 } else { 365 };
        if days < length {
            break;
        }
        days -= length;
        year += 1;
    }
    let february = if is_leap(year) { 29 } else { 28 };
    let lengths = [31, february, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];
    let mut month = 0;
    while days >= lengths[month] {
        days -= lengths[month];
        month += 1;
    }
    format!(
        "{year:04}-{:02}-{:02} {:02}:{:02}",
        month + 1,
        days + 1,
        seconds / HOUR,
        seconds % HOUR / 60
    )
}

/// A minute three days from now, which the wall clock text names exactly.
fn target() -> i64 {
    (now() + 3 * 86_400).div_euclid(60) * 60
}

async fn run(harness: &Harness, text: &str) -> String {
    let invocation =
        Invocation::new("until").option("until", CommandOptionValue::String(text.to_owned()));
    let replies = harness.invoke(invocation).await.expect("the command runs");
    replies.first().expect("a reply").content.clone()
}

fn seconds(reply: &str) -> i64 {
    reply.parse().expect("a number of seconds")
}

#[tokio::test]
async fn a_server_in_english_reads_a_moment_without_zone_as_utc() {
    let harness = harness();
    let before = now();
    let target = target();
    let reply = run(&harness, &wall_clock(target, 0)).await;
    let expected = target - before;
    assert!(
        (expected - 2..=expected).contains(&seconds(&reply)),
        "{reply} vs {expected}"
    );
}

#[tokio::test]
async fn a_japanese_server_reads_a_moment_without_zone_as_jst() {
    let harness = harness();
    harness.set_guild_locale(Locale::Ja);
    let before = now();
    let target = target();
    let reply = run(&harness, &wall_clock(target, 9 * HOUR)).await;
    let expected = target - before;
    assert!(
        (expected - 2..=expected).contains(&seconds(&reply)),
        "{reply} vs {expected}"
    );
}

#[tokio::test]
async fn the_same_text_means_another_moment_on_an_english_server() {
    let harness = harness();
    let before = now();
    let target = target();
    // The wall clock of a JST moment, read as UTC on an English server: nine hours later.
    let reply = run(&harness, &wall_clock(target, 9 * HOUR)).await;
    let expected = target + 9 * HOUR - before;
    assert!(
        (expected - 2..=expected).contains(&seconds(&reply)),
        "{reply} vs {expected}"
    );
}

#[tokio::test]
async fn an_explicit_zone_wins_over_the_server_language() {
    let harness = harness();
    harness.set_guild_locale(Locale::En);
    let before = now();
    let target = target();
    let typed = format!("{} JST", wall_clock(target, 9 * HOUR));
    let reply = run(&harness, &typed).await;
    let expected = target - before;
    assert!(
        (expected - 2..=expected).contains(&seconds(&reply)),
        "{reply} vs {expected}"
    );
}

#[tokio::test]
async fn a_server_timezone_replaces_the_language_default() {
    let harness = harness();
    harness.set_policy(GuildPolicy {
        timezone: Some(Timezone::parse("Asia/Tokyo").unwrap()),
        ..GuildPolicy::default()
    });
    // An English server would read this as UTC, nine hours later than the Tokyo moment.
    let before = now();
    let target = target();
    let reply = run(&harness, &wall_clock(target, 9 * HOUR)).await;
    let expected = target - before;
    assert!(
        (expected - 2..=expected).contains(&seconds(&reply)),
        "{reply} vs {expected}"
    );
}

#[tokio::test]
async fn a_japanese_server_with_a_utc_timezone_reads_utc() {
    let harness = harness();
    harness.set_guild_locale(Locale::Ja);
    harness.set_policy(GuildPolicy {
        timezone: Some(Timezone::parse("UTC").unwrap()),
        ..GuildPolicy::default()
    });
    let before = now();
    let target = target();
    let reply = run(&harness, &wall_clock(target, 0)).await;
    let expected = target - before;
    assert!(
        (expected - 2..=expected).contains(&seconds(&reply)),
        "{reply} vs {expected}"
    );
}

#[tokio::test]
async fn an_error_names_the_server_timezone_it_was_read_in() {
    let harness = harness();
    harness.set_policy(GuildPolicy {
        timezone: Some(Timezone::parse("-0500").unwrap()),
        ..GuildPolicy::default()
    });
    let past = wall_clock(now() - HOUR, -5 * HOUR);
    let replies = harness
        .invoke(Invocation::new("until").option("until", CommandOptionValue::String(past)))
        .await
        .expect("the command runs");
    assert!(replies[0].content.contains("UTC-05:00"), "{replies:?}");
}

#[tokio::test]
async fn a_relative_length_is_still_the_same_length() {
    let harness = harness();
    assert_eq!(run(&harness, "1h30m").await, "5400");
}

#[tokio::test]
async fn a_moment_that_has_passed_is_answered_and_nothing_else_happens() {
    let harness = harness();
    let past = wall_clock(now() - HOUR, 0);
    let replies = harness
        .invoke(Invocation::new("until").option("until", CommandOptionValue::String(past)))
        .await
        .expect("the command runs");
    assert_eq!(replies.len(), 1, "{replies:?}");
    assert!(replies[0].content.contains("passed"), "{replies:?}");
    assert_eq!(harness.jobs(), 0);
}

#[tokio::test]
async fn a_text_that_is_no_time_is_a_bad_argument() {
    let harness = harness();
    let reply = run(&harness, "banana").await;
    assert!(reply.contains("until"), "{reply}");
}
