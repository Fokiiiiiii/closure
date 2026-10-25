//! The `ModLog`: moderation cases posted to the server's log channel, written with the SDK only.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::too_many_lines)]

use closure_core::action::Action;
use closure_core::event::CommandOptionValue;
use closure_sdk::testing::{Harness, Invocation, Reply};
use closure_sdk::{
    BoundCommand, Capability, CaseKind, Command, CommandArgs, Locale, ModuleBuilder, NewCase,
    UserId,
};
use twilight_model::channel::message::Embed;
use twilight_model::id::Id;

#[derive(CommandArgs)]
struct Log {
    /// Member the case is about
    user: UserId,
    /// Kind of the case
    kind: String,
    /// Why
    reason: Option<String>,
    /// When it ends, in Unix seconds
    expires_at: Option<i64>,
}

#[derive(CommandArgs)]
struct Revoke {
    /// Number of the case
    number: i64,
    /// Why it is revoked
    reason: Option<String>,
}

/// Records a case about the member and posts it.
fn log_command(name: &str) -> BoundCommand {
    Command::<Log>::new(name, "Record a case").handler(|cx, args| async move {
        let case = cx
            .cases()
            .create(NewCase {
                kind: CaseKind::new(&args.kind)?,
                target: args.user,
                moderator: cx.user().id,
                reason: args.reason,
                expires_at: args.expires_at,
            })
            .await?;
        let posted = cx.modlog(&case).await?;
        cx.reply(if posted.is_some() {
            "posted"
        } else {
            "skipped"
        })
        .await
    })
}

fn module() -> ModuleBuilder {
    ModuleBuilder::new("log", "Logs", "0.1.0")
        .unwrap()
        .capability(Capability::Cases)
        .capability(Capability::SendMessages)
        .command(log_command("log"))
        .command(
            Command::<Revoke>::new("revoke", "Revoke a case").handler(|cx, args| async move {
                let case = cx
                    .cases()
                    .revoke(args.number, cx.user().id, args.reason)
                    .await?;
                cx.modlog(&case).await?;
                cx.reply("revoked").await
            }),
        )
}

/// Records cases but may not send messages; `try` says whether the log was refused.
fn module_without_send_messages() -> ModuleBuilder {
    ModuleBuilder::new("quiet", "Quiet", "0.1.0")
        .unwrap()
        .capability(Capability::Cases)
        .command(
            Command::<Log>::new("try", "Record a case and try to log it").handler(
                |cx, args| async move {
                    let case = cx
                        .cases()
                        .create(NewCase {
                            kind: CaseKind::new(&args.kind)?,
                            target: args.user,
                            moderator: cx.user().id,
                            reason: args.reason,
                            expires_at: args.expires_at,
                        })
                        .await?;
                    let refused = cx.modlog(&case).await.is_err();
                    cx.reply(if refused { "refused" } else { "posted" }).await
                },
            ),
        )
}

fn texts(replies: &[Reply]) -> Vec<&str> {
    replies.iter().map(|reply| reply.content.as_str()).collect()
}

/// The invocation of `command` that records a case about user 7 of `kind`.
fn record(command: &str, kind: &str) -> Invocation {
    Invocation::new(command)
        .option("user", CommandOptionValue::User(Id::new(7)))
        .option("kind", CommandOptionValue::String(kind.to_owned()))
}

fn reason(text: &str) -> CommandOptionValue {
    CommandOptionValue::String(text.to_owned())
}

/// The text of the field `name` of `embed`.
fn field<'a>(embed: &'a Embed, name: &str) -> &'a str {
    embed
        .fields
        .iter()
        .find(|field| field.name == name)
        .map(|field| field.value.as_str())
        .expect("an embed field with this name")
}

/// The channel of every message the module sent, in order.
fn sent_to(harness: &Harness) -> Vec<u64> {
    harness
        .api()
        .actions()
        .iter()
        .filter_map(|action| match action {
            Action::SendMessage { channel_id, .. } => Some(channel_id.get()),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn a_case_is_posted_to_the_log_channel() {
    let harness = Harness::new(module()).unwrap();
    harness.set_log_channel(77);
    let replies = harness
        .invoke(record("log", "warn").option("reason", reason("spam")))
        .await
        .unwrap();

    assert_eq!(texts(&replies), ["posted"]);
    assert_eq!(sent_to(&harness), [77]);
    let embeds = harness.sent_embeds();
    assert_eq!(embeds.len(), 1);
    let embed = &embeds[0];
    assert_eq!(embed.title.as_deref(), Some("Case #1 · Warning"));
    assert_eq!(field(embed, "Target"), "<@7> (7)");
    assert_eq!(field(embed, "Moderator"), "<@100> (100)");
    assert_eq!(field(embed, "Reason"), "spam");
    assert_eq!(embed.footer.as_ref().map(|f| f.text.as_str()), Some("log"));
}

#[tokio::test]
async fn the_log_is_in_the_language_of_the_guild_not_of_the_invoker() {
    let harness = Harness::new(module()).unwrap();
    harness.set_log_channel(77);
    harness.set_guild_locale(Locale::Ja);
    harness
        .invoke(
            record("log", "warn")
                .locale(Locale::En)
                .option("reason", reason("spam")),
        )
        .await
        .unwrap();

    let embeds = harness.sent_embeds();
    let embed = &embeds[0];
    assert_eq!(embed.title.as_deref(), Some("ケース #1 · 警告"));
    assert_eq!(field(embed, "対象"), "<@7> (7)");
    assert_eq!(field(embed, "実行者"), "<@100> (100)");
    assert_eq!(field(embed, "理由"), "spam");
}

#[tokio::test]
async fn without_a_log_channel_nothing_is_posted() {
    let harness = Harness::new(module()).unwrap();
    let replies = harness.invoke(record("log", "warn")).await.unwrap();

    assert_eq!(texts(&replies), ["skipped"]);
    assert!(sent_to(&harness).is_empty());
    assert!(harness.sent_embeds().is_empty());
}

#[tokio::test]
async fn mentions_and_everyone_in_the_reason_do_not_ping() {
    let harness = Harness::new(module()).unwrap();
    harness.set_log_channel(77);
    harness
        .invoke(record("log", "warn").option("reason", reason("@everyone and <@123> and <#9>")))
        .await
        .unwrap();

    let embeds = harness.sent_embeds();
    let text = field(&embeds[0], "Reason");
    assert!(!text.contains("@everyone"), "{text}");
    assert!(!text.contains("<@") && !text.contains("<#"), "{text}");
}

#[tokio::test]
async fn a_long_reason_fits_the_field() {
    // Cases keep at most 512 characters of reason; escaping doubles the asterisks.
    let harness = Harness::new(module()).unwrap();
    harness.set_log_channel(77);
    let replies = harness
        .invoke(record("log", "warn").option("reason", reason(&"*".repeat(512))))
        .await
        .unwrap();

    assert_eq!(texts(&replies), ["posted"]);
    let embeds = harness.sent_embeds();
    assert!(field(&embeds[0], "Reason").chars().count() <= 1024);
}

#[tokio::test]
async fn a_case_without_a_reason_says_so() {
    let harness = Harness::new(module()).unwrap();
    harness.set_log_channel(77);
    harness.invoke(record("log", "warn")).await.unwrap();
    harness
        .invoke(record("log", "warn").option("reason", reason("")))
        .await
        .unwrap();

    let embeds = harness.sent_embeds();
    assert_eq!(embeds.len(), 2);
    for embed in &embeds {
        assert_eq!(field(embed, "Reason"), "No reason given");
    }
}

#[tokio::test]
async fn an_expiry_shows_how_long_and_until_when() {
    let harness = Harness::new(module()).unwrap();
    harness.set_log_channel(77);
    harness
        .invoke(
            record("log", "timeout")
                .option("expires_at", CommandOptionValue::Integer(1_900_000_000)),
        )
        .await
        .unwrap();

    let embeds = harness.sent_embeds();
    assert_eq!(
        field(&embeds[0], "Expires"),
        "<t:1900000000:R> (<t:1900000000:F>)"
    );
}

#[tokio::test]
async fn a_revoked_case_is_grey_and_says_who_revoked_it_and_why() {
    let harness = Harness::new(module()).unwrap();
    harness.set_log_channel(77);
    harness
        .invoke(record("log", "warn").option("reason", reason("spam")))
        .await
        .unwrap();
    let replies = harness
        .invoke(
            Invocation::new("revoke")
                .option("number", CommandOptionValue::Integer(1))
                .option("reason", reason("mistake")),
        )
        .await
        .unwrap();

    assert_eq!(texts(&replies), ["revoked"]);
    let embeds = harness.sent_embeds();
    let embed = embeds.last().unwrap();
    assert_eq!(embed.color, Some(0x0095_A5A6));
    assert_eq!(embed.title.as_deref(), Some("Case #1 · Warning"));
    let revoked = field(embed, "Revoked");
    assert!(revoked.contains("<@100>"), "{revoked}");
    assert!(revoked.ends_with("mistake"), "{revoked}");
}

#[tokio::test]
async fn a_long_revocation_reason_is_cut_to_fit_the_field() {
    let harness = Harness::new(module()).unwrap();
    harness.set_log_channel(77);
    harness.invoke(record("log", "warn")).await.unwrap();
    let replies = harness
        .invoke(
            Invocation::new("revoke")
                .option("number", CommandOptionValue::Integer(1))
                .option("reason", reason(&"*".repeat(512))),
        )
        .await
        .unwrap();

    assert_eq!(texts(&replies), ["revoked"]);
    let embeds = harness.sent_embeds();
    let revoked = field(embeds.last().unwrap(), "Revoked");
    assert!(revoked.chars().count() <= 1024, "{revoked}");
    assert!(revoked.ends_with('…'), "{revoked}");
}

#[tokio::test]
async fn the_color_and_title_follow_the_kind() {
    let harness = Harness::new(module()).unwrap();
    harness.set_log_channel(77);
    let kinds = [
        ("warn", 0x00FE_E75C, "Warning"),
        ("timeout", 0x00FE_E75C, "Timeout"),
        ("kick", 0x00FE_E75C, "Kick"),
        ("ban", 0x00ED_4245, "Ban"),
        ("unban", 0x0057_F287, "Unban"),
        ("note", 0x0058_65F2, "Note"),
        ("custom", 0x0058_65F2, "custom"),
    ];
    for (kind, _, _) in kinds {
        harness.invoke(record("log", kind)).await.unwrap();
    }

    let embeds = harness.sent_embeds();
    assert_eq!(embeds.len(), kinds.len());
    for (number, (embed, (_, color, name))) in embeds.iter().zip(kinds).enumerate() {
        assert_eq!(embed.color, Some(color), "{name}");
        assert_eq!(
            embed.title.as_deref(),
            Some(format!("Case #{} · {name}", number + 1).as_str())
        );
    }
}

#[tokio::test]
async fn a_module_without_send_messages_is_refused_the_log() {
    let harness = Harness::new(module_without_send_messages()).unwrap();
    harness.set_log_channel(77);
    let replies = harness.invoke(record("try", "warn")).await.unwrap();

    assert_eq!(texts(&replies), ["refused"]);
    assert!(sent_to(&harness).is_empty());
}
