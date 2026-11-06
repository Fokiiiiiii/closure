//! Asking a service whether the links in a message are unsafe. The asking itself is the host's
//! business ([`LinkChecker`]); what a module does with the answer is the module's.

use async_trait::async_trait;

/// What a service said about the links in a text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LinkVerdict {
    Safe,
    /// The service flagged a link; the kind it gave, such as `PHISHING`.
    Unsafe(String),
}

#[async_trait]
pub trait LinkChecker: Send + Sync {
    /// Checks the links of `text`. An error is the service being out of reach, not a verdict.
    async fn check(&self, text: &str) -> Result<LinkVerdict, String>;
}
