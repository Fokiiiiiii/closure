//! Autocomplete: the choices an option offers while the user types, and what a refusal gives.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use closure_core::event::CommandOptionValue;
use closure_sdk::testing::{Autocomplete, Harness};
use closure_sdk::{
    AutocompleteCx, Choice, Command, CommandArgs, Level, ModuleBuilder, PermissionSpec, UserError,
};
use twilight_model::id::Id;

#[derive(CommandArgs)]
struct Find {
    /// What to look for
    #[arg(autocomplete)]
    query: String,
    /// How many to show
    #[arg(autocomplete, min = 1, max = 9)]
    #[allow(dead_code)]
    level: Option<i64>,
}

#[derive(CommandArgs)]
struct Flag {
    /// On or off
    #[allow(dead_code)]
    on: bool,
}

fn module() -> ModuleBuilder {
    ModuleBuilder::new("find", "Find", "0.1.0")
        .unwrap()
        .permission(PermissionSpec::new("find.use", Level::MODERATOR, "Find members").unwrap())
        .command(
            Command::<Find>::new("find")
                .requires("find.use")
                .handler(|cx, args| async move { cx.reply(args.query).await }),
        )
        .autocomplete("find", "query", |cx: AutocompleteCx| async move {
            let typed = cx.focused().to_owned();
            match typed.as_str() {
                "boom" => return Err(UserError::message("unavailable")),
                "many" => {
                    return Ok((0..26_i64).map(|n| Choice::new(n.to_string(), n)).collect());
                }
                "long" => return Ok(vec![Choice::new("x".repeat(101), "x")]),
                _ => {}
            }
            // The other options the user already filled in are there for the choices.
            let level = match cx.option("level") {
                Some(CommandOptionValue::Integer(level)) => *level,
                _ => 0,
            };
            Ok(["alpha", "alps", "beta"]
                .into_iter()
                .filter(|name| name.starts_with(typed.as_str()))
                .map(|name| Choice::new(format!("{name} {level}"), name))
                .collect())
        })
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
async fn choices_follow_the_typed_text_and_the_options_already_filled() {
    let harness = moderator_harness();
    let query = Autocomplete::new("find", "query", "al")
        .roles([10])
        .option("level", CommandOptionValue::Integer(3));
    let choices = harness.autocomplete(query).await.unwrap();
    assert_eq!(
        choices,
        [
            Choice::new("alpha 3", "alpha"),
            Choice::new("alps 3", "alps")
        ]
    );
}

#[tokio::test]
async fn a_user_who_may_not_run_the_command_gets_no_choices() {
    let harness = moderator_harness();
    let choices = harness
        .autocomplete(Autocomplete::new("find", "query", "al"))
        .await
        .unwrap();
    assert!(choices.is_empty());
}

#[tokio::test]
async fn a_failing_handler_or_an_answer_discord_would_refuse_gives_no_choices() {
    let harness = moderator_harness();
    for typed in ["boom", "many", "long"] {
        let query = Autocomplete::new("find", "query", typed).roles([10]);
        let choices = harness.autocomplete(query).await.unwrap();
        assert!(choices.is_empty(), "{typed}");
    }
}

#[tokio::test]
async fn an_option_without_a_handler_gets_no_choices() {
    let harness = moderator_harness();
    let query = Autocomplete::new("find", "level", "1").roles([10]);
    assert!(harness.autocomplete(query).await.unwrap().is_empty());
}

#[test]
fn the_derive_publishes_the_autocomplete_flag() {
    let options = Find::options();
    assert!(options[0].autocomplete);
    assert!(options[1].autocomplete);
}

#[test]
fn autocomplete_is_refused_for_an_option_that_is_not_text_or_a_number_or_for_no_option() {
    let on_a_flag = ModuleBuilder::new("flag", "Flag", "0.1.0")
        .unwrap()
        .command(Command::<Flag>::new("flag").handler(|_, _| async move { Ok(()) }))
        .autocomplete(
            "flag",
            "on",
            |_: AutocompleteCx| async move { Ok(Vec::new()) },
        );
    assert!(on_a_flag.build().is_err(), "a boolean option");
    let unknown =
        module().autocomplete(
            "find",
            "nope",
            |_: AutocompleteCx| async move { Ok(Vec::new()) },
        );
    assert!(
        unknown.build().is_err(),
        "an option the command does not have"
    );
}
