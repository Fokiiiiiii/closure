#![allow(clippy::unwrap_used)]

use std::sync::Arc;

use closure_config::{View, document_schema_in};
use closure_core::{locale::Locale, module::Module, registry::ModuleRegistry};
use serde_json::Value;

fn registry() -> ModuleRegistry {
    let modules: Vec<Arc<dyn Module>> = vec![
        Arc::new(closure_mod_automod::module().unwrap()),
        Arc::new(closure_mod_channels::module().unwrap()),
        Arc::new(closure_mod_dashboard::module().unwrap()),
        Arc::new(closure_mod_diagnostics::module().unwrap()),
        Arc::new(closure_mod_logs::module().unwrap()),
        Arc::new(closure_mod_moderation::module().unwrap()),
        Arc::new(closure_mod_remind::module().unwrap()),
        Arc::new(closure_mod_tags::module().unwrap()),
        Arc::new(closure_mod_welcome::module().unwrap()),
    ];
    ModuleRegistry::new(modules).unwrap()
}

fn japanese(text: &str) -> bool {
    text.chars().any(|c| matches!(c, '\u{3040}'..='\u{30ff}' | '\u{3400}'..='\u{9fff}' | '\u{f900}'..='\u{faff}'))
}

fn bare_key(text: &str) -> bool {
    let mut parts = text.split('-');
    let first = parts.next().unwrap_or_default();
    let rest: Vec<_> = parts.collect();
    !first.is_empty()
        && first
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
        && !rest.is_empty()
        && rest.iter().all(|part| {
            !part.is_empty()
                && part
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
        })
}

fn inspect(value: &Value, path: &str, locale: Locale, failures: &mut Vec<String>) {
    match value {
        Value::Object(map) => {
            if let Some(Value::String(text)) = map.get("description") {
                let empty_module = text.is_empty()
                    && path
                        .strip_prefix("/properties/modules/anyOf/0/properties/")
                        .is_some_and(|id| !id.contains('/'));
                if !empty_module
                    && (text.is_empty()
                        || bare_key(text)
                        || japanese(text) != (locale == Locale::Ja))
                {
                    failures.push(format!("{path}: {text:?}"));
                }
            }
            for (key, child) in map {
                inspect(child, &format!("{path}/{key}"), locale, failures);
            }
        }
        Value::Array(items) => {
            for (index, child) in items.iter().enumerate() {
                inspect(child, &format!("{path}/{index}"), locale, failures);
            }
        }
        _ => {}
    }
}

#[test]
fn basic_descriptions_are_localized() {
    let registry = registry();
    let mut failures = Vec::new();
    for locale in [Locale::Ja, Locale::En] {
        let schema = document_schema_in(&registry, View::Basic, locale).unwrap();
        let mut locale_failures = Vec::new();
        inspect(&schema, "", locale, &mut locale_failures);
        failures.extend(
            locale_failures
                .into_iter()
                .map(|failure| format!("{locale}: {failure}")),
        );
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

#[test]
fn basic_schema_exposes_expected_translations() {
    let registry = registry();
    let ja = document_schema_in(&registry, View::Basic, Locale::Ja).unwrap();
    let en = document_schema_in(&registry, View::Basic, Locale::En).unwrap();
    for (path, japanese_text, english_text) in [
        (
            "/$defs/Preset/description",
            "サーバー規模に合う初期設定。各項目は個別に変更できます。",
            "Server-sized defaults; each setting can still be overridden.",
        ),
        (
            "/properties/log_channel/description",
            "モデレーションログを投稿するチャンネル。",
            "Channel where the bot posts moderation logs.",
        ),
        (
            "/properties/log_webhook/description",
            "ログに使う名前付き webhook。",
            "Named webhook for logs.",
        ),
        (
            "/properties/moderator_roles/description",
            "モデレーションコマンドを使えるロール。",
            "Roles whose members may use moderation commands.",
        ),
        (
            "/properties/modules/anyOf/0/properties/diagnostics/description",
            "動作確認とバージョンのコマンドです。",
            "Liveness and version commands.",
        ),
        (
            "/properties/modules/anyOf/0/properties/welcome/properties/config/anyOf/0/properties/channel/anyOf/0/description",
            "新しいメンバーに挨拶するチャンネル。",
            "Channel where new members are greeted.",
        ),
        (
            "/properties/modules/anyOf/0/properties/welcome/properties/config/anyOf/0/properties/include_bots/anyOf/0/description",
            "Bot アカウントにも挨拶するかどうか。",
            "Whether to greet bot accounts too.",
        ),
    ] {
        assert_eq!(
            ja.pointer(path).and_then(Value::as_str),
            Some(japanese_text),
            "{path}"
        );
        assert_eq!(
            en.pointer(path).and_then(Value::as_str),
            Some(english_text),
            "{path}"
        );
    }
    assert_eq!(
        ja.pointer("/properties/modules/anyOf/0/properties/dashboard/description")
            .and_then(Value::as_str),
        Some("")
    );
    jsonschema::validator_for(&ja).unwrap();
}
