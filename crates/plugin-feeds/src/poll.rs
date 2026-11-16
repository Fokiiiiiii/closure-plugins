//! The look a job takes at one subscription: read the feed, post what is new, keep the record.

use std::sync::Arc;

use closure_core::error::ModuleError;
use closure_core::module::EventContext;
use closure_core::storage::{Expect, StorageError};
use closure_sdk::{Args, JobCx};
use serde::{Deserialize, Serialize};

use crate::feeds::{Feeds, FetchError};
use crate::model::{self, FAILURES_BEFORE_PAUSE, LAST_SEEN_MAX, POSTS_PER_RUN, Subscription};
use crate::post::{PostError, post_once};
use crate::source::Item;

/// The name of the job that looks at a subscription.
pub const POLL: &str = "poll";

#[derive(Debug, Serialize, Deserialize)]
pub struct Poll {
    pub subscription: String,
}

/// # Errors
/// The store cannot be read or written.
pub async fn poll(feeds: Arc<Feeds>, cx: JobCx) -> Result<(), ModuleError> {
    let job: Poll = cx.payload()?;
    let ctx = cx.ctx();
    let key = model::key(&job.subscription);
    let Some(entry) = ctx.store().entry(&key).await.map_err(|e| stored(&e))? else {
        return Ok(());
    };
    let mut subscription: Subscription = serde_json::from_value(entry.value)
        .map_err(|error| ModuleError::other(error.to_string()))?;
    let now = ctx.now();
    if subscription.paused || now < subscription.next_at {
        return Ok(());
    }
    let items = match feeds
        .latest(now, subscription.kind, &subscription.target)
        .await
    {
        Ok(items) => items,
        Err(FetchError::Throttled) => return Ok(()),
        Err(FetchError::Failed(reason)) => {
            tracing::debug!(%reason, "a feed could not be read");
            return back_off(&feeds, ctx, &key, entry.revision, subscription, now).await;
        }
    };
    subscription.failures = 0;
    subscription.next_at = 0;
    if !subscription.baseline_done {
        subscription.baseline_done = true;
        subscription.last_seen = record_all(&items);
        return save(ctx, &key, entry.revision, &subscription).await;
    }
    let fresh = unseen(&items, &subscription.last_seen);
    let (now_posted, later) = fresh.split_at(fresh.len().min(POSTS_PER_RUN));
    for item in now_posted {
        match post_once(ctx, &subscription, &item.id, &item.url).await {
            Ok(()) => model::remember(&mut subscription.last_seen, item.id.clone()),
            Err(PostError::Gone(reason)) => {
                return pause(ctx, &key, entry.revision, subscription, reason).await;
            }
            Err(PostError::Failed(error)) => {
                tracing::debug!(?error, "an item could not be posted");
                return back_off(&feeds, ctx, &key, entry.revision, subscription, now).await;
            }
        }
    }
    if let Some(newest) = later.last() {
        let count = u64::try_from(later.len()).unwrap_or(u64::MAX);
        let text = ctx.t_in(ctx.locale, "feed-more", &Args::new().with("count", count));
        let marker = format!("more-{}", newest.id);
        match post_once(ctx, &subscription, &marker, &text).await {
            Ok(()) => {}
            Err(PostError::Gone(reason)) => {
                return pause(ctx, &key, entry.revision, subscription, reason).await;
            }
            Err(PostError::Failed(error)) => {
                tracing::debug!(?error, "the count of new items could not be posted");
                return back_off(&feeds, ctx, &key, entry.revision, subscription, now).await;
            }
        }
        for item in later {
            model::remember(&mut subscription.last_seen, item.id.clone());
        }
    }
    save(ctx, &key, entry.revision, &subscription).await
}

/// The items not seen yet, oldest first.
fn unseen(items: &[Item], seen: &[String]) -> Vec<Item> {
    items
        .iter()
        .rev()
        .filter(|item| !seen.contains(&item.id))
        .cloned()
        .collect()
}

/// The ids of a feed, oldest first, the newest [`LAST_SEEN_MAX`] of them.
fn record_all(items: &[Item]) -> Vec<String> {
    let mut ids: Vec<String> = items.iter().rev().map(|item| item.id.clone()).collect();
    let excess = ids.len().saturating_sub(LAST_SEEN_MAX);
    ids.drain(..excess);
    ids
}

async fn back_off(
    feeds: &Feeds,
    ctx: &EventContext,
    key: &str,
    revision: i64,
    mut subscription: Subscription,
    now: i64,
) -> Result<(), ModuleError> {
    subscription.failures = subscription.failures.saturating_add(1);
    if subscription.failures >= FAILURES_BEFORE_PAUSE {
        subscription.paused = true;
        subscription.paused_reason = Some("feed-paused-failures".to_owned());
    }
    let wait = model::backoff(feeds.interval_secs(), subscription.failures);
    subscription.next_at = now.saturating_add(wait);
    save(ctx, key, revision, &subscription).await
}

async fn pause(
    ctx: &EventContext,
    key: &str,
    revision: i64,
    mut subscription: Subscription,
    reason: &str,
) -> Result<(), ModuleError> {
    subscription.paused = true;
    subscription.paused_reason = Some(reason.to_owned());
    save(ctx, key, revision, &subscription).await
}

async fn save(
    ctx: &EventContext,
    key: &str,
    revision: i64,
    subscription: &Subscription,
) -> Result<(), ModuleError> {
    match ctx
        .store()
        .replace(key, subscription, Expect::Revision(revision), None)
        .await
    {
        // Changed by another look since it was read: this look ends, the next one reads it again.
        Ok(_) | Err(StorageError::Conflict) => Ok(()),
        Err(error) => Err(stored(&error)),
    }
}

fn stored(error: &StorageError) -> ModuleError {
    ModuleError::other(error.to_string())
}
