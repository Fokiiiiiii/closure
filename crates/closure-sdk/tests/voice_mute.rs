//! Server-muting a member in voice channels, written with the SDK only.
// Test fixtures: building them must panic on failure, as in the other SDK tests.
#![allow(clippy::unwrap_used)]

use closure_core::action::Action;
use closure_core::event::CommandOptionValue;
use closure_sdk::testing::{Harness, Invocation, Reply};
use closure_sdk::{
    BoundCommand, Capability, Command, CommandArgs, DiscordError, ModuleBuilder, Reason, UserId,
};
use twilight_model::id::Id;

#[derive(CommandArgs)]
struct Mute {
    /// Member to mute
    user: UserId,
}

fn mute_command() -> BoundCommand {
    Command::<Mute>::new("mute", "Mute a member in voice").handler(|cx, args| async move {
        let guild = cx.guild_id();
        let reason = Reason::new("noisy").unwrap();
        let result = cx
            .api()
            .set_voice_mute(guild, args.user, true, reason)
            .await;
        let text = match result {
            Ok(()) => "muted",
            Err(DiscordError::CapabilityDenied(_)) => "denied",
            Err(_) => "failed",
        };
        cx.reply(text).await
    })
}

fn module() -> ModuleBuilder {
    ModuleBuilder::new("voice", "Voice", "0.1.0")
        .unwrap()
        .capability(Capability::MuteMembers)
        .command(mute_command())
}

/// Tries to mute a member but was never granted the capability.
fn module_without_the_capability() -> ModuleBuilder {
    ModuleBuilder::new("quiet", "Quiet", "0.1.0")
        .unwrap()
        .command(mute_command())
}

fn mute(command: &str) -> Invocation {
    Invocation::new(command).option("user", CommandOptionValue::User(Id::new(42)))
}

fn texts(replies: &[Reply]) -> Vec<&str> {
    replies.iter().map(|reply| reply.content.as_str()).collect()
}

fn voice_mutes(harness: &Harness) -> Vec<Action> {
    harness
        .api()
        .actions()
        .into_iter()
        .filter(|action| matches!(action, Action::SetVoiceMute { .. }))
        .collect()
}

#[tokio::test]
async fn muting_a_member_in_voice_is_recorded_with_the_capability() {
    let harness = Harness::new(module()).unwrap();
    let replies = harness.invoke(mute("mute")).await.unwrap();

    assert_eq!(texts(&replies), ["muted"]);
    let mutes = voice_mutes(&harness);
    let [
        Action::SetVoiceMute {
            user_id,
            mute: true,
            ..
        },
    ] = mutes.as_slice()
    else {
        panic!("one voice mute expected, got {mutes:?}");
    };
    assert_eq!(*user_id, Id::new(42));
}

#[tokio::test]
async fn muting_without_the_capability_is_refused_before_discord() {
    let harness = Harness::new(module_without_the_capability()).unwrap();
    let replies = harness.invoke(mute("mute")).await.unwrap();

    assert_eq!(texts(&replies), ["denied"]);
    assert!(voice_mutes(&harness).is_empty());
}
