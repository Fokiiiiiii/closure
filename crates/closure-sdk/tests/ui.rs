//! Buttons, menus, forms and pages written with the SDK only.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::too_many_lines)]

use closure_core::action::InteractionReply;
use closure_sdk::testing::{Click, Harness, Invocation, ReplyKind, Submit};
use closure_sdk::ui::{self, button, row, select, text_input};
use closure_sdk::{Command, CustomId, ModuleBuilder, Pager};
use twilight_model::channel::message::component::ButtonStyle;

fn module() -> ModuleBuilder {
    ModuleBuilder::new("ui", "Interface", "0.1.0")
        .unwrap()
        .command(Command::<()>::new("danger").handler(|cx, ()| async move {
            let user = cx.user().id;
            let yes = button(
                ButtonStyle::Danger,
                "Yes",
                CustomId::new("confirm")
                    .only_for(user)
                    .data("42")
                    .encode(cx.module()),
            );
            let no = button(
                ButtonStyle::Secondary,
                "No",
                CustomId::new("cancel").only_for(user).encode(cx.module()),
            );
            cx.reply_with(InteractionReply {
                content: Some("Sure?".into()),
                components: vec![row([yes, no])],
                ..InteractionReply::default()
            })
            .await
        }))
        .command(Command::<()>::new("report").handler(|cx, ()| async move {
            let form = ui::modal(
                CustomId::new("report").encode(cx.module()),
                "Report",
                [text_input(cx.module(), "why", "Why", true, true, 200)],
            );
            cx.show_modal(form).await
        }))
        .command(Command::<()>::new("menu").handler(|cx, ()| async move {
            let menu = select(
                CustomId::new("pick").encode(cx.module()),
                Some("Choose"),
                [
                    ("Spam".to_owned(), "spam".to_owned()),
                    ("Abuse".to_owned(), "abuse".to_owned()),
                ],
                1,
                1,
            );
            cx.reply_with(InteractionReply {
                content: Some("Which?".into()),
                components: vec![row([menu])],
                ..InteractionReply::default()
            })
            .await
        }))
        .command(Command::<()>::new("pages").handler(|cx, ()| async move {
            cx.reply_with(InteractionReply {
                content: Some("page 0".into()),
                components: vec![Pager::new("page", 0, 3).row(cx.module())],
                ..InteractionReply::default()
            })
            .await
        }))
        .command(Command::<()>::new("forge").handler(|cx, ()| async move {
            let other = closure_sdk::ModuleId::new("other").unwrap();
            let id = CustomId::new("confirm").encode(&other);
            cx.reply_with(InteractionReply {
                content: Some("hi".into()),
                components: vec![row([button(ButtonStyle::Primary, "x", id)])],
                ..InteractionReply::default()
            })
            .await
        }))
        .component("confirm", |cx| async move {
            cx.update(InteractionReply {
                content: Some(format!("done {}", cx.data())),
                ..InteractionReply::default()
            })
            .await
        })
        .component("cancel", |cx| async move {
            cx.update(InteractionReply {
                content: Some("cancelled".into()),
                ..InteractionReply::default()
            })
            .await
        })
        .component("pick", |cx| async move {
            let chosen = cx.values().join(",");
            cx.reply_private(format!("picked {chosen}")).await
        })
        .component("silent", |_cx| async move { Ok(()) })
        .component("page", |cx| async move {
            let page = cx.page_requested().unwrap_or(0);
            cx.update(InteractionReply {
                content: Some(format!("page {page}")),
                components: vec![Pager::new("page", page, 3).row(cx.module())],
                ..InteractionReply::default()
            })
            .await
        })
        .modal("report", |cx| async move {
            let why = cx.field("why").unwrap_or("-").to_owned();
            cx.reply_private(format!("thanks: {why}")).await
        })
}

#[tokio::test]
async fn a_command_offers_buttons_and_only_the_one_it_was_made_for_may_press_them() {
    let harness = Harness::new(module()).unwrap();
    let replies = harness
        .invoke(Invocation::new("danger").user(7))
        .await
        .unwrap();
    assert_eq!(replies[0].content, "Sure?");
    let ids = replies[0].custom_ids();
    assert_eq!(ids, ["ui:confirm:7:42", "ui:cancel:7:"]);

    // Somebody else presses it: told so privately, nothing changes.
    let stranger = harness.click(Click::new(&ids[0]).user(8)).await.unwrap();
    assert_eq!(stranger.len(), 1);
    assert_eq!(stranger[0].content, "That is not for you.");
    assert!(stranger[0].ephemeral);
    assert_eq!(stranger[0].kind, ReplyKind::Response);

    // The owner presses it: the message itself is changed, and the handler got its data.
    let owner = harness.click(Click::new(&ids[0]).user(7)).await.unwrap();
    assert_eq!(owner[0].kind, ReplyKind::Update);
    assert_eq!(owner[0].content, "done 42");
    let cancel = harness.click(Click::new(&ids[1]).user(7)).await.unwrap();
    assert_eq!(cancel[0].content, "cancelled");
}

#[tokio::test]
async fn a_click_nobody_answers_is_acknowledged_and_an_unknown_route_is_said_to_be_gone() {
    let harness = Harness::new(module()).unwrap();
    let silent = harness.click(Click::new("ui:silent:*:")).await.unwrap();
    assert_eq!(silent.len(), 1);
    assert_eq!(silent[0].kind, ReplyKind::Acknowledged);

    let gone = harness.click(Click::new("ui:removed:*:")).await.unwrap();
    assert_eq!(gone[0].content, "That is no longer available.");
    // Not ours, or not readable: nothing is done.
    assert!(
        harness
            .click(Click::new("other:confirm:*:"))
            .await
            .unwrap()
            .is_empty()
    );
    assert!(
        harness
            .click(Click::new("garbage"))
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn a_select_menu_hands_over_what_was_chosen() {
    let harness = Harness::new(module()).unwrap();
    let replies = harness.invoke(Invocation::new("menu")).await.unwrap();
    assert_eq!(replies[0].custom_ids(), ["ui:pick:*:"]);
    let chosen = harness
        .click(Click::new("ui:pick:*:").values(["abuse"]))
        .await
        .unwrap();
    assert_eq!(chosen[0].content, "picked abuse");
}

#[tokio::test]
async fn a_form_is_shown_and_what_is_written_in_it_comes_back() {
    let harness = Harness::new(module()).unwrap();
    let shown = harness.invoke(Invocation::new("report")).await.unwrap();
    assert_eq!(shown[0].kind, ReplyKind::Modal);
    assert_eq!(shown[0].content, "Report");
    assert_eq!(shown[0].custom_ids(), ["ui:why"]);

    let sent = harness
        .submit(Submit::new("ui:report:*:").field("ui:why", "spam in #general"))
        .await
        .unwrap();
    assert_eq!(sent[0].content, "thanks: spam in #general");
    assert!(sent[0].ephemeral);
}

#[tokio::test]
async fn the_pager_walks_through_the_pages() {
    let harness = Harness::new(module()).unwrap();
    let shown = harness.invoke(Invocation::new("pages")).await.unwrap();
    assert_eq!(
        shown[0].custom_ids(),
        ["ui:page:*:0", "ui:page:*:here", "ui:page:*:1"]
    );
    let next = harness.click(Click::new("ui:page:*:1")).await.unwrap();
    assert_eq!(next[0].content, "page 1");
    assert_eq!(next[0].custom_ids()[2], "ui:page:*:2");
    let last = harness.click(Click::new("ui:page:*:2")).await.unwrap();
    assert_eq!(last[0].content, "page 2");
}

#[tokio::test]
async fn a_module_cannot_offer_a_component_that_would_be_delivered_to_another_module() {
    let harness = Harness::new(module()).unwrap();
    let error = harness.invoke(Invocation::new("forge")).await.unwrap_err();
    assert!(error.to_string().contains("custom_id"), "{error}");
}

#[test]
fn two_handlers_for_one_route_are_refused() {
    let twice = ModuleBuilder::new("ui", "x", "0.1.0")
        .unwrap()
        .component("a", |_| async { Ok(()) })
        .component("a", |_| async { Ok(()) });
    assert!(twice.build().is_err());
    let forms = ModuleBuilder::new("ui", "x", "0.1.0")
        .unwrap()
        .modal("a", |_| async { Ok(()) })
        .modal("a", |_| async { Ok(()) });
    assert!(forms.build().is_err());
}
