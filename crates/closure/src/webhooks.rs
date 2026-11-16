//! Named Discord webhooks, read from the environment: `WEBHOOK_<NAME>=<URL>` (`.env` included).

use anyhow::{Result, anyhow};
use closure_core::action::{WebhookRegistry, WebhookToken};
use twilight_model::id::Id;

const PREFIX: &str = "WEBHOOK_";
const MAX_NAME_LEN: usize = 32;
const MAX_TOKEN_LEN: usize = 256;

/// The webhooks named by `WEBHOOK_*` variables. A malformed one stops the start-up; the error
/// names the variable and never its URL.
pub fn from_env() -> Result<WebhookRegistry> {
    parse(std::env::vars_os().map(|(name, value)| {
        (
            name.to_string_lossy().into_owned(),
            value.to_string_lossy().into_owned(),
        )
    }))
}

fn parse(vars: impl IntoIterator<Item = (String, String)>) -> Result<WebhookRegistry> {
    let mut registry = WebhookRegistry::new();
    for (variable, url) in vars {
        let Some(name) = variable.strip_prefix(PREFIX) else {
            continue;
        };
        let valid_name = !name.is_empty()
            && name.len() <= MAX_NAME_LEN
            && name
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-');
        if !valid_name {
            return Err(anyhow!("{variable}: invalid name"));
        }
        let (id, token) = webhook_address(&url).ok_or_else(|| {
            anyhow!(
                "{variable}: invalid URL, expected https://discord.com/api/webhooks/<id>/<token>"
            )
        })?;
        registry.insert(name.to_owned(), (Id::new(id), WebhookToken::new(token)));
    }
    Ok(registry)
}

/// The id and token of a Discord webhook URL, or `None` when the URL is not one.
fn webhook_address(url: &str) -> Option<(u64, &str)> {
    let rest = url.strip_prefix("https://")?;
    let (host, path) = rest.split_once('/')?;
    if ![
        "discord.com",
        "discordapp.com",
        "canary.discord.com",
        "ptb.discord.com",
    ]
    .contains(&host)
    {
        return None;
    }
    let path = path
        .strip_prefix("api/webhooks/")
        .or_else(|| path.strip_prefix("api/v10/webhooks/"))?;
    let (id, token) = path.split_once('/')?;
    if id.is_empty() || !id.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let id = id.parse::<u64>().ok().filter(|id| *id != 0)?;
    let valid_token = !token.is_empty()
        && token.len() <= MAX_TOKEN_LEN
        && token
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-');
    valid_token.then_some((id, token))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vars(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
            .collect()
    }

    #[test]
    fn named_webhooks_are_read_from_their_variables() {
        let registry = parse(vars(&[
            (
                "WEBHOOK_LOGS",
                "https://discord.com/api/webhooks/8/fake-token",
            ),
            (
                "WEBHOOK_MODS",
                "https://ptb.discord.com/api/v10/webhooks/9/other",
            ),
        ]))
        .unwrap();
        assert_eq!(registry.len(), 2);
        assert!(registry.contains_key("LOGS"));
        assert!(registry.contains_key("MODS"));
    }

    #[test]
    fn a_name_keeps_its_case_and_may_hold_dashes_and_underscores() {
        let registry = parse(vars(&[(
            "WEBHOOK_Logs-1_x",
            "https://discord.com/api/webhooks/8/fake-token",
        )]))
        .unwrap();
        assert!(registry.contains_key("Logs-1_x"));
    }

    #[test]
    fn other_variables_are_not_webhooks() {
        let registry = parse(vars(&[
            ("DISCORD_TOKEN", "not-a-webhook"),
            ("WEBHOOKS", "https://discord.com/api/webhooks/8/fake-token"),
        ]))
        .unwrap();
        assert!(registry.is_empty());
    }

    #[test]
    fn no_webhook_variables_means_no_webhooks() {
        assert!(parse(Vec::new()).unwrap().is_empty());
    }

    #[test]
    fn a_malformed_url_is_refused_naming_the_variable_and_never_the_token() {
        for value in [
            "http://discord.com/api/webhooks/8/fake-token",
            "https://evil.example/api/webhooks/8/fake-token",
            "https://discord.com/api/webhooks/8/fake-token?q=1",
            "https://discord.com/api/webhooks/0/fake-token",
            "https://discord.com/api/webhooks/8/",
            "",
        ] {
            let error = parse(vars(&[("WEBHOOK_LOGS", value)])).unwrap_err();
            let shown = format!("{error:?} {error}");
            assert!(shown.contains("WEBHOOK_LOGS"), "{shown}");
            assert!(!shown.contains("fake-token"), "{shown}");
        }
    }

    #[test]
    fn an_empty_or_invalid_name_is_refused() {
        for name in [
            "WEBHOOK_",
            "WEBHOOK_A.B",
            &format!("WEBHOOK_{}", "x".repeat(MAX_NAME_LEN + 1)),
        ] {
            let result = parse(vars(&[(
                name,
                "https://discord.com/api/webhooks/8/fake-token",
            )]));
            assert!(result.is_err(), "{name} must be refused");
        }
    }

    #[test]
    fn the_removed_webhook_file_variable_is_refused_rather_than_ignored() {
        assert!(parse(vars(&[("WEBHOOK_FILE", "webhooks.env")])).is_err());
    }

    #[test]
    fn debug_output_never_shows_a_token() {
        let registry = parse(vars(&[(
            "WEBHOOK_LOGS",
            "https://discord.com/api/webhooks/8/fake-token",
        )]))
        .unwrap();
        assert!(!format!("{registry:?}").contains("fake-token"));
    }
}
