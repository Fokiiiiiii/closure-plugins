use std::fmt::Write as _;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

use anyhow::bail;
use async_trait::async_trait;
use closure_core::action::{Action, MentionPolicy};
use closure_core::error::DiscordError;
use closure_sdk::testing::{Harness, Invocation};
use closure_sdk::{CommandOptionValue, Level};
use plugin_kit::net::{Get, Http, Post};
use twilight_model::channel::Channel;
use twilight_model::guild::Permissions;
use twilight_model::id::Id;

use crate::feeds::Feeds;
use crate::model::{Kind, Subscription};
use crate::{FeedSettings, builder_with};

const GUILD: u64 = 1;
const CHANNEL: u64 = 5;
const ADMIN_ROLE: u64 = 200;
const WATCH_ROLE: u64 = 300;
const BOT: u64 = 9_999;
const CHANNEL_ID: &str = "UC_x5XG1OV2P6uZZ5FSM9Ttw";

/// A feed that answers with what the test sets, and counts the reads.
#[derive(Default)]
struct Fake {
    body: Mutex<Vec<u8>>,
    reads: AtomicUsize,
    failing: AtomicBool,
    urls: Mutex<Vec<String>>,
}

impl Fake {
    fn set(&self, ids: &[&str]) {
        *self.body.lock().unwrap() = feed_xml(ids).into_bytes();
    }

    fn reads(&self) -> usize {
        self.reads.load(Ordering::Relaxed)
    }

    fn urls(&self) -> Vec<String> {
        self.urls.lock().unwrap().clone()
    }
}

#[async_trait]
impl Post for Fake {
    async fn post_json(
        &self,
        _url: &str,
        _headers: &[(&str, &str)],
        _body: &serde_json::Value,
    ) -> anyhow::Result<Vec<u8>> {
        bail!("the feed test does not post")
    }
}

#[async_trait]
impl Get for Fake {
    async fn get(&self, url: &str) -> anyhow::Result<Vec<u8>> {
        self.urls.lock().unwrap().push(url.to_owned());
        self.reads.fetch_add(1, Ordering::Relaxed);
        if self.failing.load(Ordering::Relaxed) {
            bail!("the feed is down");
        }
        Ok(self.body.lock().unwrap().clone())
    }
}

/// A feed of these videos, newest first, as `YouTube` writes it.
fn feed_xml(ids: &[&str]) -> String {
    let entries = ids.iter().fold(String::new(), |mut out, id| {
        let _ = write!(
            out,
            r#"<entry><id>yt:video:{id}</id><yt:videoId>{id}</yt:videoId><title>Video {id}</title><link rel="alternate" href="https://www.youtube.com/watch?v={id}"/></entry>"#
        );
        out
    });
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?><feed xmlns:yt="http://www.youtube.com/xml/schemas/2015" xmlns="http://www.w3.org/2005/Atom"><title>t</title>{entries}</feed>"#
    )
}

fn fake(ids: &[&str]) -> Arc<Fake> {
    let fake = Arc::new(Fake::default());
    fake.set(ids);
    fake
}

fn feeds(fake: &Arc<Fake>) -> Arc<Feeds> {
    feeds_with(fake, FeedSettings::default())
}

fn feeds_with(fake: &Arc<Fake>, settings: FeedSettings) -> Arc<Feeds> {
    let http: Arc<dyn Http> = Arc::clone(fake);
    Feeds::new(http, settings, None)
}

fn channel_json() -> Channel {
    serde_json::from_value(serde_json::json!({
        "id": CHANNEL.to_string(), "type": 0, "guild_id": GUILD.to_string(), "name": "news",
    }))
    .unwrap()
}

/// A server with one channel the bot may post in, and one moderator.
fn harness(feeds: &Arc<Feeds>) -> Harness {
    let harness = Harness::new(builder_with(feeds).unwrap()).unwrap();
    harness
        .api()
        .set_channels(Id::new(GUILD), vec![channel_json()]);
    harness
        .api()
        .add_channel(Id::new(CHANNEL), Some(Id::new(GUILD)));
    harness.set_bot_id(Some(Id::new(BOT)));
    harness.set_bot_permissions(Permissions::VIEW_CHANNEL | Permissions::SEND_MESSAGES);
    harness.permissions(|permissions| {
        permissions
            .role_levels
            .insert(Id::new(ADMIN_ROLE), Level::ADMIN);
    });
    harness
}

fn text(value: &str) -> CommandOptionValue {
    CommandOptionValue::String(value.to_owned())
}

fn url() -> String {
    format!("https://www.youtube.com/channel/{CHANNEL_ID}")
}

fn add(target: &str) -> Invocation {
    add_kind("youtube", target)
}

fn add_kind(kind: &str, target: &str) -> Invocation {
    Invocation::new("feed")
        .subcommand("add")
        .option("kind", text(kind))
        .option("target", text(target))
        .channel(CHANNEL)
        .roles([ADMIN_ROLE])
}

fn sub_command(name: &str, id: &str) -> Invocation {
    Invocation::new("feed")
        .subcommand(name)
        .option("id", text(id))
        .roles([ADMIN_ROLE])
}

fn list() -> Invocation {
    Invocation::new("feed")
        .subcommand("list")
        .roles([ADMIN_ROLE])
}

async fn subscriptions(harness: &Harness) -> Vec<Subscription> {
    harness
        .store()
        .list("sub:", 100)
        .await
        .unwrap()
        .into_iter()
        .map(|(_, entry)| serde_json::from_value(entry.value).unwrap())
        .collect()
}

async fn only(harness: &Harness) -> Subscription {
    let mut all = subscriptions(harness).await;
    assert_eq!(all.len(), 1, "{all:?}");
    all.remove(0)
}

/// The messages the module sent: the content and who may be mentioned.
fn posts(harness: &Harness) -> Vec<(String, MentionPolicy)> {
    harness
        .actions()
        .into_iter()
        .filter_map(|action| match action {
            Action::SendMessage {
                content: Some(content),
                mentions,
                ..
            } => Some((content, mentions)),
            _ => None,
        })
        .collect()
}

fn watch(id: &str) -> String {
    format!("https://www.youtube.com/watch?v={id}")
}

async fn run_later(harness: &Harness) {
    harness.advance(Duration::from_secs(301));
    let ran = harness.run_due_jobs().await;
    assert!(ran.iter().all(Result::is_ok), "{ran:?}");
}

#[tokio::test]
async fn add_list_and_remove_manage_the_subscriptions() {
    let fake = fake(&["v2", "v1"]);
    let harness = harness(&feeds(&fake));
    harness.invoke(add(&url())).await.unwrap();
    let subscription = only(&harness).await;
    assert_eq!(subscription.kind, Kind::YouTube);
    assert_eq!(subscription.target, CHANNEL_ID);
    assert_eq!(subscription.channel, Id::new(CHANNEL));
    assert_eq!(harness.jobs(), 1);

    let listed = harness.invoke(list()).await.unwrap();
    assert!(listed[0].content.contains(&subscription.id), "{listed:?}");

    harness
        .invoke(sub_command("remove", &subscription.id))
        .await
        .unwrap();
    assert!(subscriptions(&harness).await.is_empty());
    assert_eq!(harness.jobs(), 0);
}

#[tokio::test]
async fn only_a_moderator_adds_and_a_bad_target_or_kind_is_refused() {
    let fake = fake(&["v1"]);
    let harness = harness(&feeds(&fake));
    let _ = harness.invoke(add("@somebody")).await;
    let _ = harness.invoke(add("UC_short")).await;
    let _ = harness
        .invoke(
            Invocation::new("feed")
                .subcommand("add")
                .option("kind", text("twitch"))
                .option("target", text("r/rust"))
                .channel(CHANNEL)
                .roles([ADMIN_ROLE]),
        )
        .await;
    assert!(subscriptions(&harness).await.is_empty());
    let _ = harness.invoke(add(&url()).roles(Vec::<u64>::new())).await;
    assert!(
        subscriptions(&harness).await.is_empty(),
        "a member cannot add"
    );
}

#[tokio::test]
async fn a_channel_the_bot_cannot_post_in_is_refused_on_add() {
    let fake = fake(&["v1"]);
    let harness = harness(&feeds(&fake));
    harness.set_bot_permissions(Permissions::VIEW_CHANNEL);
    let _ = harness.invoke(add(&url())).await;
    assert!(subscriptions(&harness).await.is_empty());
    assert_eq!(harness.jobs(), 0);
}

#[tokio::test]
async fn the_first_run_records_the_feed_and_posts_nothing() {
    let fake = fake(&["v3", "v2", "v1"]);
    let harness = harness(&feeds(&fake));
    harness.invoke(add(&url())).await.unwrap();
    let ran = harness.run_due_jobs().await;
    assert!(ran.iter().all(Result::is_ok), "{ran:?}");
    assert!(posts(&harness).is_empty(), "{:?}", posts(&harness));
    let subscription = only(&harness).await;
    assert!(subscription.baseline_done);
    assert_eq!(subscription.last_seen, vec!["v1", "v2", "v3"]);
}

#[tokio::test]
async fn a_later_run_posts_only_the_new_videos_oldest_first() {
    let fake = fake(&["v3", "v2", "v1"]);
    let harness = harness(&feeds(&fake));
    harness.invoke(add(&url())).await.unwrap();
    harness.run_due_jobs().await;
    fake.set(&["v5", "v4", "v3", "v2", "v1"]);
    run_later(&harness).await;
    let sent = posts(&harness);
    assert_eq!(sent.len(), 2, "{sent:?}");
    assert_eq!(sent[0].0, watch("v4"));
    assert_eq!(sent[1].0, watch("v5"));
}

#[tokio::test]
async fn a_video_is_never_posted_twice() {
    let fake = fake(&["v1"]);
    let harness = harness(&feeds(&fake));
    harness.invoke(add(&url())).await.unwrap();
    harness.run_due_jobs().await;
    fake.set(&["v2", "v1"]);
    run_later(&harness).await;
    run_later(&harness).await;
    assert_eq!(posts(&harness).len(), 1, "{:?}", posts(&harness));

    // A post made earlier (even if the module did not remember it) is not made again.
    let subscription = only(&harness).await;
    harness
        .store()
        .set(&format!("posted:{}:v3", subscription.id), &true)
        .await
        .unwrap();
    fake.set(&["v3", "v2", "v1"]);
    run_later(&harness).await;
    assert_eq!(posts(&harness).len(), 1, "{:?}", posts(&harness));
}

#[tokio::test]
async fn one_run_posts_at_most_three_and_says_how_many_more() {
    let fake = fake(&["v0"]);
    let harness = harness(&feeds(&fake));
    harness.invoke(add(&url())).await.unwrap();
    harness.run_due_jobs().await;
    fake.set(&["v6", "v5", "v4", "v3", "v2", "v1", "v0"]);
    run_later(&harness).await;
    let sent = posts(&harness);
    assert_eq!(sent.len(), 4, "{sent:?}");
    assert_eq!(sent[0].0, watch("v1"));
    assert_eq!(sent[2].0, watch("v3"));
    assert!(sent[3].0.contains('3'), "{sent:?}");
    assert!(
        sent.iter().all(|(text, _)| !text.contains("v4")),
        "{sent:?}"
    );
    let subscription = only(&harness).await;
    assert_eq!(subscription.last_seen.len(), 7, "{subscription:?}");
    run_later(&harness).await;
    assert_eq!(posts(&harness).len(), 4);
}

#[tokio::test]
async fn a_failed_read_backs_off_and_pauses_after_ten_failures() {
    let fake = fake(&["v1"]);
    let harness = harness(&feeds(&fake));
    harness.invoke(add(&url())).await.unwrap();
    harness.run_due_jobs().await;
    fake.failing.store(true, Ordering::Relaxed);
    run_later(&harness).await;
    let now = harness.context().now();
    let first = only(&harness).await;
    assert_eq!(first.failures, 1);
    assert!(!first.paused);
    assert_eq!(first.next_at, now + 300, "{first:?}");

    for _ in 0..9 {
        harness.advance(Duration::from_hours(6));
        harness.run_due_jobs().await;
    }
    let paused = only(&harness).await;
    assert!(paused.paused, "{paused:?}");
    assert!(paused.paused_reason.is_some());
    let reads = fake.reads();
    run_later(&harness).await;
    assert_eq!(fake.reads(), reads, "a paused feed is not read");

    let listed = harness.invoke(list()).await.unwrap();
    assert!(listed[0].content.contains("paused"), "{listed:?}");
    fake.failing.store(false, Ordering::Relaxed);
    harness
        .invoke(sub_command("resume", &paused.id))
        .await
        .unwrap();
    assert!(!only(&harness).await.paused);
}

#[tokio::test]
async fn a_missing_channel_pauses_the_subscription_until_it_is_resumed() {
    let fake = fake(&["v1"]);
    let harness = harness(&feeds(&fake));
    harness.invoke(add(&url())).await.unwrap();
    harness.run_due_jobs().await;
    fake.set(&["v2", "v1"]);
    harness.api().fail_nth(1, DiscordError::NotFound);
    run_later(&harness).await;
    let paused = only(&harness).await;
    assert!(paused.paused, "{paused:?}");
    assert!(paused.paused_reason.is_some());
    assert!(!paused.last_seen.contains(&"v2".to_owned()));

    let listed = harness.invoke(list()).await.unwrap();
    assert!(listed[0].content.contains("paused"), "{listed:?}");
    harness
        .invoke(sub_command("resume", &paused.id))
        .await
        .unwrap();
    run_later(&harness).await;
    assert_eq!(posts(&harness).len(), 1, "{:?}", posts(&harness));
    assert_eq!(posts(&harness)[0].0, watch("v2"));
}

#[tokio::test]
async fn two_servers_watching_one_channel_read_it_once() {
    let fake = fake(&["v1"]);
    let shared = feeds(&fake);
    let first = harness(&shared);
    let second = harness(&shared);
    first.invoke(add(&url())).await.unwrap();
    second.invoke(add(&url())).await.unwrap();
    first.run_due_jobs().await;
    second.run_due_jobs().await;
    assert_eq!(fake.reads(), 1);
}

#[tokio::test]
async fn the_mention_is_the_chosen_role_only() {
    let fake = fake(&["v1"]);
    let harness = harness(&feeds(&fake));
    harness
        .invoke(
            Invocation::new("feed")
                .subcommand("add")
                .option("kind", text("youtube"))
                .option("target", text(&url()))
                .option("role", CommandOptionValue::Role(Id::new(WATCH_ROLE)))
                .channel(CHANNEL)
                .roles([ADMIN_ROLE]),
        )
        .await
        .unwrap();
    harness.run_due_jobs().await;
    fake.set(&["v2", "v1"]);
    run_later(&harness).await;
    let sent = posts(&harness);
    assert_eq!(sent.len(), 1, "{sent:?}");
    assert!(
        sent[0].0.starts_with(&format!("<@&{WATCH_ROLE}>")),
        "{sent:?}"
    );
    let MentionPolicy::Roles(roles) = &sent[0].1 else {
        panic!("only the role may be mentioned: {sent:?}");
    };
    assert_eq!(roles, &vec![Id::new(WATCH_ROLE)]);
}

#[tokio::test]
async fn twitch_is_refused_without_its_credentials() {
    let fake = fake(&["v1"]);
    let harness = harness(&feeds(&fake));
    let _ = harness.invoke(add_kind("twitch", "someone")).await;
    assert!(subscriptions(&harness).await.is_empty());
}

#[tokio::test]
async fn x_and_pixiv_are_refused_until_the_operator_sets_rsshub() {
    let fake = fake(&["v1"]);
    let harness = harness(&feeds(&fake));
    let _ = harness.invoke(add_kind("x", "@someone")).await;
    let _ = harness.invoke(add_kind("pixiv", "12345")).await;
    assert!(subscriptions(&harness).await.is_empty());
}

#[tokio::test]
async fn an_x_feed_is_read_through_the_configured_rsshub() {
    let fake = fake(&["v1"]);
    let settings = FeedSettings {
        rsshub_base_url: Some("https://rsshub.example.org/".to_owned()),
        ..FeedSettings::default()
    };
    let harness = harness(&feeds_with(&fake, settings));
    harness.invoke(add_kind("x", "@Someone")).await.unwrap();
    let subscription = only(&harness).await;
    assert_eq!(subscription.kind, Kind::X);
    assert_eq!(subscription.target, "someone");
    harness.run_due_jobs().await;
    let wanted = "https://rsshub.example.org/twitter/user/someone".to_owned();
    assert!(fake.urls().contains(&wanted), "{:?}", fake.urls());
}

#[tokio::test]
async fn a_subreddit_is_read_from_its_new_feed() {
    let fake = fake(&["v1"]);
    let harness = harness(&feeds(&fake));
    harness.invoke(add_kind("reddit", "r/Rust")).await.unwrap();
    let subscription = only(&harness).await;
    assert_eq!(subscription.target, "rust");
    harness.run_due_jobs().await;
    let wanted = "https://www.reddit.com/r/rust/new/.rss".to_owned();
    assert!(fake.urls().contains(&wanted), "{:?}", fake.urls());
}

#[test]
fn the_wait_after_a_failure_doubles_up_to_six_hours() {
    assert_eq!(crate::model::backoff(300, 1), 300);
    assert_eq!(crate::model::backoff(300, 3), 1200);
    assert_eq!(crate::model::backoff(300, 20), 6 * 3600);
}

#[test]
fn the_last_seen_list_keeps_the_newest_fifty() {
    let mut seen: Vec<String> = Vec::new();
    for n in 0..60 {
        crate::model::remember(&mut seen, format!("v{n}"));
    }
    assert_eq!(seen.len(), 50);
    assert_eq!(seen[0], "v10");
    assert_eq!(seen[49], "v59");
}
