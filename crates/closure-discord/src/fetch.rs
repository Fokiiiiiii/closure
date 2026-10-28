//! Fetches the files of attachments from Discord's CDN over HTTPS (see `closure_core::attachments`).

use closure_core::attachments::{Fetcher, MAX_FILE_BYTES};
use http_body_util::{BodyExt as _, Empty};
use hyper::body::Bytes;
use hyper_util::client::legacy::Client;
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::rt::TokioExecutor;

/// Fetches over HTTPS with the platform's certificates. A redirect is an error, and a body stops at
/// the limit.
pub struct HttpFetcher {
    client: Client<hyper_rustls::HttpsConnector<HttpConnector>, Empty<Bytes>>,
}

impl HttpFetcher {
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
}

impl std::fmt::Debug for HttpFetcher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HttpFetcher").finish_non_exhaustive()
    }
}

impl Default for HttpFetcher {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait::async_trait]
impl Fetcher for HttpFetcher {
    async fn fetch(&self, url: &str) -> Result<Vec<u8>, String> {
        let request = hyper::Request::get(url)
            .body(Empty::<Bytes>::new())
            .map_err(|error| error.to_string())?;
        let response = self
            .client
            .request(request)
            .await
            .map_err(|error| error.to_string())?;
        if !response.status().is_success() {
            return Err(format!("status {}", response.status()));
        }
        let limit = usize::try_from(MAX_FILE_BYTES).unwrap_or(usize::MAX);
        let mut body = response.into_body();
        let mut data = Vec::new();
        while let Some(frame) = body.frame().await {
            let frame = frame.map_err(|error| error.to_string())?;
            if let Some(chunk) = frame.data_ref() {
                if data.len() + chunk.len() > limit {
                    return Err("the file is larger than the limit".into());
                }
                data.extend_from_slice(chunk);
            }
        }
        Ok(data)
    }
}
