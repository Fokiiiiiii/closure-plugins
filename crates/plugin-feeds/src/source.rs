//! How each kind of feed is read: where it is, what its target is, and the items it has now.

use anyhow::Result;
use plugin_kit::net::Get;

use crate::model::Kind;
use crate::{rss, youtube};

/// One thing a feed has: what is posted for it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Item {
    pub id: String,
    pub title: String,
    pub url: String,
}

/// The kinds of feed that can be read. A kind without one is not supported yet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Source {
    YouTube,
    Reddit,
    /// X and Pixiv, read through the `RSSHub` the operator runs: the kind and that address.
    RssHub(Kind, String),
}

impl Source {
    /// The source that reads `kind` here; none when the kind is not supported, or when it needs an
    /// `RSSHub` and none is set.
    #[must_use]
    pub fn of(kind: Kind, rsshub: Option<&str>) -> Option<Self> {
        match kind {
            Kind::YouTube => Some(Self::YouTube),
            Kind::Reddit => Some(Self::Reddit),
            Kind::X | Kind::Pixiv => rsshub.map(|base| Self::RssHub(kind, base.to_owned())),
            Kind::Twitch => None,
        }
    }

    /// The target a moderator typed, as the feed names it; none when it is not one.
    #[must_use]
    pub fn target(&self, text: &str) -> Option<String> {
        match self {
            Self::YouTube => youtube::channel_id(text),
            Self::Reddit => reddit_name(text),
            Self::RssHub(Kind::Pixiv, _) => pixiv_id(text),
            Self::RssHub(_, _) => x_handle(text),
        }
    }

    /// The address the feed of `target` is read from.
    #[must_use]
    pub fn feed_url(&self, target: &str) -> String {
        match self {
            Self::YouTube => youtube::feed_url(target),
            Self::Reddit => format!("https://www.reddit.com/r/{target}/new/.rss"),
            Self::RssHub(Kind::Pixiv, base) => format!("{base}/pixiv/user/{target}"),
            Self::RssHub(_, base) => format!("{base}/twitter/user/{target}"),
        }
    }

    /// # Errors
    /// The feed is out of reach, too large, or not what it should be.
    pub async fn read(&self, get: &dyn Get, target: &str) -> Result<Vec<Item>> {
        let body = get
            .get_large(&self.feed_url(target), youtube::MAX_FEED_BYTES)
            .await?;
        match self {
            Self::YouTube => youtube::parse_feed(&body),
            Self::Reddit | Self::RssHub(..) => rss::parse(&body),
        }
    }
}

/// The `RSSHub` address the operator set, when it is usable: `https://`, with no user info, query
/// or fragment. Anything else counts as not set.
#[must_use]
pub fn rsshub_base(setting: Option<&str>) -> Option<String> {
    let base = setting?.trim().trim_end_matches('/');
    if base.is_empty() {
        return None;
    }
    let usable = base
        .strip_prefix("https://")
        .is_some_and(|rest| !rest.is_empty() && !rest.contains(['@', '?', '#']));
    if usable {
        return Some(base.to_owned());
    }
    tracing::warn!(
        "rsshub_base_url is not an https address; the X and Pixiv feeds stay unsupported"
    );
    None
}

/// Letters, digits and `_`, `min` to `max` bytes long.
fn is_word(text: &str, min: usize, max: usize) -> bool {
    (min..=max).contains(&text.len())
        && text.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
}

/// The subreddit a moderator typed (`rust`, `r/rust`, `/r/rust` or a reddit link), in lower case.
fn reddit_name(text: &str) -> Option<String> {
    let text = text.trim();
    let name = if let Some(link) = text.strip_prefix("https://") {
        let host = link
            .strip_prefix("www.")
            .or_else(|| link.strip_prefix("old."))
            .unwrap_or(link);
        host.strip_prefix("reddit.com/r/")?
    } else {
        let plain = text.strip_prefix('/').unwrap_or(text);
        plain.strip_prefix("r/").unwrap_or(plain)
    };
    let name = name.split(['/', '?', '#']).next().unwrap_or_default();
    is_word(name, 2, 21).then(|| name.to_ascii_lowercase())
}

/// The X user name a moderator typed (`name`, `@name` or an x.com or twitter.com link), in lower case.
fn x_handle(text: &str) -> Option<String> {
    let text = text.trim();
    let name = if let Some(link) = text.strip_prefix("https://") {
        let host = link.strip_prefix("www.").unwrap_or(link);
        host.strip_prefix("x.com/")
            .or_else(|| host.strip_prefix("twitter.com/"))?
            .split(['/', '?', '#'])
            .next()
            .unwrap_or_default()
    } else {
        text.strip_prefix('@').unwrap_or(text)
    };
    is_word(name, 1, 15).then(|| name.to_ascii_lowercase())
}

/// The Pixiv user id a moderator typed: the digits, or a link to the user.
fn pixiv_id(text: &str) -> Option<String> {
    let text = text.trim();
    let id = match text.strip_prefix("https://www.pixiv.net/users/") {
        Some(rest) => rest.split(['/', '?', '#']).next().unwrap_or_default(),
        None => text,
    };
    let valid = (1..=12).contains(&id.len()) && id.bytes().all(|b| b.is_ascii_digit());
    valid.then(|| id.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_subreddit_is_read_from_its_name_or_a_reddit_link_in_lower_case() {
        assert_eq!(reddit_name("rust").as_deref(), Some("rust"));
        assert_eq!(reddit_name(" r/Rust ").as_deref(), Some("rust"));
        assert_eq!(reddit_name("/r/rust").as_deref(), Some("rust"));
        assert_eq!(
            reddit_name("https://www.reddit.com/r/rust/new").as_deref(),
            Some("rust")
        );
        assert_eq!(
            reddit_name("https://old.reddit.com/r/rust/").as_deref(),
            Some("rust")
        );
        assert_eq!(reddit_name("a"), None);
        assert_eq!(reddit_name("r/this_name_is_far_too_long"), None);
        assert_eq!(reddit_name("rust!"), None);
        assert_eq!(reddit_name("https://example.org/r/rust"), None);
    }

    #[test]
    fn an_x_user_is_read_from_its_name_an_at_name_or_a_link() {
        assert_eq!(x_handle("someone").as_deref(), Some("someone"));
        assert_eq!(x_handle("@Someone").as_deref(), Some("someone"));
        assert_eq!(
            x_handle("https://x.com/Someone/status/1").as_deref(),
            Some("someone")
        );
        assert_eq!(
            x_handle("https://twitter.com/someone?lang=en").as_deref(),
            Some("someone")
        );
        assert_eq!(x_handle("@"), None);
        assert_eq!(x_handle("abcdefghijklmnop"), None);
        assert_eq!(x_handle("https://example.org/someone"), None);
    }

    #[test]
    fn a_pixiv_user_is_read_from_its_id_or_a_link() {
        assert_eq!(pixiv_id("12345").as_deref(), Some("12345"));
        assert_eq!(
            pixiv_id("https://www.pixiv.net/users/12345").as_deref(),
            Some("12345")
        );
        assert_eq!(
            pixiv_id("https://www.pixiv.net/users/12345/artworks").as_deref(),
            Some("12345")
        );
        assert_eq!(pixiv_id("abc"), None);
        assert_eq!(pixiv_id("1234567890123"), None);
        assert_eq!(pixiv_id(""), None);
    }

    #[test]
    fn the_rsshub_address_must_be_https_without_user_info_query_or_fragment() {
        let host = "https://rsshub.example.org";
        assert_eq!(
            rsshub_base(Some("https://rsshub.example.org/")).as_deref(),
            Some(host)
        );
        assert_eq!(rsshub_base(None), None);
        assert_eq!(rsshub_base(Some("")), None);
        assert_eq!(rsshub_base(Some("http://rsshub.example.org")), None);
        assert_eq!(rsshub_base(Some("https://user@rsshub.example.org")), None);
        assert_eq!(rsshub_base(Some("https://rsshub.example.org/?a=1")), None);
        assert_eq!(rsshub_base(Some("https://rsshub.example.org/#x")), None);
    }

    #[test]
    fn x_and_pixiv_need_rsshub_and_twitch_is_never_supported() {
        let host = "https://rsshub.example.org";
        assert_eq!(Source::of(Kind::X, None), None);
        assert_eq!(Source::of(Kind::Pixiv, None), None);
        assert_eq!(Source::of(Kind::Twitch, Some(host)), None);
        assert_eq!(
            Source::of(Kind::X, Some(host)),
            Some(Source::RssHub(Kind::X, host.to_owned()))
        );
        assert_eq!(Source::of(Kind::Reddit, None), Some(Source::Reddit));
    }

    #[test]
    fn each_source_reads_its_feed_from_its_own_address() {
        assert_eq!(
            Source::Reddit.feed_url("rust"),
            "https://www.reddit.com/r/rust/new/.rss"
        );
        let base = "https://rsshub.example.org";
        assert_eq!(
            Source::RssHub(Kind::X, base.to_owned()).feed_url("someone"),
            "https://rsshub.example.org/twitter/user/someone"
        );
        assert_eq!(
            Source::RssHub(Kind::Pixiv, base.to_owned()).feed_url("12345"),
            "https://rsshub.example.org/pixiv/user/12345"
        );
    }
}
