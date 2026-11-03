//! The welcome module, run against the SDK test Discord.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use closure_core::module::Module;
use closure_i18n::missing_keys;
use closure_mod_welcome::builder;

const EN: &str = include_str!("../locales/en.ftl");
const JA: &str = include_str!("../locales/ja.ftl");

#[test]
fn both_catalogs_define_the_same_messages() {
    assert_eq!(missing_keys(EN, JA).unwrap(), Vec::<String>::new());
    assert_eq!(missing_keys(JA, EN).unwrap(), Vec::<String>::new());
}

#[test]
fn the_module_has_no_public_commands() {
    let module = builder().unwrap().build().unwrap();
    assert!(module.manifest().commands.is_empty());
}
