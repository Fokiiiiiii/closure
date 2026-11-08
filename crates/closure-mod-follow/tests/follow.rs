//! The `follow` module, run against the SDK's pretend Discord.

use closure_i18n::missing_keys;

const EN: &str = include_str!("../locales/en.ftl");
const JA: &str = include_str!("../locales/ja.ftl");

#[test]
fn both_catalogs_define_the_same_messages() {
    assert_eq!(missing_keys(EN, JA).unwrap(), Vec::<String>::new());
    assert_eq!(missing_keys(JA, EN).unwrap(), Vec::<String>::new());
}
