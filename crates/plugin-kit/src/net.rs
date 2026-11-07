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
/// How long a picture or a video may take.
const LARGE_TIMEOUT: Duration = Duration::from_secs(20);
const AGENT: &str = "closure-plugins (https://github.com/Fokiiiiiii/closure-plugins)";

/// Where the module reads a page from; a test puts its own in place of the network.
#[async_trait]
pub trait Get: Send + Sync {
    /// The body of `url`.
    ///
    /// # Errors
    /// The service is out of reach, slow, or answers with something else than a small success.
    async fn get(&self, url: &str) -> Result<Vec<u8>>;

    /// The body of `url`, a picture or a video, of at most `max` bytes.
    ///
    /// # Errors
    /// The file is out of reach, slow, or larger than `max`.
    async fn get_large(&self, url: &str, max: usize) -> Result<Vec<u8>> {
        let body = self.get(url).await?;
        if body.len() > max {
            bail!("the file is larger than the limit");
        }
        Ok(body)
    }
}

/// A number of calls to another [`Get`] and no more: what one event may ask of the network, so a
/// post with a long thread of quotes cannot hold the handler for as long as it takes to follow all.
pub struct Budget<'a> {
    inner: &'a dyn Get,
    left: std::sync::atomic::AtomicUsize,
}

impl std::fmt::Debug for Budget<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Budget").finish_non_exhaustive()
    }
}

impl<'a> Budget<'a> {
    #[must_use]
    pub fn new(inner: &'a dyn Get, calls: usize) -> Self {
        Self {
            inner,
            left: std::sync::atomic::AtomicUsize::new(calls),
        }
    }

    fn spend(&self) -> Result<()> {
        use std::sync::atomic::Ordering;
        self.left
            .try_update(Ordering::AcqRel, Ordering::Acquire, |left| {
                left.checked_sub(1)
            })
            .map(drop)
            .map_err(|_| anyhow::anyhow!("the calls this event may make are used up"))
    }
}

#[async_trait]
impl Get for Budget<'_> {
    async fn get(&self, url: &str) -> Result<Vec<u8>> {
        self.spend()?;
        self.inner.get(url).await
    }

    async fn get_large(&self, url: &str, max: usize) -> Result<Vec<u8>> {
        self.spend()?;
        self.inner.get_large(url, max).await
    }
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
    async fn fetch(&self, url: &str, max: usize, within: Duration) -> Result<Vec<u8>> {
        let request = hyper::Request::get(url)
            .header("user-agent", AGENT)
            .header("accept", "*/*")
            .body(Empty::<Bytes>::new())?;
        tokio::time::timeout(within, async {
            let response = self.client.request(request).await.context("asking")?;
            if !response.status().is_success() {
                bail!("status {}", response.status());
            }
            let mut body = response.into_body();
            let mut data = Vec::new();
            while let Some(frame) = body.frame().await {
                if let Some(chunk) = frame.context("reading")?.data_ref() {
                    if data.len() + chunk.len() > max {
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
        self.fetch(url, MAX_BYTES, TIMEOUT).await
    }

    async fn get_large(&self, url: &str, max: usize) -> Result<Vec<u8>> {
        self.fetch(url, max, LARGE_TIMEOUT).await
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

#[cfg(test)]
mod budget_tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    struct Counting(AtomicUsize);

    #[async_trait]
    impl Get for Counting {
        async fn get(&self, _url: &str) -> Result<Vec<u8>> {
            self.0.fetch_add(1, Ordering::Relaxed);
            Ok(Vec::new())
        }
    }

    #[tokio::test]
    async fn a_budget_lets_through_its_calls_and_no_more() {
        let inner = Counting(AtomicUsize::new(0));
        let budget = Budget::new(&inner, 2);
        assert!(budget.get("a").await.is_ok());
        assert!(budget.get_large("b", 10).await.is_ok());
        assert!(budget.get("c").await.is_err());
        assert_eq!(inner.0.load(Ordering::Relaxed), 2);
    }
}
