use std::collections::BTreeMap;
use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

/// A language the bot can speak.
///
/// Adding a language means adding a variant here, a Discord tag mapping below and a catalog
/// per module. Everything else falls back to [`Locale::DEFAULT`] when a translation is missing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum Locale {
    En,
    Ja,
}

impl Locale {
    pub const DEFAULT: Self = Self::En;
    pub const ALL: [Self; 2] = [Self::En, Self::Ja];

    /// The BCP 47 language tag used in catalogs and the config file.
    #[must_use]
    pub const fn tag(self) -> &'static str {
        match self {
            Self::En => "en",
            Self::Ja => "ja",
        }
    }

    /// Maps a Discord locale tag (`en-US`, `en-GB`, `ja`, ...) to a supported locale.
    #[must_use]
    pub fn from_discord_tag(tag: &str) -> Option<Self> {
        let language = tag.split(['-', '_']).next()?;
        Self::ALL
            .into_iter()
            .find(|locale| locale.tag().eq_ignore_ascii_case(language))
    }

    /// The tags Discord expects in `name_localizations` for this locale.
    #[must_use]
    pub const fn discord_tags(self) -> &'static [&'static str] {
        match self {
            Self::En => &["en-US", "en-GB"],
            Self::Ja => &["ja"],
        }
    }
}

impl fmt::Display for Locale {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.tag())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("unsupported locale `{0}`")]
pub struct UnsupportedLocale(String);

impl FromStr for Locale {
    type Err = UnsupportedLocale;

    fn from_str(tag: &str) -> Result<Self, Self::Err> {
        Self::from_discord_tag(tag).ok_or_else(|| UnsupportedLocale(tag.to_owned()))
    }
}

/// A value substituted into a translated message.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ArgValue {
    Text(String),
    Number(f64),
}

impl From<&str> for ArgValue {
    fn from(value: &str) -> Self {
        Self::Text(value.to_owned())
    }
}

impl From<String> for ArgValue {
    fn from(value: String) -> Self {
        Self::Text(value)
    }
}

impl From<i64> for ArgValue {
    #[allow(clippy::cast_precision_loss)]
    fn from(value: i64) -> Self {
        Self::Number(value as f64)
    }
}

impl From<u64> for ArgValue {
    #[allow(clippy::cast_precision_loss)]
    fn from(value: u64) -> Self {
        Self::Number(value as f64)
    }
}

/// Named arguments for a message, e.g. `{ user: "alice", count: 3 }`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Args(BTreeMap<String, ArgValue>);

impl Args {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    #[must_use]
    pub fn with(mut self, name: impl Into<String>, value: impl Into<ArgValue>) -> Self {
        self.0.insert(name.into(), value.into());
        self
    }

    pub fn iter(&self) -> impl Iterator<Item = (&str, &ArgValue)> {
        self.0.iter().map(|(name, value)| (name.as_str(), value))
    }
}

/// A message catalog in Fluent (`.ftl`) syntax for one locale.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Catalog {
    pub locale: Locale,
    pub source: String,
}

/// Looks up translated text.
///
/// Implementations must never fail or return an empty string: a missing translation falls
/// back to [`Locale::DEFAULT`] and finally to the key itself, so a catalog mistake degrades
/// a message instead of breaking a moderation action.
pub trait Translator: Send + Sync {
    fn translate(&self, locale: Locale, namespace: &str, key: &str, args: &Args) -> String;

    /// The translation of `key` in exactly `locale`, without any fallback. Used to tell a
    /// missing translation from a present one, e.g. when publishing localised command names.
    fn exact(&self, _locale: Locale, _namespace: &str, _key: &str, _args: &Args) -> Option<String> {
        None
    }
}

/// A translator that returns keys unchanged. Useful in tests and as a safe default.
#[derive(Debug, Clone, Copy, Default)]
pub struct KeyTranslator;

impl Translator for KeyTranslator {
    fn translate(&self, _locale: Locale, _namespace: &str, key: &str, _args: &Args) -> String {
        key.to_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn discord_tags_map_to_supported_locales() {
        assert_eq!(Locale::from_discord_tag("en-US"), Some(Locale::En));
        assert_eq!(Locale::from_discord_tag("en-GB"), Some(Locale::En));
        assert_eq!(Locale::from_discord_tag("ja"), Some(Locale::Ja));
        assert_eq!(Locale::from_discord_tag("JA"), Some(Locale::Ja));
        assert_eq!(Locale::from_discord_tag("fr"), None);
        assert_eq!(Locale::from_discord_tag(""), None);
    }

    #[test]
    fn every_locale_tag_round_trips() {
        for locale in Locale::ALL {
            assert_eq!(locale.tag().parse::<Locale>().unwrap(), locale);
            for tag in locale.discord_tags() {
                assert_eq!(Locale::from_discord_tag(tag), Some(locale));
            }
        }
    }

    #[test]
    fn serde_uses_the_tag() {
        assert_eq!(serde_json::to_string(&Locale::Ja).unwrap(), "\"ja\"");
    }

    #[test]
    fn key_translator_returns_the_key() {
        let text = KeyTranslator.translate(Locale::Ja, "core", "greeting", &Args::new());
        assert_eq!(text, "greeting");
    }
}
