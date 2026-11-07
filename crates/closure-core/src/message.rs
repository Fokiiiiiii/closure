//! A message as a server writes it, in settings or in the dashboard: text and embeds.
//!
//! The writer sees plain data (`MessageSpec`); the bot sends Discord's own embeds. Turning one into
//! the other checks Discord's limits, so a message that cannot be sent is refused when it is
//! written and not when a member joins.

use serde::Deserialize;
use twilight_model::channel::message::component::{ActionRow, Button, ButtonStyle};
use twilight_model::channel::message::embed::{
    EmbedAuthor, EmbedField, EmbedFooter, EmbedImage, EmbedThumbnail,
};
use twilight_model::channel::message::{Component, Embed};
use twilight_model::util::Timestamp;

pub const CONTENT_LIMIT: usize = 2000;
pub const MAX_EMBEDS: usize = 10;
const TITLE_LIMIT: usize = 256;
const DESCRIPTION_LIMIT: usize = 4096;
const FIELDS_LIMIT: usize = 25;
const FIELD_NAME_LIMIT: usize = 256;
const FIELD_VALUE_LIMIT: usize = 1024;
const FOOTER_LIMIT: usize = 2048;
const AUTHOR_LIMIT: usize = 256;
const TOTAL_LIMIT: usize = 6000;
const BUTTON_LABEL_LIMIT: usize = 80;
const URL_LIMIT: usize = 2048;
/// Discord takes five rows of five buttons.
pub const MAX_BUTTONS: usize = 25;
/// Where a role button leads: the `roles` module, its `toggle` route, for anyone, with the role.
const ROLE_BUTTON_ID: &str = "roles:toggle:*:";

/// The JSON Schema of a [`MessageSpec`], for a setting that holds a message.
#[must_use]
pub fn schema() -> serde_json::Value {
    use serde_json::json;
    let link = json!({"type": ["string", "null"], "pattern": "^https?://", "maxLength": URL_LIMIT});
    let text = |limit: usize| json!({"type": ["string", "null"], "maxLength": limit});
    json!({
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "content": text(CONTENT_LIMIT),
            "embeds": {
                "type": "array",
                "maxItems": MAX_EMBEDS,
                "items": {
                    "type": "object",
                    "additionalProperties": false,
                    "properties": {
                        "title": text(TITLE_LIMIT),
                        "description": text(DESCRIPTION_LIMIT),
                        "url": link,
                        "color": {"anyOf": [
                            {"type": "integer", "minimum": 0, "maximum": 0xFF_FFFF},
                            {"type": "string", "pattern": "^#[0-9a-fA-F]{6}$"},
                            {"type": "null"}
                        ]},
                        "author": {"type": ["object", "null"], "additionalProperties": false, "properties": {
                            "name": {"type": "string", "maxLength": AUTHOR_LIMIT},
                            "url": link,
                            "icon_url": link
                        }},
                        "thumbnail": link,
                        "image": link,
                        "footer": {"type": ["object", "null"], "additionalProperties": false, "properties": {
                            "text": {"type": "string", "maxLength": FOOTER_LIMIT},
                            "icon_url": link
                        }},
                        "timestamp": {"type": "boolean"},
                        "fields": {"type": "array", "maxItems": FIELDS_LIMIT, "items": {
                            "type": "object", "additionalProperties": false,
                            "properties": {
                                "name": {"type": "string", "maxLength": FIELD_NAME_LIMIT},
                                "value": {"type": "string", "maxLength": FIELD_VALUE_LIMIT},
                                "inline": {"type": "boolean"}
                            }
                        }}
                    }
                }
            }
        }
    })
}

impl MessageSpec {
    /// Every text the writer typed, in the order the message shows them: where a template can
    /// hold a variable.
    #[must_use]
    pub fn texts(&self) -> Vec<&str> {
        let mut out: Vec<&str> = self.content.iter().map(String::as_str).collect();
        for embed in &self.embeds {
            out.extend(embed.title.as_deref());
            out.extend(embed.description.as_deref());
            out.extend(embed.author.iter().map(|a| a.name.as_str()));
            out.extend(embed.footer.iter().map(|f| f.text.as_str()));
            for field in &embed.fields {
                out.push(&field.name);
                out.push(&field.value);
            }
        }
        out
    }
}

/// What a message holds: text, and up to ten embeds.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct MessageSpec {
    pub content: Option<String>,
    pub embeds: Vec<EmbedSpec>,
    /// Buttons that give or take a role (see `closure-mod-roles`); only a posted message holds
    /// them, never a setting.
    pub buttons: Vec<ButtonSpec>,
}

/// What is posted: the text, the embeds and the rows of buttons.
pub type Post = (Option<String>, Vec<Embed>, Vec<Component>);

/// A button under a message that gives or takes the role `role` of whoever presses it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct ButtonSpec {
    pub label: String,
    /// The id of the role.
    pub role: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct EmbedSpec {
    pub title: Option<String>,
    pub description: Option<String>,
    pub url: Option<String>,
    pub color: Option<ColorSpec>,
    pub author: Option<AuthorSpec>,
    pub thumbnail: Option<String>,
    pub image: Option<String>,
    pub footer: Option<FooterSpec>,
    /// Shows the moment the message is sent.
    pub timestamp: bool,
    pub fields: Vec<FieldSpec>,
}

/// A colour: its number, or `#rrggbb`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(untagged)]
pub enum ColorSpec {
    Number(u32),
    Hex(String),
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct AuthorSpec {
    pub name: String,
    pub url: Option<String>,
    pub icon_url: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct FooterSpec {
    pub text: String,
    pub icon_url: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct FieldSpec {
    pub name: String,
    pub value: String,
    pub inline: bool,
}

/// The first thing that makes a message unsendable.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum MessageError {
    #[error("the message has no text and no embed")]
    Empty,
    #[error("an embed has nothing to show")]
    EmptyEmbed,
    #[error("{part} is longer than {limit} characters")]
    TooLong { part: &'static str, limit: usize },
    #[error("the embeds together are longer than {TOTAL_LIMIT} characters")]
    TooLongInAll,
    #[error("there are more than {limit} {part}")]
    TooMany { part: &'static str, limit: usize },
    #[error("{0} must be an http:// or https:// address")]
    InvalidUrl(&'static str),
    #[error("the colour must be #rrggbb")]
    InvalidColor,
    #[error("a field needs a name and a value")]
    EmptyField,
    #[error("the time cannot be shown")]
    InvalidTime,
    #[error("a button needs a label of at most {BUTTON_LABEL_LIMIT} characters and a role")]
    InvalidButton,
}

fn text(
    part: &'static str,
    value: Option<String>,
    limit: usize,
) -> Result<Option<String>, MessageError> {
    let Some(value) = value.filter(|v| !v.trim().is_empty()) else {
        return Ok(None);
    };
    if value.chars().count() > limit {
        return Err(MessageError::TooLong { part, limit });
    }
    Ok(Some(value))
}

fn web(part: &'static str, value: Option<String>) -> Result<Option<String>, MessageError> {
    let Some(address) = value.filter(|v| !v.trim().is_empty()) else {
        return Ok(None);
    };
    // The same address the schema takes: http or https, with a host, and at most 2048 characters.
    let valid = address.chars().count() <= URL_LIMIT
        && (address.starts_with("https://") || address.starts_with("http://"))
        && url::Url::parse(&address)
            .is_ok_and(|url| matches!(url.scheme(), "http" | "https") && url.has_host());
    if valid {
        Ok(Some(address))
    } else {
        Err(MessageError::InvalidUrl(part))
    }
}

fn length(value: Option<&String>) -> usize {
    value.map_or(0, |v| v.chars().count())
}

impl ColorSpec {
    fn number(&self) -> Result<u32, MessageError> {
        match self {
            Self::Number(n) if *n <= 0xFF_FFFF => Ok(*n),
            Self::Hex(hex) => hex
                .strip_prefix('#')
                .filter(|h| h.len() == 6)
                .and_then(|h| u32::from_str_radix(h, 16).ok())
                .ok_or(MessageError::InvalidColor),
            Self::Number(_) => Err(MessageError::InvalidColor),
        }
    }
}

impl EmbedSpec {
    /// The embed to send and how many characters it counts toward Discord's total.
    fn build(self, now: i64, map: &dyn Fn(&str) -> String) -> Result<(Embed, usize), MessageError> {
        let apply = |value: Option<String>| value.map(|v| map(&v));
        let title = text("the title", apply(self.title), TITLE_LIMIT)?;
        let description = text(
            "the description",
            apply(self.description),
            DESCRIPTION_LIMIT,
        )?;
        let author = match self.author.filter(|a| !a.name.trim().is_empty()) {
            Some(a) => Some(EmbedAuthor {
                name: text("the author", Some(map(&a.name)), AUTHOR_LIMIT)?.unwrap_or_default(),
                url: web("the author link", a.url)?,
                icon_url: web("the author icon", a.icon_url)?,
                proxy_icon_url: None,
            }),
            None => None,
        };
        let footer = match self.footer.filter(|f| !f.text.trim().is_empty()) {
            Some(f) => Some(EmbedFooter {
                text: text("the footer", Some(map(&f.text)), FOOTER_LIMIT)?.unwrap_or_default(),
                icon_url: web("the footer icon", f.icon_url)?,
                proxy_icon_url: None,
            }),
            None => None,
        };
        if self.fields.len() > FIELDS_LIMIT {
            return Err(MessageError::TooMany {
                part: "fields",
                limit: FIELDS_LIMIT,
            });
        }
        let mut fields = Vec::new();
        for field in self.fields {
            let name = text("a field name", Some(map(&field.name)), FIELD_NAME_LIMIT)?;
            let value = text("a field value", Some(map(&field.value)), FIELD_VALUE_LIMIT)?;
            let (Some(name), Some(value)) = (name, value) else {
                return Err(MessageError::EmptyField);
            };
            fields.push(EmbedField {
                name,
                value,
                inline: field.inline,
            });
        }
        let thumbnail = web("the thumbnail", self.thumbnail)?.map(|url| EmbedThumbnail {
            url,
            height: None,
            width: None,
            proxy_url: None,
        });
        let image = web("the image", self.image)?.map(|url| EmbedImage {
            url,
            height: None,
            width: None,
            proxy_url: None,
        });
        if title.is_none()
            && description.is_none()
            && fields.is_empty()
            && author.is_none()
            && footer.is_none()
            && image.is_none()
            && thumbnail.is_none()
        {
            return Err(MessageError::EmptyEmbed);
        }
        let counted = length(title.as_ref())
            + length(description.as_ref())
            + author.as_ref().map_or(0, |a| a.name.chars().count())
            + footer.as_ref().map_or(0, |f| f.text.chars().count())
            + fields
                .iter()
                .map(|f| f.name.chars().count() + f.value.chars().count())
                .sum::<usize>();
        let timestamp = if self.timestamp {
            Some(Timestamp::from_secs(now).map_err(|_| MessageError::InvalidTime)?)
        } else {
            None
        };
        let embed = Embed {
            author,
            color: self.color.as_ref().map(ColorSpec::number).transpose()?,
            description,
            fields,
            footer,
            image,
            kind: "rich".to_owned(),
            provider: None,
            thumbnail,
            timestamp,
            title,
            url: web("the title link", self.url)?,
            video: None,
        };
        Ok((embed, counted))
    }
}

/// The rows of buttons for `buttons`, five to a row.
fn role_buttons(buttons: &[ButtonSpec]) -> Result<Vec<Component>, MessageError> {
    if buttons.len() > MAX_BUTTONS {
        return Err(MessageError::TooMany {
            part: "buttons",
            limit: MAX_BUTTONS,
        });
    }
    let mut components = Vec::new();
    for row in buttons.chunks(5) {
        let mut cells = Vec::new();
        for button in row {
            let label = button.label.trim();
            let role = button.role.trim();
            let valid_role =
                (1..=20).contains(&role.len()) && role.bytes().all(|b| b.is_ascii_digit());
            if label.is_empty() || label.chars().count() > BUTTON_LABEL_LIMIT || !valid_role {
                return Err(MessageError::InvalidButton);
            }
            cells.push(Component::Button(Button {
                id: None,
                custom_id: Some(format!("{ROLE_BUTTON_ID}{role}")),
                disabled: false,
                emoji: None,
                label: Some(label.to_owned()),
                style: ButtonStyle::Secondary,
                url: None,
                sku_id: None,
            }));
        }
        components.push(Component::ActionRow(ActionRow {
            id: None,
            components: cells,
        }));
    }
    Ok(components)
}

impl MessageSpec {
    /// The text, the embeds and the buttons to post (see [`Self::into_parts`]).
    ///
    /// # Errors
    /// The first limit the message breaks.
    pub fn into_post(self, now: i64, map: &dyn Fn(&str) -> String) -> Result<Post, MessageError> {
        let buttons = self.buttons.clone();
        let (content, embeds) = self.into_parts(now, map)?;
        Ok((content, embeds, role_buttons(&buttons)?))
    }

    /// The text and the embeds to send. `map` is applied to every text the writer typed (a
    /// template fills its variables in there); a link is never passed through it.
    ///
    /// # Errors
    /// The first limit the message breaks.
    pub fn into_parts(
        self,
        now: i64,
        map: &dyn Fn(&str) -> String,
    ) -> Result<(Option<String>, Vec<Embed>), MessageError> {
        let content = text("the text", self.content.map(|c| map(&c)), CONTENT_LIMIT)?;
        if self.embeds.len() > MAX_EMBEDS {
            return Err(MessageError::TooMany {
                part: "embeds",
                limit: MAX_EMBEDS,
            });
        }
        let mut embeds = Vec::new();
        let mut total = 0;
        for spec in self.embeds {
            let (embed, counted) = spec.build(now, map)?;
            total += counted;
            embeds.push(embed);
        }
        if total > TOTAL_LIMIT {
            return Err(MessageError::TooLongInAll);
        }
        if content.is_none() && embeds.is_empty() {
            return Err(MessageError::Empty);
        }
        Ok((content, embeds))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn spec(value: serde_json::Value) -> MessageSpec {
        serde_json::from_value(value).unwrap()
    }

    fn same(text: &str) -> String {
        text.to_owned()
    }

    #[test]
    fn role_buttons_are_laid_out_five_to_a_row_and_point_at_the_roles_module() {
        let buttons: Vec<_> = (1..=7)
            .map(|n| json!({"label": format!("Role {n}"), "role": n.to_string()}))
            .collect();
        let (_, _, rows) = spec(json!({"content": "pick", "buttons": buttons}))
            .into_post(0, &same)
            .unwrap();
        assert_eq!(rows.len(), 2);
        let Component::ActionRow(first) = &rows[0] else {
            panic!("a row")
        };
        assert_eq!(first.components.len(), 5);
        let Component::Button(button) = &first.components[0] else {
            panic!("a button")
        };
        assert_eq!(button.custom_id.as_deref(), Some("roles:toggle:*:1"));
        for bad in [
            json!([{"label": "", "role": "1"}]),
            json!([{"label": "x", "role": "role"}]),
        ] {
            let result = spec(json!({"content": "pick", "buttons": bad})).into_post(0, &same);
            assert_eq!(result.unwrap_err(), MessageError::InvalidButton);
        }
    }

    #[test]
    fn a_full_embed_becomes_discords_embed() {
        let (content, embeds) = spec(json!({
            "content": "hi",
            "embeds": [{
                "title": "T", "description": "D", "url": "https://example.org", "color": "#5865f2",
                "author": {"name": "A", "url": "https://a.example", "icon_url": "https://a.example/i.png"},
                "thumbnail": "https://t.example/t.png", "image": "https://t.example/i.png",
                "footer": {"text": "F", "icon_url": "https://f.example/f.png"},
                "timestamp": true,
                "fields": [{"name": "n", "value": "v", "inline": true}]
            }]
        }))
        .into_parts(1_700_000_000, &same)
        .unwrap();
        assert_eq!(content.as_deref(), Some("hi"));
        let embed = &embeds[0];
        assert_eq!(embed.color, Some(0x58_65f2));
        assert_eq!(embed.author.as_ref().unwrap().name, "A");
        assert_eq!(embed.footer.as_ref().unwrap().text, "F");
        assert!(embed.timestamp.is_some() && embed.thumbnail.is_some() && embed.image.is_some());
        assert_eq!(embed.fields.len(), 1);
    }

    #[test]
    fn the_map_reaches_every_text_but_no_link() {
        let up = |t: &str| t.to_uppercase();
        let (content, embeds) = spec(json!({
            "content": "a",
            "embeds": [{"title": "b", "url": "https://x.example/lower", "footer": {"text": "c"}}]
        }))
        .into_parts(0, &up)
        .unwrap();
        assert_eq!(content.as_deref(), Some("A"));
        assert_eq!(embeds[0].title.as_deref(), Some("B"));
        assert_eq!(embeds[0].footer.as_ref().unwrap().text, "C");
        assert_eq!(embeds[0].url.as_deref(), Some("https://x.example/lower"));
    }

    #[test]
    fn what_cannot_be_sent_is_refused_with_its_reason() {
        let parts = |v| spec(v).into_parts(0, &same);
        assert_eq!(parts(json!({})), Err(MessageError::Empty));
        assert_eq!(
            parts(json!({"embeds": [{}]})),
            Err(MessageError::EmptyEmbed)
        );
        assert_eq!(
            parts(json!({"content": "a".repeat(2001)})),
            Err(MessageError::TooLong {
                part: "the text",
                limit: 2000
            })
        );
        assert_eq!(
            parts(json!({"embeds": [{"title": "t", "image": "ftp://x"}]})),
            Err(MessageError::InvalidUrl("the image"))
        );
        assert_eq!(
            parts(json!({"embeds": [{"title": "t", "color": "red"}]})),
            Err(MessageError::InvalidColor)
        );
        assert_eq!(
            parts(json!({"embeds": [{"fields": [{"name": "n", "value": ""}]}]})),
            Err(MessageError::EmptyField)
        );
        let big = json!({"embeds": [{"description": "a".repeat(4000)}, {"description": "b".repeat(4000)}]});
        assert_eq!(parts(big), Err(MessageError::TooLongInAll));
        let many = json!({"embeds": (0..11).map(|_| json!({"title": "t"})).collect::<Vec<_>>()});
        assert_eq!(
            parts(many),
            Err(MessageError::TooMany {
                part: "embeds",
                limit: 10
            })
        );
    }

    #[test]
    fn an_unknown_key_is_refused_on_reading() {
        assert!(serde_json::from_value::<MessageSpec>(json!({"embed": {}})).is_err());
        assert!(serde_json::from_value::<MessageSpec>(json!({"embeds": [{"colour": 1}]})).is_err());
    }

    /// What the writer is allowed to save is what the bot is allowed to send: a message that
    /// `into_post` accepts passes the checks of the action that posts it, at the edges too.
    #[test]
    fn a_message_that_is_accepted_is_one_that_can_be_posted() {
        let embed = json!({
            "title": "t".repeat(256), "description": "d".repeat(1000),
            "url": "https://example.com/a?b=c", "color": "#ffffff",
            "author": {"name": "a", "url": "http://example.com", "icon_url": "https://example.com/i.png"},
            "footer": {"text": "f"}, "timestamp": true,
            "fields": [{"name": "n", "value": "v", "inline": true}]
        });
        let buttons: Vec<_> = (1..=MAX_BUTTONS)
            .map(|n| json!({"label": "r", "role": n.to_string()}))
            .collect();
        let messages = [
            json!({"content": "c".repeat(CONTENT_LIMIT)}),
            json!({"embeds": [embed.clone(), embed.clone(), embed]}),
            json!({"content": "pick", "buttons": buttons}),
            json!({"embeds": (0..MAX_EMBEDS).map(|n| json!({"title": format!("e{n}")})).collect::<Vec<_>>()}),
        ];
        for message in messages {
            let (content, embeds, components) =
                spec(message).into_post(1_700_000_000, &same).unwrap();
            let action = crate::action::Action::PostMessage {
                channel_id: twilight_model::id::Id::new(1),
                message_id: None,
                reply_to: None,
                content,
                embeds,
                components,
                files: Vec::new(),
            };
            assert_eq!(action.validate(), Ok(()));
        }
    }

    #[test]
    fn an_address_is_http_or_https_with_a_host() {
        for good in ["https://example.com", "http://example.com/a?b=c#d"] {
            let result =
                spec(json!({"embeds": [{"title": "t", "url": good}]})).into_parts(0, &same);
            assert!(result.is_ok(), "{good}");
        }
        for bad in [
            "https://",
            "http:// bad",
            "ftp://example.com",
            "https:example",
            "javascript:alert(1)",
        ] {
            let result = spec(json!({"embeds": [{"title": "t", "url": bad}]})).into_parts(0, &same);
            assert_eq!(
                result.unwrap_err(),
                MessageError::InvalidUrl("the title link"),
                "{bad}"
            );
        }
    }
}
