//! The warn module, run against the SDK's pretend Discord.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::too_many_lines)]

use std::time::Duration;

use closure_core::event::CommandOptionValue;
use closure_mod_warn::builder;
use closure_sdk::testing::{Click, Harness, Invocation, Reply, ReplyKind};
use closure_sdk::{Level, Locale};
use serde_json::json;
use twilight_model::id::Id;

const MODERATOR_ROLE: u64 = 10;
const ADMIN_ROLE: u64 = 20;
const MEMBER: u64 = 7;
const MODERATOR: u64 = 100;

fn harness() -> Harness {
    let harness = Harness::new(builder().unwrap()).unwrap();
    harness.permissions(|permissions| {
        permissions
            .role_levels
            .insert(Id::new(MODERATOR_ROLE), Level::MODERATOR);
        permissions
            .role_levels
            .insert(Id::new(ADMIN_ROLE), Level::ADMIN);
    });
    harness
}

fn sub(name: &str, user: u64) -> Invocation {
    Invocation::new("warn")
        .subcommand(name)
        .option("user", CommandOptionValue::User(Id::new(user)))
}

fn add(user: u64, reason: &str) -> Invocation {
    sub("add", user)
        .option("reason", CommandOptionValue::String(reason.to_owned()))
        .roles([MODERATOR_ROLE])
        .user(MODERATOR)
}

fn text(replies: &[Reply]) -> &str {
    &replies[0].content
}

#[tokio::test]
async fn a_moderator_warns_and_sees_the_warnings() {
    let harness = harness();
    let replies = harness.invoke(add(MEMBER, "spam")).await.unwrap();
    assert!(
        text(&replies).contains("Warned <@7> (1 active)"),
        "{replies:?}"
    );
    assert!(
        replies[0].ephemeral,
        "the warning is for the moderator, not the channel"
    );
    harness.advance(Duration::from_secs(5));
    harness.invoke(add(MEMBER, "")).await.unwrap();

    let list = harness
        .invoke(sub("list", MEMBER).roles([MODERATOR_ROLE]))
        .await
        .unwrap();
    let shown = text(&list);
    assert!(shown.contains("2 active, 2 given in all"), "{shown}");
    assert!(shown.contains("#1 spam (by <@100>)"), "{shown}");
    assert!(shown.contains("#2 "), "{shown}");
}

#[tokio::test]
async fn permissions_and_the_cooldown_apply_before_anything_is_stored() {
    let harness = harness();
    // Nobody in particular may not warn.
    let denied = harness.invoke(sub("add", MEMBER).user(55)).await.unwrap();
    assert!(text(&denied).contains("level"), "{denied:?}");
    // A moderator may warn but not clear.
    let denied = harness
        .invoke(sub("clear", MEMBER).roles([MODERATOR_ROLE]))
        .await
        .unwrap();
    assert!(text(&denied).contains("level"), "{denied:?}");
    // Warning twice in a row is held back.
    harness.invoke(add(MEMBER, "a")).await.unwrap();
    let held = harness.invoke(add(MEMBER, "b")).await.unwrap();
    assert!(text(&held).to_lowercase().contains("wait"), "{held:?}");

    let list = harness
        .invoke(sub("list", MEMBER).roles([MODERATOR_ROLE]))
        .await
        .unwrap();
    assert!(
        text(&list).contains("1 active"),
        "only the first one counted"
    );
    assert_eq!(harness.jobs(), 1);
}

#[tokio::test]
async fn a_moderator_cannot_warn_themselves() {
    let harness = harness();
    let replies = harness.invoke(add(MODERATOR, "oops")).await.unwrap();
    assert_eq!(text(&replies), "You cannot warn yourself.");
    assert_eq!(harness.jobs(), 0);
}

#[tokio::test]
async fn a_warning_expires_after_the_configured_days_and_a_second_run_does_no_harm() {
    let harness = harness();
    harness.set_config(json!({ "expire_days": 2 }));
    harness.invoke(add(MEMBER, "x")).await.unwrap();
    harness.advance(Duration::from_secs(5));
    harness.invoke(add(MEMBER, "y")).await.unwrap();

    harness.advance(Duration::from_secs(2 * 86_400 - 6));
    assert!(harness.run_due_jobs().await.is_empty(), "not yet");
    harness.advance(Duration::from_secs(1));
    let ended = harness.run_due_jobs().await;
    assert_eq!(
        ended.len(),
        1,
        "the first has expired, the second has a few seconds to go"
    );
    assert!(ended[0].is_ok());

    let list = harness
        .invoke(sub("list", MEMBER).roles([MODERATOR_ROLE]))
        .await
        .unwrap();
    assert!(text(&list).contains("1 active, 2 given in all"), "{list:?}");

    harness.advance(Duration::from_secs(10));
    assert!(harness.run_due_jobs().await.iter().all(Result::is_ok));
    let list = harness
        .invoke(sub("list", MEMBER).roles([MODERATOR_ROLE]))
        .await
        .unwrap();
    assert!(text(&list).contains("no active warnings"), "{list:?}");
    assert_eq!(harness.jobs(), 0);
}

#[tokio::test]
async fn clearing_asks_first_and_only_the_one_who_asked_can_answer() {
    let harness = harness();
    harness.invoke(add(MEMBER, "a")).await.unwrap();
    harness.advance(Duration::from_secs(5));
    harness.invoke(add(MEMBER, "b")).await.unwrap();

    let asked = harness
        .invoke(sub("clear", MEMBER).roles([ADMIN_ROLE]).user(200))
        .await
        .unwrap();
    assert!(text(&asked).contains("Remove all 2 warnings"), "{asked:?}");
    let ids = asked[0].custom_ids();
    assert_eq!(ids.len(), 2);

    // Somebody else pressing the button changes nothing.
    let other = harness
        .click(Click::new(ids[0].clone()).user(300).roles([ADMIN_ROLE]))
        .await
        .unwrap();
    assert!(other[0].ephemeral);
    let list = harness
        .invoke(sub("list", MEMBER).roles([MODERATOR_ROLE]))
        .await
        .unwrap();
    assert!(text(&list).contains("2 active"), "{list:?}");

    // "Keep" keeps; "Remove" removes, and the expiry jobs go with the warnings.
    let kept = harness
        .click(Click::new(ids[1].clone()).user(200).roles([ADMIN_ROLE]))
        .await
        .unwrap();
    assert_eq!(kept[0].kind, ReplyKind::Update);
    assert_eq!(text(&kept), "Nothing was removed.");
    assert_eq!(harness.jobs(), 2);

    let done = harness
        .click(Click::new(ids[0].clone()).user(200).roles([ADMIN_ROLE]))
        .await
        .unwrap();
    assert_eq!(done[0].kind, ReplyKind::Update);
    assert!(
        text(&done).contains("Removed 2 warnings of <@7>"),
        "{done:?}"
    );
    assert!(done[0].components.is_empty(), "the buttons are gone");
    assert_eq!(harness.jobs(), 0);
}

#[tokio::test]
async fn the_button_asks_the_permission_again() {
    let harness = harness();
    harness.invoke(add(MEMBER, "a")).await.unwrap();
    let asked = harness
        .invoke(sub("clear", MEMBER).roles([ADMIN_ROLE]).user(200))
        .await
        .unwrap();
    let ids = asked[0].custom_ids();
    // The role was taken away between the question and the click.
    let replies = harness
        .click(Click::new(ids[0].clone()).user(200).roles([MODERATOR_ROLE]))
        .await
        .unwrap();
    assert!(text(&replies).contains("level"), "{replies:?}");
    let list = harness
        .invoke(sub("list", MEMBER).roles([MODERATOR_ROLE]))
        .await
        .unwrap();
    assert!(text(&list).contains("1 active"), "{list:?}");
}

#[tokio::test]
async fn at_most_25_warnings_are_kept_per_member() {
    let harness = harness();
    for n in 0..25 {
        harness.advance(Duration::from_secs(5));
        let replies = harness.invoke(add(MEMBER, &format!("n{n}"))).await.unwrap();
        assert!(text(&replies).contains("Warned"), "{n}: {replies:?}");
    }
    harness.advance(Duration::from_secs(5));
    let replies = harness.invoke(add(MEMBER, "one more")).await.unwrap();
    assert!(text(&replies).contains("the most warnings"), "{replies:?}");
    assert_eq!(harness.jobs(), 25, "the refused one left no job behind");

    let list = harness
        .invoke(sub("list", MEMBER).roles([MODERATOR_ROLE]))
        .await
        .unwrap();
    assert!(
        text(&list).contains("25 active, 25 given in all"),
        "{list:?}"
    );
    assert!(text(&list).len() < 2000, "a message holds 2000 characters");
    assert_eq!(
        text(&list).matches('#').count(),
        10,
        "the newest ten are shown"
    );
}

#[tokio::test]
async fn long_reasons_are_cut_in_the_list_and_japanese_is_spoken_to_japanese_speakers() {
    let harness = harness();
    let long = "あ".repeat(200);
    let replies = harness
        .invoke(add(MEMBER, &long).locale(Locale::Ja))
        .await
        .unwrap();
    assert!(text(&replies).contains("に警告しました"), "{replies:?}");

    let list = harness
        .invoke(
            sub("list", MEMBER)
                .roles([MODERATOR_ROLE])
                .locale(Locale::Ja),
        )
        .await
        .unwrap();
    let shown = text(&list);
    assert!(shown.contains("有効 1 件"), "{shown}");
    assert!(shown.contains('…') && !shown.contains(&long), "{shown}");
}

#[test]
fn both_languages_define_the_same_messages() {
    let en = include_str!("../locales/en.ftl");
    let ja = include_str!("../locales/ja.ftl");
    assert_eq!(
        closure_i18n::missing_keys(en, ja).unwrap(),
        Vec::<String>::new()
    );
    assert_eq!(
        closure_i18n::missing_keys(ja, en).unwrap(),
        Vec::<String>::new()
    );
}
