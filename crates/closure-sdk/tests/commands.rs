//! A module written with the SDK only: what it does for the author.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::time::Duration;

use closure_core::event::CommandOptionValue;
use closure_core::module::Module;
use closure_sdk::testing::{Harness, Invocation, Reply, ReplyKind};
use closure_sdk::{
    Args, Attachment, BoundCommand, Capability, Command, CommandArgs, Cooldown, Event, EventKind,
    Flow, Intents, Level, Locale, Mentionable, ModuleBuilder, OptionKind, PermissionKey,
    PermissionSpec, SubcommandGroup, Subscription, UserError, UserId, UserInfo,
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
        ["❌ You are not allowed to do that here. It needs level 50 and you are at level 1."]
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
    assert_eq!(texts(&replies), ["❌ You are not allowed to do that here."]);
}

#[tokio::test]
async fn the_cooldown_holds_a_user_back_and_says_for_how_long() {
    let harness = moderator_harness();
    harness.invoke(warn(7).roles([10])).await.unwrap();
    harness.advance(Duration::from_secs(3));
    let replies = harness.invoke(warn(7).roles([10])).await.unwrap();
    let shown = texts(&replies);
    assert_eq!(shown.len(), 1, "{shown:?}");
    assert!(
        shown[0].starts_with("❌ Please try again <t:") && shown[0].ends_with(":R>."),
        "{shown:?}"
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
    assert_eq!(texts(&replies), ["❌ That is too many warnings."]);
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
        ["❌ The value of `list` is missing or not valid."]
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

fn message_event(channel: u64, bot: bool, roles: &[u64]) -> Event {
    Event::MessageCreated(closure_core::event::MessageInfo {
        removed_attachments: Vec::new(),
        previous_content: None,
        id: Id::new(1),
        channel_id: Id::new(channel),
        guild_id: Id::new(1),
        author: UserInfo {
            id: Id::new(7),
            name: "u".into(),
            bot,
        },
        author_roles: roles.iter().map(|id| Id::new(*id)).collect(),
        content: "hello".into(),
        mentioned_users: vec![],
        mentions_everyone: false,
        mentioned_roles: Vec::new(),
        attachments: Vec::new(),
        embed_count: 0,
        sticker_count: 0,
        reply_to: None,
        from_webhook: false,
        created_at: 0,
        edited_at: None,
    })
}

#[tokio::test]
async fn an_event_handler_gets_what_passes_its_filter_and_says_whether_the_event_goes_on() {
    let module = ModuleBuilder::new("watch", "Watching", "0.1.0")
        .unwrap()
        .intents(Intents::GUILD_MESSAGES)
        .capability(Capability::Storage)
        .on(
            Subscription::new(EventKind::MessageCreated)
                .ignore_bots()
                .not_in_channels([Id::new(9)])
                .priority(5),
            |ctx, event| async move {
                let Event::MessageCreated(message) = event else {
                    return Ok(Flow::Continue);
                };
                let seen = ctx
                    .store()
                    .counter_add("seen", message.author.id, 1)
                    .await
                    .map_err(|error| closure_sdk::ModuleError::other(error.to_string()))?;
                // The third message of a user is dealt with here.
                Ok(if seen >= 3 {
                    Flow::Stop
                } else {
                    Flow::Continue
                })
            },
        );
    let harness = Harness::new(module).unwrap();

    assert_eq!(
        harness.emit(message_event(3, false, &[])).await.unwrap(),
        Some(Flow::Continue)
    );
    // Not delivered: a bot, and a channel the module left out.
    assert_eq!(
        harness.emit(message_event(3, true, &[])).await.unwrap(),
        None
    );
    assert_eq!(
        harness.emit(message_event(9, false, &[])).await.unwrap(),
        None
    );
    // A kind it never subscribed to.
    let left = Event::MemberLeft {
        guild_id: Id::new(1),
        user: UserInfo {
            id: Id::new(7),
            name: "u".into(),
            bot: false,
        },
    };
    assert_eq!(harness.emit(left).await.unwrap(), None);
    harness.emit(message_event(3, false, &[])).await.unwrap();
    assert_eq!(
        harness.emit(message_event(3, false, &[])).await.unwrap(),
        Some(Flow::Stop)
    );
    assert_eq!(
        harness.store().counter_total("seen", 7_u64).await.unwrap(),
        3
    );
}

#[test]
fn a_second_handler_for_a_kind_and_a_subscription_to_commands_are_refused() {
    let twice = ModuleBuilder::new("watch", "w", "0.1.0")
        .unwrap()
        .intents(Intents::GUILD_MESSAGES)
        .on(Subscription::new(EventKind::MessageCreated), |_, _| async {
            Ok(Flow::Continue)
        })
        .on(Subscription::new(EventKind::MessageCreated), |_, _| async {
            Ok(Flow::Continue)
        });
    assert!(twice.build().is_err());
    let commands = ModuleBuilder::new("watch", "w", "0.1.0")
        .unwrap()
        .on(Subscription::new(EventKind::Command), |_, _| async {
            Ok(Flow::Continue)
        });
    assert!(commands.build().is_err());
}

/// A module that keeps a count of warnings per user, as the SDK is meant to be used.
fn counting_module() -> ModuleBuilder {
    ModuleBuilder::new("count", "Counting", "0.1.0")
        .unwrap()
        .capability(Capability::Storage)
        .command(
            Command::<Warn>::new("count", "Count a warning").handler(|cx, args| async move {
                let total = cx.store().counter_add("warns", args.user, 1).await?;
                let recent = cx
                    .store()
                    .counter_recent("warns", args.user, Duration::from_secs(600))
                    .await?;
                cx.store().set("last", &args.user.get()).await?;
                cx.reply(format!("total {total}, recent {recent}")).await
            }),
        )
        .command(
            Command::<()>::new("last", "Who was counted last").handler(|cx, ()| async move {
                let last: Option<u64> = cx.store().get("last").await?;
                cx.reply(format!("{last:?}")).await
            }),
        )
}

#[tokio::test]
async fn a_handler_keeps_counters_and_entries_that_follow_the_clock_of_the_test() {
    let harness = Harness::new(counting_module()).unwrap();
    let count =
        |user| Invocation::new("count").option("user", CommandOptionValue::User(Id::new(user)));

    assert_eq!(
        texts(&harness.invoke(count(7)).await.unwrap()),
        ["total 1, recent 1"]
    );
    harness.advance(Duration::from_secs(300));
    assert_eq!(
        texts(&harness.invoke(count(7)).await.unwrap()),
        ["total 2, recent 2"]
    );
    // After the window the warnings still count in total, no longer as recent.
    harness.advance(Duration::from_secs(900));
    assert_eq!(
        texts(&harness.invoke(count(7)).await.unwrap()),
        ["total 3, recent 1"]
    );
    assert_eq!(
        texts(&harness.invoke(count(8)).await.unwrap()),
        ["total 1, recent 1"]
    );
    assert_eq!(
        texts(&harness.invoke(Invocation::new("last")).await.unwrap()),
        ["Some(8)"]
    );
    // The test can look at the same data.
    assert_eq!(
        harness.store().counter_total("warns", 7_u64).await.unwrap(),
        3
    );
}

#[tokio::test]
async fn a_module_without_the_capability_cannot_keep_data() {
    // The same module, but it did not declare `storage`.
    let module = ModuleBuilder::new("count", "Counting", "0.1.0")
        .unwrap()
        .command(
            Command::<()>::new("last", "Who was counted last").handler(|cx, ()| async move {
                cx.store().set("k", &1).await?;
                Ok(())
            }),
        );
    let harness = Harness::new(module).unwrap();
    let error = harness.invoke(Invocation::new("last")).await.unwrap_err();
    assert!(error.to_string().contains("storage"), "{error}");
}

fn message_watcher() -> ModuleBuilder {
    ModuleBuilder::new("watch", "w", "0.1.0")
        .unwrap()
        .on(Subscription::new(EventKind::MessageCreated), |_, _| async {
            Ok(Flow::Continue)
        })
}

#[test]
fn events_the_gateway_would_never_send_are_refused_when_the_module_is_built() {
    let error = message_watcher().build().unwrap_err().to_string();
    assert!(error.contains("GUILD_MESSAGES"), "{error}");
    message_watcher()
        .intents(Intents::GUILD_MESSAGES)
        .build()
        .unwrap();
    // Members are a privileged intent, so it has to be asked for in words.
    let members = ModuleBuilder::new("watch", "w", "0.1.0")
        .unwrap()
        .on(Subscription::new(EventKind::MemberJoined), |_, _| async {
            Ok(Flow::Continue)
        });
    assert!(members.build().is_err());
}

#[test]
fn the_order_of_the_calls_does_not_matter_for_a_permission_and_its_command() {
    let module = ModuleBuilder::new("warn", "w", "0.1.0")
        .unwrap()
        .command(
            Command::<()>::new("a", "d")
                .requires("warn.add")
                .handler(|_cx, ()| async { Ok(()) }),
        )
        .permission(PermissionSpec::new("warn.add", Level::MODERATOR, "Warn").unwrap());
    module.build().unwrap();
}

#[test]
fn buttons_and_forms_are_not_subscribed_to_and_routes_survive_a_custom_id() {
    for kind in [EventKind::Component, EventKind::Modal] {
        let module = ModuleBuilder::new("ui", "u", "0.1.0")
            .unwrap()
            .on(Subscription::new(kind), |_, _| async { Ok(Flow::Continue) });
        assert!(module.build().is_err(), "{kind:?}");
    }
    for route in ["", "a:b"] {
        let component = ModuleBuilder::new("ui", "u", "0.1.0")
            .unwrap()
            .component(route, |_cx| async { Ok(()) });
        assert!(component.build().is_err(), "{route:?}");
        let modal = ModuleBuilder::new("ui", "u", "0.1.0")
            .unwrap()
            .modal(route, |_cx| async { Ok(()) });
        assert!(modal.build().is_err(), "{route:?}");
    }
}

#[test]
fn a_route_that_makes_every_custom_id_too_long_is_refused() {
    // The shortest custom_id of a route is "{module}:{route}:*:": 2 + route + 4 characters.
    let fits = "r".repeat(94);
    let too_long = "r".repeat(95);
    let module = || ModuleBuilder::new("ui", "u", "0.1.0").unwrap();
    assert!(
        module()
            .component(&fits, |_cx| async { Ok(()) })
            .build()
            .is_ok()
    );
    assert!(
        module()
            .component(&too_long, |_cx| async { Ok(()) })
            .build()
            .is_err()
    );
    assert!(
        module()
            .modal(&fits, |_cx| async { Ok(()) })
            .build()
            .is_ok()
    );
    assert!(
        module()
            .modal(&too_long, |_cx| async { Ok(()) })
            .build()
            .is_err()
    );
}

#[test]
fn a_route_is_measured_in_characters_not_bytes() {
    // Discord counts a custom_id in characters: "あ" is one, though it is three bytes.
    let fits = "あ".repeat(94);
    let too_long = "あ".repeat(95);
    let module = || ModuleBuilder::new("ui", "u", "0.1.0").unwrap();
    assert!(
        module()
            .component(&fits, |_cx| async { Ok(()) })
            .build()
            .is_ok()
    );
    assert!(
        module()
            .component(&too_long, |_cx| async { Ok(()) })
            .build()
            .is_err()
    );
    assert!(
        module()
            .modal(&fits, |_cx| async { Ok(()) })
            .build()
            .is_ok()
    );
    assert!(
        module()
            .modal(&too_long, |_cx| async { Ok(()) })
            .build()
            .is_err()
    );
}

#[test]
fn a_subcommand_cannot_carry_what_only_a_command_can() {
    let group = ModuleBuilder::new("warn", "w", "0.1.0").unwrap().group(
        "warn",
        "d",
        [Command::<()>::new("add", "d")
            .default_member_permissions(twilight_model::guild::Permissions::BAN_MEMBERS)
            .handler(|_cx, ()| async { Ok(()) })],
    );
    assert!(group.build().is_err());
}

#[tokio::test]
async fn a_command_asks_about_its_own_permission_keys_only() {
    let module = ModuleBuilder::new("warn", "w", "0.1.0")
        .unwrap()
        .permission(PermissionSpec::new("warn.add", Level::MODERATOR, "Warn").unwrap())
        .command(
            Command::<()>::new("probe", "d").handler(|cx, ()| async move {
                cx.require("warn.add").await?;
                // The server's key of another module is not for this one to ask about.
                cx.require("ban.use").await?;
                cx.reply("both").await
            }),
        );
    let harness = Harness::new(module).unwrap();
    harness.permissions(|permissions| {
        permissions.role_levels.insert(Id::new(10), Level::OWNER);
    });
    let result = harness.invoke(Invocation::new("probe").roles([10])).await;
    let error = result.unwrap_err().to_string();
    assert!(error.contains("its own permission keys"), "{error}");
}

#[derive(CommandArgs)]
struct Upload {
    /// The file to keep
    file: Attachment,
    /// Who it is about
    who: Option<Mentionable>,
}

fn upload_module() -> ModuleBuilder {
    ModuleBuilder::new("upload", "Upload", "0.1.0")
        .unwrap()
        .command(
            Command::<Upload>::new("upload", "Keep a file").handler(|cx, args| async move {
                let about = match args.who {
                    Some(Mentionable::User(_)) => "user",
                    Some(Mentionable::Role(_)) => "role",
                    None => "nobody",
                };
                cx.reply(format!("{} {} {about}", args.file.filename, args.file.url))
                    .await
            }),
        )
}

#[tokio::test]
async fn attachment_and_mentionable_options_are_published_and_parsed() {
    let options = Upload::options();
    assert_eq!(options[0].kind, OptionKind::Attachment);
    assert_eq!(options[1].kind, OptionKind::Mentionable);
    let file = Attachment {
        id: 55,
        filename: "a.png".into(),
        size: 12,
        content_type: Some("image/png".into()),
        url: "https://cdn.example/a.png".into(),
    };
    let harness = Harness::new(upload_module()).unwrap();
    let invoke = |who: Option<CommandOptionValue>| {
        let mut invocation =
            Invocation::new("upload").option("file", CommandOptionValue::Attachment(file.clone()));
        if let Some(who) = who {
            invocation = invocation.option("who", who);
        }
        harness.invoke(invocation)
    };
    let replies = invoke(Some(CommandOptionValue::Mentionable(Mentionable::Role(
        Id::new(10),
    ))))
    .await
    .unwrap();
    assert_eq!(texts(&replies), ["a.png https://cdn.example/a.png role"]);
    let replies = invoke(Some(CommandOptionValue::Mentionable(Mentionable::User(
        Id::new(9),
    ))))
    .await
    .unwrap();
    assert_eq!(texts(&replies), ["a.png https://cdn.example/a.png user"]);
    let replies = invoke(None).await.unwrap();
    assert_eq!(texts(&replies), ["a.png https://cdn.example/a.png nobody"]);
}

fn tree_module() -> ModuleBuilder {
    ModuleBuilder::new("tree", "Tree", "0.1.0")
        .unwrap()
        .permission(PermissionSpec::new("tree.edit", Level::MODERATOR, "Edit the tree").unwrap())
        .groups(
            "warn",
            "Warnings",
            [
                SubcommandGroup::new("list", "Look at warnings")
                    .subcommand(
                        Command::<()>::new("all", "Every warning")
                            .cooldown(Cooldown::per_user(Duration::from_secs(10)))
                            .handler(|cx, ()| async move { cx.reply("all").await }),
                    )
                    .subcommand(
                        Command::<()>::new("kind", "Those of one kind")
                            .requires("tree.edit")
                            .handler(|cx, ()| async move { cx.reply("kind").await }),
                    ),
                SubcommandGroup::new("clear", "Remove warnings").subcommand(
                    Command::<()>::new("all", "Everything")
                        .handler(|cx, ()| async move { cx.reply("cleared").await }),
                ),
            ],
        )
}

#[tokio::test]
async fn subcommands_of_a_group_route_by_both_names_with_their_permission_and_cooldown() {
    let harness = Harness::new(tree_module()).unwrap();
    harness.permissions(|permissions| {
        permissions
            .role_levels
            .insert(Id::new(10), Level::MODERATOR);
    });
    let all = Invocation::new("warn").subcommand("list all").user(7);
    assert_eq!(texts(&harness.invoke(all.clone()).await.unwrap()), ["all"]);
    // The cooldown of the nested command holds the user back, as for a flat subcommand.
    assert_ne!(texts(&harness.invoke(all).await.unwrap()), ["all"]);
    assert_eq!(
        texts(
            &harness
                .invoke(Invocation::new("warn").subcommand("clear all").user(8))
                .await
                .unwrap()
        ),
        ["cleared"]
    );
    let kind = Invocation::new("warn").subcommand("list kind").user(9);
    assert_ne!(
        texts(&harness.invoke(kind.clone()).await.unwrap()),
        ["kind"],
        "without the permission"
    );
    assert_eq!(
        texts(&harness.invoke(kind.roles([10])).await.unwrap()),
        ["kind"]
    );
    // A name that is no route of the command is not answered.
    assert!(
        harness
            .invoke(Invocation::new("warn").subcommand("all"))
            .await
            .unwrap()
            .is_empty()
    );
}

#[test]
fn a_group_is_refused_when_none_is_empty_or_two_routes_share_a_name() {
    let empty = ModuleBuilder::new("tree", "Tree", "0.1.0").unwrap().groups(
        "warn",
        "Warnings",
        [SubcommandGroup::new("list", "Look")],
    );
    assert!(empty.build().is_err(), "a group without subcommands");
    let twice = ModuleBuilder::new("tree", "Tree", "0.1.0").unwrap().groups(
        "warn",
        "Warnings",
        [
            SubcommandGroup::new("list", "a")
                .subcommand(Command::<()>::new("all", "a").handler(|_, ()| async move { Ok(()) })),
            SubcommandGroup::new("list", "b")
                .subcommand(Command::<()>::new("all", "b").handler(|_, ()| async move { Ok(()) })),
        ],
    );
    assert!(twice.build().is_err(), "`list all` declared twice");
}

/// A plain subcommand that answers with its own name.
fn plain(name: &str) -> BoundCommand {
    let reply = name.to_owned();
    Command::<()>::new(name, "Plain").handler(move |cx, ()| {
        let reply = reply.clone();
        async move { cx.reply(reply).await }
    })
}

/// A subcommand `sub` inside the group `group`, answering with `group sub`.
fn grouped(group: &str, sub: &str) -> SubcommandGroup {
    SubcommandGroup::new(group, "Grouped").subcommand(plain(sub))
}

/// The command `warn`: a plain `status`, which needs `tree.edit`, and the group `list` with `all`
/// and the group `clear` with `all`, in whichever order the calls are made.
fn tree(first_groups: bool) -> ModuleBuilder {
    let base = ModuleBuilder::new("tree", "Tree", "0.1.0")
        .unwrap()
        .permission(PermissionSpec::new("tree.edit", Level::MODERATOR, "Edit the tree").unwrap());
    let plain_call = |builder: ModuleBuilder| {
        builder.group(
            "warn",
            "Warnings",
            [Command::<()>::new("status", "Plain")
                .requires("tree.edit")
                .handler(|cx, ()| async move { cx.reply("status").await })],
        )
    };
    let groups_call = |builder: ModuleBuilder| {
        builder.groups(
            "warn",
            "Warnings",
            [grouped("list", "all"), grouped("clear", "all")],
        )
    };
    if first_groups {
        plain_call(groups_call(base))
    } else {
        groups_call(plain_call(base))
    }
}

#[tokio::test]
async fn plain_subcommands_and_groups_share_one_command_in_either_order() {
    for first_groups in [false, true] {
        let module = tree(first_groups).build().unwrap();
        let commands = &module.manifest().commands;
        assert_eq!(commands.len(), 1, "one command named warn is published");
        let mut names: Vec<&str> = commands[0]
            .subcommands
            .iter()
            .map(|sub| sub.name.as_str())
            .collect();
        names.sort_unstable();
        assert_eq!(
            names,
            ["clear", "list", "status"],
            "first_groups={first_groups}"
        );

        let harness = Harness::new(tree(first_groups)).unwrap();
        harness.permissions(|permissions| {
            permissions
                .role_levels
                .insert(Id::new(10), Level::MODERATOR);
        });
        let plain = Invocation::new("warn").subcommand("status").user(7);
        assert_ne!(
            texts(&harness.invoke(plain.clone()).await.unwrap()),
            ["status"],
            "the plain subcommand needs its permission"
        );
        assert_eq!(
            texts(&harness.invoke(plain.roles([10])).await.unwrap()),
            ["status"]
        );
        let grouped = Invocation::new("warn").subcommand("list all").user(8);
        assert_eq!(
            texts(&harness.invoke(grouped).await.unwrap()),
            ["all"],
            "a group's subcommand is routed by both names"
        );
    }
}

#[test]
fn a_plain_name_and_a_group_name_cannot_clash_nor_a_command_with_options() {
    let base = || ModuleBuilder::new("tree", "Tree", "0.1.0").unwrap();
    let with_options = base().command(
        Command::<Kind>::new("warn", "Warnings").handler(|_cx, _args| async move { Ok(()) }),
    );
    assert!(
        with_options
            .group("warn", "Warnings", [plain("status")])
            .build()
            .is_err(),
        "a command with options cannot take subcommands"
    );
    let no_options = base().command(plain("warn"));
    assert!(
        no_options
            .group("warn", "Warnings", [plain("status")])
            .build()
            .is_err(),
        "a plain command cannot take subcommands"
    );
    let command_after = base()
        .group("warn", "Warnings", [plain("status")])
        .command(plain("warn"));
    assert!(
        command_after.build().is_err(),
        "a command with subcommands cannot become a plain command"
    );
    let plain_then_group = base().group("warn", "Warnings", [plain("list")]).groups(
        "warn",
        "Warnings",
        [grouped("list", "all")],
    );
    assert!(
        plain_then_group.build().is_err(),
        "a subcommand and a group share a name"
    );
    let group_then_plain = base()
        .groups("warn", "Warnings", [grouped("list", "all")])
        .group("warn", "Warnings", [plain("list")]);
    assert!(group_then_plain.build().is_err());
    let groups_twice = base()
        .groups("warn", "Warnings", [grouped("list", "all")])
        .groups("warn", "Warnings", [grouped("list", "clear")]);
    assert!(groups_twice.build().is_err(), "a group named twice");
    let plain_twice = base().group("warn", "Warnings", [plain("status")]).group(
        "warn",
        "Warnings",
        [plain("status")],
    );
    assert!(plain_twice.build().is_err(), "a subcommand named twice");
}

/// `plain` plain subcommands and `groups` groups in one command, split over two calls.
fn many(plain_count: usize, groups_count: usize) -> ModuleBuilder {
    let base = ModuleBuilder::new("tree", "Tree", "0.1.0").unwrap();
    base.group(
        "warn",
        "Warnings",
        (0..plain_count).map(|i| plain(&format!("p{i}"))),
    )
    .groups(
        "warn",
        "Warnings",
        (0..groups_count).map(|i| grouped(&format!("g{i}"), "all")),
    )
}

#[test]
fn a_command_takes_at_most_25_subcommands_and_groups() {
    assert!(many(12, 13).build().is_ok(), "25 is the limit");
    assert!(many(13, 13).build().is_err(), "26 is too many for Discord");
}
