//! The tags module, run against the SDK's pretend Discord.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use closure_i18n::missing_keys;
use closure_mod_tags::builder;
use closure_sdk::testing::{Autocomplete, Harness, Invocation};
use closure_sdk::{Action, CommandOptionValue, InteractionResponse, Level, PermissionKey};

const EN: &str = include_str!("../locales/en.ftl");
const JA: &str = include_str!("../locales/ja.ftl");

fn harness() -> Harness {
    Harness::new(builder().unwrap()).unwrap()
}

fn add(name: &str, text: &str) -> Invocation {
    Invocation::new("tag")
        .subcommand("add")
        .option("name", CommandOptionValue::String(name.to_owned()))
        .option("text", CommandOptionValue::String(text.to_owned()))
        .administrator()
}

fn show(name: &str) -> Invocation {
    Invocation::new("tag")
        .subcommand("show")
        .option("name", CommandOptionValue::String(name.to_owned()))
}

#[tokio::test]
async fn add_show_list_and_remove_use_the_guild_store() {
    let harness = harness();
    let saved = harness
        .invoke(add("Welcome", "Hello <@123>"))
        .await
        .unwrap();
    assert!(saved[0].content.contains("welcome"));

    let shown = harness.invoke(show("WELCOME")).await.unwrap();
    assert_eq!(shown[0].content, "Hello <@123>");

    let listed = harness
        .invoke(Invocation::new("tag").subcommand("list"))
        .await
        .unwrap();
    assert!(listed.iter().any(|reply| reply.content == "welcome"));

    let removed = harness
        .invoke(
            Invocation::new("tag")
                .subcommand("remove")
                .option("name", CommandOptionValue::String("welcome".to_owned()))
                .administrator(),
        )
        .await
        .unwrap();
    assert!(removed[0].content.contains("welcome"));
    assert!(
        harness.invoke(show("welcome")).await.unwrap()[0]
            .content
            .contains("No saved message")
    );
}

#[tokio::test]
async fn names_are_normalized_and_invalid_names_are_refused() {
    let harness = harness();
    harness.invoke(add("UPPER_case-1", "text")).await.unwrap();
    assert_eq!(
        harness
            .store()
            .get::<String>("tag:upper_case-1")
            .await
            .unwrap(),
        Some("text".to_owned())
    );

    let invalid = harness.invoke(add("bad name", "text")).await.unwrap();
    assert!(invalid[0].content.contains("Names must"));
    assert!(harness.store().get::<String>("tag:bad name").await.is_err());
}

#[tokio::test]
async fn duplicate_and_capacity_limits_do_not_overwrite_or_add() {
    let harness = harness();
    harness.invoke(add("same", "first")).await.unwrap();
    let duplicate = harness.invoke(add("SAME", "second")).await.unwrap();
    assert!(duplicate[0].content.contains("already exists"));
    assert_eq!(
        harness.store().get::<String>("tag:same").await.unwrap(),
        Some("first".to_owned())
    );

    for index in 0..99 {
        harness
            .store()
            .set(&format!("tag:seed{index}"), &"seed")
            .await
            .unwrap();
    }
    let listed = harness
        .invoke(Invocation::new("tag").subcommand("list"))
        .await
        .unwrap();
    assert_eq!(listed.len(), 2);
    let full = harness.invoke(add("extra", "text")).await.unwrap();
    assert!(full[0].content.contains("maximum of 100"));
    assert!(
        harness
            .store()
            .get::<String>("tag:extra")
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn text_length_is_bounded_and_manage_permission_is_enforced() {
    let harness = harness();
    harness.invoke(add("max", &"x".repeat(2000))).await.unwrap();
    assert_eq!(
        harness
            .store()
            .get::<String>("tag:max")
            .await
            .unwrap()
            .unwrap()
            .len(),
        2000
    );
    let empty = harness.invoke(add("empty", "")).await.unwrap();
    assert!(!empty.is_empty());
    assert!(
        harness
            .store()
            .get::<String>("tag:empty")
            .await
            .unwrap()
            .is_none()
    );
    let too_long = harness
        .invoke(add("long", &"x".repeat(2001)))
        .await
        .unwrap();
    assert!(too_long[0].content.contains("1 to 2,000"));
    assert!(
        harness
            .store()
            .get::<String>("tag:long")
            .await
            .unwrap()
            .is_none()
    );

    let denied = harness
        .invoke(
            Invocation::new("tag")
                .subcommand("add")
                .option("name", CommandOptionValue::String("denied".to_owned()))
                .option("text", CommandOptionValue::String("text".to_owned())),
        )
        .await
        .unwrap();
    assert!(!denied.is_empty());
    assert!(
        harness
            .store()
            .get::<String>("tag:denied")
            .await
            .unwrap()
            .is_none()
    );

    let allowed = harness.invoke(add("allowed", "text")).await.unwrap();
    assert!(allowed[0].content.contains("allowed"));
}

#[tokio::test]
async fn show_posts_mentions_as_text_and_name_autocompletes() {
    let harness = harness();
    harness
        .invoke(add("welcome", "@everyone <@123>"))
        .await
        .unwrap();
    let shown = harness.invoke(show("welcome")).await.unwrap();
    assert_eq!(shown[0].content, "@everyone <@123>");

    let public = harness.actions().into_iter().any(|action| match action {
        Action::Respond {
            response: InteractionResponse::Message { reply },
            ..
        } => {
            !reply.ephemeral
                && reply
                    .embeds
                    .iter()
                    .any(|embed| embed.description.as_deref() == Some("@everyone <@123>"))
        }
        _ => false,
    });
    assert!(public);

    let prefixed = harness
        .invoke(show("welcome").text_command())
        .await
        .unwrap();
    assert_eq!(prefixed[0].content, "@everyone <@123>");

    let choices = harness
        .autocomplete(Autocomplete::new("tag", "name", "wel").subcommand("show"))
        .await
        .unwrap();
    assert!(choices.iter().any(|choice| choice.name == "welcome"));
    assert!(!choices.iter().any(|choice| choice.name == "other"));
}

#[tokio::test]
async fn use_and_manage_permissions_allow_and_deny() {
    let harness = harness();
    harness.invoke(add("visible", "body")).await.unwrap();
    assert_eq!(
        harness.invoke(show("visible")).await.unwrap()[0].content,
        "body"
    );

    let denied_manage = harness
        .invoke(
            Invocation::new("tag")
                .subcommand("add")
                .option("name", CommandOptionValue::String("denied".to_owned()))
                .option("text", CommandOptionValue::String("text".to_owned())),
        )
        .await
        .unwrap();
    assert!(!denied_manage.is_empty());
    assert!(
        harness
            .store()
            .get::<String>("tag:denied")
            .await
            .unwrap()
            .is_none()
    );
    harness.invoke(add("kept", "text")).await.unwrap();
    let denied_remove = harness
        .invoke(
            Invocation::new("tag")
                .subcommand("remove")
                .option("name", CommandOptionValue::String("kept".to_owned())),
        )
        .await
        .unwrap();
    assert!(!denied_remove.is_empty());
    assert!(
        harness
            .store()
            .get::<String>("tag:kept")
            .await
            .unwrap()
            .is_some()
    );

    harness.permissions(|permissions| {
        permissions
            .defaults
            .insert(PermissionKey::new("tags.use").unwrap(), Level::MODERATOR);
    });
    let denied_use = harness.invoke(show("visible")).await.unwrap();
    assert_ne!(denied_use[0].content, "body");
}

#[test]
fn both_catalogs_define_the_same_messages() {
    assert_eq!(missing_keys(EN, JA).unwrap(), Vec::<String>::new());
    assert_eq!(missing_keys(JA, EN).unwrap(), Vec::<String>::new());
}
