//! How a handler's answers turn into responses, follow-ups and edits.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::too_many_lines)]

use closure_core::action::InteractionReply;
use closure_sdk::testing::{Click, Harness, Invocation, ReplyKind};
use closure_sdk::ui::{button, row};
use closure_sdk::{Command, CustomId, DiscordError, ModuleBuilder, UserError};
use twilight_model::channel::message::component::ButtonStyle;

const EN: &str = "
secret = Only for you.
";

fn module() -> ModuleBuilder {
    ModuleBuilder::new("replies", "Replies", "0.1.0")
        .unwrap()
        .command(
            Command::<()>::new("slow-public", "Takes a while, then fails privately").handler(
                |cx, ()| async move {
                    cx.defer(false).await?;
                    Err(UserError::message("secret"))
                },
            ),
        )
        .command(
            Command::<()>::new("slow-private", "Takes a while, privately").handler(
                |cx, ()| async move {
                    cx.defer(true).await?;
                    cx.reply_private_t("secret", &closure_sdk::Args::new())
                        .await
                },
            ),
        )
        .command(
            Command::<()>::new("too-long", "A first answer that cannot be sent").handler(
                |cx, ()| async move {
                    // Refused before anything is sent; the handler tries again with less.
                    if cx.reply("x".repeat(2001)).await.is_err() {
                        cx.reply("short").await?;
                    }
                    Ok(())
                },
            ),
        )
        .command(
            Command::<()>::new("menu", "A message with a button").handler(|cx, ()| async move {
                let id = CustomId::new("go").encode(cx.module());
                cx.reply_with(InteractionReply {
                    content: Some("choose".into()),
                    components: vec![row([button(ButtonStyle::Primary, "Go", id)])],
                    ..InteractionReply::default()
                })
                .await
            }),
        )
        .component("go", |cx| async move {
            // Acknowledge at once, work, then change the message.
            cx.defer_update().await?;
            cx.update(InteractionReply {
                content: Some("done".into()),
                ..InteractionReply::default()
            })
            .await
        })
        .command(
            Command::<()>::new("ask", "Asks with a button whose answer is private").handler(
                |cx, ()| async move {
                    let id = CustomId::new("open").encode(cx.module());
                    cx.reply_with(InteractionReply {
                        content: Some("choose".into()),
                        components: vec![row([button(ButtonStyle::Primary, "Open", id)])],
                        ..InteractionReply::default()
                    })
                    .await
                },
            ),
        )
        .component("open", |cx| async move {
            cx.defer_update().await?;
            cx.reply_private("only you").await
        })
        .command(
            Command::<()>::new(
                "ask-update",
                "Asks with two buttons that change the message",
            )
            .handler(|cx, ()| async move {
                let fresh = CustomId::new("update").encode(cx.module());
                let deferred = CustomId::new("update-after-defer").encode(cx.module());
                cx.reply_with(InteractionReply {
                    content: Some("choose".into()),
                    components: vec![row([
                        button(ButtonStyle::Primary, "Fresh", fresh),
                        button(ButtonStyle::Primary, "Deferred", deferred),
                    ])],
                    ..InteractionReply::default()
                })
                .await
            }),
        )
        .component("update", |cx| async move {
            let secret = InteractionReply {
                content: Some("secret".into()),
                ephemeral: true,
                ..InteractionReply::default()
            };
            if cx.update(secret).await.is_err() {
                cx.reply_private("only you").await
            } else {
                Ok(())
            }
        })
        .component("update-after-defer", |cx| async move {
            cx.defer_update().await?;
            let secret = InteractionReply {
                content: Some("secret".into()),
                ephemeral: true,
                ..InteractionReply::default()
            };
            if cx.update(secret).await.is_err() {
                cx.reply_private("only you").await
            } else {
                Ok(())
            }
        })
        .command(
            Command::<()>::new(
                "public-then-secret",
                "Closes a public wait, then answers privately",
            )
            .handler(|cx, ()| async move {
                cx.defer(false).await?;
                // If closing the public message fails, the retry must not leak the secret.
                if cx
                    .reply_private_t("secret", &closure_sdk::Args::new())
                    .await
                    .is_err()
                {
                    cx.reply_private_t("secret", &closure_sdk::Args::new())
                        .await?;
                }
                Ok(())
            }),
        )
        .catalog(closure_sdk::Locale::En, EN)
}

#[tokio::test]
async fn a_private_answer_after_a_public_deferral_is_not_written_into_the_public_message() {
    let harness = Harness::new(module()).unwrap();
    let replies = harness
        .invoke(Invocation::new("slow-public"))
        .await
        .unwrap();
    let kinds: Vec<ReplyKind> = replies.iter().map(|reply| reply.kind).collect();
    assert_eq!(
        kinds,
        [ReplyKind::Deferred, ReplyKind::Edit, ReplyKind::Followup]
    );
    assert!(!replies[1].content.contains("Only for you"), "{replies:?}");
    assert!(!replies[1].ephemeral);
    assert_eq!(replies[2].content, "Only for you.");
    assert!(replies[2].ephemeral, "the secret goes to the user alone");
}

#[tokio::test]
async fn after_a_private_deferral_the_answer_edits_the_private_message() {
    let harness = Harness::new(module()).unwrap();
    let replies = harness
        .invoke(Invocation::new("slow-private"))
        .await
        .unwrap();
    let kinds: Vec<ReplyKind> = replies.iter().map(|reply| reply.kind).collect();
    assert_eq!(kinds, [ReplyKind::Deferred, ReplyKind::Edit]);
    assert_eq!(replies[1].content, "Only for you.");
}

#[tokio::test]
async fn an_answer_that_could_not_be_sent_leaves_the_first_answer_to_the_next_one() {
    let harness = Harness::new(module()).unwrap();
    let replies = harness.invoke(Invocation::new("too-long")).await.unwrap();
    assert_eq!(replies.len(), 1, "{replies:?}");
    assert_eq!(replies[0].kind, ReplyKind::Response);
    assert_eq!(replies[0].content, "short");
}

#[tokio::test]
async fn a_click_can_be_acknowledged_first_and_the_message_changed_after() {
    let harness = Harness::new(module()).unwrap();
    let asked = harness.invoke(Invocation::new("menu")).await.unwrap();
    let id = asked[0].custom_ids().remove(0);
    let replies = harness.click(Click::new(id)).await.unwrap();
    let kinds: Vec<ReplyKind> = replies.iter().map(|reply| reply.kind).collect();
    assert_eq!(kinds, [ReplyKind::Acknowledged, ReplyKind::Edit]);
    assert_eq!(replies[1].content, "done");
    assert!(replies[1].components.is_empty(), "the button is taken off");
}

#[tokio::test]
async fn a_private_answer_after_acknowledging_a_click_does_not_rewrite_the_message() {
    let harness = Harness::new(module()).unwrap();
    let asked = harness.invoke(Invocation::new("ask")).await.unwrap();
    let id = asked[0].custom_ids().remove(0);
    let replies = harness.click(Click::new(id)).await.unwrap();
    let kinds: Vec<ReplyKind> = replies.iter().map(|reply| reply.kind).collect();
    assert_eq!(kinds, [ReplyKind::Acknowledged, ReplyKind::Followup]);
    assert_eq!(replies[1].content, "only you");
    assert!(replies[1].ephemeral, "the answer goes to the user alone");
}

#[tokio::test]
async fn an_update_cannot_be_private_because_it_changes_a_shared_message() {
    let harness = Harness::new(module()).unwrap();
    let asked = harness.invoke(Invocation::new("ask-update")).await.unwrap();
    let ids = asked[0].custom_ids();

    let fresh = harness.click(Click::new(ids[0].clone())).await.unwrap();
    let kinds: Vec<ReplyKind> = fresh.iter().map(|reply| reply.kind).collect();
    assert_eq!(kinds, [ReplyKind::Response], "{fresh:?}");
    assert!(fresh[0].ephemeral);
    assert_eq!(fresh[0].content, "only you");

    let deferred = harness.click(Click::new(ids[1].clone())).await.unwrap();
    let kinds: Vec<ReplyKind> = deferred.iter().map(|reply| reply.kind).collect();
    assert_eq!(
        kinds,
        [ReplyKind::Acknowledged, ReplyKind::Followup],
        "{deferred:?}"
    );
    assert!(deferred[1].ephemeral);
    assert_eq!(deferred[1].content, "only you");

    assert!(
        fresh
            .iter()
            .chain(&deferred)
            .all(|reply| reply.content != "secret"),
        "the secret went into the shared message: {fresh:?} {deferred:?}"
    );
}

#[tokio::test]
async fn a_failed_attempt_to_close_the_public_message_does_not_let_the_retry_leak() {
    let harness = Harness::new(module()).unwrap();
    // Call 1 is the deferral, call 2 the "Done." edit that closes the public message.
    harness
        .api()
        .fail_nth(2, DiscordError::Transient("discord is down".into()));
    let replies = harness
        .invoke(Invocation::new("public-then-secret"))
        .await
        .unwrap();
    let kinds: Vec<ReplyKind> = replies.iter().map(|reply| reply.kind).collect();
    assert_eq!(
        kinds,
        [ReplyKind::Deferred, ReplyKind::Edit, ReplyKind::Followup]
    );
    assert!(!replies[1].content.contains("Only for you"), "{replies:?}");
    assert!(!replies[1].ephemeral);
    assert_eq!(replies[2].content, "Only for you.");
    assert!(replies[2].ephemeral, "the secret goes to the user alone");
}
