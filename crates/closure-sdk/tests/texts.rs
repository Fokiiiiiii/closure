#![allow(clippy::unwrap_used, clippy::expect_used, clippy::too_many_lines)]

use closure_core::locale::Args;
use closure_core::module::Module;
use closure_sdk::testing::{Harness, Invocation};
use closure_sdk::{Command, ModuleBuilder};
use serde::Deserialize;
use serde_json::json;

/// Discord takes at most this many characters in a message's content.
const DISCORD_LIMIT: usize = 2_000;

const EN: &str = "
warned = Watch your step, { $reason }.
plain = Watch your step.
long = Short.
";

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Settings {
    limit: u32,
}

fn module() -> ModuleBuilder {
    ModuleBuilder::new("texts", "Texts", "0.1.0")
        .unwrap()
        .config(
            json!({
                "type": "object",
                "properties": { "limit": { "type": "integer" } },
                "additionalProperties": false
            }),
            json!({ "limit": 1 }),
        )
        .command(Command::<()>::new("warn").handler(|cx, ()| async move {
            cx.reply_t("warned", &Args::new().with("reason", "spam"))
                .await
        }))
        .command(
            Command::<()>::new("plain")
                .handler(|cx, ()| async move { cx.reply_t("plain", &Args::new()).await }),
        )
        .command(
            Command::<()>::new("long")
                .handler(|cx, ()| async move { cx.reply_t("long", &Args::new()).await }),
        )
        .command(Command::<()>::new("quiet").handler(|_cx, ()| async move { Ok(()) }))
        .command(Command::<()>::new("limit").handler(|cx, ()| async move {
            let settings: Settings = cx.config()?;
            cx.reply(settings.limit.to_string()).await
        }))
        .catalog(closure_sdk::Locale::En, EN)
}

fn harness_with(config: serde_json::Value) -> Harness {
    let harness = Harness::new(module()).unwrap();
    harness.set_config(config);
    harness
}

/// The text of the one embed that `command` answers with.
async fn shown(harness: &Harness, command: &str) -> String {
    let replies = harness.invoke(Invocation::new(command)).await.unwrap();
    assert_eq!(replies.len(), 1, "{replies:?}");
    replies[0].embeds[0].description.clone().unwrap()
}

#[tokio::test]
async fn an_administrators_text_replaces_the_module_text() {
    let harness = harness_with(json!({ "texts": { "warned": "Careful, {reason}! {user}" } }));
    // `{reason}` is filled in; `{user}` is no argument, so it stays as it is.
    assert_eq!(shown(&harness, "warn").await, "Careful, spam! {user}");
}

#[tokio::test]
async fn an_empty_or_missing_text_keeps_the_module_text() {
    let missing = Harness::new(module()).unwrap();
    assert_eq!(shown(&missing, "warn").await, "Watch your step, spam.");

    let empty = harness_with(json!({ "texts": { "warned": "" } }));
    assert_eq!(shown(&empty, "warn").await, "Watch your step, spam.");
}

#[tokio::test]
async fn a_value_that_is_not_a_text_or_an_unknown_key_is_ignored() {
    let configs = [
        json!({ "texts": { "warned": 5, "plain": null, "plain-unknown": "x" } }),
        json!({ "texts": { "warned": ["a"], "plain": { "nested": "x" } } }),
        json!({ "texts": "not an object" }),
        json!({ "texts": null }),
        json!({ "texts": [] }),
    ];
    for config in configs {
        let harness = harness_with(config.clone());
        assert_eq!(
            shown(&harness, "warn").await,
            "Watch your step, spam.",
            "{config}"
        );
        assert_eq!(
            shown(&harness, "plain").await,
            "Watch your step.",
            "{config}"
        );
    }
}

#[tokio::test]
async fn the_module_still_reads_its_own_settings_next_to_the_texts() {
    let harness = harness_with(json!({ "limit": 3, "texts": { "warned": 5 } }));
    assert_eq!(shown(&harness, "limit").await, "3");

    let harness = harness_with(json!({ "limit": 4, "texts": { "warned": "Hi, {reason}." } }));
    assert_eq!(shown(&harness, "limit").await, "4");
}

#[tokio::test]
async fn the_shared_texts_of_the_sdk_take_the_override_too() {
    // A handler that answers nothing gets the SDK's own "Done.", from the core catalog.
    let harness = harness_with(json!({ "texts": { "sdk-done": "Noted." } }));
    assert_eq!(shown(&harness, "quiet").await, "Noted.");
    let harness = Harness::new(module()).unwrap();
    assert_eq!(shown(&harness, "quiet").await, "Done.");
}

#[tokio::test]
async fn a_text_longer_than_discord_takes_is_cut_with_an_ellipsis() {
    let harness = harness_with(json!({ "texts": { "long": "a".repeat(2_500) } }));
    let text = shown(&harness, "long").await;
    assert_eq!(text.chars().count(), DISCORD_LIMIT, "{text}");
    assert!(text.ends_with('…'), "{text}");
}

#[test]
fn the_schema_takes_any_value_under_texts_and_a_module_may_not_declare_it() {
    let built = module().build().unwrap();
    let texts = &built.manifest().config_schema["properties"]["texts"];
    // No type and no other keyword: the schema refuses no value, so a bad text cannot refuse
    // the whole configuration.
    assert!(
        texts
            .as_object()
            .unwrap()
            .keys()
            .all(|key| key == "description"),
        "{texts}"
    );

    let declared = ModuleBuilder::new("texts", "Texts", "0.1.0")
        .unwrap()
        .config(
            json!({ "type": "object", "properties": { "texts": { "type": "object" } } }),
            json!({}),
        )
        .build();
    assert!(declared.is_err());
}
