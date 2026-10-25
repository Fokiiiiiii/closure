//! Replies as embeds, the tones they take, and the `replies` setting every SDK module has.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::too_many_lines)]

use closure_core::module::Module;
use closure_sdk::testing::{Harness, Invocation, ReplyKind};
use closure_sdk::{Command, ModuleBuilder, Tone, UserError};
use serde::Deserialize;
use serde_json::json;

const INFO: u32 = 0x0058_65F2;
const SUCCESS: u32 = 0x0057_F287;
const WARNING: u32 = 0x00FE_E75C;
const ERROR: u32 = 0x00ED_4245;

const EN: &str = "
secret = Only for you.
";

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Settings {
    limit: u32,
}

fn module() -> ModuleBuilder {
    ModuleBuilder::new("embed-replies", "Embed replies", "0.1.0")
        .unwrap()
        .config(
            json!({
                "type": "object",
                "properties": { "limit": { "type": "integer" } },
                "additionalProperties": false
            }),
            json!({ "limit": 1 }),
        )
        .command(
            Command::<()>::new("plain", "A plain reply")
                .handler(|cx, ()| async move { cx.reply("Hello.").await }),
        )
        .command(
            Command::<()>::new("private", "A private reply")
                .handler(|cx, ()| async move { cx.reply_private("Hello.").await }),
        )
        .command(
            Command::<()>::new("tones", "One reply per tone").handler(|cx, ()| async move {
                cx.say(Tone::Success, "one").await?;
                cx.say(Tone::Warning, "two").await?;
                cx.say(Tone::Error, "three").await
            }),
        )
        .command(
            Command::<()>::new("empty", "An empty reply is refused").handler(|cx, ()| async move {
                if cx.reply("").await.is_err() {
                    cx.reply("refused").await?;
                }
                Ok(())
            }),
        )
        .command(
            Command::<()>::new("fits", "The longest description Discord takes")
                .handler(|cx, ()| async move { cx.reply("x".repeat(4096)).await }),
        )
        .command(
            Command::<()>::new("defer-false", "Defers privately or publicly").handler(
                |cx, ()| async move {
                    cx.defer(false).await?;
                    cx.reply("done.").await
                },
            ),
        )
        .command(
            Command::<()>::new("defer-true", "Defers publicly or privately").handler(
                |cx, ()| async move {
                    cx.defer(true).await?;
                    cx.reply("done.").await
                },
            ),
        )
        .command(
            Command::<()>::new("fail", "Fails with a message for the user")
                .handler(|_cx, ()| async move { Err(UserError::message("secret")) }),
        )
        .command(
            Command::<()>::new("limit", "Reads its own configuration").handler(
                |cx, ()| async move {
                    let settings: Settings = cx.config()?;
                    cx.reply(settings.limit.to_string()).await
                },
            ),
        )
        .catalog(closure_sdk::Locale::En, EN)
}

fn harness_with(config: serde_json::Value) -> Harness {
    let harness = Harness::new(module()).unwrap();
    harness.set_config(config);
    harness
}

#[tokio::test]
async fn a_reply_is_one_info_embed_with_the_text_as_its_description() {
    let harness = Harness::new(module()).unwrap();
    let replies = harness.invoke(Invocation::new("plain")).await.unwrap();
    assert_eq!(replies.len(), 1, "{replies:?}");
    assert_eq!(replies[0].kind, ReplyKind::Response);
    assert_eq!(replies[0].embeds.len(), 1, "{replies:?}");
    let embed = &replies[0].embeds[0];
    assert_eq!(embed.description.as_deref(), Some("Hello."));
    assert_eq!(embed.color, Some(INFO));
    assert!(embed.title.is_none() && embed.footer.is_none(), "{embed:?}");
    assert!(!replies[0].ephemeral);
}

#[tokio::test]
async fn a_private_reply_is_ephemeral_and_still_one_info_embed() {
    let harness = Harness::new(module()).unwrap();
    let replies = harness.invoke(Invocation::new("private")).await.unwrap();
    assert_eq!(replies.len(), 1, "{replies:?}");
    assert_eq!(replies[0].embeds.len(), 1, "{replies:?}");
    assert_eq!(replies[0].embeds[0].description.as_deref(), Some("Hello."));
    assert_eq!(replies[0].embeds[0].color, Some(INFO));
    assert!(replies[0].ephemeral);
}

#[tokio::test]
async fn each_tone_takes_its_own_colour() {
    let harness = Harness::new(module()).unwrap();
    let replies = harness.invoke(Invocation::new("tones")).await.unwrap();
    let colours: Vec<Option<u32>> = replies.iter().map(|reply| reply.embeds[0].color).collect();
    assert_eq!(colours, [Some(SUCCESS), Some(WARNING), Some(ERROR)]);
    assert_eq!(replies[0].content, "one");
    assert_eq!(replies[2].content, "three");
}

#[tokio::test]
async fn an_empty_reply_is_refused_and_nothing_is_sent() {
    let harness = Harness::new(module()).unwrap();
    let replies = harness.invoke(Invocation::new("empty")).await.unwrap();
    assert_eq!(replies.len(), 1, "{replies:?}");
    assert_eq!(replies[0].kind, ReplyKind::Response);
    assert_eq!(replies[0].content, "refused");
}

#[tokio::test]
async fn a_description_of_4096_characters_is_sent() {
    let harness = Harness::new(module()).unwrap();
    let replies = harness.invoke(Invocation::new("fits")).await.unwrap();
    assert_eq!(replies.len(), 1, "{replies:?}");
    assert_eq!(
        replies[0].embeds[0].description.as_deref().map(str::len),
        Some(4096)
    );
}

#[tokio::test]
async fn replies_private_makes_a_plain_reply_ephemeral() {
    let harness = harness_with(json!({ "replies": "private" }));
    let replies = harness.invoke(Invocation::new("plain")).await.unwrap();
    assert!(replies[0].ephemeral, "{replies:?}");
    assert_eq!(replies[0].embeds[0].color, Some(INFO));
}

#[tokio::test]
async fn replies_public_makes_a_private_reply_public() {
    let harness = harness_with(json!({ "replies": "public" }));
    let replies = harness.invoke(Invocation::new("private")).await.unwrap();
    assert!(!replies[0].ephemeral, "{replies:?}");
}

#[tokio::test]
async fn without_the_setting_each_reply_keeps_its_own_visibility() {
    let harness = harness_with(json!({}));
    let plain = harness.invoke(Invocation::new("plain")).await.unwrap();
    let private = harness.invoke(Invocation::new("private")).await.unwrap();
    assert!(!plain[0].ephemeral);
    assert!(private[0].ephemeral);
}

#[tokio::test]
async fn the_setting_decides_the_visibility_of_a_deferral() {
    let private = harness_with(json!({ "replies": "private" }));
    let replies = private
        .invoke(Invocation::new("defer-false"))
        .await
        .unwrap();
    assert_eq!(replies[0].kind, ReplyKind::Deferred);
    assert!(replies[0].ephemeral, "{replies:?}");

    let public = harness_with(json!({ "replies": "public" }));
    let replies = public.invoke(Invocation::new("defer-true")).await.unwrap();
    assert_eq!(replies[0].kind, ReplyKind::Deferred);
    assert!(!replies[0].ephemeral, "{replies:?}");
}

#[tokio::test]
async fn an_error_is_an_error_embed_and_private_even_when_replies_are_public() {
    let harness = harness_with(json!({ "replies": "public" }));
    let replies = harness.invoke(Invocation::new("fail")).await.unwrap();
    assert_eq!(replies.len(), 1, "{replies:?}");
    assert!(replies[0].ephemeral, "{replies:?}");
    assert_eq!(replies[0].embeds[0].color, Some(ERROR));
    assert_eq!(
        replies[0].embeds[0].description.as_deref(),
        Some("Only for you.")
    );
}

#[tokio::test]
async fn a_config_that_denies_unknown_fields_reads_despite_the_replies_key() {
    let harness = harness_with(json!({ "limit": 3, "replies": "public" }));
    let replies = harness.invoke(Invocation::new("limit")).await.unwrap();
    assert_eq!(replies[0].content, "3", "{replies:?}");
    assert!(!replies[0].ephemeral);
}

#[test]
fn a_module_that_declares_replies_itself_does_not_build() {
    let declared = ModuleBuilder::new("declares-replies", "Declares", "0.1.0")
        .unwrap()
        .config(
            json!({ "type": "object", "properties": { "replies": { "type": "boolean" } } }),
            json!({}),
        )
        .build();
    assert!(declared.is_err());
}

#[test]
fn the_manifest_schema_has_the_replies_setting_without_a_declared_schema() {
    let module = ModuleBuilder::new("bare", "Bare", "0.1.0")
        .unwrap()
        .build()
        .unwrap();
    let schema = &module.manifest().config_schema;
    assert_eq!(
        schema["properties"]["replies"]["enum"],
        json!(["private", "public"]),
        "{schema}"
    );
}

#[test]
fn the_manifest_schema_keeps_its_properties_and_adds_the_replies_setting() {
    let module = ModuleBuilder::new("with-props", "With properties", "0.1.0")
        .unwrap()
        .config(
            json!({ "type": "object", "properties": { "limit": { "type": "integer" } } }),
            json!({}),
        )
        .build()
        .unwrap();
    let properties = &module.manifest().config_schema["properties"];
    assert_eq!(properties["limit"], json!({ "type": "integer" }));
    assert_eq!(
        properties["replies"]["enum"],
        json!(["private", "public"]),
        "{properties}"
    );
}
