//! The showcase of the automod module: the alert and the reply of a rule, as `closure-xtask replay`
//! posts them (see `docs/preview.md`). The names of the tests start with `showcase_`.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "test code: a broken setup should panic at once"
)]

mod common;

use closure_sdk::Locale;
use common::*;

/// A member with a realistic id: the replies name it, and the replay leaves out the test ids.
const SHOWCASE_MEMBER: u64 = 512_345_678_901_234_567;
/// The bot as the cards name it: the alert and the reply come from it.
const SHOWCASE_BOT: u64 = 1_100_000_000_000_000_010;
use serde_json::{Value, json};
use twilight_model::id::Id;

fn words(word: &str) -> Value {
    json!({ "type": "words", "words": [word] })
}

fn spoiler_rule(actions: Value) -> Value {
    rules(&[("spoilers", rule(json!([words("spoiler")]), actions))])
}

#[tokio::test]
async fn showcase_alert_says_who_which_rule_and_what_was_found() {
    let harness = with_rules(spoiler_rule(json!([
        { "do": "alert", "channel": NOTIFY.to_string() }
    ])));
    harness.set_guild_locale(Locale::Ja);
    harness.set_bot_id(Some(Id::new(SHOWCASE_BOT)));

    say(&harness, SHOWCASE_MEMBER, "spoiler").await;

    let texts = sent_texts(&harness);
    assert_eq!(texts.len(), 1, "{texts:?}");
    assert_eq!(texts[0].0, NOTIFY);
}

#[tokio::test]
async fn showcase_reply_asks_the_member_to_keep_it_civil() {
    let harness = with_rules(spoiler_rule(json!([
        { "do": "reply", "text": "{user}、ネタバレは控えてください。" }
    ])));
    harness.set_guild_locale(Locale::Ja);
    harness.set_bot_id(Some(Id::new(SHOWCASE_BOT)));

    say(&harness, SHOWCASE_MEMBER, "spoiler").await;

    let texts = sent_texts(&harness);
    assert_eq!(texts.len(), 1, "{texts:?}");
}
