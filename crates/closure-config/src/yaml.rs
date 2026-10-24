use serde_saphyr::{Budget, Options};

use crate::error::ConfigError;
use crate::model::{CURRENT_VERSION, EffectiveConfig, GuildOverrides};

/// Configuration documents are hand-written; 64 KiB is far above any legitimate one.
pub const MAX_YAML_BYTES: usize = 64 * 1024;

/// Parser limits for untrusted input: anchors and aliases stay available for convenience but
/// are capped so a "billion laughs" document cannot exhaust memory.
fn options() -> Options {
    let mut budget = Budget::default();
    budget.max_events = 100_000;
    budget.max_aliases = 100;
    budget.max_anchors = 100;
    let mut options = Options::default();
    options.budget = Some(budget);
    options
}

/// Parses a YAML document into sparse overrides. An empty document means "all defaults".
///
/// Errors carry the 1-based line and column so the editor can point at the problem.
pub fn parse_overrides(yaml: &str) -> Result<GuildOverrides, ConfigError> {
    if yaml.len() > MAX_YAML_BYTES {
        return Err(ConfigError::TooLarge {
            size: yaml.len(),
            limit: MAX_YAML_BYTES,
        });
    }
    if yaml.trim().is_empty() {
        return Ok(GuildOverrides::default());
    }
    serde_saphyr::from_str_with_options(yaml, options()).map_err(|error| {
        let location = error.location();
        ConfigError::Syntax {
            message: error.to_string(),
            line: location.map(|location| location.line()),
            column: location.map(|location| location.column()),
        }
    })
}

/// Writes overrides as YAML, always stamping the schema version so the document stays readable
/// after the format evolves.
pub fn overrides_to_yaml(overrides: &GuildOverrides) -> Result<String, ConfigError> {
    let mut stamped = overrides.clone();
    stamped.version = Some(CURRENT_VERSION);
    serde_saphyr::to_string(&stamped).map_err(|error| ConfigError::Serialize(error.to_string()))
}

/// Writes the fully resolved configuration: the "show me everything" view of the advanced editor.
pub fn effective_to_yaml(effective: &EffectiveConfig) -> Result<String, ConfigError> {
    serde_saphyr::to_string(effective).map_err(|error| ConfigError::Serialize(error.to_string()))
}

#[cfg(test)]
mod tests {
    use closure_core::locale::Locale;
    use twilight_model::id::Id;

    use super::*;
    use crate::model::{Language, Preset};

    const FULL: &str = "
version: 1
preset: large
language: ja
log_channel: 1563460727941890048
moderator_roles: [1, 2]
limits:
  concurrency: 12
modules:
  warn:
    enabled: true
    config:
      limit: 5
      words: [a, b]
";

    #[test]
    fn parses_a_full_document() {
        let overrides = parse_overrides(FULL).unwrap();
        assert_eq!(overrides.preset, Some(Preset::Large));
        assert_eq!(overrides.language, Some(Language(Some(Locale::Ja))));
        assert_eq!(
            overrides.log_channel,
            Some(Id::new(1_563_460_727_941_890_048))
        );
        assert_eq!(
            overrides.moderator_roles,
            Some(vec![Id::new(1), Id::new(2)])
        );
        assert_eq!(overrides.limits.unwrap().concurrency, Some(12));
        let warn = &overrides.modules["warn"];
        assert_eq!(warn.enabled, Some(true));
        assert_eq!(warn.config.as_ref().unwrap()["words"][1], "b");
    }

    #[test]
    fn empty_and_blank_documents_mean_defaults() {
        assert_eq!(parse_overrides("").unwrap(), GuildOverrides::default());
        assert_eq!(
            parse_overrides("  \n\n").unwrap(),
            GuildOverrides::default()
        );
    }

    #[test]
    fn language_accepts_auto_and_rejects_unknown_tags_with_guidance() {
        let auto = parse_overrides("language: auto").unwrap();
        assert_eq!(auto.language, Some(Language(None)));
        let error = parse_overrides("language: klingon")
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("expected `auto` or one of: en, ja"),
            "{error}"
        );
    }

    #[test]
    fn unknown_keys_are_rejected_with_a_position() {
        let error = parse_overrides("preset: small\npreest: large\n").unwrap_err();
        let ConfigError::Syntax { line, message, .. } = error else {
            panic!("expected a syntax error, got {error:?}");
        };
        assert_eq!(line, Some(2));
        assert!(message.contains("preest"), "{message}");
    }

    #[test]
    fn type_errors_point_at_the_offending_line() {
        let error = parse_overrides("preset: small\nlimits:\n  concurrency: lots\n").unwrap_err();
        let ConfigError::Syntax { line, .. } = error else {
            panic!("expected a syntax error");
        };
        assert_eq!(line, Some(3));
    }

    #[test]
    fn broken_yaml_is_a_syntax_error_not_a_panic() {
        assert!(matches!(
            parse_overrides("preset: [small"),
            Err(ConfigError::Syntax { .. })
        ));
    }

    #[test]
    fn oversized_documents_are_refused_before_parsing() {
        let big = format!("# {}\n", "x".repeat(MAX_YAML_BYTES));
        assert!(matches!(
            parse_overrides(&big),
            Err(ConfigError::TooLarge { .. })
        ));
    }

    #[test]
    fn alias_expansion_bombs_are_refused() {
        // Each level repeats the previous one nine times, so `levels` levels expand to 9^levels
        // items. It sits in a free-form module config, where the schema itself accepts any key,
        // so a failure can only come from the parser's budget.
        let document = |levels: usize| {
            let mut entries = vec!["l0: &l0 [x, x, x, x, x, x, x, x, x]".to_owned()];
            for level in 1..levels {
                let previous = format!("*l{}", level - 1);
                let items = vec![previous; 9].join(", ");
                entries.push(format!("l{level}: &l{level} [{items}]"));
            }
            format!(
                "modules: {{ warn: {{ config: {{ {} }} }} }}",
                entries.join(", ")
            )
        };

        assert!(
            parse_overrides(&document(3)).is_ok(),
            "modest alias use must work"
        );
        assert!(
            matches!(
                parse_overrides(&document(12)),
                Err(ConfigError::Syntax { .. })
            ),
            "a 9^12 expansion must be refused"
        );
    }

    #[test]
    fn written_overrides_parse_back_identically_apart_from_the_version() {
        let mut overrides = parse_overrides(FULL).unwrap();
        let yaml = overrides_to_yaml(&overrides).unwrap();
        let reparsed = parse_overrides(&yaml).unwrap();
        overrides.version = Some(CURRENT_VERSION);
        assert_eq!(reparsed, overrides);
    }

    #[test]
    fn written_documents_always_carry_the_version() {
        let yaml = overrides_to_yaml(&GuildOverrides::default()).unwrap();
        assert!(yaml.contains("version: 1"), "{yaml}");
    }
}
