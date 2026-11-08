//! The rules the dashboard offers ready-made (`closure-dashboard`, `src/templates.rs`) stay rules
//! this module accepts, and do what their names say.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "test code: a broken setup should panic at once"
)]

mod common;

use std::sync::Arc;

use closure_config::{ConfigCompiler, parse_overrides};
use closure_core::module::Module;
use closure_core::registry::ModuleRegistry;
use common::*;
use serde_json::{Value, json};

fn timeout(duration: &str) -> Value {
    json!([{"do": "delete"}, {"do": "timeout", "duration": duration}])
}

fn templates() -> Vec<(&'static str, Value)> {
    let rule = |name: &str, triggers: Value, actions: Value, enabled: bool| json!({"name": name, "enabled": enabled, "triggers": triggers, "actions": actions});
    vec![
        (
            "invites",
            rule("t", json!([{"type": "invites"}]), timeout("10m"), true),
        ),
        (
            "mention_spam",
            rule(
                "t",
                json!([{"type": "spam", "measure": "mentions", "amount": 8, "within": "30s"}]),
                timeout("1h"),
                true,
            ),
        ),
        (
            "duplicates",
            rule(
                "t",
                json!([{"type": "spam", "measure": "duplicates", "amount": 4, "within": "30s"}]),
                timeout("10m"),
                true,
            ),
        ),
        (
            "flood",
            rule(
                "t",
                json!([{"type": "spam", "measure": "messages", "amount": 7, "within": "10s"}]),
                timeout("10m"),
                true,
            ),
        ),
        (
            "files",
            rule(
                "t",
                json!([{"type": "attachments",
                    "blocked_extensions": ["exe", "scr", "bat", "cmd", "msi", "jar", "vbs", "ps1"]}]),
                json!([{"do": "delete"}]),
                true,
            ),
        ),
        (
            "caps",
            rule(
                "t",
                json!([{"type": "caps", "max_ratio": 0.8, "min_letters": 12}]),
                json!([{"do": "delete"}]),
                true,
            ),
        ),
        (
            "words",
            rule(
                "t",
                json!([{"type": "words", "words": ["example"], "whole_words": false,
                    "normalize": true, "loose_matching": true}]),
                json!([{"do": "delete"}]),
                false,
            ),
        ),
    ]
}

#[test]
fn every_template_is_accepted_by_the_schema() {
    let registry = Arc::new(
        ModuleRegistry::new(vec![
            Arc::new(closure_mod_automod::module().unwrap()) as Arc<dyn Module>
        ])
        .unwrap(),
    );
    let compiler = ConfigCompiler::new(registry).unwrap();
    for (key, rule) in templates() {
        let config = json!({"rules": {key: rule}});
        let yaml = format!(
            "modules:\n  automod:\n    config: {}\n",
            serde_json::to_string(&config).unwrap()
        );
        assert!(
            compiler.compile(&parse_overrides(&yaml).unwrap()).is_ok(),
            "{key}"
        );
    }
}

#[tokio::test]
async fn the_templates_do_what_their_names_say() {
    let by_key = |key: &str| {
        templates()
            .into_iter()
            .find(|(name, _)| *name == key)
            .unwrap()
            .1
    };
    let trips = |key: &'static str, content: &'static str| async move {
        let harness = with_rules(rules(&[(key, by_key(key))]));
        say(&harness, MEMBER, content).await;
        deletions(&harness) > 0
    };
    assert!(trips("invites", "join discord.gg/abc123").await);
    assert!(!trips("invites", "hello").await);
    assert!(trips("caps", "THIS IS ALL CAPITALS").await);
    assert!(!trips("caps", "this is fine").await);
    assert!(!trips("words", "e.x.a.m.p.l.e").await);
    let mut on = by_key("words");
    on["enabled"] = json!(true);
    let harness = with_rules(rules(&[("words", on)]));
    say(&harness, MEMBER, "e.x.a.m.p.l.e").await;
    assert_eq!(deletions(&harness), 1);
}
