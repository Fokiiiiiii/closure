use std::io::{self, Read};
use std::path::Path;

use anyhow::{Result, bail};
use closure_core::action::{WebhookRegistry, WebhookToken};
use twilight_model::id::Id;

fn parse(input: &str) -> Result<WebhookRegistry> {
    let mut registry = WebhookRegistry::new();
    for (index, line) in input.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let fail = |reason: &str| anyhow::anyhow!("line {}: {reason}", index + 1);
        let (name, value) = line
            .split_once('=')
            .ok_or_else(|| fail("expected NAME=URL"))?;
        let name = name.trim();
        if name.is_empty()
            || name.len() > 32
            || !name
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
        {
            return Err(fail("invalid name"));
        }
        let value = value.trim();
        let value = if value.starts_with(['\'', '"']) {
            if value.len() < 2 || !value.ends_with(&value[..1]) {
                return Err(fail("invalid quoting"));
            }
            &value[1..value.len() - 1]
        } else {
            value
        };
        let rest = value
            .strip_prefix("https://")
            .ok_or_else(|| fail("invalid URL"))?;
        let (host, path) = rest.split_once('/').ok_or_else(|| fail("invalid URL"))?;
        if ![
            "discord.com",
            "discordapp.com",
            "canary.discord.com",
            "ptb.discord.com",
        ]
        .contains(&host)
        {
            return Err(fail("invalid host"));
        }
        let path = path
            .strip_prefix("api/webhooks/")
            .or_else(|| path.strip_prefix("api/v10/webhooks/"))
            .ok_or_else(|| fail("invalid path"))?;
        let (id, token) = path.split_once('/').ok_or_else(|| fail("invalid path"))?;
        if id.is_empty() || !id.bytes().all(|b| b.is_ascii_digit()) {
            return Err(fail("invalid id"));
        }
        let id = id
            .parse::<u64>()
            .ok()
            .filter(|id| *id != 0)
            .ok_or_else(|| fail("invalid id"))?;
        if token.is_empty()
            || token.len() > 256
            || !token
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
        {
            return Err(fail("invalid token"));
        }
        if registry
            .insert(name.to_owned(), (Id::new(id), WebhookToken::new(token)))
            .is_some()
        {
            return Err(fail("duplicate name"));
        }
    }
    Ok(registry)
}

pub fn load(path: &Path) -> Result<WebhookRegistry> {
    let mut file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(WebhookRegistry::new()),
        Err(_) => bail!("could not open webhook file"),
    };
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let metadata = file
            .metadata()
            .map_err(|_| anyhow::anyhow!("could not inspect webhook file"))?;
        if metadata.permissions().mode() & 0o077 != 0 {
            tracing::warn!("webhook file is accessible by group or other users");
        }
    }
    let mut input = String::new();
    file.read_to_string(&mut input)
        .map_err(|_| anyhow::anyhow!("could not read webhook file"))?;
    parse(&input)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn webhook_file_parsing_and_redaction() {
        let registry = parse("# example\n\nONE='https://discord.com/api/webhooks/8/fake-token'\nTWO=\"https://ptb.discord.com/api/v10/webhooks/9/other\"\n").unwrap();
        assert_eq!(registry.len(), 2);
        assert!(!format!("{registry:?}").contains("fake-token"));
        for input in [
            "X=https://evil.example/api/webhooks/8/fake-token",
            "X=https://discord.com/api/webhooks/8/fake-token?q=1",
            "X=https://discord.com/api/webhooks/0/fake-token",
            "X=https://discord.com/api/webhooks/8/fake-token\nX=https://discord.com/api/webhooks/9/other",
        ] {
            let error = parse(input).unwrap_err();
            assert!(!format!("{error:?}").contains("fake-token"));
            assert!(!error.to_string().contains("fake-token"));
        }
    }

    #[test]
    fn webhook_file_missing_and_invalid() {
        let dir = std::env::temp_dir().join(format!("closure-webhook-test-{}", std::process::id()));
        std::fs::create_dir(&dir).unwrap();
        let path = dir.join("fixture");
        assert!(load(&path).unwrap().is_empty());
        std::fs::write(&path, "invalid").unwrap();
        assert!(load(&path).is_err());
        std::fs::remove_file(&path).unwrap();
        std::fs::remove_dir(&dir).unwrap();
    }
}
