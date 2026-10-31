//! Message history, written with the SDK only.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::too_many_lines)]

use closure_sdk::testing::{Harness, Invocation, Reply};
use closure_sdk::{Capability, Command, ModuleBuilder, SavedMessage};
use twilight_model::id::Id;

fn reader() -> ModuleBuilder {
    ModuleBuilder::new("automod", "Automod", "0.1.0")
        .unwrap()
        .capability(Capability::ReadMessageHistory)
        .command(Command::<()>::new("recent").handler(|cx, ()| async move {
            let found = cx.history().by_author(cx.user().id, 0, 10).await?;
            cx.reply(format!("{}", found.len())).await
        }))
}

/// A module that did not declare the capability `message_history`.
fn plain() -> ModuleBuilder {
    ModuleBuilder::new("plain", "Plain", "0.1.0")
        .unwrap()
        .command(Command::<()>::new("peek").handler(|cx, ()| async move {
            cx.history().by_author(cx.user().id, 0, 10).await?;
            cx.reply("Read.").await
        }))
}

fn message(id: u64, author: u64, content: &str) -> SavedMessage {
    SavedMessage {
        id: Id::new(id),
        channel_id: Id::new(5),
        // The harness moves every seeded message to its own guild.
        guild_id: Id::new(1),
        author: Id::new(author),
        author_bot: false,
        content: content.into(),
        attachments: Vec::new(),
        embed_count: 0,
        sticker_count: 0,
        mentioned_users: Vec::new(),
        mentioned_roles: Vec::new(),
        mentions_everyone: false,
        reply_to: None,
        created_at: 1_700_000_000,
        edited_at: None,
        deleted_at: None,
    }
}

fn texts(replies: &[Reply]) -> Vec<&str> {
    replies.iter().map(|reply| reply.content.as_str()).collect()
}

#[tokio::test]
async fn a_module_reads_the_messages_kept_for_its_guild() {
    let harness = Harness::new(reader()).unwrap();
    harness.seed_message(message(10, 7, "hello")).await.unwrap();
    harness.seed_message(message(11, 8, "other")).await.unwrap();
    let replies = harness
        .invoke(Invocation::new("recent").user(7))
        .await
        .unwrap();
    assert_eq!(texts(&replies), ["1"]);
}

#[tokio::test]
async fn a_seeded_message_reads_back_through_the_harness() {
    let harness = Harness::new(reader()).unwrap();
    harness.seed_message(message(10, 7, "hello")).await.unwrap();
    let stored = harness
        .history()
        .get(Id::new(5), Id::new(10))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(stored.content, "hello");
    assert_eq!(stored.guild_id, Id::new(1));
}

#[tokio::test]
async fn a_module_without_the_capability_is_refused() {
    let harness = Harness::new(plain()).unwrap();
    let error = harness
        .invoke(Invocation::new("peek").user(7))
        .await
        .unwrap_err();
    assert!(error.to_string().contains("message_history"), "{error}");
}
