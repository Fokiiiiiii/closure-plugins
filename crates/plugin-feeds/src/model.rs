//! What is kept of a subscription, and the limits it works under.

use std::time::Duration;

use closure_core::event::{ChannelId, RoleId};
use serde::{Deserialize, Serialize};

/// The most subscriptions one server keeps.
pub const MAX_PER_GUILD: usize = 25;
/// The most subscriptions of one kind a server keeps.
pub const MAX_PER_KIND: usize = 10;
/// How many posted items are remembered, so that an item is not taken for new again.
pub const LAST_SEEN_MAX: usize = 50;
/// The most items one look posts; the rest is told as a count.
pub const POSTS_PER_RUN: usize = 3;
/// Reads that fail in a row before the subscription is paused.
pub const FAILURES_BEFORE_PAUSE: u32 = 10;
/// The longest wait between two reads of a feed that keeps failing.
pub const MAX_BACKOFF_SECS: i64 = 6 * 3600;
/// How long a post is remembered, so that it is not made twice.
pub const POSTED_TTL: Duration = Duration::from_hours(30 * 24);
/// The prefix of the keys of subscriptions.
pub const KEY_PREFIX: &str = "sub:";
const ID_ALPHABET: &[u8] = b"abcdefghijklmnopqrstuvwxyz0123456789";
const ID_LEN: usize = 8;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    YouTube,
    Reddit,
    Twitch,
    X,
    Pixiv,
}

impl Kind {
    /// The name a moderator types.
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        match text.trim().to_ascii_lowercase().as_str() {
            "youtube" => Some(Self::YouTube),
            "reddit" => Some(Self::Reddit),
            "twitch" => Some(Self::Twitch),
            "x" => Some(Self::X),
            "pixiv" => Some(Self::Pixiv),
            _ => None,
        }
    }

    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::YouTube => "YouTube",
            Self::Reddit => "Reddit",
            Self::Twitch => "Twitch",
            Self::X => "X",
            Self::Pixiv => "Pixiv",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Subscription {
    pub id: String,
    pub kind: Kind,
    pub target: String,
    pub channel: ChannelId,
    pub role: Option<RoleId>,
    pub created_by: u64,
    pub created_at: i64,
    /// The items already dealt with, oldest first.
    pub last_seen: Vec<String>,
    pub baseline_done: bool,
    pub failures: u32,
    pub paused: bool,
    /// The key of the text that tells why it is paused.
    pub paused_reason: Option<String>,
    /// Unix seconds before which the feed is not read again.
    pub next_at: i64,
}

/// The key of a subscription in the store.
#[must_use]
pub fn key(id: &str) -> String {
    format!("{KEY_PREFIX}{id}")
}

/// The key that says an item of a subscription was posted.
#[must_use]
pub fn posted_key(subscription: &str, marker: &str) -> String {
    format!("posted:{subscription}:{marker}")
}

#[must_use]
pub fn new_id() -> String {
    (0..ID_LEN)
        .map(|_| char::from(ID_ALPHABET[fastrand::usize(..ID_ALPHABET.len())]))
        .collect()
}

/// The wait before the next read after `failures` failures in a row: the interval, doubled for
/// each failure after the first, up to [`MAX_BACKOFF_SECS`].
#[must_use]
pub fn backoff(interval_secs: i64, failures: u32) -> i64 {
    let doublings = failures.saturating_sub(1).min(30);
    interval_secs
        .saturating_mul(1_i64 << doublings)
        .min(MAX_BACKOFF_SECS)
}

/// Adds `id` as the newest of `seen`, and keeps only the newest [`LAST_SEEN_MAX`].
pub fn remember(seen: &mut Vec<String>, id: String) {
    seen.retain(|known| known != &id);
    seen.push(id);
    let excess = seen.len().saturating_sub(LAST_SEEN_MAX);
    seen.drain(..excess);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_kind_is_read_by_its_name_in_any_case() {
        assert_eq!(Kind::parse("YouTube"), Some(Kind::YouTube));
        assert_eq!(Kind::parse(" x "), Some(Kind::X));
        assert_eq!(Kind::parse("mastodon"), None);
    }

    #[test]
    fn a_new_id_is_short_and_lowercase_letters_or_digits() {
        let id = new_id();
        assert_eq!(id.len(), ID_LEN);
        assert!(id.bytes().all(|b| ID_ALPHABET.contains(&b)));
    }
}
