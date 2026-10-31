//! Fluent-backed translations.
//!
//! Every module owns a catalog per [`Locale`] (declared in its manifest). Lookups fall back
//! from the requested locale to [`Locale::DEFAULT`], then to the shared `core` catalog and
//! finally to the key itself, so a missing translation can never break a response.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::fmt;
use std::sync::Mutex;

use closure_core::error::DiscordError;
use closure_core::locale::{ArgValue, Args, Locale, Translator};
use closure_core::registry::ModuleRegistry;
use fluent_bundle::concurrent::FluentBundle;
use fluent_bundle::{FluentArgs, FluentResource};
use fluent_syntax::ast::Entry;
use unic_langid::LanguageIdentifier;

/// Namespace of the catalogs shared by every module.
pub const CORE_NAMESPACE: &str = "core";

pub const CORE_EN: &str = include_str!("../locales/en/core.ftl");
pub const CORE_JA: &str = include_str!("../locales/ja/core.ftl");

#[derive(Debug, thiserror::Error)]
pub enum I18nError {
    #[error("catalog `{namespace}` ({locale}) has {count} syntax error(s)")]
    Syntax {
        namespace: String,
        locale: Locale,
        count: usize,
    },
    #[error("catalog `{namespace}` ({locale}) defines a message that already exists")]
    Duplicate { namespace: String, locale: Locale },
    #[error("invalid language tag `{0}`")]
    LanguageTag(String),
    #[error("fluent source has {0} syntax error(s)")]
    InvalidSource(usize),
}

type Bundle = FluentBundle<FluentResource>;
type Bundles = HashMap<Locale, HashMap<String, Bundle>>;

/// Collects catalogs and validates them up front: a malformed catalog fails startup instead of
/// surfacing as a missing message in production.
pub struct TranslatorBuilder {
    bundles: Bundles,
}

impl fmt::Debug for TranslatorBuilder {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TranslatorBuilder").finish_non_exhaustive()
    }
}

impl TranslatorBuilder {
    /// Starts with the shared `core` catalogs already loaded.
    pub fn new() -> Result<Self, I18nError> {
        let mut builder = Self {
            bundles: HashMap::new(),
        };
        builder.add_catalog(CORE_NAMESPACE, Locale::En, CORE_EN)?;
        builder.add_catalog(CORE_NAMESPACE, Locale::Ja, CORE_JA)?;
        Ok(builder)
    }

    pub fn add_catalog(
        &mut self,
        namespace: &str,
        locale: Locale,
        source: &str,
    ) -> Result<&mut Self, I18nError> {
        let resource = FluentResource::try_new(source.to_owned()).map_err(|(_, errors)| {
            I18nError::Syntax {
                namespace: namespace.to_owned(),
                locale,
                count: errors.len(),
            }
        })?;
        let language: LanguageIdentifier = locale
            .tag()
            .parse()
            .map_err(|_| I18nError::LanguageTag(locale.tag().to_owned()))?;

        let bundle = self
            .bundles
            .entry(locale)
            .or_default()
            .entry(namespace.to_owned())
            .or_insert_with(|| {
                let mut bundle = FluentBundle::new_concurrent(vec![language]);
                // Isolation marks would show up as stray characters in Discord messages.
                bundle.set_use_isolating(false);
                bundle
            });
        bundle
            .add_resource(resource)
            .map_err(|_| I18nError::Duplicate {
                namespace: namespace.to_owned(),
                locale,
            })?;
        Ok(self)
    }

    /// Adds the catalogs declared in every module manifest, namespaced by module id.
    pub fn add_registry(&mut self, registry: &ModuleRegistry) -> Result<&mut Self, I18nError> {
        for module in registry.iter() {
            let manifest = module.manifest();
            for catalog in &manifest.catalogs {
                self.add_catalog(manifest.id.as_str(), catalog.locale, &catalog.source)?;
            }
        }
        Ok(self)
    }

    #[must_use]
    pub fn build(self) -> FluentTranslator {
        FluentTranslator {
            bundles: self.bundles,
            reported: Mutex::new(HashSet::new()),
        }
    }
}

pub struct FluentTranslator {
    bundles: Bundles,
    /// Missing keys already logged, so a bad key warns once instead of on every message.
    reported: Mutex<HashSet<String>>,
}

impl fmt::Debug for FluentTranslator {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FluentTranslator").finish_non_exhaustive()
    }
}

impl FluentTranslator {
    fn lookup(&self, locale: Locale, namespace: &str, key: &str, args: &Args) -> Option<String> {
        let bundle = self.bundles.get(&locale)?.get(namespace)?;
        let (id, attribute) = match key.split_once('.') {
            Some((id, attribute)) => (id, Some(attribute)),
            None => (key, None),
        };
        let message = bundle.get_message(id)?;
        let pattern = match attribute {
            Some(name) => message.get_attribute(name)?.value(),
            None => message.value()?,
        };

        let mut fluent_args = FluentArgs::new();
        for (name, value) in args.iter() {
            match value {
                ArgValue::Text(text) => fluent_args.set(name, text.as_str()),
                ArgValue::Number(number) => fluent_args.set(name, *number),
            }
        }
        let mut errors = Vec::new();
        let text = bundle.format_pattern(pattern, Some(&fluent_args), &mut errors);
        if !errors.is_empty() {
            tracing::warn!(namespace, key, %locale, ?errors, "translation formatting errors");
        }
        Some(text.into_owned())
    }

    fn report_missing(&self, locale: Locale, namespace: &str, key: &str) {
        let first_time = self
            .reported
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(format!("{namespace}:{key}"));
        if first_time {
            tracing::warn!(namespace, key, %locale, "missing translation");
        }
    }
}

impl Translator for FluentTranslator {
    fn exact(&self, locale: Locale, namespace: &str, key: &str, args: &Args) -> Option<String> {
        self.lookup(locale, namespace, key, args)
            .filter(|text| !text.is_empty())
    }

    fn translate(&self, locale: Locale, namespace: &str, key: &str, args: &Args) -> String {
        let candidates = [
            (locale, namespace),
            (Locale::DEFAULT, namespace),
            (locale, CORE_NAMESPACE),
            (Locale::DEFAULT, CORE_NAMESPACE),
        ];
        for (candidate_locale, candidate_namespace) in candidates {
            if let Some(text) = self.lookup(candidate_locale, candidate_namespace, key, args)
                && !text.is_empty()
            {
                return text;
            }
        }
        self.report_missing(locale, namespace, key);
        key.to_owned()
    }
}

/// The catalog key describing `error` to an end user.
#[must_use]
pub fn error_key(error: &DiscordError) -> &'static str {
    match error {
        DiscordError::Invalid(_) => "error-invalid",
        DiscordError::CapabilityDenied(_) => "error-capability-denied",
        DiscordError::Forbidden(_) => "error-forbidden",
        DiscordError::HierarchyViolation(_) => "error-hierarchy",
        DiscordError::NotFound => "error-not-found",
        DiscordError::DirectMessagesClosed => "error-dm-closed",
        DiscordError::NotInVoice => "error-not-in-voice",
        DiscordError::RateLimited { .. } => "error-rate-limited",
        DiscordError::Transient(_) => "error-transient",
        DiscordError::Fatal(_) => "error-fatal",
    }
}

/// A message safe to show to a Discord user: localised, and free of internal details such as
/// HTTP bodies.
#[must_use]
pub fn user_message(translator: &dyn Translator, locale: Locale, error: &DiscordError) -> String {
    let args = match error {
        DiscordError::Invalid(detail) => Args::new().with("detail", detail.as_str()),
        _ => Args::new(),
    };
    translator.translate(locale, CORE_NAMESPACE, error_key(error), &args)
}

/// The message ids (`id`) and attribute ids (`id.attribute`) a catalog defines.
pub fn message_keys(source: &str) -> Result<BTreeSet<String>, I18nError> {
    let resource = fluent_syntax::parser::parse(source)
        .map_err(|(_, errors)| I18nError::InvalidSource(errors.len()))?;
    let mut keys = BTreeSet::new();
    for entry in &resource.body {
        if let Entry::Message(message) = entry {
            keys.insert(message.id.name.to_owned());
            for attribute in &message.attributes {
                keys.insert(format!("{}.{}", message.id.name, attribute.id.name));
            }
        }
    }
    Ok(keys)
}

/// Keys present in `reference` but absent from `other`. Modules use this in a test to prove
/// every locale covers the same messages.
pub fn missing_keys(reference: &str, other: &str) -> Result<Vec<String>, I18nError> {
    let other = message_keys(other)?;
    Ok(message_keys(reference)?
        .into_iter()
        .filter(|key| !other.contains(key))
        .collect())
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    fn translator(catalogs: &[(&str, Locale, &str)]) -> FluentTranslator {
        let mut builder = TranslatorBuilder::new().unwrap();
        for (namespace, locale, source) in catalogs {
            builder.add_catalog(namespace, *locale, source).unwrap();
        }
        builder.build()
    }

    #[test]
    fn translates_with_arguments_in_each_language() {
        let t = translator(&[
            (
                "warn",
                Locale::En,
                "warned = { $user } was warned { $count } times.",
            ),
            (
                "warn",
                Locale::Ja,
                "warned = { $user } さんは { $count } 回警告されました。",
            ),
        ]);
        let args = Args::new().with("user", "alice").with("count", 3_i64);
        assert_eq!(
            t.translate(Locale::En, "warn", "warned", &args),
            "alice was warned 3 times."
        );
        assert_eq!(
            t.translate(Locale::Ja, "warn", "warned", &args),
            "alice さんは 3 回警告されました。"
        );
    }

    #[test]
    fn output_has_no_bidi_isolation_marks() {
        let t = translator(&[("m", Locale::En, "hi = Hello { $name }")]);
        let text = t.translate(Locale::En, "m", "hi", &Args::new().with("name", "bob"));
        assert_eq!(text, "Hello bob");
        assert!(!text.contains(['\u{2068}', '\u{2069}']));
    }

    #[test]
    fn missing_locale_falls_back_to_english() {
        let t = translator(&[("m", Locale::En, "only-en = English only")]);
        assert_eq!(
            t.translate(Locale::Ja, "m", "only-en", &Args::new()),
            "English only"
        );
    }

    #[test]
    fn module_namespace_falls_back_to_core_then_to_the_key() {
        let t = translator(&[]);
        let core = t.translate(Locale::Ja, "some-module", "error-not-found", &Args::new());
        assert!(core.contains("見つかりません"));
        assert_eq!(
            t.translate(Locale::En, "some-module", "no-such-key", &Args::new()),
            "no-such-key"
        );
    }

    #[test]
    fn exact_lookups_never_fall_back() {
        let t = translator(&[("m", Locale::En, "only-en = English only")]);
        assert_eq!(
            t.exact(Locale::En, "m", "only-en", &Args::new()).as_deref(),
            Some("English only")
        );
        assert_eq!(t.exact(Locale::Ja, "m", "only-en", &Args::new()), None);
        assert_eq!(t.exact(Locale::En, "other", "only-en", &Args::new()), None);
    }

    #[test]
    fn attributes_are_addressed_with_a_dot() {
        let t = translator(&[(
            "ping",
            Locale::Ja,
            "command-ping = ping\n    .description = 動作確認をします",
        )]);
        assert_eq!(
            t.translate(Locale::Ja, "ping", "command-ping.description", &Args::new()),
            "動作確認をします"
        );
    }

    #[test]
    fn malformed_and_duplicate_catalogs_fail_at_build_time() {
        let mut builder = TranslatorBuilder::new().unwrap();
        assert!(matches!(
            builder.add_catalog("m", Locale::En, "= broken"),
            Err(I18nError::Syntax { .. })
        ));
        builder.add_catalog("m", Locale::En, "a = 1").unwrap();
        assert!(matches!(
            builder.add_catalog("m", Locale::En, "a = 2"),
            Err(I18nError::Duplicate { .. })
        ));
    }

    #[test]
    fn core_catalogs_cover_the_same_messages_in_every_locale() {
        assert_eq!(
            missing_keys(CORE_EN, CORE_JA).unwrap(),
            Vec::<String>::new()
        );
        assert_eq!(
            missing_keys(CORE_JA, CORE_EN).unwrap(),
            Vec::<String>::new()
        );
    }

    #[test]
    fn every_locale_has_a_core_catalog() {
        let t = translator(&[]);
        for locale in Locale::ALL {
            let text = t.translate(locale, CORE_NAMESPACE, "error-fatal", &Args::new());
            assert_ne!(text, "error-fatal", "{locale} has no core catalog");
        }
    }

    #[test]
    fn every_discord_error_has_a_localised_user_message() {
        let t = translator(&[]);
        let errors = [
            DiscordError::Invalid("x".into()),
            DiscordError::CapabilityDenied(closure_core::capability::Capability::BanMembers),
            DiscordError::Forbidden("secret detail".into()),
            DiscordError::HierarchyViolation("secret detail".into()),
            DiscordError::NotFound,
            DiscordError::DirectMessagesClosed,
            DiscordError::NotInVoice,
            DiscordError::RateLimited {
                retry_after: Some(Duration::from_secs(1)),
            },
            DiscordError::Transient("secret detail".into()),
            DiscordError::Fatal("secret detail".into()),
        ];
        for error in &errors {
            for locale in Locale::ALL {
                let text = user_message(&t, locale, error);
                assert_ne!(text, error_key(error), "{error:?} / {locale}");
                assert!(
                    !text.contains("secret detail"),
                    "internal detail leaked: {text}"
                );
            }
        }
    }

    #[test]
    fn invalid_actions_show_their_detail() {
        let t = translator(&[]);
        let text = user_message(&t, Locale::En, &DiscordError::Invalid("too long".into()));
        assert!(text.contains("too long"));
    }
}
