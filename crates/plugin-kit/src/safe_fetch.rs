//! Bounded HTTPS GET and JSON POST requests for form and integration endpoints.
use std::{
    future::{Ready, ready},
    io,
    net::{IpAddr, SocketAddr},
    task::{Context, Poll},
    time::Duration,
};

use anyhow::{Result, anyhow, bail};
use http_body_util::{BodyExt as _, Full};
use hyper::{
    Method, Request, Uri,
    body::Bytes,
    header::{HeaderName, HeaderValue},
};
use hyper_util::{
    client::legacy::{Client, connect::HttpConnector, connect::dns::Name},
    rt::TokioExecutor,
};
use tower_service::Service;

const TIMEOUT: Duration = Duration::from_secs(5);
const MAX_BYTES: usize = 256 * 1024;
const MAX_REQUEST_BYTES: usize = 64 * 1024;
const AGENT: &str = "closure-plugins (https://github.com/Fokiiiiiii/closure-plugins)";

/// A bounded GET and JSON POST fetcher for public HTTPS endpoints.
///
/// Requests default to five seconds, with a 256 KiB response and 64 KiB
/// JSON request-body limit.
/// Callers may shorten the deadline.
/// Redirects are rejected, and connections go directly to pinned addresses without
/// reading proxy environment variables. Caller-supplied headers go only to the URL's
/// original host; reserved auth/routing headers are rejected and Bearer auth is separate.
/// URL, header, body, and token values are omitted from errors.
/// No request is retried automatically, although a failed POST may already have run remotely.
#[derive(Debug, Default, Clone, Copy)]
pub struct SafeFetcher;

impl SafeFetcher {
    #[must_use]
    pub const fn new() -> Self {
        Self
    }

    /// Fetch using the default five-second deadline.
    ///
    /// # Errors
    /// The URL is invalid, resolves to a non-public address, cannot be reached
    /// with valid TLS, redirects, times out, or returns more than 256 KiB.
    pub async fn get(&self, url: &str, headers: &[(&str, &str)]) -> Result<Vec<u8>> {
        self.get_with_timeout(url, headers, TIMEOUT).await
    }

    /// Fetch using the caller's deadline, capped at five seconds.
    ///
    /// # Errors
    /// The URL is invalid, resolves to a non-public address, cannot be reached
    /// with valid TLS, redirects, times out, or returns more than 256 KiB.
    pub async fn get_with_timeout(
        &self,
        url: &str,
        headers: &[(&str, &str)],
        within: Duration,
    ) -> Result<Vec<u8>> {
        self.request(Method::GET, url, headers, None, None, within)
            .await
    }

    /// Send a JSON POST to a public HTTPS endpoint.
    ///
    /// The request body is limited to 64 KiB and the response to 256 KiB.
    /// Use custom headers for non-reserved values; authentication uses
    /// `get_with_bearer` or `post_json_with_bearer`.
    ///
    /// # Errors
    /// The URL is invalid, resolves to a non-public address, cannot be reached
    /// with valid TLS, redirects, times out, or exceeds a body limit.
    pub async fn post_json(
        &self,
        url: &str,
        headers: &[(&str, &str)],
        body: &serde_json::Value,
    ) -> Result<Vec<u8>> {
        self.post_json_with_timeout(url, headers, body, TIMEOUT)
            .await
    }

    /// Send a JSON POST with a caller deadline capped at five seconds.
    ///
    /// # Errors
    /// The URL is invalid, resolves to a non-public address, cannot be reached
    /// with valid TLS, redirects, times out, or exceeds a body limit.
    pub async fn post_json_with_timeout(
        &self,
        url: &str,
        headers: &[(&str, &str)],
        body: &serde_json::Value,
        within: Duration,
    ) -> Result<Vec<u8>> {
        self.request(Method::POST, url, headers, Some(body), None, within)
            .await
    }

    /// Send a JSON POST with a server-side Bearer token.
    ///
    /// The token is added as Authorization internally and is not accepted in
    /// the custom header list. URL and token values are omitted from errors.
    ///
    /// # Errors
    /// The URL, bearer token, or headers are invalid, the address is non-public,
    /// the request fails or times out, the endpoint returns an error status,
    /// or a request/response size limit is exceeded.
    pub async fn post_json_with_bearer(
        &self,
        url: &str,
        headers: &[(&str, &str)],
        bearer_token: &str,
        body: &serde_json::Value,
        within: Duration,
    ) -> Result<Vec<u8>> {
        self.request(
            Method::POST,
            url,
            headers,
            Some(body),
            Some(bearer_token),
            within,
        )
        .await
    }

    /// Send a GET with a server-side Bearer token.
    ///
    /// The token is added as Authorization internally and is not accepted in
    /// the custom header list. URL and token values are omitted from errors.
    ///
    /// # Errors
    /// The URL, bearer token, or headers are invalid, the address is non-public,
    /// the request fails or times out, the endpoint returns an error status,
    /// or the response exceeds the size limit.
    pub async fn get_with_bearer(
        &self,
        url: &str,
        headers: &[(&str, &str)],
        bearer_token: &str,
        within: Duration,
    ) -> Result<Vec<u8>> {
        self.request(Method::GET, url, headers, None, Some(bearer_token), within)
            .await
    }

    async fn request(
        &self,
        method: Method,
        url: &str,
        headers: &[(&str, &str)],
        json_body: Option<&serde_json::Value>,
        bearer_token: Option<&str>,
        within: Duration,
    ) -> Result<Vec<u8>> {
        let uri: Uri = url.parse().map_err(|_| anyhow!("invalid HTTPS URL"))?;
        if !uri
            .scheme_str()
            .is_some_and(|scheme| scheme.eq_ignore_ascii_case("https"))
        {
            bail!("only HTTPS URLs are allowed");
        }
        let authority = uri
            .authority()
            .ok_or_else(|| anyhow!("HTTPS URL has no host"))?;
        if authority.as_str().contains('@') {
            bail!("URL userinfo is not allowed");
        }

        let raw_host = uri.host().ok_or_else(|| anyhow!("HTTPS URL has no host"))?;
        let host = raw_host
            .strip_prefix('[')
            .and_then(|without_open| without_open.strip_suffix(']'))
            .unwrap_or(raw_host);
        if host.is_empty() || host.contains('%') {
            bail!("invalid HTTPS host");
        }
        let host = host.to_owned();
        let port = uri.port_u16().unwrap_or(443);
        if port == 0 {
            bail!("invalid HTTPS port");
        }
        let within = within.min(TIMEOUT);

        tokio::time::timeout(within, async move {
            let addresses = resolve_public(&host, port).await?;
            let resolver = PinnedResolver {
                hostname: host,
                addresses,
            };
            let mut http = HttpConnector::new_with_resolver(resolver);
            http.enforce_http(false);
            http.set_connect_timeout(Some(TIMEOUT));
            let connector = hyper_rustls::HttpsConnectorBuilder::new()
                .with_platform_verifier()
                .https_only()
                .enable_http1()
                .wrap_connector(http);
            let mut client_builder = Client::builder(TokioExecutor::new());
            client_builder.retry_canceled_requests(false);
            let client = client_builder.build(connector);

            let body = if let Some(body) = json_body {
                serde_json::to_vec(body).map_err(|_| anyhow!("invalid JSON request body"))?
            } else {
                Vec::new()
            };
            if body.len() > MAX_REQUEST_BYTES {
                bail!("JSON request body exceeds the size limit");
            }
            if bearer_token.is_some_and(str::is_empty) {
                bail!("Bearer token is empty");
            }

            let mut request_builder = Request::builder()
                .method(method)
                .uri(uri)
                .header("user-agent", AGENT)
                .header("accept", "*/*");
            if json_body.is_some() {
                request_builder = request_builder.header("content-type", "application/json");
            }
            if let Some(token) = bearer_token {
                request_builder =
                    request_builder.header("authorization", format!("Bearer {token}"));
            }
            let mut request = request_builder
                .body(Full::new(Bytes::from(body)))
                .map_err(|_| anyhow!("invalid HTTPS request"))?;
            for (name, value) in headers {
                let name = HeaderName::from_bytes(name.as_bytes())
                    .map_err(|_| anyhow!("invalid request header"))?;
                if is_forbidden_header(name.as_str()) {
                    bail!("unsupported request header");
                }
                let value = HeaderValue::from_bytes(value.as_bytes())
                    .map_err(|_| anyhow!("invalid request header"))?;
                request.headers_mut().insert(name, value);
            }

            let response = client
                .request(request)
                .await
                .map_err(|_| anyhow!("HTTPS request failed"))?;
            if !response.status().is_success() {
                bail!(
                    "HTTPS endpoint returned status {}",
                    response.status().as_u16()
                );
            }

            let mut response_body = response.into_body();
            let mut data = Vec::new();
            while let Some(frame) = response_body.frame().await {
                let frame = frame.map_err(|_| anyhow!("failed reading HTTPS response"))?;
                if let Some(chunk) = frame.data_ref() {
                    if chunk.len() > MAX_BYTES.saturating_sub(data.len()) {
                        bail!("HTTPS response exceeds the size limit");
                    }
                    data.extend_from_slice(chunk);
                }
            }
            Ok(data)
        })
        .await
        .map_err(|_| anyhow!("HTTPS request timed out"))?
    }
}

#[derive(Clone)]
struct PinnedResolver {
    hostname: String,
    addresses: Vec<SocketAddr>,
}

impl Service<Name> for PinnedResolver {
    type Response = std::vec::IntoIter<SocketAddr>;
    type Error = io::Error;
    type Future = Ready<std::result::Result<Self::Response, Self::Error>>;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<std::result::Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, name: Name) -> Self::Future {
        if name.as_str().eq_ignore_ascii_case(&self.hostname) {
            ready(Ok(self.addresses.clone().into_iter()))
        } else {
            ready(Err(io::Error::other(
                "DNS name did not match the request host",
            )))
        }
    }
}

async fn resolve_public(host: &str, port: u16) -> Result<Vec<SocketAddr>> {
    let addresses = if let Ok(ip) = host.parse::<IpAddr>() {
        vec![SocketAddr::new(ip, port)]
    } else {
        tokio::net::lookup_host((host, port))
            .await
            .map_err(|_| anyhow!("DNS lookup failed"))?
            .map(|address| SocketAddr::new(address.ip(), port))
            .collect()
    };
    if addresses.is_empty() {
        bail!("DNS lookup returned no addresses");
    }
    if addresses.iter().any(|address| !is_public_ip(address.ip())) {
        bail!("DNS lookup returned a non-public address");
    }
    Ok(addresses)
}

fn is_public_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(address) => is_public_ipv4(address),
        IpAddr::V6(address) => is_public_ipv6(address),
    }
}

fn is_public_ipv4(address: std::net::Ipv4Addr) -> bool {
    let [first, second, third, _] = address.octets();
    // shortcut: special-use ranges can change, refresh this list when IANA's registry changes.
    !(address.is_unspecified()
        || address.is_private()
        || address.is_loopback()
        || address.is_link_local()
        || address.is_broadcast()
        || address.is_multicast()
        || first == 0
        || (first == 100 && (64..=127).contains(&second))
        || (first == 192 && second == 0 && (third == 0 || third == 2))
        || (first == 192 && second == 88 && third == 99)
        || (first == 198 && (second == 18 || second == 19))
        || (first == 198 && second == 51 && third == 100)
        || (first == 203 && second == 0 && third == 113)
        || first >= 240)
}

fn is_public_ipv6(address: std::net::Ipv6Addr) -> bool {
    let segments = address.segments();
    // shortcut: global-unicast exclusions are explicit; refresh this list when IANA ranges change.
    (segments[0] & 0xe000) == 0x2000
        && !address.is_unspecified()
        && !address.is_loopback()
        && !address.is_multicast()
        && !address.is_unique_local()
        && !address.is_unicast_link_local()
        && address.to_ipv4_mapped().is_none()
        && !(segments[0] == 0x2001 && (segments[1] & 0xfe00) == 0)
        && !(segments[0] == 0x2001 && segments[1] == 0x0db8)
        && segments[0] != 0x2002
        && !(segments[0] == 0x3fff && (segments[1] & 0xfff0) == 0)
}

fn is_forbidden_header(name: &str) -> bool {
    matches!(
        name,
        "authorization"
            | "cookie"
            | "host"
            | "user-agent"
            | "content-type"
            | "content-length"
            | "connection"
            | "forwarded"
            | "keep-alive"
            | "te"
            | "trailer"
            | "transfer-encoding"
            | "upgrade"
            | "via"
    ) || name.starts_with("proxy-")
        || name.starts_with("x-forwarded-")
        || name.starts_with("x-proxy-")
        || name.starts_with("cf-")
}

#[cfg(test)]
mod tests {
    use super::is_public_ip;
    use std::net::IpAddr;

    #[test]
    fn public_address_filter_blocks_special_use_ranges() {
        for address in [
            "0.1.2.3",
            "10.0.0.1",
            "100.64.0.1",
            "127.0.0.1",
            "169.254.1.1",
            "192.0.0.9",
            "192.0.2.1",
            "198.18.0.1",
            "203.0.113.1",
            "240.0.0.1",
            "::1",
            "fc00::1",
            "fe80::1",
            "ff02::1",
            "2001:db8::1",
            "2002::1",
            "3fff::1",
            "::ffff:127.0.0.1",
            "::ffff:8.8.8.8",
        ] {
            assert!(
                !is_public_ip(address.parse::<IpAddr>().unwrap()),
                "{address}"
            );
        }
        assert!(is_public_ip("8.8.8.8".parse().unwrap()));
        assert!(is_public_ip("2606:4700:4700::1111".parse().unwrap()));
    }
}
