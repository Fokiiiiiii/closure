//! The operator's settings file: per-plugin sections, and the secrets they use, named by the
//! environment variable that holds them. The file never carries a secret value.

use std::collections::BTreeMap;
use std::fmt;
use std::path::Path;

use secrecy::SecretString;
use serde::Deserialize;
use serde::de::DeserializeOwned;

/// What the operator writes in `closure.yaml`, after the file is read and checked.
#[derive(Default)]
pub struct OperatorSettings {
    plugins: BTreeMap<String, serde_json::Value>,
    secrets: BTreeMap<String, SecretString>,
}

#[derive(Debug, thiserror::Error)]
pub enum SettingsError {
    #[error("could not read the settings file {path}")]
    Read { path: String },
    #[error("the settings file is not valid: {message}")]
    Syntax {
        message: String,
        line: Option<u64>,
        column: Option<u64>,
    },
    #[error("the settings of plugin `{name}` do not fit its type: {message}")]
    Plugin { name: String, message: String },
}

/// The file as written: unknown keys are refused so a misspelt section is not silently ignored.
#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields, default)]
struct Document {
    plugins: BTreeMap<String, serde_json::Value>,
    secrets: BTreeMap<String, String>,
}

/// An empty document means no settings; a `null` document does too.
fn parse(text: &str) -> Result<Document, SettingsError> {
    if text.trim().is_empty() {
        return Ok(Document::default());
    }
    let document: Option<Document> = serde_saphyr::from_str(text).map_err(|error| {
        let location = error.location();
        SettingsError::Syntax {
            message: error.to_string(),
            line: location.map(|location| location.line()),
            column: location.map(|location| location.column()),
        }
    })?;
    Ok(document.unwrap_or_default())
}

impl OperatorSettings {
    /// Reads the file; a missing file means empty settings.
    pub fn load(path: &Path) -> Result<Self, SettingsError> {
        match std::fs::read_to_string(path) {
            Ok(text) => Self::from_text(&text, |name| std::env::var(name).ok()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(_) => Err(SettingsError::Read {
                path: path.display().to_string(),
            }),
        }
    }

    /// Reads the text of the file; `env` looks up the variables the secrets name.
    fn from_text(text: &str, env: impl Fn(&str) -> Option<String>) -> Result<Self, SettingsError> {
        let document = parse(text)?;
        let secrets = document
            .secrets
            .into_iter()
            .filter_map(|(name, variable)| {
                let value = env(&variable).filter(|value| !value.is_empty());
                if value.is_none() {
                    tracing::warn!(secret = %name, variable = %variable, "the environment variable of a secret is not set");
                }
                value.map(|value| (name, SecretString::from(value)))
            })
            .collect();
        Ok(Self {
            plugins: document.plugins,
            secrets,
        })
    }

    /// The section of one plugin, or `None` when the file has none.
    pub fn plugin_section<T: DeserializeOwned>(
        &self,
        name: &str,
    ) -> Result<Option<T>, SettingsError> {
        match self.plugins.get(name) {
            None | Some(serde_json::Value::Null) => Ok(None),
            Some(value) => serde_json::from_value(value.clone())
                .map(Some)
                .map_err(|error| SettingsError::Plugin {
                    name: name.to_owned(),
                    message: error.to_string(),
                }),
        }
    }

    /// The secret `name`, when its environment variable is set and not empty.
    #[must_use]
    pub fn secret(&self, name: &str) -> Option<&SecretString> {
        self.secrets.get(name)
    }
}

impl fmt::Debug for OperatorSettings {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OperatorSettings")
            .field("plugins", &self.plugins.keys().collect::<Vec<_>>())
            .field("secrets", &self.secrets.keys().collect::<Vec<_>>())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use secrecy::ExposeSecret;
    use serde::Deserialize;

    use super::*;

    #[derive(Debug, Deserialize, PartialEq)]
    #[serde(deny_unknown_fields)]
    struct Warn {
        limit: u32,
    }

    fn from_text(
        text: &str,
        variables: &[(&str, &str)],
    ) -> Result<OperatorSettings, SettingsError> {
        let variables: BTreeMap<String, String> = variables
            .iter()
            .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
            .collect();
        OperatorSettings::from_text(text, |name| variables.get(name).cloned())
    }

    #[test]
    fn a_plugin_section_is_read_into_its_type() {
        let settings = from_text("plugins:\n  warn:\n    limit: 5\n", &[]).unwrap();
        assert_eq!(
            settings.plugin_section::<Warn>("warn").unwrap(),
            Some(Warn { limit: 5 })
        );
        assert_eq!(settings.plugin_section::<Warn>("absent").unwrap(), None);
    }

    #[test]
    fn a_section_that_does_not_fit_its_type_is_refused_with_the_plugin_name() {
        let settings = from_text("plugins:\n  warn:\n    limit: lots\n", &[]).unwrap();
        let error = settings.plugin_section::<Warn>("warn").unwrap_err();
        assert!(
            matches!(&error, SettingsError::Plugin { name, .. } if name == "warn"),
            "{error}"
        );
    }

    #[test]
    fn an_unknown_top_level_key_is_refused_with_its_line() {
        let error = from_text("plugins: {}\npluginz: {}\n", &[]).unwrap_err();
        let SettingsError::Syntax { line, message, .. } = &error else {
            panic!("expected a syntax error, got {error}");
        };
        assert_eq!(*line, Some(2));
        assert!(message.contains("pluginz"), "{message}");
    }

    #[test]
    fn broken_yaml_is_a_syntax_error_not_a_panic() {
        assert!(matches!(
            from_text("plugins: [warn", &[]),
            Err(SettingsError::Syntax { .. })
        ));
    }

    #[test]
    fn an_empty_file_means_empty_settings() {
        for text in ["", "  \n\n"] {
            let settings = from_text(text, &[]).unwrap();
            assert_eq!(settings.plugin_section::<Warn>("warn").unwrap(), None);
            assert!(settings.secret("gemini").is_none());
        }
    }

    #[test]
    fn a_secret_is_resolved_from_the_variable_it_names() {
        let settings = from_text(
            "secrets:\n  gemini: GEMINI_API_KEY\n",
            &[("GEMINI_API_KEY", "key-value")],
        )
        .unwrap();
        assert_eq!(
            settings.secret("gemini").unwrap().expose_secret(),
            "key-value"
        );
    }

    #[test]
    fn a_secret_whose_variable_is_unset_or_empty_is_absent_and_not_an_error() {
        let settings = from_text(
            "secrets:\n  gemini: GEMINI_API_KEY\n  twitch: TWITCH_SECRET\n",
            &[("TWITCH_SECRET", "")],
        )
        .unwrap();
        assert!(settings.secret("gemini").is_none());
        assert!(settings.secret("twitch").is_none());
    }

    #[test]
    fn debug_output_names_the_secrets_but_never_shows_their_values() {
        let settings = from_text(
            "plugins:\n  warn:\n    limit: 5\nsecrets:\n  gemini: GEMINI_API_KEY\n",
            &[("GEMINI_API_KEY", "sekrit-value-123")],
        )
        .unwrap();
        let shown = format!("{settings:?}");
        assert!(!shown.contains("sekrit-value-123"), "{shown}");
        assert!(shown.contains("gemini"), "{shown}");
        assert!(shown.contains("warn"), "{shown}");
    }

    #[test]
    fn a_missing_file_is_empty_settings() {
        let path = std::env::temp_dir().join(format!(
            "closure-settings-missing-{}.yaml",
            std::process::id()
        ));
        let settings = OperatorSettings::load(&path).unwrap();
        assert_eq!(settings.plugin_section::<Warn>("warn").unwrap(), None);
    }

    #[test]
    fn a_file_is_read_from_disk() {
        let path =
            std::env::temp_dir().join(format!("closure-settings-{}.yaml", std::process::id()));
        std::fs::write(&path, "plugins:\n  warn:\n    limit: 7\n").unwrap();
        let loaded = OperatorSettings::load(&path);
        std::fs::remove_file(&path).unwrap();
        assert_eq!(
            loaded.unwrap().plugin_section::<Warn>("warn").unwrap(),
            Some(Warn { limit: 7 })
        );
    }
}
