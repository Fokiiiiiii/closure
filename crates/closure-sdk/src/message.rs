//! A message a server wrote, with variables in its texts; `parse` checks them against the allowed names.

use closure_core::message::{MessageError, MessageSpec};
use twilight_model::channel::message::Embed;

use crate::template::{Template, Values};

/// A message whose texts are templates that use only the variables it was allowed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MessageTemplate {
    spec: MessageSpec,
    used: Vec<String>,
}

impl MessageTemplate {
    /// Checks `spec`: each text must be a template using only `allowed` variables.
    pub fn parse(spec: MessageSpec, allowed: &[&str]) -> Result<Self, String> {
        let mut used = Vec::new();
        for text in spec.texts() {
            let template = Template::parse(text).map_err(|error| error.to_string())?;
            if !template.uses_only(allowed) {
                return Err("the message has an unknown variable".into());
            }
            for name in allowed {
                if template.uses(name) && !used.iter().any(|u| u == name) {
                    used.push((*name).to_owned());
                }
            }
        }
        spec.clone()
            .into_parts(0, &str::to_owned)
            .map_err(|error| error.to_string())?;
        Ok(Self { spec, used })
    }

    /// Whether any text of the message holds the variable.
    #[must_use]
    pub fn uses(&self, name: &str) -> bool {
        self.used.iter().any(|u| u == name)
    }

    /// The text and the embeds to send, with the variables filled in.
    /// Fails when the rendered message breaks a Discord limit.
    pub fn render(
        &self,
        now: i64,
        values: &Values,
    ) -> Result<(Option<String>, Vec<Embed>), MessageError> {
        self.spec.clone().into_parts(now, &|text| {
            Template::parse(text)
                .map_or_else(|_| text.to_owned(), |template| template.render(values))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn spec(value: serde_json::Value) -> MessageSpec {
        serde_json::from_value(value).unwrap()
    }

    #[test]
    fn variables_are_checked_and_filled_in_every_text() {
        let message = MessageTemplate::parse(
            spec(json!({"content": "hi {user}", "embeds": [{"title": "{server}", "footer": {"text": "{user}"}}]})),
            &["user", "server", "count"],
        )
        .unwrap();
        assert!(message.uses("user") && message.uses("server") && !message.uses("count"));
        let values = Values::new().with("user", "<@1>").with("server", "Home");
        let (content, embeds) = message.render(0, &values).unwrap();
        assert_eq!(content.as_deref(), Some("hi <@1>"));
        assert_eq!(embeds[0].title.as_deref(), Some("Home"));
        assert_eq!(embeds[0].footer.as_ref().unwrap().text, "<@1>");
    }

    #[test]
    fn an_unknown_variable_or_an_unsendable_message_is_refused() {
        let allowed = &["user"];
        assert!(MessageTemplate::parse(spec(json!({"content": "{nobody}"})), allowed).is_err());
        assert!(MessageTemplate::parse(spec(json!({"content": "{user"})), allowed).is_err());
        assert!(MessageTemplate::parse(spec(json!({})), allowed).is_err());
    }
}
