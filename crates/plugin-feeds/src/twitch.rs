//! Twitch: which channels are live now, read from the Helix API with the operator's app credentials.

use std::sync::Mutex;

use anyhow::{Context as _, Error, Result, anyhow};
use plugin_kit::net::{Http, HttpStatus};
use plugin_kit::translator::ApiKey;
use serde::Deserialize;
use serde_json::json;

use crate::feeds::lock;
use crate::source::Item;

const TOKEN_URL: &str = "https://id.twitch.tv/oauth2/token";
const STREAMS_URL: &str = "https://api.twitch.tv/helix/streams";
/// A token is asked again this long before it runs out, so a read never sends one that has.
const RENEW_BEFORE_SECS: i64 = 300;

/// The Twitch app the operator set up. Neither value is ever printed.
#[derive(Debug, Clone)]
pub struct TwitchCredentials {
    pub client_id: ApiKey,
    pub client_secret: ApiKey,
}

#[derive(Debug)]
struct Token {
    key: ApiKey,
    renew_at: i64,
}

/// Reads the live streams with one app token, kept until it nearly runs out.
#[derive(Debug)]
pub struct Twitch {
    credentials: TwitchCredentials,
    cached: Mutex<Option<Token>>,
}

#[derive(Deserialize)]
struct TokenReply {
    access_token: String,
    expires_in: i64,
}

#[derive(Deserialize)]
struct StreamsReply {
    data: Vec<Stream>,
}

#[derive(Deserialize)]
struct Stream {
    id: String,
    user_login: String,
    title: String,
    #[serde(rename = "type")]
    kind: String,
}

/// The address the live streams of `login` are read from.
#[must_use]
pub fn streams_url(login: &str) -> String {
    format!("{STREAMS_URL}?user_login={login}")
}

impl Twitch {
    #[must_use]
    pub fn new(credentials: TwitchCredentials) -> Self {
        Self {
            credentials,
            cached: Mutex::new(None),
        }
    }

    /// The live stream of `login`, as one item, or none when it is offline.
    ///
    /// # Errors
    /// The token or the streams cannot be read.
    pub async fn read(&self, http: &dyn Http, now: i64, login: &str) -> Result<Vec<Item>> {
        let token = self.token(http, now).await?;
        match streams(http, &self.credentials.client_id, &token, login).await {
            Err(error) if is_unauthorized(&error) => {
                *lock(&self.cached) = None;
                let token = self.token(http, now).await?;
                streams(http, &self.credentials.client_id, &token, login).await
            }
            answer => answer,
        }
    }

    /// The kept token while it is fresh; otherwise a new one. Two reads at once may both ask for
    /// one; the lock is never held across the request.
    async fn token(&self, http: &dyn Http, now: i64) -> Result<ApiKey> {
        let kept = lock(&self.cached)
            .as_ref()
            .filter(|token| now < token.renew_at)
            .map(|token| token.key.clone());
        if let Some(key) = kept {
            return Ok(key);
        }
        let fresh = request_token(http, &self.credentials, now).await?;
        let key = fresh.key.clone();
        *lock(&self.cached) = Some(fresh);
        Ok(key)
    }
}

/// # Errors
/// The token service is out of reach or refuses the app. The error names no address and no secret.
async fn request_token(
    http: &dyn Http,
    credentials: &TwitchCredentials,
    now: i64,
) -> Result<Token> {
    let url = format!(
        "{TOKEN_URL}?client_id={}&client_secret={}&grant_type=client_credentials",
        credentials.client_id.expose(),
        credentials.client_secret.expose()
    );
    let bytes = http
        .post_json(&url, &[], &json!({}))
        .await
        .map_err(|error| token_failure(&error))?;
    let reply: TokenReply = serde_json::from_slice(&bytes)
        .context("the Twitch token answer is not what it should be")?;
    Ok(Token {
        key: ApiKey::new(reply.access_token),
        renew_at: now.saturating_add(reply.expires_in.saturating_sub(RENEW_BEFORE_SECS)),
    })
}

fn token_failure(error: &Error) -> Error {
    match error.downcast_ref::<HttpStatus>() {
        Some(status) => anyhow!("the Twitch token request failed: {status}"),
        None => anyhow!("the Twitch token request failed"),
    }
}

/// The live streams of `login`: one item when it is live, none when it is not.
async fn streams(
    http: &dyn Http,
    client_id: &ApiKey,
    token: &ApiKey,
    login: &str,
) -> Result<Vec<Item>> {
    let bearer = format!("Bearer {}", token.expose());
    let headers = [
        ("authorization", bearer.as_str()),
        ("client-id", client_id.expose()),
    ];
    let bytes = http.get_with_headers(&streams_url(login), &headers).await?;
    let reply: StreamsReply = serde_json::from_slice(&bytes)
        .context("the Twitch streams answer is not what it should be")?;
    Ok(reply
        .data
        .into_iter()
        .filter(|stream| stream.kind == "live" && is_login(&stream.user_login))
        .map(|stream| Item {
            url: format!("https://www.twitch.tv/{}", stream.user_login),
            id: stream.id,
            title: stream.title,
        })
        .collect())
}

fn is_unauthorized(error: &Error) -> bool {
    error.downcast_ref::<HttpStatus>() == Some(&HttpStatus(401))
}

/// Lower-case letters, digits and `_`: the only logins a link is built from.
fn is_login(text: &str) -> bool {
    !text.is_empty()
        && text
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use anyhow::bail;
    use async_trait::async_trait;
    use plugin_kit::net::{Get, Post};
    use serde_json::Value;

    use super::*;

    const CLIENT_ID: &str = "client-id-abc";
    const SECRET: &str = "client-secret-xyz";
    const TOKEN_BODY: &str = r#"{"access_token":"tok-1","expires_in":3600,"token_type":"bearer"}"#;
    const LIVE_BODY: &str = r#"{"data":[{"id":"42","user_login":"someone","user_name":"Someone","title":"Hello","type":"live","started_at":"2026-01-01T00:00:00Z"}]}"#;

    /// `Err` is the HTTP status the service answers with.
    type Answer = Result<Vec<u8>, u16>;

    /// Answers the token and the streams requests in turn, and records what was asked.
    struct Stub {
        token: Answer,
        streams: Mutex<VecDeque<Answer>>,
        token_asks: AtomicUsize,
        bearers: Mutex<Vec<String>>,
    }

    impl Stub {
        fn new(token: Answer, streams: Vec<Answer>) -> Self {
            Self {
                token,
                streams: Mutex::new(streams.into()),
                token_asks: AtomicUsize::new(0),
                bearers: Mutex::new(Vec::new()),
            }
        }
    }

    fn status(code: u16) -> anyhow::Error {
        HttpStatus(code).into()
    }

    #[async_trait]
    impl Get for Stub {
        async fn get(&self, _url: &str) -> Result<Vec<u8>> {
            bail!("the Twitch streams are always asked with their headers")
        }

        async fn get_with_headers(&self, _url: &str, headers: &[(&str, &str)]) -> Result<Vec<u8>> {
            let bearer = headers
                .iter()
                .find(|(name, _)| *name == "authorization")
                .map(|(_, value)| (*value).to_owned())
                .unwrap_or_default();
            self.bearers.lock().unwrap().push(bearer);
            let next = self.streams.lock().unwrap().pop_front().unwrap_or(Err(500));
            next.map_err(status)
        }
    }

    #[async_trait]
    impl Post for Stub {
        async fn post_json(
            &self,
            _url: &str,
            _headers: &[(&str, &str)],
            _body: &Value,
        ) -> Result<Vec<u8>> {
            self.token_asks.fetch_add(1, Ordering::Relaxed);
            self.token.clone().map_err(status)
        }
    }

    fn credentials() -> TwitchCredentials {
        TwitchCredentials {
            client_id: ApiKey::new(CLIENT_ID),
            client_secret: ApiKey::new(SECRET),
        }
    }

    fn item(id: &str, title: &str, login: &str) -> Item {
        Item {
            id: id.to_owned(),
            title: title.to_owned(),
            url: format!("https://www.twitch.tv/{login}"),
        }
    }

    #[tokio::test]
    async fn a_live_stream_is_one_item_read_with_the_app_token() {
        let stub = Stub::new(
            Ok(TOKEN_BODY.as_bytes().to_vec()),
            vec![Ok(LIVE_BODY.as_bytes().to_vec())],
        );
        let twitch = Twitch::new(credentials());
        let items = twitch.read(&stub, 0, "someone").await.unwrap();
        assert_eq!(items, vec![item("42", "Hello", "someone")]);
        assert_eq!(stub.bearers.lock().unwrap().as_slice(), ["Bearer tok-1"]);
    }

    #[tokio::test]
    async fn an_offline_channel_has_no_items() {
        let stub = Stub::new(
            Ok(TOKEN_BODY.as_bytes().to_vec()),
            vec![Ok(br#"{"data":[]}"#.to_vec())],
        );
        let items = Twitch::new(credentials())
            .read(&stub, 0, "someone")
            .await
            .unwrap();
        assert!(items.is_empty());
    }

    #[tokio::test]
    async fn only_a_live_stream_is_posted_and_a_bad_login_is_skipped() {
        let body = r#"{"data":[
            {"id":"1","user_login":"someone","title":"Rerun","type":"rerun"},
            {"id":"2","user_login":"Bad-Name","title":"Odd","type":"live"}
        ]}"#;
        let stub = Stub::new(
            Ok(TOKEN_BODY.as_bytes().to_vec()),
            vec![Ok(body.as_bytes().to_vec())],
        );
        let items = Twitch::new(credentials())
            .read(&stub, 0, "someone")
            .await
            .unwrap();
        assert!(items.is_empty(), "{items:?}");
    }

    #[tokio::test]
    async fn the_token_is_reused_until_it_nearly_runs_out() {
        let stub = Stub::new(
            Ok(TOKEN_BODY.as_bytes().to_vec()),
            vec![
                Ok(LIVE_BODY.as_bytes().to_vec()),
                Ok(LIVE_BODY.as_bytes().to_vec()),
                Ok(LIVE_BODY.as_bytes().to_vec()),
            ],
        );
        let twitch = Twitch::new(credentials());
        twitch.read(&stub, 0, "someone").await.unwrap();
        twitch.read(&stub, 10, "someone").await.unwrap();
        assert_eq!(stub.token_asks.load(Ordering::Relaxed), 1);
        twitch.read(&stub, 3300, "someone").await.unwrap();
        assert_eq!(stub.token_asks.load(Ordering::Relaxed), 2);
    }

    #[tokio::test]
    async fn an_unauthorized_stream_gets_one_new_token_and_then_succeeds() {
        let stub = Stub::new(
            Ok(TOKEN_BODY.as_bytes().to_vec()),
            vec![Err(401), Ok(LIVE_BODY.as_bytes().to_vec())],
        );
        let items = Twitch::new(credentials())
            .read(&stub, 0, "someone")
            .await
            .unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(stub.token_asks.load(Ordering::Relaxed), 2);
    }

    #[tokio::test]
    async fn a_refused_token_is_an_error_that_holds_no_secret() {
        let stub = Stub::new(Err(500), Vec::new());
        let error = Twitch::new(credentials())
            .read(&stub, 0, "someone")
            .await
            .unwrap_err();
        let text = format!("{error:#}");
        assert!(text.contains("the Twitch token request failed"), "{text}");
        assert!(!text.contains(SECRET), "{text}");
        assert!(!text.contains(CLIENT_ID), "{text}");
        assert!(!text.contains("oauth2"), "{text}");
    }

    #[test]
    fn the_secrets_are_not_in_debug_output() {
        let twitch = Twitch::new(credentials());
        let debug = format!("{twitch:?} {:?}", credentials());
        assert!(!debug.contains(SECRET), "{debug}");
        assert!(!debug.contains(CLIENT_ID), "{debug}");
    }
}
