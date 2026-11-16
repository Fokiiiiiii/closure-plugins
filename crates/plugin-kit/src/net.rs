//! Reading a page over HTTPS, for the services the plugins ask: `FxTwitter`, and the translator.

use async_trait::async_trait;
use serde_json::Value;

use crate::safe_fetch::SafeFetcher;

use std::time::Duration;

use anyhow::{Context as _, Result, bail};
use http_body_util::{BodyExt as _, Full};
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
/// The only headers a service is sent on request: a session cookie, for the services that need one.
const FORWARDED_HEADERS: &[&str] = &["cookie"];

/// Where the module reads a page from; a test puts its own in place of the network.
#[async_trait]
pub trait Get: Send + Sync {
    /// The body of `url`.
    /// # Errors
    /// The service is out of reach, slow, or answers with something else than a small success.
    async fn get(&self, url: &str) -> Result<Vec<u8>>;

    /// The body of `url`, a picture or a video, of at most `max` bytes.
    /// # Errors
    /// The file is out of reach, slow, or larger than `max`.
    async fn get_large(&self, url: &str, max: usize) -> Result<Vec<u8>> {
        let body = self.get(url).await?;
        if body.len() > max {
            bail!("the file is larger than the limit");
        }
        Ok(body)
    }

    /// The body of `url`, a picture of at most `max` bytes, sent with `referer` as its `Referer`
    /// when the file server needs one.
    /// # Errors
    /// The file is out of reach, slow, or larger than `max`.
    async fn get_media(&self, url: &str, max: usize, _referer: Option<&str>) -> Result<Vec<u8>> {
        self.get_large(url, max).await
    }

    /// The body of `url`, sent with `headers`. Only the headers in [`FORWARDED_HEADERS`] are sent;
    /// the default sends none and reads as [`Get::get`].
    /// # Errors
    /// As [`Get::get`].
    async fn get_with_headers(&self, url: &str, _headers: &[(&str, &str)]) -> Result<Vec<u8>> {
        self.get(url).await
    }
}

/// Where the module sends a JSON request; a test puts its own in place of the network.
#[async_trait]
pub trait Post: Send + Sync {
    /// The body of a JSON POST to `url`, with `headers` added.
    /// # Errors
    /// The service is out of reach, slow, answers with an error status (see [`HttpStatus`]),
    /// or its answer is larger than the limit.
    async fn post_json(&self, url: &str, headers: &[(&str, &str)], body: &Value)
    -> Result<Vec<u8>>;
}

/// Both ways to reach the network, for the code that uses both.
pub trait Http: Get + Post {}

impl<T: Get + Post + ?Sized> Http for T {}

/// The status a service answered with, when it is not a success.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HttpStatus(pub u16);

impl std::fmt::Display for HttpStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "status {}", self.0)
    }
}

impl std::error::Error for HttpStatus {}

/// The calls one event may make to the network: a post with a long thread of quotes
/// cannot hold the handler for as long as it takes to follow all.
pub struct Budget<'a> {
    inner: Inner<'a>,
    left: std::sync::atomic::AtomicUsize,
}

#[derive(Clone, Copy)]
enum Inner<'a> {
    Get(&'a dyn Get),
    Http(&'a dyn Http),
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
            inner: Inner::Get(inner),
            left: std::sync::atomic::AtomicUsize::new(calls),
        }
    }

    /// Like [`Budget::new`], for the code that also posts; a post spends a call too.
    #[must_use]
    pub fn with_http(inner: &'a dyn Http, calls: usize) -> Self {
        Self {
            inner: Inner::Http(inner),
            left: std::sync::atomic::AtomicUsize::new(calls),
        }
    }

    fn get_inner(&self) -> &dyn Get {
        match self.inner {
            Inner::Get(get) => get,
            Inner::Http(http) => http,
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
        self.get_inner().get(url).await
    }

    async fn get_large(&self, url: &str, max: usize) -> Result<Vec<u8>> {
        self.spend()?;
        self.get_inner().get_large(url, max).await
    }

    async fn get_media(&self, url: &str, max: usize, referer: Option<&str>) -> Result<Vec<u8>> {
        self.spend()?;
        self.get_inner().get_media(url, max, referer).await
    }

    async fn get_with_headers(&self, url: &str, headers: &[(&str, &str)]) -> Result<Vec<u8>> {
        self.spend()?;
        self.get_inner().get_with_headers(url, headers).await
    }
}

#[async_trait]
impl Post for Budget<'_> {
    async fn post_json(
        &self,
        url: &str,
        headers: &[(&str, &str)],
        body: &Value,
    ) -> Result<Vec<u8>> {
        let Inner::Http(http) = self.inner else {
            bail!("this budget has no POST");
        };
        self.spend()?;
        http.post_json(url, headers, body).await
    }
}

pub struct Fetcher {
    client: Client<hyper_rustls::HttpsConnector<HttpConnector>, Full<Bytes>>,
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
    async fn fetch(
        &self,
        url: &str,
        max: usize,
        within: Duration,
        referer: Option<&str>,
        headers: &[(&str, &str)],
    ) -> Result<Vec<u8>> {
        let mut builder = hyper::Request::get(url)
            .header("user-agent", AGENT)
            .header("accept", "*/*");
        if let Some(referer) = referer {
            builder = builder.header("referer", referer);
        }
        for (name, value) in headers {
            if FORWARDED_HEADERS
                .iter()
                .any(|allowed| name.eq_ignore_ascii_case(allowed))
            {
                builder = builder.header(*name, *value);
            }
        }
        let request = builder.body(Full::new(Bytes::new()))?;
        self.send(request, max, within).await
    }

    /// Sends `request` and reads the body; an error status is an [`HttpStatus`] error.
    async fn send(
        &self,
        request: hyper::Request<Full<Bytes>>,
        max: usize,
        within: Duration,
    ) -> Result<Vec<u8>> {
        tokio::time::timeout(within, async {
            let response = self.client.request(request).await.context("asking")?;
            if !response.status().is_success() {
                return Err(anyhow::Error::new(HttpStatus(response.status().as_u16())));
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
        self.fetch(url, MAX_BYTES, TIMEOUT, None, &[]).await
    }

    async fn get_large(&self, url: &str, max: usize) -> Result<Vec<u8>> {
        self.fetch(url, max, LARGE_TIMEOUT, None, &[]).await
    }

    async fn get_media(&self, url: &str, max: usize, referer: Option<&str>) -> Result<Vec<u8>> {
        self.fetch(url, max, LARGE_TIMEOUT, referer, &[]).await
    }

    async fn get_with_headers(&self, url: &str, headers: &[(&str, &str)]) -> Result<Vec<u8>> {
        self.fetch(url, MAX_BYTES, TIMEOUT, None, headers).await
    }
}

#[async_trait]
impl Post for Fetcher {
    /// Sent by [`SafeFetcher`]: HTTPS only, public addresses only, and a pinned connection.
    async fn post_json(
        &self,
        url: &str,
        headers: &[(&str, &str)],
        body: &Value,
    ) -> Result<Vec<u8>> {
        SafeFetcher::new().post_json(url, headers, body).await
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

    #[tokio::test]
    async fn a_budget_spends_a_call_on_a_get_with_headers() {
        let inner = Counting(AtomicUsize::new(0));
        let budget = Budget::new(&inner, 1);
        assert!(
            budget
                .get_with_headers("a", &[("cookie", "x=1")])
                .await
                .is_ok()
        );
        assert!(budget.get_with_headers("b", &[]).await.is_err());
        assert_eq!(inner.0.load(Ordering::Relaxed), 1);
    }

    struct Both(AtomicUsize);

    #[async_trait]
    impl Get for Both {
        async fn get(&self, _url: &str) -> Result<Vec<u8>> {
            self.0.fetch_add(1, Ordering::Relaxed);
            Ok(Vec::new())
        }
    }

    #[async_trait]
    impl Post for Both {
        async fn post_json(
            &self,
            _url: &str,
            _headers: &[(&str, &str)],
            _body: &Value,
        ) -> Result<Vec<u8>> {
            self.0.fetch_add(1, Ordering::Relaxed);
            Ok(Vec::new())
        }
    }

    #[tokio::test]
    async fn a_budget_with_http_counts_posts_with_gets_and_refuses_past_its_calls() {
        let inner = Both(AtomicUsize::new(0));
        let budget = Budget::with_http(&inner, 2);
        let body = serde_json::json!({});
        assert!(
            budget
                .post_json("https://a.example/", &[], &body)
                .await
                .is_ok()
        );
        assert!(budget.get("https://b.example/").await.is_ok());
        assert!(
            budget
                .post_json("https://c.example/", &[], &body)
                .await
                .is_err()
        );
        assert_eq!(inner.0.load(Ordering::Relaxed), 2);
    }

    #[tokio::test]
    async fn a_budget_without_http_refuses_a_post_and_spends_no_call_on_it() {
        let inner = Counting(AtomicUsize::new(0));
        let budget = Budget::new(&inner, 1);
        let body = serde_json::json!({});
        assert!(
            budget
                .post_json("https://a.example/", &[], &body)
                .await
                .is_err()
        );
        assert!(budget.get("https://b.example/").await.is_ok());
        assert_eq!(inner.0.load(Ordering::Relaxed), 1);
    }
}

#[cfg(test)]
mod post_tests {
    use super::*;

    #[tokio::test]
    async fn a_post_to_a_non_public_address_is_refused_before_connecting() {
        let error = Fetcher::new()
            .post_json("https://127.0.0.1/", &[], &serde_json::json!({}))
            .await
            .unwrap_err();
        assert!(error.to_string().contains("non-public"), "{error}");
    }

    #[tokio::test]
    async fn a_post_without_https_is_refused() {
        let error = Fetcher::new()
            .post_json("http://93.184.216.34/", &[], &serde_json::json!({}))
            .await
            .unwrap_err();
        assert!(error.to_string().contains("only HTTPS"), "{error}");
    }
}
