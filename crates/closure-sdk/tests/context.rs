//! Context-menu commands: used on a user or on a message, with the permission and cooldown of a
//! command.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::time::Duration;

use closure_sdk::testing::{ContextUse, Harness, Reply};
use closure_sdk::{ContextCommand, Cooldown, Level, ModuleBuilder, PermissionSpec};
use twilight_model::id::Id;

fn module() -> ModuleBuilder {
    ModuleBuilder::new("review", "Review", "0.1.0")
        .unwrap()
        .permission(PermissionSpec::new("review.use", Level::MODERATOR, "Review").unwrap())
        .context(ContextCommand::user("profile").handler(|cx| async move {
            let who = cx.target_user().map_or(0, |user| user.id.get());
            cx.reply(format!("user {who}")).await
        }))
        .context(
            ContextCommand::message("report")
                .requires("review.use")
                .cooldown(Cooldown::per_user(Duration::from_secs(10)))
                .handler(|cx| async move {
                    let text = cx
                        .target_message()
                        .map(|message| message.content.clone())
                        .unwrap_or_default();
                    cx.reply(format!("reported {text}")).await
                }),
        )
}

fn texts(replies: &[Reply]) -> Vec<&str> {
    replies.iter().map(|reply| reply.content.as_str()).collect()
}

fn moderator_harness() -> Harness {
    let harness = Harness::new(module()).unwrap();
    harness.permissions(|permissions| {
        permissions
            .role_levels
            .insert(Id::new(10), Level::MODERATOR);
    });
    harness
}

#[tokio::test]
async fn the_user_a_context_command_is_used_on_is_its_target() {
    let harness = moderator_harness();
    let replies = harness
        .context_command(ContextUse::on_user("profile", 42))
        .await
        .unwrap();
    assert_eq!(texts(&replies), ["user 42"]);
}

#[tokio::test]
async fn the_message_a_context_command_is_used_on_is_its_target() {
    let harness = moderator_harness();
    let replies = harness
        .context_command(ContextUse::on_message("report", 80, 7, "spam link").roles([10]))
        .await
        .unwrap();
    assert_eq!(texts(&replies), ["reported spam link"]);
}

#[tokio::test]
async fn the_permission_and_the_cooldown_of_a_command_apply_to_a_context_command() {
    let harness = moderator_harness();
    let report = || ContextUse::on_message("report", 80, 7, "spam link");
    let refused = harness.context_command(report()).await.unwrap();
    assert_ne!(
        texts(&refused),
        ["reported spam link"],
        "without the permission"
    );
    let allowed = harness.context_command(report().roles([10])).await.unwrap();
    assert_eq!(texts(&allowed), ["reported spam link"]);
    let held_back = harness.context_command(report().roles([10])).await.unwrap();
    assert_ne!(
        texts(&held_back),
        ["reported spam link"],
        "during the cooldown"
    );
    harness.advance(Duration::from_secs(11));
    let again = harness.context_command(report().roles([10])).await.unwrap();
    assert_eq!(texts(&again), ["reported spam link"]);
}

#[tokio::test]
async fn a_context_command_of_the_other_kind_or_of_no_name_is_not_answered() {
    let harness = moderator_harness();
    let on_message = harness
        .context_command(ContextUse::on_message("profile", 80, 7, "x"))
        .await
        .unwrap();
    assert!(on_message.is_empty(), "a user command used on a message");
    let unknown = harness
        .context_command(ContextUse::on_user("nothing", 42))
        .await
        .unwrap();
    assert!(unknown.is_empty());
}

#[test]
fn at_most_five_user_and_five_message_commands_are_built() {
    let mut builder = ModuleBuilder::new("review", "Review", "0.1.0").unwrap();
    for n in 0..6 {
        builder = builder.user_command(
            &format!("user {n}"),
            |cx| async move { cx.reply("x").await },
        );
    }
    assert!(builder.build().is_err(), "six user commands");
}
