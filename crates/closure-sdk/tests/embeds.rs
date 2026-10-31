//! Embeds in replies, written with the SDK only.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use closure_core::action::{Action, InteractionReply};
use closure_sdk::files::{self, LogEntry};
use closure_sdk::testing::{Harness, Invocation};
use closure_sdk::{Capability, Color, Command, EmbedBuilder, ModuleBuilder, ModuleError};
use twilight_model::id::Id;

fn module() -> ModuleBuilder {
    ModuleBuilder::new("cards", "Cards", "0.1.0")
        .unwrap()
        .command(
            Command::<()>::new("card", "Show a card").handler(|cx, ()| async move {
                let embed = EmbedBuilder::new()
                    .title("t")
                    .field("a", "b", true)
                    .color(Color::Warning)
                    .build()?;
                cx.reply_with(InteractionReply {
                    embeds: vec![embed],
                    ..InteractionReply::default()
                })
                .await
            }),
        )
        .command(
            Command::<()>::new("broken", "Show a card with a title that is too long").handler(
                |cx, ()| async move {
                    let embed = EmbedBuilder::new().title("a".repeat(257)).build()?;
                    cx.reply_with(InteractionReply {
                        embeds: vec![embed],
                        ..InteractionReply::default()
                    })
                    .await
                },
            ),
        )
}

#[tokio::test]
async fn a_reply_carries_the_embed() {
    let harness = Harness::new(module()).unwrap();

    let replies = harness.invoke(Invocation::new("card")).await.unwrap();

    assert_eq!(replies.len(), 1);
    assert_eq!(replies[0].embeds.len(), 1);
    let embed = &replies[0].embeds[0];
    assert_eq!(embed.title.as_deref(), Some("t"));
    assert_eq!(embed.fields.len(), 1);
    assert_eq!(embed.color, Some(0x00FA_A61A));
}

#[tokio::test]
async fn an_embed_over_a_limit_is_a_module_failure() {
    let harness = Harness::new(module()).unwrap();

    let error = harness.invoke(Invocation::new("broken")).await.unwrap_err();

    assert!(
        matches!(&error, ModuleError::Other(message) if message.contains("the title is 257 characters")),
        "{error:?}"
    );
}

/// A module whose command posts six entries of 1500 characters each, as a bulk deletion log does.
fn posting_module() -> ModuleBuilder {
    ModuleBuilder::new("posts", "Posts", "0.1.0")
        .unwrap()
        .capability(Capability::SendMessages)
        .command(
            Command::<()>::new("post", "Post six long entries").handler(|cx, ()| async move {
                let entries = (0..6)
                    .map(|_| LogEntry {
                        embed: EmbedBuilder::new()
                            .description("x".repeat(1500))
                            .build()
                            .unwrap(),
                        files: Vec::new(),
                    })
                    .collect();
                files::post(cx.ctx(), Id::new(77), entries).await?;
                cx.reply("posted").await
            }),
        )
}

#[tokio::test]
async fn long_log_entries_are_posted_so_that_no_message_passes_the_character_limit() {
    let harness = Harness::new(posting_module()).unwrap();
    harness.set_log_channel(77);
    harness.invoke(Invocation::new("post")).await.unwrap();

    // Discord refuses a message whose embeds add up to more than 6000 characters.
    let message_chars: Vec<usize> = harness
        .api()
        .actions()
        .iter()
        .filter_map(|action| match action {
            Action::SendMessage { embeds, .. } => Some(
                embeds
                    .iter()
                    .map(|embed| {
                        embed
                            .description
                            .as_deref()
                            .map_or(0, |text| text.chars().count())
                    })
                    .sum(),
            ),
            _ => None,
        })
        .collect();

    assert_eq!(
        harness.sent_embeds().len(),
        6,
        "every entry arrives: {message_chars:?}"
    );
    assert!(
        message_chars.iter().all(|&chars| chars <= 6000),
        "a message passes the 6000 character limit: {message_chars:?}"
    );
}
