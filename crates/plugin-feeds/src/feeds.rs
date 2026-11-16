//! The feeds the modules read, shared by every server that watches them.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use plugin_kit::net::Get;

use crate::FeedSettings;
use crate::model::Kind;
use crate::source::{Item, Source};

/// The shortest interval between two looks, whatever the operator sets.
const MIN_INTERVAL_SECS: u64 = 60;
/// How many feed reads the whole bot makes in a second, and how many it may make at once.
const READS_PER_SECOND: u32 = 2;

/// Why a feed was not read now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FetchError {
    /// The bot is already reading as many feeds as it may; the next look tries again.
    Throttled,
    Failed(String),
}

#[derive(Debug, Clone, Copy)]
struct Bucket {
    tokens: u32,
    at: Option<i64>,
}

impl Bucket {
    fn take(&mut self, now: i64) -> bool {
        let elapsed = self.at.map_or(0, |at| now.saturating_sub(at).max(0));
        let refill = u32::try_from(elapsed)
            .unwrap_or(u32::MAX)
            .saturating_mul(READS_PER_SECOND);
        self.tokens = self.tokens.saturating_add(refill).min(READS_PER_SECOND);
        self.at = Some(now);
        match self.tokens.checked_sub(1) {
            Some(left) => {
                self.tokens = left;
                true
            }
            None => false,
        }
    }
}

/// What was read: the time of the read and the items, by kind and target.
type Cache = HashMap<(Kind, String), (i64, Vec<Item>)>;

/// The network the feeds are read through, and what was read recently.
pub struct Feeds {
    get: Arc<dyn Get>,
    settings: FeedSettings,
    /// What was read, and when. Kept for half an interval; one entry per feed watched.
    cache: Mutex<Cache>,
    bucket: Mutex<Bucket>,
}

impl std::fmt::Debug for Feeds {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Feeds")
            .field("settings", &self.settings)
            .finish_non_exhaustive()
    }
}

impl Feeds {
    #[must_use]
    pub fn new(get: Arc<dyn Get>, settings: FeedSettings) -> Arc<Self> {
        Arc::new(Self {
            get,
            settings,
            cache: Mutex::new(HashMap::new()),
            bucket: Mutex::new(Bucket {
                tokens: READS_PER_SECOND,
                at: None,
            }),
        })
    }

    /// The time between two looks at one subscription.
    #[must_use]
    pub fn interval(&self) -> Duration {
        Duration::from_secs(self.interval_secs_u64())
    }

    fn interval_secs_u64(&self) -> u64 {
        self.settings.interval_secs.max(MIN_INTERVAL_SECS)
    }

    /// The same interval in unix seconds, for the arithmetic of the looks.
    #[must_use]
    pub fn interval_secs(&self) -> i64 {
        i64::try_from(self.interval_secs_u64()).unwrap_or(i64::MAX)
    }

    /// The items of a feed, newest first, read now unless it was read lately.
    ///
    /// # Errors
    /// The read is throttled, or the feed could not be read.
    pub async fn latest(
        &self,
        now: i64,
        kind: Kind,
        target: &str,
    ) -> Result<Vec<Item>, FetchError> {
        let Some(source) = Source::of(kind) else {
            return Err(FetchError::Failed(
                "this kind of feed is not supported".into(),
            ));
        };
        let key = (kind, target.to_owned());
        let fresh_for = self.interval_secs() / 2;
        if let Some(items) = fresh(&lock(&self.cache), &key, now, fresh_for) {
            return Ok(items);
        }
        if !lock(&self.bucket).take(now) {
            return Err(FetchError::Throttled);
        }
        let items = source
            .read(&*self.get, target)
            .await
            .map_err(|error| FetchError::Failed(format!("{error:#}")))?;
        lock(&self.cache).insert(key, (now, items.clone()));
        Ok(items)
    }
}

fn fresh(cache: &Cache, key: &(Kind, String), now: i64, fresh_for: i64) -> Option<Vec<Item>> {
    let (read_at, items) = cache.get(key)?;
    (now.saturating_sub(*read_at) < fresh_for).then(|| items.clone())
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    // A panic while a lock is held leaves plain data behind; it is still good to read.
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}
