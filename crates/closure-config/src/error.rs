use std::fmt;

/// One problem found in a configuration document.
///
/// `code` is stable and language independent so a dashboard can translate it; `message` is an
/// English description for logs and API clients.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Issue {
    /// Dotted path into the document, e.g. `modules.warn.config.limit`.
    pub path: String,
    pub code: &'static str,
    pub message: String,
}

impl Issue {
    pub(crate) fn new(
        path: impl Into<String>,
        code: &'static str,
        message: impl Into<String>,
    ) -> Self {
        Self {
            path: path.into(),
            code,
            message: message.into(),
        }
    }
}

impl fmt::Display for Issue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.path.is_empty() {
            f.write_str(&self.message)
        } else {
            write!(f, "{}: {}", self.path, self.message)
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("configuration is {size} bytes, over the {limit} byte limit")]
    TooLarge { size: usize, limit: usize },
    /// The YAML is malformed, or does not match the schema. Line and column are 1-based.
    #[error("{message}")]
    Syntax {
        message: String,
        line: Option<u64>,
        column: Option<u64>,
    },
    #[error("unsupported configuration version {found}; this build supports {supported}")]
    UnsupportedVersion { found: u32, supported: u32 },
    #[error("configuration is invalid ({} problem(s)): {}", .0.len(), join(.0))]
    Invalid(Vec<Issue>),
    #[error("module `{module}` has an unusable config schema: {message}")]
    BadModuleSchema { module: String, message: String },
    #[error("could not write configuration: {0}")]
    Serialize(String),
}

fn join(issues: &[Issue]) -> String {
    issues
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("; ")
}

impl ConfigError {
    /// The individual problems, when the error is a validation failure.
    #[must_use]
    pub fn issues(&self) -> &[Issue] {
        match self {
            Self::Invalid(issues) => issues,
            _ => &[],
        }
    }
}
