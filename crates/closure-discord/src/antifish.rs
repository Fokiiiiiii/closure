//! Asks the `AntiFish` service (<https://anti-fish.bitflow.dev>) whether the links of a message are
//! phishing or scams. It needs no key; the host and the path are the only things sent besides the
//! links themselves.

use std::time::Duration;

use closure_core::links::{LinkChecker, LinkVerdict};
use http_body_util::{BodyExt as _, Full};
use hyper::body::Bytes;
use hyper_util::client::legacy::Client;
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::rt::TokioExecutor;
use serde::Deserialize;

const ENDPOINT: &str = "https://anti-fish.bitflow.dev/check";
const AGENT: &str = "closure (https://github.com/Fokiiiiiii/closure)";
/// A check holds up the message it is for, so it is short; a message that waits longer goes by.
const TIMEOUT: Duration = Duration::from_secs(4);
const MAX_ANSWER_BYTES: usize = 64 * 1024;

#[derive(Deserialize)]
struct Answer {
    #[serde(default, rename = "match")]
    matched: bool,
    #[serde(default)]
    matches: Vec<Found>,
}

#[derive(Deserialize)]
struct Found {
    #[serde(default, rename = "type")]
    kind: String,
}

pub struct AntiFish {
    client: Client<hyper_rustls::HttpsConnector<HttpConnector>, Full<Bytes>>,
}

impl AntiFish {
    #[must_use]
    pub fn new() -> Self {
        let connector = hyper_rustls::HttpsConnectorBuilder::new()
            .with_platform_verifier()
            .https_only()
            .enable_http1()
            .build();
        Self {
            client: Client::builder(TokioExecutor::new()).build(connector),
        }
    }

    async fn ask(&self, text: &str) -> Result<LinkVerdict, String> {
        let body = serde_json::json!({ "message": text }).to_string();
        let request = hyper::Request::post(ENDPOINT)
            .header("content-type", "application/json")
            .header("user-agent", AGENT)
            .body(Full::new(Bytes::from(body)))
            .map_err(|error| error.to_string())?;
        let response = self
            .client
            .request(request)
            .await
            .map_err(|error| error.to_string())?;
        let status = response.status();
        // The service answers 404 with `{"match":false}` when nothing was found.
        if !status.is_success() && status != hyper::StatusCode::NOT_FOUND {
            return Err(format!("status {status}"));
        }
        let mut body = response.into_body();
        let mut data = Vec::new();
        while let Some(frame) = body.frame().await {
            let frame = frame.map_err(|error| error.to_string())?;
            if let Some(chunk) = frame.data_ref() {
                if data.len() + chunk.len() > MAX_ANSWER_BYTES {
                    return Err("the answer is larger than the limit".into());
                }
                data.extend_from_slice(chunk);
            }
        }
        let answer: Answer = serde_json::from_slice(&data).map_err(|error| error.to_string())?;
        Ok(if answer.matched {
            LinkVerdict::Unsafe(
                answer
                    .matches
                    .into_iter()
                    .map(|found| found.kind)
                    .find(|kind| !kind.is_empty())
                    .unwrap_or_else(|| "PHISHING".to_owned()),
            )
        } else {
            LinkVerdict::Safe
        })
    }
}

impl std::fmt::Debug for AntiFish {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AntiFish").finish_non_exhaustive()
    }
}

impl Default for AntiFish {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait::async_trait]
impl LinkChecker for AntiFish {
    async fn check(&self, text: &str) -> Result<LinkVerdict, String> {
        tokio::time::timeout(TIMEOUT, self.ask(text))
            .await
            .map_err(|_| "the service did not answer in time".to_owned())?
    }
}

#[cfg(test)]
mod tests {
    use super::Answer;

    #[test]
    fn the_answer_of_the_service_is_read() {
        let hit: Answer = serde_json::from_str(
            r#"{"match":true,"matches":[{"followed":false,"domain":"x.gift","type":"PHISHING"}]}"#,
        )
        .unwrap();
        assert!(hit.matched && hit.matches[0].kind == "PHISHING");
        let miss: Answer = serde_json::from_str(r#"{"match":false}"#).unwrap();
        assert!(!miss.matched && miss.matches.is_empty());
    }
}
