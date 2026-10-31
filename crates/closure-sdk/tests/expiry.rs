//! Expiring components: a press before the expiry works, a press after it is refused.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::time::Duration;

use closure_sdk::testing::{Click, Harness, Invocation, Reply};
use closure_sdk::ui::{button, row};
use closure_sdk::{Command, CustomId, InteractionReply, ModuleBuilder};
use twilight_model::channel::message::component::ButtonStyle;

fn module() -> ModuleBuilder {
    ModuleBuilder::new("menu", "Menu", "0.1.0")
        .unwrap()
        .command(Command::<()>::new("menu").handler(|cx, ()| async move {
            let press = button(
                ButtonStyle::Primary,
                "Go",
                CustomId::new("go")
                    .expires_in(cx.now(), Duration::from_secs(60))
                    .encode(cx.module()),
            );
            cx.reply_with(InteractionReply {
                content: Some("Menu".into()),
                components: vec![row([press])],
                ..InteractionReply::default()
            })
            .await
        }))
        .component("go", |cx| async move { cx.reply_private("went").await })
}

fn texts(replies: &[Reply]) -> Vec<&str> {
    replies.iter().map(|reply| reply.content.as_str()).collect()
}

#[tokio::test]
async fn a_press_before_the_expiry_works_and_a_press_after_it_is_refused() {
    let harness = Harness::new(module()).unwrap();
    let menu = harness.invoke(Invocation::new("menu")).await.unwrap();
    let custom_id = menu[0].custom_ids()[0].clone();
    let early = harness.click(Click::new(custom_id.clone())).await.unwrap();
    assert_eq!(texts(&early), ["went"]);
    harness.advance(Duration::from_secs(61));
    let late = harness.click(Click::new(custom_id)).await.unwrap();
    assert_eq!(texts(&late), ["This has expired, use the command again."]);
    assert!(late[0].ephemeral);
}
