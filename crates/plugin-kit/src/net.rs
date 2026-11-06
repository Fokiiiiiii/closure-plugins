//! Reading a page over HTTPS, for the services the plugins ask: `FxTwitter`, and the translator.

use async_trait::async_trait;

use std::time::Duration;

use anyhow::{Context as _, Result, bail};
use http_body_util::{BodyExt as _, Empty};
use hyper::body::Bytes;
use hyper_util::client::legacy::Client;
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::rt::TokioExecutor;

/// How long the service may take: a reply waits for it.
const TIMEOUT: Duration = Duration::from_secs(5);
/// A post's answer is a few kilobytes; more than this is not one.
const MAX_BYTES: usize = 256 * 1024;
const AGENT: &str = "unfurl (https://github.com/Fokiiiiiii/unfurl)";

/// Where the module reads a page from; a test puts its own in place of the network.
#[async_trait]
pub trait Get: Send + Sync {
    /// The body of `url`.
    ///
    /// # Errors
    /// The service is out of reach, slow, or answers with something else than a small success.
    async fn get(&self, url: &str) -> Result<Vec<u8>>;
}

pub struct Fetcher {
    client: Client<hyper_rustls::HttpsConnector<HttpConnector>, Empty<Bytes>>,
}

impl Fetcher {
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

    /// The body of `url`. A redirect, an error status and a body past the limit are errors.
    async fn fetch(&self, url: &str) -> Result<Vec<u8>> {
        let request = hyper::Request::get(url)
            .header("user-agent", AGENT)
            .header("accept", "application/json")
            .body(Empty::<Bytes>::new())?;
        tokio::time::timeout(TIMEOUT, async {
            let response = self.client.request(request).await.context("asking")?;
            if !response.status().is_success() {
                bail!("status {}", response.status());
            }
            let mut body = response.into_body();
            let mut data = Vec::new();
            while let Some(frame) = body.frame().await {
                if let Some(chunk) = frame.context("reading")?.data_ref() {
                    if data.len() + chunk.len() > MAX_BYTES {
                        bail!("the answer is larger than the limit");
                    }
                    data.extend_from_slice(chunk);
                }
            }
            Ok(data)
        })
        .await
        .context("the service did not answer in time")?
    }
}

#[async_trait]
impl Get for Fetcher {
    async fn get(&self, url: &str) -> Result<Vec<u8>> {
        self.fetch(url).await
    }
}

impl std::fmt::Debug for Fetcher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Fetcher").finish_non_exhaustive()
    }
}

impl Default for Fetcher {
    fn default() -> Self {
        Self::new()
    }
}
