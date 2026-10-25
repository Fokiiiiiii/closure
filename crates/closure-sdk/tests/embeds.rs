//! Embeds in replies, written with the SDK only.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use closure_core::action::InteractionReply;
use closure_sdk::testing::{Harness, Invocation};
use closure_sdk::{Color, Command, EmbedBuilder, ModuleBuilder, ModuleError};

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
    assert_eq!(embed.color, Some(0x00FE_E75C));
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
