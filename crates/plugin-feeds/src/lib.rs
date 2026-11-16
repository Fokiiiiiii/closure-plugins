//! Feeds: a server subscribes to a feed (a `YouTube` channel), and each new item is posted to a channel.
//!
//! `/feed add` subscribes; a job per subscription looks at the feed every few minutes and posts what
//! is new. The first look only records what is there, so an old video is never posted.

#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

pub mod commands;
pub mod feeds;
pub mod model;
pub mod poll;
pub mod post;
pub mod source;
pub mod youtube;

#[cfg(test)]
mod tests;

use std::sync::Arc;

use closure_sdk::{
    Capability, Command, CoreError, Level, Locale, ModuleBuilder, PermissionSpec, SdkModule,
};
use plugin_kit::net::Fetcher;

use crate::commands::{Add, Of};
use crate::feeds::Feeds;
use crate::poll::POLL;

const EN: &str = include_str!("../locales/en.ftl");
const JA: &str = include_str!("../locales/ja.ftl");
const PERMISSION: &str = "feeds.manage";

/// The operator's settings for this module, from the `plugins.feeds` section of the settings file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct FeedSettings {
    /// Seconds between two looks at one feed; at least 60.
    pub interval_secs: u64,
}

impl Default for FeedSettings {
    fn default() -> Self {
        Self { interval_secs: 300 }
    }
}

/// # Errors
/// The module declaration is refused by the SDK.
pub fn builder_with(feeds: &Arc<Feeds>) -> Result<ModuleBuilder, CoreError> {
    let (add_feeds, test_feeds, poll_feeds) =
        (Arc::clone(feeds), Arc::clone(feeds), Arc::clone(feeds));
    Ok(
        ModuleBuilder::new("feeds", "Feeds", env!("CARGO_PKG_VERSION"))?
            .capability(Capability::Storage)
            .capability(Capability::SendMessages)
            .capability(Capability::ReadGuild)
            .capability(Capability::Schedule)
            .capability(Capability::RespondToInteractions)
            .permission(PermissionSpec::new(
                PERMISSION,
                Level::ADMIN,
                "permission-feeds-manage",
            )?)
            .group(
                "feed",
                [
                    Command::<Add>::new("add")
                        .requires(PERMISSION)
                        .handler(move |cx, args| commands::add(Arc::clone(&add_feeds), cx, args)),
                    Command::<()>::new("list")
                        .requires(PERMISSION)
                        .handler(commands::list),
                    Command::<Of>::new("remove")
                        .requires(PERMISSION)
                        .handler(commands::remove),
                    Command::<Of>::new("test")
                        .requires(PERMISSION)
                        .handler(move |cx, args| {
                            commands::test_one(Arc::clone(&test_feeds), cx, args)
                        }),
                    Command::<Of>::new("resume")
                        .requires(PERMISSION)
                        .handler(commands::resume),
                ],
            )
            .job(POLL, move |cx| poll::poll(Arc::clone(&poll_feeds), cx))
            .catalog(Locale::En, EN)
            .catalog(Locale::Ja, JA),
    )
}

/// # Errors
/// The module declaration is refused by the SDK.
pub fn module_with(settings: FeedSettings) -> Result<SdkModule, CoreError> {
    builder_with(&Feeds::new(Arc::new(Fetcher::new()), settings))?.build()
}

/// The module with the default settings.
/// # Errors
/// The module declaration is refused by the SDK.
pub fn module() -> Result<SdkModule, CoreError> {
    module_with(FeedSettings::default())
}
