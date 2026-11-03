//! The `remind` module, run against the SDK's pretend Discord.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use closure_i18n::missing_keys;
use closure_mod_remind::builder;
use closure_sdk::Locale;
use closure_sdk::testing::{Harness, Invocation};

const EN: &str = include_str!("../locales/en.ftl");
const JA: &str = include_str!("../locales/ja.ftl");

#[tokio::test]
async fn the_list_is_localized() {
    let harness = Harness::new(builder().unwrap()).unwrap();
    let english = harness
        .invoke(
            Invocation::new("remind")
                .subcommand("list")
                .locale(Locale::En),
        )
        .await
        .unwrap();
    assert!(english[0].content.contains("no pending"));
    let japanese = harness
        .invoke(
            Invocation::new("remind")
                .subcommand("list")
                .locale(Locale::Ja),
        )
        .await
        .unwrap();
    assert!(japanese[0].content.contains("ありません"));
}

#[test]
fn both_catalogs_define_the_same_messages() {
    assert_eq!(missing_keys(EN, JA).unwrap(), Vec::<String>::new());
    assert_eq!(missing_keys(JA, EN).unwrap(), Vec::<String>::new());
}
