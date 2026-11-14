//! The harness hands its module's context to a test, makes store writes fail on purpose, and
//! gives the bot a role with the permissions a test needs.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use closure_core::action::{Action, Outcome};
use closure_core::storage::StorageError;
use closure_sdk::testing::Harness;
use closure_sdk::{Capability, EventContext, ModuleBuilder};
use twilight_model::guild::Permissions;
use twilight_model::id::Id;

fn module() -> ModuleBuilder {
    ModuleBuilder::new("notes", "Notes", "0.1.0")
        .unwrap()
        .capability(Capability::Storage)
        .capability(Capability::ReadGuild)
}

/// A handler's work: it gets the context and writes through its store.
async fn save(ctx: &EventContext, key: &str, text: &str) -> Result<(), StorageError> {
    ctx.store().set(key, &text.to_owned()).await
}

#[tokio::test]
async fn the_context_of_a_harness_is_the_one_its_handlers_get() {
    let harness = Harness::new(module()).unwrap();
    let ctx = harness.context();
    assert_eq!(ctx.guild_id, Id::new(1));
    save(&ctx, "note", "hello").await.unwrap();
    let stored: Option<String> = harness.store().get("note").await.unwrap();
    assert_eq!(stored.as_deref(), Some("hello"));
}

#[tokio::test]
async fn a_context_can_be_taken_for_another_guild() {
    let harness = Harness::new(module()).unwrap();
    let ctx = harness.context_in(Id::new(2));
    assert_eq!(ctx.guild_id, Id::new(2));
    save(&ctx, "note", "elsewhere").await.unwrap();
    let here: Option<String> = harness.store().get("note").await.unwrap();
    assert_eq!(here, None);
}

#[tokio::test]
async fn the_next_store_writes_can_be_made_to_fail() {
    let harness = Harness::new(module()).unwrap();
    let ctx = harness.context();
    harness.fail_next_store_writes(1, StorageError::Backend("disk full".to_owned()));
    assert_eq!(
        save(&ctx, "note", "lost").await,
        Err(StorageError::Backend("disk full".to_owned()))
    );
    let stored: Option<String> = harness.store().get("note").await.unwrap();
    assert_eq!(stored, None);
    save(&ctx, "note", "kept").await.unwrap();
}

#[tokio::test]
async fn writes_under_a_prefix_fail_until_they_are_recovered() {
    let harness = Harness::new(module()).unwrap();
    let ctx = harness.context();
    harness.fail_store_writes_for("draft:", StorageError::Backend("down".to_owned()));
    assert!(save(&ctx, "draft:1", "text").await.is_err());
    save(&ctx, "note", "fine").await.unwrap();
    harness.recover_store_writes();
    save(&ctx, "draft:1", "text").await.unwrap();
}

#[tokio::test]
async fn writes_after_some_under_a_prefix_fail_until_they_are_recovered() {
    let harness = Harness::new(module()).unwrap();
    let ctx = harness.context();
    harness.fail_store_writes_after("draft:", 1, StorageError::Backend("down".to_owned()));
    save(&ctx, "draft:1", "first").await.unwrap();
    assert!(save(&ctx, "draft:2", "second").await.is_err());
    save(&ctx, "note", "fine").await.unwrap();
    harness.recover_store_writes();
    save(&ctx, "draft:2", "second").await.unwrap();
}

#[tokio::test]
async fn the_bot_can_be_made_a_member_holding_the_permissions_it_needs() {
    let harness = Harness::new(module()).unwrap();
    harness.set_bot_permissions(Permissions::ADMINISTRATOR);
    let ctx = harness.context();
    let bot = ctx.bot().expect("the bot has an id");
    let Outcome::Member(member) = ctx
        .api
        .execute(Action::GetMember {
            guild_id: ctx.guild_id,
            user_id: bot,
        })
        .await
        .unwrap()
    else {
        panic!("the bot is a member of the guild");
    };
    let roles = harness.api().roles(ctx.guild_id);
    assert!(member.roles.iter().any(|held| {
        roles
            .iter()
            .any(|role| role.id == *held && role.permissions.contains(Permissions::ADMINISTRATOR))
    }));
}
