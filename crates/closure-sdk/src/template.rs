//! A small text template for messages a module sends: `{name}` and `{a.b.c}` are replaced by
//! the values a module gives, `{{` and `}}` stand for literal braces, and a name the values do
//! not have renders as nothing. There are no functions, so a template can only read the data it
//! is given. A render is cut at [`MAX_OUTPUT_CHARS`].

use std::borrow::Cow;
use std::collections::BTreeMap;

use thiserror::Error;

pub mod values;

/// The longest render, in characters.
pub const MAX_OUTPUT_CHARS: usize = 4_000;

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum TemplateError {
    #[error("a lone closing brace at character {position}; write a literal one as a doubled brace")]
    UnmatchedClose { position: usize },
    #[error("an opening brace at character {position} sits inside a variable")]
    NestedOpen { position: usize },
    #[error("the variable opened at character {position} is never closed")]
    Unclosed { position: usize },
}

/// A value a template can show: what a module gives it, or what JSON holds.
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Null,
    Bool(bool),
    Number(f64),
    Text(String),
    List(Vec<Value>),
    Map(BTreeMap<String, Value>),
}

impl Value {
    /// Converts JSON; every number becomes an `f64`.
    #[must_use]
    pub fn from_json(json: serde_json::Value) -> Self {
        match json {
            serde_json::Value::Null => Self::Null,
            serde_json::Value::Bool(flag) => Self::Bool(flag),
            serde_json::Value::Number(number) => number.as_f64().map_or(Self::Null, Self::Number),
            serde_json::Value::String(text) => Self::Text(text),
            serde_json::Value::Array(items) => {
                Self::List(items.into_iter().map(Self::from_json).collect())
            }
            serde_json::Value::Object(map) => Self::Map(
                map.into_iter()
                    .map(|(key, item)| (key, Self::from_json(item)))
                    .collect(),
            ),
        }
    }

    /// The text a template shows for this value. A list shows its items joined by commas; a map
    /// shows nothing, so a template has to name a leaf of it.
    #[must_use]
    pub fn to_text(&self) -> String {
        match self {
            Self::Null | Self::Map(_) => String::new(),
            Self::Bool(flag) => flag.to_string(),
            Self::Number(number) => number_text(*number),
            Self::Text(text) => text.clone(),
            Self::List(items) => items
                .iter()
                .map(Self::to_text)
                .collect::<Vec<_>>()
                .join(","),
        }
    }
}

fn number_text(number: f64) -> String {
    if number.is_nan() {
        "NaN".to_owned()
    } else if number.is_infinite() {
        if number.is_sign_negative() {
            "-Infinity"
        } else {
            "Infinity"
        }
        .to_owned()
    } else {
        number.to_string()
    }
}

impl From<&str> for Value {
    fn from(text: &str) -> Self {
        Self::Text(text.to_owned())
    }
}

impl From<String> for Value {
    fn from(text: String) -> Self {
        Self::Text(text)
    }
}

impl From<bool> for Value {
    fn from(flag: bool) -> Self {
        Self::Bool(flag)
    }
}

impl From<f64> for Value {
    fn from(number: f64) -> Self {
        Self::Number(number)
    }
}

impl From<i32> for Value {
    fn from(number: i32) -> Self {
        Self::Number(f64::from(number))
    }
}

impl From<Vec<Value>> for Value {
    fn from(items: Vec<Value>) -> Self {
        Self::List(items)
    }
}

/// The values of one render: the names a template may use, at the top level.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Values(BTreeMap<String, Value>);

impl Values {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds `key`, replacing what was there.
    #[must_use]
    pub fn with(mut self, key: &str, value: impl Into<Value>) -> Self {
        self.0.insert(key.to_owned(), value.into());
        self
    }

    /// The values of a JSON object; `None` when the JSON is not an object.
    #[must_use]
    pub fn from_json(json: serde_json::Value) -> Option<Self> {
        match Value::from_json(json) {
            Value::Map(map) => Some(Self(map)),
            _ => None,
        }
    }

    /// The value at a dotted path, such as `user.name` or `tags.0`.
    fn lookup(&self, path: &str) -> Option<&Value> {
        let mut segments = path.split('.');
        let mut cursor = self.0.get(segments.next()?)?;
        for segment in segments {
            cursor = match cursor {
                Value::Map(map) => map.get(segment)?,
                Value::List(items) => items.get(segment.parse::<usize>().ok()?)?,
                _ => return None,
            };
        }
        Some(cursor)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Part {
    Text(String),
    Path(String),
}

/// A parsed template, ready to render with any values.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Template {
    parts: Vec<Part>,
}

impl Template {
    /// Reads a template. A lone `}` and a variable that is never closed are errors, with the
    /// character position where they were found.
    pub fn parse(text: &str) -> Result<Self, TemplateError> {
        let chars: Vec<char> = text.chars().collect();
        let mut parts = Vec::new();
        let mut literal = String::new();
        let mut at = 0;
        while let Some(&ch) = chars.get(at) {
            match (ch, chars.get(at + 1).copied()) {
                ('{', Some('{')) => {
                    literal.push('{');
                    at += 2;
                }
                ('}', Some('}')) => {
                    literal.push('}');
                    at += 2;
                }
                ('}', _) => return Err(TemplateError::UnmatchedClose { position: at }),
                ('{', _) => {
                    let (name, end) = read_name(&chars, at)?;
                    if !literal.is_empty() {
                        parts.push(Part::Text(std::mem::take(&mut literal)));
                    }
                    parts.push(Part::Path(name));
                    at = end;
                }
                _ => {
                    literal.push(ch);
                    at += 1;
                }
            }
        }
        if !literal.is_empty() {
            parts.push(Part::Text(literal));
        }
        Ok(Self { parts })
    }

    /// The text with every variable replaced, cut at [`MAX_OUTPUT_CHARS`].
    #[must_use]
    pub fn render(&self, values: &Values) -> String {
        let mut out = String::new();
        let mut written = 0;
        for part in &self.parts {
            if written >= MAX_OUTPUT_CHARS {
                break;
            }
            let piece: Cow<'_, str> = match part {
                Part::Text(text) => Cow::Borrowed(text.as_str()),
                Part::Path(path) => {
                    Cow::Owned(values.lookup(path).map_or_else(String::new, Value::to_text))
                }
            };
            for ch in piece.chars().take(MAX_OUTPUT_CHARS - written) {
                out.push(ch);
                written += 1;
            }
        }
        out
    }
}

/// The name between the brace at `open` and its closing brace, and the index after that brace.
fn read_name(chars: &[char], open: usize) -> Result<(String, usize), TemplateError> {
    let mut name = String::new();
    for (at, &ch) in chars.iter().enumerate().skip(open + 1) {
        match ch {
            '}' => return Ok((name, at + 1)),
            '{' => return Err(TemplateError::NestedOpen { position: at }),
            _ => name.push(ch),
        }
    }
    Err(TemplateError::Unclosed { position: open })
}

/// Parses and renders `text` in one step.
pub fn render(text: &str, values: &Values) -> Result<String, TemplateError> {
    Ok(Template::parse(text)?.render(values))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn user() -> Value {
        Value::Map(BTreeMap::from([
            ("name".to_owned(), Value::from("Ann")),
            ("level".to_owned(), Value::from(3)),
        ]))
    }

    #[test]
    fn plain_text_is_kept() {
        let values = Values::new();
        assert_eq!(render("hello world", &values), Ok("hello world".to_owned()));
    }

    #[test]
    fn names_and_dotted_paths_are_replaced() {
        let values = Values::new().with("user", user()).with("server", "Example");
        assert_eq!(
            render("{server}: {user.name} is level {user.level}", &values),
            Ok("Example: Ann is level 3".to_owned())
        );
    }

    #[test]
    fn a_name_the_values_do_not_have_renders_as_nothing() {
        let values = Values::new().with("user", user());
        assert_eq!(
            render("[{missing}][{user.missing}][{user.name.deeper}]", &values),
            Ok("[][][]".to_owned())
        );
    }

    #[test]
    fn doubled_braces_are_literal_braces() {
        assert_eq!(
            render("{{x}} and }}", &Values::new()),
            Ok("{x} and }".to_owned())
        );
        assert_eq!(
            render("{{{a}}}", &Values::new().with("a", "b")),
            Ok("{b}".to_owned())
        );
    }

    #[test]
    fn a_lone_closing_brace_is_an_error_at_its_position() {
        assert_eq!(
            Template::parse("ab}c"),
            Err(TemplateError::UnmatchedClose { position: 2 })
        );
    }

    #[test]
    fn an_unclosed_or_nested_variable_is_an_error_at_its_opening() {
        assert_eq!(
            Template::parse("x {ab"),
            Err(TemplateError::Unclosed { position: 2 })
        );
        assert_eq!(
            Template::parse("{a{b}"),
            Err(TemplateError::NestedOpen { position: 2 })
        );
        assert_eq!(
            Template::parse("{"),
            Err(TemplateError::Unclosed { position: 0 })
        );
    }

    #[test]
    fn values_show_as_text() {
        let values = Values::new()
            .with("number", 5.07)
            .with("whole", 5.0)
            .with("flag", true)
            .with("nothing", Value::Null)
            .with("tags", vec![Value::from("a"), Value::Null, Value::from(2)]);
        assert_eq!(
            render("{number}|{whole}|{flag}|{nothing}|{tags}|{tags.2}", &values),
            Ok("5.07|5|true||a,,2|2".to_owned())
        );
        assert_eq!(
            render("[{user}]", &Values::new().with("user", user())),
            Ok("[]".to_owned())
        );
    }

    #[test]
    fn a_list_item_is_reached_by_its_index() {
        let values = Values::new().with("tags", vec![Value::from("a"), Value::from("b")]);
        assert_eq!(render("{tags.1}{tags.9}", &values), Ok("b".to_owned()));
    }

    #[test]
    fn json_objects_become_values_and_other_json_does_not() {
        let values = Values::from_json(serde_json::json!({ "a": { "b": 1 }, "c": [true] }));
        assert_eq!(
            render("{a.b} {c.0}", &values.clone().unwrap()),
            Ok("1 true".to_owned())
        );
        assert_eq!(Values::from_json(serde_json::json!([1])), None);
    }

    #[test]
    fn a_render_is_cut_at_the_output_limit() {
        let long = "x".repeat(MAX_OUTPUT_CHARS + 500);
        let out = render(&long, &Values::new()).unwrap();
        assert_eq!(out.chars().count(), MAX_OUTPUT_CHARS);
    }

    #[test]
    fn a_huge_template_parses_and_renders_within_the_limit() {
        let template = "{a}".repeat(100_000);
        let parsed = Template::parse(&template).unwrap();
        let out = parsed.render(&Values::new().with("a", "yy"));
        assert_eq!(out.chars().count(), MAX_OUTPUT_CHARS);
    }
}
