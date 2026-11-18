//! The `/feed` commands: add, list, remove, test, resume.

use std::sync::Arc;
use std::time::Duration;

use closure_core::error::ModuleError;
use closure_core::event::{ChannelId, RoleId};
use closure_core::module::EventContext;
use closure_sdk::{Args, CommandArgs, CommandCx, UserError};
use plugin_kit::truncate;

use crate::feeds::{Feeds, FetchError};
use crate::model::{self, Kind, MAX_PER_GUILD, MAX_PER_KIND, Subscription};
use crate::poll::{POLL, Poll};
use crate::post::{Access, bot_can_post, send};

/// The most subscriptions read from the store for one command.
const LIST_LIMIT: usize = 1000;
/// What a list may fill: a message holds 2000 characters.
const LIST_CHARS: usize = 1900;

#[derive(Debug, CommandArgs)]
pub struct Add {
    /// What kind of feed: youtube, reddit, twitch, x or pixiv.
    #[arg(max_length = 20)]
    kind: String,
    /// The feed: a `YouTube` channel id (UC...) or its link, a subreddit (r/name), an X user name,
    /// or a Pixiv user id.
    #[arg(max_length = 200)]
    target: String,
    /// Where the new items are posted; this channel when left out.
    channel: Option<ChannelId>,
    /// The role mentioned with each new item; nobody when left out.
    role: Option<RoleId>,
}

#[derive(Debug, CommandArgs)]
pub struct Of {
    /// The id of the subscription, as `/feed list` shows it.
    #[arg(max_length = 20)]
    id: String,
}

fn failed(error: &impl ToString) -> UserError {
    UserError::from(ModuleError::other(error.to_string()))
}

/// Every subscription of this server.
///
/// # Errors
/// The store cannot be read.
pub async fn subscriptions(ctx: &EventContext) -> Result<Vec<Subscription>, ModuleError> {
    ctx.store()
        .list(model::KEY_PREFIX, LIST_LIMIT)
        .await
        .map_err(|error| ModuleError::other(error.to_string()))?
        .into_iter()
        .map(|(_, entry)| {
            serde_json::from_value(entry.value)
                .map_err(|error| ModuleError::other(error.to_string()))
        })
        .collect()
}

/// # Errors
/// The store or Discord cannot be used.
pub async fn add(feeds: Arc<Feeds>, cx: CommandCx, args: Add) -> Result<(), UserError> {
    let Some(kind) = Kind::parse(&args.kind) else {
        return cx.reply_private_t("feed-unknown-kind", &Args::new()).await;
    };
    let Some(source) = feeds.source(kind) else {
        let args = Args::new().with("kind", kind.name());
        return cx.reply_private_t("feed-unsupported-kind", &args).await;
    };
    let Some(target) = source.target(&args.target) else {
        let key = if kind == Kind::YouTube && args.target.trim().starts_with('@') {
            "feed-handle-unsupported"
        } else {
            "feed-bad-target"
        };
        return cx.reply_private_t(key, &Args::new()).await;
    };
    let ctx = cx.ctx();
    let channel = args.channel.unwrap_or_else(|| cx.channel_id());
    let existing = subscriptions(ctx).await.map_err(|e| failed(&e))?;
    let key = if existing.len() >= MAX_PER_GUILD {
        Some("feed-full")
    } else if existing.iter().filter(|s| s.kind == kind).count() >= MAX_PER_KIND {
        Some("feed-kind-full")
    } else if existing
        .iter()
        .any(|s| s.kind == kind && s.target == target && s.channel == channel)
    {
        Some("feed-exists")
    } else {
        None
    };
    if let Some(key) = key {
        return cx.reply_private_t(key, &Args::new()).await;
    }
    match bot_can_post(ctx, channel).await.map_err(|e| failed(&e))? {
        Access::Allowed => {}
        Access::Missing => {
            return cx
                .reply_private_t("feed-channel-missing", &Args::new())
                .await;
        }
        Access::Denied => {
            return cx
                .reply_private_t("feed-channel-denied", &Args::new())
                .await;
        }
    }
    let subscription = Subscription {
        id: model::new_id(),
        kind,
        target,
        channel,
        role: args.role,
        created_by: cx.user().id.get(),
        created_at: cx.now(),
        last_seen: Vec::new(),
        baseline_done: false,
        failures: 0,
        paused: false,
        paused_reason: None,
        next_at: 0,
    };
    let key = model::key(&subscription.id);
    ctx.store()
        .set(&key, &subscription)
        .await
        .map_err(|e| failed(&e))?;
    let job = Poll {
        subscription: subscription.id.clone(),
    };
    let scheduled = match cx.scheduler().job(POLL, &job) {
        Ok(builder) => {
            builder
                .key(&subscription.id)
                .unless_waiting()
                .every(feeds.interval(), Duration::ZERO)
                .await
        }
        Err(error) => Err(error),
    };
    if let Err(error) = scheduled {
        if let Err(forget) = ctx.store().delete(&key).await {
            tracing::debug!(
                ?forget,
                "a subscription without its job could not be removed"
            );
        }
        return Err(failed(&error));
    }
    let args = Args::new().with("id", subscription.id.clone());
    cx.reply_private_t("feed-added", &args).await
}

/// # Errors
/// The store cannot be read.
pub async fn list(cx: CommandCx, _args: ()) -> Result<(), UserError> {
    let subscriptions = subscriptions(cx.ctx()).await.map_err(|e| failed(&e))?;
    if subscriptions.is_empty() {
        return cx.reply_private_t("feed-none", &Args::new()).await;
    }
    let lines: Vec<String> = subscriptions
        .iter()
        .map(|subscription| line(cx.ctx(), subscription))
        .collect();
    cx.reply_private(truncate(&lines.join("\n"), LIST_CHARS))
        .await
}

fn line(ctx: &EventContext, subscription: &Subscription) -> String {
    let state = match (subscription.paused, &subscription.paused_reason) {
        (true, Some(reason)) => format!(" · {}", ctx.t(reason)),
        (true, None) => " · paused".to_owned(),
        (false, _) => String::new(),
    };
    format!(
        "`{}` {} <#{}>{state}",
        subscription.id,
        subscription.kind.name(),
        subscription.channel
    )
}

/// # Errors
/// The store or the scheduler cannot be used.
pub async fn remove(cx: CommandCx, args: Of) -> Result<(), UserError> {
    let removed = cx
        .store()
        .delete(&model::key(&args.id))
        .await
        .map_err(|e| failed(&e))?;
    if !removed {
        return cx.reply_private_t("feed-not-found", &Args::new()).await;
    }
    cx.scheduler()
        .cancel(&args.id)
        .await
        .map_err(|e| failed(&e))?;
    cx.reply_private_t("feed-removed", &Args::new()).await
}

/// # Errors
/// The store cannot be used.
pub async fn resume(cx: CommandCx, args: Of) -> Result<(), UserError> {
    let key = model::key(&args.id);
    let Some(mut subscription) = cx
        .store()
        .get::<Subscription>(&key)
        .await
        .map_err(|e| failed(&e))?
    else {
        return cx.reply_private_t("feed-not-found", &Args::new()).await;
    };
    subscription.paused = false;
    subscription.paused_reason = None;
    subscription.failures = 0;
    subscription.next_at = 0;
    cx.store()
        .set(&key, &subscription)
        .await
        .map_err(|e| failed(&e))?;
    cx.reply_private_t("feed-resumed", &Args::new()).await
}

/// Posts the newest item of a subscription now, as a test; nothing is recorded.
///
/// # Errors
/// The store or Discord cannot be used.
pub async fn test_one(feeds: Arc<Feeds>, cx: CommandCx, args: Of) -> Result<(), UserError> {
    let Some(subscription) = cx
        .store()
        .get::<Subscription>(&model::key(&args.id))
        .await
        .map_err(|e| failed(&e))?
    else {
        return cx.reply_private_t("feed-not-found", &Args::new()).await;
    };
    let items = match feeds
        .latest(cx.now(), subscription.kind, &subscription.target)
        .await
    {
        Ok(items) => items,
        Err(FetchError::Throttled) => return cx.reply_private_t("feed-busy", &Args::new()).await,
        Err(FetchError::Failed(reason)) => {
            tracing::debug!(%reason, "a feed could not be read for a test");
            return cx.reply_private_t("feed-read-failed", &Args::new()).await;
        }
    };
    let Some(newest) = items.first() else {
        return cx.reply_private_t("feed-empty", &Args::new()).await;
    };
    send(
        cx.ctx(),
        subscription.channel,
        subscription.role,
        &newest.url,
    )
    .await
    .map_err(|e| failed(&e))?;
    cx.reply_private_t("feed-tested", &Args::new()).await
}
