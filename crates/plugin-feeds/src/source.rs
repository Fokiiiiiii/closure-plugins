//! How each kind of feed is read: the items it has now, newest first.

use anyhow::Result;
use plugin_kit::net::Get;

use crate::model::Kind;
use crate::youtube;

/// One thing a feed has: what is posted for it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Item {
    pub id: String,
    pub title: String,
    pub url: String,
}

/// The kinds of feed that can be read. A kind without one is not supported yet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    YouTube,
}

impl Source {
    #[must_use]
    pub fn of(kind: Kind) -> Option<Self> {
        match kind {
            Kind::YouTube => Some(Self::YouTube),
            Kind::Reddit | Kind::Twitch | Kind::X | Kind::Pixiv => None,
        }
    }

    /// The target a moderator typed, as the feed names it; none when it is not one.
    #[must_use]
    pub fn target(self, text: &str) -> Option<String> {
        match self {
            Self::YouTube => youtube::channel_id(text),
        }
    }

    /// # Errors
    /// The feed is out of reach, too large, or not what it should be.
    pub async fn read(self, get: &dyn Get, target: &str) -> Result<Vec<Item>> {
        match self {
            Self::YouTube => {
                let body = get
                    .get_large(&youtube::feed_url(target), youtube::MAX_FEED_BYTES)
                    .await?;
                youtube::parse_feed(&body)
            }
        }
    }
}
