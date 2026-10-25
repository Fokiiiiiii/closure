//! A module written with the SDK only: what it does for the author.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::time::Duration;

use closure_core::event::CommandOptionValue;
use closure_sdk::testing::{Harness, Invocation, Reply, ReplyKind};
use closure_sdk::{
    Args, Command, CommandArgs, Cooldown, Level, Locale, ModuleBuilder, PermissionKey,
    PermissionSpec, UserError, UserId,
};
use twilight_model::id::Id;

const EN: &str = "
warned = Warned user { $user } ({ $count } so far).
too-many = That is too many warnings.
";
const JA: &str = "
warned = ユーザー { $user } に警告しました (これまで { $count } 回)。
too-many = 警告が多すぎます。
";

#[derive(CommandArgs)]
struct Warn {
    /// Member to warn
    user: UserId,
    /// Why
    #[arg(max_length = 512)]
    #[allow(dead_code)]
    reason: Option<String>,
    /// How serious
    #[arg(min = 1, max = 5)]
    level: Option<i64>,
}

#[derive(CommandArgs)]
struct Kind {
    /// Which list
    #[arg(choices = ["spam", "abuse"], name = "list")]
    #[allow(dead_code)]
    which: String,
}

fn module() -> ModuleBuilder {
    ModuleBuilder::new("warn", "Warnings", "0.1.0")
        .unwrap()
        .permission(PermissionSpec::new("warn.add", Level::MODERATOR, "Warn members").unwrap())
        .command(
            Command::<Warn>::new("warn", "Warn a member")
                .requires("warn.add")
                .cooldown(Cooldown::per_user(Duration::from_secs(10)))
                .handler(|cx, args| async move {
                    if args.level == Some(5) {
                        return Err(UserError::message("too-many"));
                    }
                    cx.reply_t(
                        "warned",
                        &Args::new()
                            .with("user", args.user.get())
                            .with("count", 1_i64),
                    )
                    .await
                }),
        )
        .group(
            "warnings",
            "Look at warnings",
            [
                Command::<Kind>::new("show", "Show a list")
                    .handler(|_cx, _args| async move { Ok(()) }),
                Command::<()>::new("clear", "Clear everything")
                    .requires("warn.add")
                    .handler(|cx, ()| async move {
                        cx.defer(false).await?;
                        cx.reply("cleared").await?;
                        cx.reply("and again").await
                    }),
            ],
        )
        .catalog(Locale::En, EN)
        .catalog(Locale::Ja, JA)
}

fn warn(user: u64) -> Invocation {
    Invocation::new("warn").option("user", CommandOptionValue::User(Id::new(user)))
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
async fn the_options_are_published_from_the_struct() {
    let harness = Harness::new(module()).unwrap();
    drop(harness);
    let options = Warn::options();
    let names: Vec<_> = options
        .iter()
        .map(|o| (o.name.as_str(), o.required))
        .collect();
    assert_eq!(names, [("user", true), ("reason", false), ("level", false)]);
    assert_eq!(options[0].description, "Member to warn");
    assert_eq!(options[1].max_length, Some(512));
    assert_eq!(
        (options[2].min_value, options[2].max_value),
        (Some(1.0), Some(5.0))
    );
    let choices = &Kind::options()[0];
    assert_eq!(choices.name, "list");
    assert_eq!(choices.choices.len(), 2);
}

#[tokio::test]
async fn a_moderator_runs_the_command_and_is_answered_in_their_language() {
    let harness = moderator_harness();
    let replies = harness.invoke(warn(7).roles([10])).await.unwrap();
    assert_eq!(texts(&replies), ["Warned user 7 (1 so far)."]);
    assert_eq!(replies[0].kind, ReplyKind::Response);
    assert!(!replies[0].ephemeral);

    harness.advance(Duration::from_secs(11));
    let replies = harness
        .invoke(warn(7).roles([10]).locale(Locale::Ja))
        .await
        .unwrap();
    assert_eq!(
        texts(&replies),
        ["ユーザー 7 に警告しました (これまで 1 回)。"]
    );
}

#[tokio::test]
async fn someone_without_the_permission_is_told_why_and_the_handler_does_not_run() {
    let harness = moderator_harness();
    let replies = harness.invoke(warn(7)).await.unwrap();
    assert_eq!(
        texts(&replies),
        ["You are not allowed to do that here. It needs level 50 and you are at level 1."]
    );
    assert!(replies[0].ephemeral);
    let ja = harness.invoke(warn(7).locale(Locale::Ja)).await.unwrap();
    assert!(ja[0].content.contains("レベル 50"), "{}", ja[0].content);

    // Discord's Administrator is level 75, the owner of the server is never refused.
    assert_eq!(
        texts(&harness.invoke(warn(7).administrator()).await.unwrap()),
        ["Warned user 7 (1 so far)."]
    );
    harness.advance(Duration::from_secs(11));
    assert_eq!(
        texts(&harness.invoke(warn(7).owner()).await.unwrap()),
        ["Warned user 7 (1 so far)."]
    );

    // A server can take the permission away from one role.
    harness.permissions(|permissions| {
        permissions
            .keys
            .entry(PermissionKey::new("warn.add").unwrap())
            .or_default()
            .deny_roles
            .push(Id::new(10));
    });
    harness.advance(Duration::from_secs(11));
    let replies = harness.invoke(warn(7).roles([10])).await.unwrap();
    assert_eq!(texts(&replies), ["You are not allowed to do that here."]);
}

#[tokio::test]
async fn the_cooldown_holds_a_user_back_and_says_for_how_long() {
    let harness = moderator_harness();
    harness.invoke(warn(7).roles([10])).await.unwrap();
    harness.advance(Duration::from_secs(3));
    let replies = harness.invoke(warn(7).roles([10])).await.unwrap();
    assert_eq!(
        texts(&replies),
        ["Please wait 8 more second(s) before using that again."]
    );
    assert!(replies[0].ephemeral);
    // Someone else is not held back, and after the period the user is free again.
    let other = harness.invoke(warn(7).roles([10]).user(101)).await.unwrap();
    assert_eq!(texts(&other), ["Warned user 7 (1 so far)."]);
    harness.advance(Duration::from_secs(8));
    assert_eq!(
        texts(&harness.invoke(warn(7).roles([10])).await.unwrap()),
        ["Warned user 7 (1 so far)."]
    );
}

#[tokio::test]
async fn a_message_error_is_answered_privately_from_the_modules_catalog() {
    let harness = moderator_harness();
    let replies = harness
        .invoke(
            warn(7)
                .roles([10])
                .option("level", CommandOptionValue::Integer(5)),
        )
        .await
        .unwrap();
    assert_eq!(texts(&replies), ["That is too many warnings."]);
    assert!(replies[0].ephemeral);
}

#[tokio::test]
async fn a_handler_that_says_nothing_is_answered_and_options_of_the_wrong_type_are_refused() {
    let harness = moderator_harness();
    let replies = harness
        .invoke(
            Invocation::new("warnings")
                .subcommand("show")
                .option("list", CommandOptionValue::String("spam".into())),
        )
        .await
        .unwrap();
    assert_eq!(texts(&replies), ["Done."]);

    let replies = harness
        .invoke(
            Invocation::new("warnings")
                .subcommand("show")
                .option("list", CommandOptionValue::Integer(1)),
        )
        .await
        .unwrap();
    assert_eq!(
        texts(&replies),
        ["The value of `list` is missing or not valid."]
    );
}

#[tokio::test]
async fn a_deferred_command_edits_its_first_answer_and_follows_up_after() {
    let harness = moderator_harness();
    let replies = harness
        .invoke(Invocation::new("warnings").subcommand("clear").roles([10]))
        .await
        .unwrap();
    let kinds: Vec<_> = replies.iter().map(|r| r.kind).collect();
    assert_eq!(
        kinds,
        [ReplyKind::Deferred, ReplyKind::Edit, ReplyKind::Followup]
    );
    assert_eq!(texts(&replies), ["", "cleared", "and again"]);
}

#[tokio::test]
async fn a_failure_of_discord_reaches_the_dispatcher_instead_of_being_swallowed() {
    let harness = moderator_harness();
    harness
        .api()
        .fail_with(closure_sdk::DiscordError::Forbidden("no".into()));
    let error = harness.invoke(warn(7).roles([10])).await.unwrap_err();
    assert!(
        matches!(error, closure_sdk::ModuleError::Discord(_)),
        "{error:?}"
    );
}

#[test]
fn a_declaration_that_cannot_work_is_refused_when_the_module_is_built() {
    // A permission nobody declared.
    let undeclared = ModuleBuilder::new("warn", "w", "0.1.0").unwrap().command(
        Command::<()>::new("a", "d")
            .requires("warn.nope")
            .handler(|_cx, ()| async { Ok(()) }),
    );
    assert!(undeclared.build().is_err());
    // A key outside the module's own namespace.
    let outside = ModuleBuilder::new("warn", "w", "0.1.0").unwrap().command(
        Command::<()>::new("a", "d")
            .requires("Not A Key")
            .handler(|_cx, ()| async { Ok(()) }),
    );
    assert!(outside.build().is_err());
    // The same command twice.
    let twice = ModuleBuilder::new("warn", "w", "0.1.0")
        .unwrap()
        .command(Command::<()>::new("a", "d").handler(|_cx, ()| async { Ok(()) }))
        .command(Command::<()>::new("a", "d").handler(|_cx, ()| async { Ok(()) }));
    assert!(twice.build().is_err());
}
