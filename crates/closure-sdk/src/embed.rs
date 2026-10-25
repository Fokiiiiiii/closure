//! Discord embeds, checked against Discord's limits when they are built rather than refused
//! by Discord when they are sent.

use closure_core::error::ModuleError;
use thiserror::Error;
use twilight_model::channel::message::Embed;
use twilight_model::channel::message::embed::{EmbedAuthor, EmbedField, EmbedFooter};
use twilight_model::util::Timestamp;

use crate::UserError;

const TITLE_LIMIT: usize = 256;
const DESCRIPTION_LIMIT: usize = 4096;
const FIELDS_LIMIT: usize = 25;
const FIELD_NAME_LIMIT: usize = 256;
const FIELD_VALUE_LIMIT: usize = 1024;
const FOOTER_LIMIT: usize = 2048;
const AUTHOR_LIMIT: usize = 256;
/// Discord refuses an embed whose text, added up, is longer.
const TOTAL_LIMIT: usize = 6000;

/// The colors of the bar on the left of an embed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Color {
    Success,
    Warning,
    Error,
    Info,
    /// A 24-bit `0xRRGGBB` value.
    Rgb(u32),
}

impl Color {
    fn value(self) -> u32 {
        match self {
            Self::Success => 0x0057_F287,
            Self::Warning => 0x00FE_E75C,
            Self::Error => 0x00ED_4245,
            Self::Info => 0x0058_65F2,
            Self::Rgb(value) => value,
        }
    }
}

/// Builds an embed; [`Self::build`] checks it against Discord's limits.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EmbedBuilder {
    title: Option<String>,
    description: Option<String>,
    url: Option<String>,
    color: Option<Color>,
    /// Name, value and whether the field sits inline.
    fields: Vec<(String, String, bool)>,
    footer: Option<String>,
    author: Option<String>,
    timestamp: Option<i64>,
}

impl EmbedBuilder {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    #[must_use]
    pub fn title(mut self, title: impl Into<String>) -> Self {
        self.title = Some(title.into());
        self
    }

    #[must_use]
    pub fn description(mut self, description: impl Into<String>) -> Self {
        self.description = Some(description.into());
        self
    }

    /// Makes the title a link. Only `http://` and `https://` are accepted.
    #[must_use]
    pub fn url(mut self, url: impl Into<String>) -> Self {
        self.url = Some(url.into());
        self
    }

    #[must_use]
    pub fn color(mut self, color: Color) -> Self {
        self.color = Some(color);
        self
    }

    #[must_use]
    pub fn field(
        mut self,
        name: impl Into<String>,
        value: impl Into<String>,
        inline: bool,
    ) -> Self {
        self.fields.push((name.into(), value.into(), inline));
        self
    }

    #[must_use]
    pub fn footer(mut self, text: impl Into<String>) -> Self {
        self.footer = Some(text.into());
        self
    }

    #[must_use]
    pub fn author(mut self, name: impl Into<String>) -> Self {
        self.author = Some(name.into());
        self
    }

    /// Shows the moment as Discord does, in each reader's time zone.
    #[must_use]
    pub fn timestamp(mut self, unix_seconds: i64) -> Self {
        self.timestamp = Some(unix_seconds);
        self
    }

    /// The twilight value to send, or the first limit the embed breaks.
    pub fn build(self) -> Result<Embed, EmbedError> {
        if self.title.is_none() && self.description.is_none() && self.fields.is_empty() {
            return Err(EmbedError::Empty);
        }
        let mut total = 0;
        if let Some(title) = &self.title {
            total += checked("title", title, TITLE_LIMIT)?;
        }
        if let Some(description) = &self.description {
            total += checked("description", description, DESCRIPTION_LIMIT)?;
        }
        if self.fields.len() > FIELDS_LIMIT {
            return Err(EmbedError::TooManyFields {
                limit: FIELDS_LIMIT,
            });
        }
        for (name, value, _) in &self.fields {
            total += checked("field name", name, FIELD_NAME_LIMIT)?;
            total += checked("field value", value, FIELD_VALUE_LIMIT)?;
        }
        if let Some(footer) = &self.footer {
            total += checked("footer", footer, FOOTER_LIMIT)?;
        }
        if let Some(author) = &self.author {
            total += checked("author", author, AUTHOR_LIMIT)?;
        }
        if total > TOTAL_LIMIT {
            return Err(EmbedError::TooLongInAll {
                length: total,
                limit: TOTAL_LIMIT,
            });
        }
        if self.url.as_ref().is_some_and(|url| !is_web_url(url)) {
            return Err(EmbedError::InvalidUrl);
        }
        if matches!(self.color, Some(Color::Rgb(value)) if value > 0xFF_FFFF) {
            return Err(EmbedError::InvalidColor);
        }
        let timestamp = self
            .timestamp
            .map(Timestamp::from_secs)
            .transpose()
            .map_err(|_| EmbedError::InvalidTime)?;

        Ok(Embed {
            author: self.author.map(|name| EmbedAuthor {
                icon_url: None,
                name,
                proxy_icon_url: None,
                url: None,
            }),
            color: self.color.map(Color::value),
            description: self.description,
            fields: self
                .fields
                .into_iter()
                .map(|(name, value, inline)| EmbedField {
                    inline,
                    name,
                    value,
                })
                .collect(),
            footer: self.footer.map(|text| EmbedFooter {
                icon_url: None,
                proxy_icon_url: None,
                text,
            }),
            image: None,
            kind: "rich".to_owned(),
            provider: None,
            thumbnail: None,
            timestamp,
            title: self.title,
            url: self.url,
            video: None,
        })
    }
}

/// Why an embed cannot be built. These are mistakes in the module, so the messages are for its
/// author.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum EmbedError {
    #[error("the embed has no title, description or field")]
    Empty,
    #[error("the {part} is {length} characters, the limit is {limit}")]
    TooLong {
        part: &'static str,
        length: usize,
        limit: usize,
    },
    #[error("an embed has at most {limit} fields")]
    TooManyFields { limit: usize },
    #[error("the embed is {length} characters in all, the limit is {limit}")]
    TooLongInAll { length: usize, limit: usize },
    #[error("the {part} is empty")]
    EmptyText { part: &'static str },
    #[error("the url must start with http:// or https://")]
    InvalidUrl,
    #[error("the color is not a 24-bit RGB value")]
    InvalidColor,
    #[error("the timestamp is out of range")]
    InvalidTime,
}

impl From<EmbedError> for UserError {
    fn from(error: EmbedError) -> Self {
        Self::Module(ModuleError::other(error.to_string()))
    }
}

/// Checks one part of the text and returns its length in characters. Discord counts characters,
/// not bytes, and refuses an empty part.
fn checked(part: &'static str, text: &str, limit: usize) -> Result<usize, EmbedError> {
    if text.is_empty() {
        return Err(EmbedError::EmptyText { part });
    }
    let length = text.chars().count();
    if length > limit {
        return Err(EmbedError::TooLong {
            part,
            length,
            limit,
        });
    }
    Ok(length)
}

fn is_web_url(url: &str) -> bool {
    url.starts_with("http://") || url.starts_with("https://")
}

/// Shortens `text` to `max_chars` characters; a cut text ends with `…`.
#[must_use]
pub fn truncate(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_owned();
    }
    if max_chars == 0 {
        return String::new();
    }
    let mut cut: String = text.chars().take(max_chars - 1).collect();
    cut.push('…');
    cut
}

/// Makes text a user wrote safe to show: Markdown is escaped, and mentions and channel links are
/// broken by a zero-width space so that they do not ping anyone.
#[must_use]
pub fn escape(text: &str) -> String {
    let mut escaped = String::with_capacity(text.len());
    for ch in text.chars() {
        if matches!(ch, '\\' | '*' | '_' | '~' | '`' | '|' | '>' | '#') {
            escaped.push('\\');
        }
        escaped.push(ch);
    }
    escaped
        .replace("@everyone", "@\u{200B}everyone")
        .replace("@here", "@\u{200B}here")
        .replace("<@", "<\u{200B}@")
        .replace("<#", "<\u{200B}#")
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    /// The part is accepted at its limit and refused one character over it.
    fn assert_limit(make: impl Fn(String) -> EmbedBuilder, part: &'static str, limit: usize) {
        let at = "a".repeat(limit);
        let over = "a".repeat(limit + 1);
        assert!(make(at).build().is_ok(), "{part} at its limit");
        assert_eq!(
            make(over).build(),
            Err(EmbedError::TooLong {
                part,
                length: limit + 1,
                limit,
            })
        );
    }

    #[test]
    fn builds_every_field() {
        let embed = EmbedBuilder::new()
            .title("title")
            .description("description")
            .url("https://example.com")
            .color(Color::Warning)
            .field("name", "value", true)
            .footer("footer")
            .author("author")
            .timestamp(1_700_000_000)
            .build()
            .unwrap();

        assert_eq!(embed.kind, "rich");
        assert_eq!(embed.title.as_deref(), Some("title"));
        assert_eq!(embed.description.as_deref(), Some("description"));
        assert_eq!(embed.url.as_deref(), Some("https://example.com"));
        assert_eq!(embed.color, Some(0x00FE_E75C));
        assert_eq!(
            embed.fields,
            vec![EmbedField {
                name: "name".into(),
                value: "value".into(),
                inline: true,
            }]
        );
        assert_eq!(
            embed.footer,
            Some(EmbedFooter {
                text: "footer".into(),
                icon_url: None,
                proxy_icon_url: None,
            })
        );
        assert_eq!(
            embed.author,
            Some(EmbedAuthor {
                name: "author".into(),
                icon_url: None,
                proxy_icon_url: None,
                url: None,
            })
        );
        assert_eq!(
            embed.timestamp,
            Some(Timestamp::from_secs(1_700_000_000).unwrap())
        );
    }

    #[test]
    fn color_names_map_to_discord_colors() {
        let value = |color| {
            EmbedBuilder::new()
                .title("t")
                .color(color)
                .build()
                .unwrap()
                .color
        };
        assert_eq!(value(Color::Success), Some(0x0057_F287));
        assert_eq!(value(Color::Warning), Some(0x00FE_E75C));
        assert_eq!(value(Color::Error), Some(0x00ED_4245));
        assert_eq!(value(Color::Info), Some(0x0058_65F2));
        assert_eq!(value(Color::Rgb(0xFF_FFFF)), Some(0xFF_FFFF));
    }

    #[test]
    fn title_is_limited_to_256_characters() {
        assert_limit(|text| EmbedBuilder::new().title(text), "title", 256);
    }

    #[test]
    fn description_is_limited_to_4096_characters() {
        assert_limit(
            |text| EmbedBuilder::new().description(text),
            "description",
            4096,
        );
    }

    #[test]
    fn field_name_is_limited_to_256_characters() {
        assert_limit(
            |text| EmbedBuilder::new().field(text, "v", false),
            "field name",
            256,
        );
    }

    #[test]
    fn field_value_is_limited_to_1024_characters() {
        assert_limit(
            |text| EmbedBuilder::new().field("n", text, false),
            "field value",
            1024,
        );
    }

    #[test]
    fn footer_is_limited_to_2048_characters() {
        assert_limit(
            |text| EmbedBuilder::new().title("t").footer(text),
            "footer",
            2048,
        );
    }

    #[test]
    fn author_is_limited_to_256_characters() {
        assert_limit(
            |text| EmbedBuilder::new().title("t").author(text),
            "author",
            256,
        );
    }

    #[test]
    fn fields_are_limited_to_25() {
        let with = |count: usize| {
            (0..count).fold(EmbedBuilder::new().title("t"), |builder, _| {
                builder.field("a", "b", true)
            })
        };
        assert!(with(25).build().is_ok());
        assert_eq!(
            with(26).build(),
            Err(EmbedError::TooManyFields { limit: 25 })
        );
    }

    #[test]
    fn whole_embed_is_limited_to_6000_characters() {
        // 256 + 4096 + 1000 + 256 + 256 leaves 136 characters for the field value.
        let with_value = |value: usize| {
            EmbedBuilder::new()
                .title("a".repeat(256))
                .description("a".repeat(4096))
                .footer("a".repeat(1000))
                .author("a".repeat(256))
                .field("a".repeat(256), "a".repeat(value), false)
        };
        assert!(with_value(136).build().is_ok());
        assert_eq!(
            with_value(137).build(),
            Err(EmbedError::TooLongInAll {
                length: 6001,
                limit: 6000,
            })
        );
    }

    #[test]
    fn counts_japanese_characters_not_bytes() {
        let title = |count: usize| EmbedBuilder::new().title("あ".repeat(count));
        assert!(title(256).build().is_ok());
        assert_eq!(
            title(257).build(),
            Err(EmbedError::TooLong {
                part: "title",
                length: 257,
                limit: 256,
            })
        );
        // Twelve thousand bytes, but within the 4096 characters Discord counts.
        assert!(
            EmbedBuilder::new()
                .description("あ".repeat(4096))
                .build()
                .is_ok()
        );
    }

    #[test]
    fn refuses_an_embed_without_title_description_or_field() {
        assert_eq!(EmbedBuilder::new().build(), Err(EmbedError::Empty));
        assert_eq!(
            EmbedBuilder::new().footer("footer").build(),
            Err(EmbedError::Empty)
        );
    }

    #[test]
    fn refuses_empty_field_name_and_value() {
        assert_eq!(
            EmbedBuilder::new().field("", "v", false).build(),
            Err(EmbedError::EmptyText { part: "field name" })
        );
        assert_eq!(
            EmbedBuilder::new().field("n", "", false).build(),
            Err(EmbedError::EmptyText {
                part: "field value"
            })
        );
    }

    #[test]
    fn refuses_a_url_that_is_not_http() {
        let base = || EmbedBuilder::new().title("t");
        assert!(base().url("https://example.com").build().is_ok());
        assert!(base().url("http://example.com").build().is_ok());
        assert_eq!(base().url("ftp://x").build(), Err(EmbedError::InvalidUrl));
    }

    #[test]
    fn refuses_a_color_beyond_24_bits() {
        assert!(
            EmbedBuilder::new()
                .title("t")
                .color(Color::Rgb(0xFF_FFFF))
                .build()
                .is_ok()
        );
        assert_eq!(
            EmbedBuilder::new()
                .title("t")
                .color(Color::Rgb(0x100_0000))
                .build(),
            Err(EmbedError::InvalidColor)
        );
    }

    #[test]
    fn refuses_a_timestamp_out_of_range() {
        assert_eq!(
            EmbedBuilder::new().title("t").timestamp(i64::MAX).build(),
            Err(EmbedError::InvalidTime)
        );
    }

    #[test]
    fn truncate_keeps_short_text_and_cuts_long_text_with_an_ellipsis() {
        assert_eq!(truncate("abc", 4), "abc");
        assert_eq!(truncate("abcd", 4), "abcd");
        assert_eq!(truncate("abcde", 4), "abc…");
        assert_eq!(truncate("あいうえお", 3), "あい…");
        assert_eq!(truncate("abc", 1), "…");
        assert_eq!(truncate("a", 1), "a");
        assert_eq!(truncate("abc", 0), "");
    }

    #[test]
    fn escape_disables_markdown_characters() {
        for symbol in ["\\", "*", "_", "~", "`", "|", ">", "#"] {
            assert_eq!(escape(symbol), format!("\\{symbol}"), "{symbol}");
        }
    }

    #[test]
    fn escape_disables_mentions_and_channel_links() {
        let everyone = escape("@everyone");
        assert!(!everyone.contains("@everyone"), "{everyone}");
        let here = escape("@here");
        assert!(!here.contains("@here"), "{here}");
        // The markup that Discord turns into a ping or a link starts with `<@` or `<#`; the
        // escaped text must not hold either, however the rest of it was changed.
        for mention in ["<@123>", "<@&5>", "<#9>", "x <@1> y"] {
            let escaped = escape(mention);
            assert!(
                !escaped.contains("<@") && !escaped.contains("<#"),
                "{escaped}"
            );
        }
    }

    #[test]
    fn escape_keeps_ordinary_text() {
        let text = "こんにちは、Hello world! 123.";
        assert_eq!(escape(text), text);
    }
}
