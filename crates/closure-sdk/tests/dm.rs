//! Direct messages to members of the server, written with the SDK only.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::too_many_lines)]

use closure_core::action::Action;
use closure_core::event::CommandOptionValue;
use closure_sdk::testing::{Harness, Invocation, Reply};
use closure_sdk::{
    BoundCommand, Capability, Command, CommandArgs, DirectMessage, DiscordError, EmbedBuilder,
    ModuleBuilder, Sent, UserError, UserId,
};
use twilight_model::id::Id;

#[derive(CommandArgs)]
struct Notify {
    /// Member to message
    user: UserId,
}

/// What the module answered: how the message went.
fn describe(result: &Result<Sent, UserError>) -> &'static str {
    match result {
        Ok(Sent::Delivered(_)) => "delivered",
        Ok(Sent::Closed) => "closed",
        Err(_) => "failed",
    }
}

fn text_command() -> BoundCommand {
    Command::<Notify>::new("notify", "Message a member").handler(|cx, args| async move {
        let sent = cx
            .direct_message(args.user, DirectMessage::text("hi"))
            .await;
        cx.reply(describe(&sent)).await
    })
}

fn embed_command() -> BoundCommand {
    Command::<Notify>::new("notice", "Message a member an embed").handler(|cx, args| async move {
        let embed = EmbedBuilder::new().title("Notice").build().unwrap();
        let sent = cx
            .direct_message(args.user, DirectMessage::embed(embed))
            .await;
        cx.reply(describe(&sent)).await
    })
}

fn module() -> ModuleBuilder {
    ModuleBuilder::new("dm", "Direct messages", "0.1.0")
        .unwrap()
        .capability(Capability::SendDirectMessages)
        .command(text_command())
        .command(embed_command())
}

/// Tries to message a member but was never granted the capability.
fn module_without_the_capability() -> ModuleBuilder {
    ModuleBuilder::new("quiet", "Quiet", "0.1.0")
        .unwrap()
        .command(text_command())
}

fn notify(command: &str) -> Invocation {
    Invocation::new(command).option("user", CommandOptionValue::User(Id::new(7)))
}

fn texts(replies: &[Reply]) -> Vec<&str> {
    replies.iter().map(|reply| reply.content.as_str()).collect()
}

fn sent_direct_messages(harness: &Harness) -> Vec<Action> {
    harness
        .api()
        .actions()
        .into_iter()
        .filter(|action| matches!(action, Action::SendDirectMessage { .. }))
        .collect()
}

#[tokio::test]
async fn a_member_gets_the_message_in_the_guild_of_the_command() {
    let harness = Harness::new(module()).unwrap();
    let replies = harness.invoke(notify("notify")).await.unwrap();

    assert_eq!(texts(&replies), ["delivered"]);
    assert_eq!(
        sent_direct_messages(&harness),
        [Action::SendDirectMessage {
            guild_id: Id::new(1),
            user_id: Id::new(7),
            content: Some("hi".into()),
            embeds: vec![],
        }]
    );
}

#[tokio::test]
async fn an_embed_is_sent_without_text() {
    let harness = Harness::new(module()).unwrap();
    let replies = harness.invoke(notify("notice")).await.unwrap();

    assert_eq!(texts(&replies), ["delivered"]);
    let actions = sent_direct_messages(&harness);
    let [
        Action::SendDirectMessage {
            content, embeds, ..
        },
    ] = actions.as_slice()
    else {
        panic!("one direct message expected, got {actions:?}");
    };
    assert_eq!(content, &None);
    assert_eq!(embeds.len(), 1);
    assert_eq!(embeds[0].title.as_deref(), Some("Notice"));
}

#[tokio::test]
async fn a_user_who_closed_direct_messages_is_not_an_error() {
    let harness = Harness::new(module()).unwrap();
    harness
        .api()
        .fail_nth(1, DiscordError::DirectMessagesClosed);
    let replies = harness.invoke(notify("notify")).await.unwrap();

    assert_eq!(texts(&replies), ["closed"]);
    assert!(sent_direct_messages(&harness).is_empty());
}

#[tokio::test]
async fn other_failures_are_reported_as_failures() {
    let harness = Harness::new(module()).unwrap();
    harness
        .api()
        .fail_nth(1, DiscordError::Forbidden("no".into()));
    let replies = harness.invoke(notify("notify")).await.unwrap();

    assert_eq!(texts(&replies), ["failed"]);
}

#[tokio::test]
async fn a_module_without_the_capability_sends_nothing() {
    let harness = Harness::new(module_without_the_capability()).unwrap();
    let replies = harness.invoke(notify("notify")).await.unwrap();

    assert_eq!(texts(&replies), ["failed"]);
    assert!(sent_direct_messages(&harness).is_empty());
}
