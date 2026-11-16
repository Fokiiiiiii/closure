#![allow(clippy::unwrap_used, clippy::expect_used)]

use closure_core::action::{Action, FileUpload};
use closure_sdk::notify;
use closure_sdk::testing::{Harness, Invocation};
use closure_sdk::{Capability, Command, ModuleBuilder};
use twilight_model::id::Id;

fn module() -> ModuleBuilder {
    ModuleBuilder::new("notes", "Notes", "0.1.0")
        .unwrap()
        .capability(Capability::SendMessages)
        .command(Command::<()>::new("quiet").handler(|cx, ()| async move {
            notify::send_silent(cx.ctx(), Id::new(77), "https://x.com/a/status/1").await?;
            cx.reply("posted").await
        }))
}

#[tokio::test]
async fn a_message_sent_silently_is_recorded_as_silent() {
    let harness = Harness::new(module()).unwrap();
    harness.api().add_channel(Id::new(77), Some(Id::new(1)));

    harness.invoke(Invocation::new("quiet")).await.unwrap();

    let silent: Vec<bool> = harness
        .api()
        .actions()
        .iter()
        .filter_map(|action| match action {
            Action::SendMessage { silent, .. } => Some(*silent),
            _ => None,
        })
        .collect();
    assert_eq!(silent, vec![true]);
}

#[tokio::test]
async fn a_silent_post_keeps_its_files_and_is_recorded_as_silent() {
    let module = ModuleBuilder::new("cards", "Cards", "0.1.0")
        .unwrap()
        .capability(Capability::SendMessages)
        .command(Command::<()>::new("card").handler(|cx, ()| async move {
            let posted = notify::send_silent_with(
                cx.ctx(),
                Id::new(77),
                notify::SilentPost {
                    content: Some("card".into()),
                    files: vec![FileUpload {
                        filename: "a.png".into(),
                        data: vec![1, 2, 3],
                    }],
                    ..notify::SilentPost::default()
                },
            )
            .await?;
            cx.reply(if posted.is_some() {
                "posted"
            } else {
                "missing"
            })
            .await
        }));
    let harness = Harness::new(module).unwrap();
    harness.api().add_channel(Id::new(77), Some(Id::new(1)));

    harness.invoke(Invocation::new("card")).await.unwrap();

    let posts: Vec<(bool, usize)> = harness
        .api()
        .actions()
        .iter()
        .filter_map(|action| match action {
            Action::PostMessage { silent, files, .. } => Some((*silent, files.len())),
            _ => None,
        })
        .collect();
    assert_eq!(posts, vec![(true, 1)]);
}
