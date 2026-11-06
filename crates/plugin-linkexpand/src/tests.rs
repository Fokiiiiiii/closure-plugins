use std::sync::Arc;

use anyhow::{Result, bail};
use async_trait::async_trait;
use closure_core::action::Action;
use closure_core::event::{GuildId, UserId};
use closure_sdk::testing::{Click, Harness, Message};
use serde_json::json;
use twilight_model::channel::Channel;
use twilight_model::channel::message::Component;
use twilight_model::guild::Role;
use twilight_model::id::Id;

use crate::builder_with;
use plugin_kit::net::Get;

const GUILD: u64 = 1;
const VIEW: u64 = (1 << 10) | (1 << 16);
const POST: &str = r#"{"code":200,"tweet":{
    "url":"https://x.com/jack/status/20","text":"just setting up my twttr",
    "author":{"name":"jack","screen_name":"jack"},"likes":10,"retweets":3,"replies":1,
    "media":{"photos":[{"url":"https://p.example/1.jpg"},{"url":"https://p.example/2.jpg"}],
             "videos":[{"url":"https://v.example/v.mp4"}]}}}"#;

/// The services, answering what `answer` says for an address, or failing when it says nothing.
struct Fake(fn(&str) -> Option<&'static str>);

#[async_trait]
impl Get for Fake {
    async fn get(&self, url: &str) -> Result<Vec<u8>> {
        match (self.0)(url) {
            Some(body) => Ok(body.as_bytes().to_vec()),
            None => bail!("down"),
        }
    }
}

fn down(_: &str) -> Option<&'static str> {
    None
}

#[allow(
    clippy::unnecessary_wraps,
    reason = "an answerer, as the type of `Fake` says"
)]
fn plain(_: &str) -> Option<&'static str> {
    Some(POST)
}

const REPLY: &str = r#"{"code":200,"tweet":{
    "url":"https://x.com/jack/status/20","text":"my answer",
    "author":{"name":"jack","screen_name":"jack"},"likes":1,"retweets":2,"replies":3,
    "replying_to_status":"19",
    "quote":{"url":"https://x.com/dom/status/5","text":"quoted words",
             "author":{"name":"dom","screen_name":"dom"}},
    "media":{"photos":[{"url":"https://p.example/1.jpg"}]}}}"#;

const PARENT: &str = r#"{"tweet":{"url":"https://x.com/ev/status/19","text":"the first post",
    "author":{"name":"ev","screen_name":"ev"}}}"#;

const TRANSLATED: &str =
    r#"[[["a translated post","just setting up my twttr",null,null,10]],null,"en"]"#;

fn thread(url: &str) -> Option<&'static str> {
    if url.contains("translate_a") {
        Some(TRANSLATED)
    } else if url.contains("/status/19") {
        Some(PARENT)
    } else if url.contains("/status/20") {
        Some(REPLY)
    } else {
        None
    }
}

fn channel(id: u64, overwrites: &serde_json::Value) -> Channel {
    serde_json::from_value(json!({
        "id": id.to_string(), "type": 0, "guild_id": GUILD.to_string(),
        "name": format!("channel-{id}"), "permission_overwrites": overwrites,
    }))
    .unwrap()
}

fn everyone() -> Role {
    serde_json::from_value(json!({
        "id": GUILD.to_string(), "name": "@everyone", "color": 0,
        "colors": {"primary_color": 0, "secondary_color": null, "tertiary_color": null},
        "hoist": false, "managed": false, "mentionable": false,
        "permissions": VIEW.to_string(), "position": 0, "flags": 0,
    }))
    .unwrap()
}

/// Channel 7 is open to everybody, channel 8 hides itself from `@everyone`.
fn harness(service: Fake) -> Harness {
    let harness = Harness::new(builder_with(Arc::new(service)).unwrap()).unwrap();
    let guild: GuildId = Id::new(GUILD);
    harness.api().set_channels(
        guild,
        vec![
            channel(7, &json!([])),
            channel(
                8,
                &json!([{"id": GUILD.to_string(), "type": 0, "allow": "0", "deny": "1024"}]),
            ),
        ],
    );
    harness.api().add_channel(Id::new(5), Some(guild));
    harness.api().set_roles(guild, vec![everyone()]);
    harness.api().set_owner(guild, UserId::new(999));
    harness
}

fn sent(harness: &Harness) -> Vec<(Option<String>, usize, bool)> {
    harness
        .actions()
        .iter()
        .filter_map(|a| match a {
            Action::PostMessage {
                content,
                embeds,
                reply_to,
                ..
            } => Some((content.clone(), embeds.len(), reply_to.is_some())),
            _ => None,
        })
        .collect()
}

async fn say(harness: &Harness, content: &str) {
    harness
        .message(Message::new(50, 5).content(content))
        .await
        .unwrap();
}

#[tokio::test]
async fn a_link_to_a_readable_message_is_answered_with_a_quote() {
    let harness = harness(Fake(down));
    say(&harness, "look https://discord.com/channels/1/7/42").await;
    assert_eq!(sent(&harness), [(None, 1, true)]);
    let Some(Action::PostMessage { embeds, .. }) = harness
        .actions()
        .into_iter()
        .find(|a| matches!(a, Action::PostMessage { .. }))
    else {
        panic!("a message was sent");
    };
    let field = &embeds[0].fields[0].value;
    assert!(
        field.contains("#channel-7") && field.contains("/channels/1/7/42"),
        "{field}"
    );
}

#[tokio::test]
async fn a_channel_the_author_cannot_read_is_not_quoted() {
    let harness = harness(Fake(down));
    say(&harness, "https://discord.com/channels/1/8/42").await;
    assert!(sent(&harness).is_empty());
}

#[tokio::test]
async fn other_servers_unwrapped_requests_and_bots_are_left_alone() {
    let harness = harness(Fake(down));
    say(&harness, "https://discord.com/channels/2/7/42").await;
    say(&harness, "<https://discord.com/channels/1/7/42>").await;
    harness
        .message(
            Message::new(51, 5)
                .content("https://discord.com/channels/1/7/42")
                .bot(),
        )
        .await
        .unwrap();
    assert!(sent(&harness).is_empty());
}

#[tokio::test]
async fn a_post_on_x_is_answered_with_its_pictures_and_a_video_link() {
    let harness = harness(Fake(plain));
    say(&harness, "https://x.com/jack/status/20").await;
    assert_eq!(
        sent(&harness),
        [(Some("https://v.example/v.mp4".to_owned()), 2, true)]
    );
}

#[tokio::test]
async fn a_service_that_is_down_leaves_the_link_alone() {
    let harness = harness(Fake(down));
    say(&harness, "https://x.com/jack/status/20").await;
    assert!(sent(&harness).is_empty());
}

#[tokio::test]
async fn each_kind_of_link_can_be_turned_off() {
    let harness = harness(Fake(plain));
    harness.set_config(json!({"message_links": false, "x_links": false}));
    say(
        &harness,
        "https://x.com/jack/status/20 https://discord.com/channels/1/7/42",
    )
    .await;
    assert!(sent(&harness).is_empty());
    harness.set_config(json!({"x_links": false}));
    say(
        &harness,
        "https://x.com/jack/status/20 https://discord.com/channels/1/7/42",
    )
    .await;
    assert_eq!(sent(&harness), [(None, 1, true)]);
}

fn embeds_of(harness: &Harness) -> usize {
    sent(harness).first().map_or(0, |(_, embeds, _)| *embeds)
}

#[tokio::test]
async fn a_reply_brings_the_post_it_answers_and_a_quote_the_post_it_quotes() {
    let harness = harness(Fake(thread));
    say(&harness, "https://x.com/jack/status/20").await;
    assert_eq!(
        embeds_of(&harness),
        3,
        "the first post, the reply, the quoted post"
    );
    let Some(Action::PostMessage { embeds, .. }) = harness
        .actions()
        .into_iter()
        .find(|a| matches!(a, Action::PostMessage { .. }))
    else {
        panic!("a message was sent");
    };
    let texts: Vec<Option<&str>> = embeds.iter().map(|e| e.description.as_deref()).collect();
    assert_eq!(
        texts,
        [
            Some("the first post"),
            Some("my answer"),
            Some("quoted words")
        ]
    );
}

#[tokio::test]
async fn the_post_answered_and_the_post_quoted_can_each_be_left_out() {
    let harness = harness(Fake(thread));
    harness.set_config(json!({"replied_post": false}));
    say(&harness, "https://x.com/jack/status/20").await;
    assert_eq!(embeds_of(&harness), 2);
    let harness = self::harness(Fake(thread));
    harness.set_config(json!({"quoted_post": false, "replied_post": false}));
    say(&harness, "https://x.com/jack/status/20").await;
    assert_eq!(embeds_of(&harness), 1);
}

/// The buttons of the answer: the `custom_id` or the link of each.
fn buttons(harness: &Harness) -> Vec<String> {
    let Some(Action::PostMessage { components, .. }) = harness
        .actions()
        .into_iter()
        .find(|a| matches!(a, Action::PostMessage { .. }))
    else {
        return Vec::new();
    };
    components
        .iter()
        .flat_map(|row| match row {
            Component::ActionRow(row) => row.components.clone(),
            _ => Vec::new(),
        })
        .filter_map(|c| match c {
            Component::Button(b) => b.custom_id.or(b.url),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn the_answer_has_buttons_to_translate_to_open_and_to_delete() {
    let harness = harness(Fake(plain));
    say(&harness, "https://x.com/jack/status/20").await;
    assert_eq!(
        buttons(&harness),
        [
            "linkexpand:translate:*:20".to_owned(),
            "https://x.com/jack/status/20".to_owned(),
            "linkexpand:delete:50:".to_owned(),
        ]
    );
    let quoted = self::harness(Fake(down));
    say(&quoted, "https://discord.com/channels/1/7/42").await;
    assert_eq!(buttons(&quoted), ["linkexpand:delete:50:".to_owned()]);
    let without = self::harness(Fake(plain));
    without.set_config(json!({"translate_button": false}));
    say(&without, "https://x.com/jack/status/20").await;
    assert_eq!(buttons(&without).len(), 2);
}

#[tokio::test]
async fn only_the_one_who_posted_the_link_deletes_the_answer() {
    let harness = harness(Fake(down));
    let deleted = |h: &Harness| {
        h.actions()
            .iter()
            .any(|a| matches!(a, Action::DeleteMessage { .. }))
    };
    let _ = harness
        .click(Click::new("linkexpand:delete:50:").user(51))
        .await;
    assert!(!deleted(&harness), "somebody else cannot");
    harness
        .click(Click::new("linkexpand:delete:50:").user(50))
        .await
        .unwrap();
    assert!(deleted(&harness));
}

#[tokio::test]
async fn the_translate_button_answers_with_the_post_in_another_language() {
    let harness = harness(Fake(thread));
    let replies = harness
        .click(Click::new("linkexpand:translate:*:20"))
        .await
        .unwrap();
    assert!(
        replies
            .iter()
            .any(|r| r.content.contains("a translated post")),
        "{replies:?}"
    );
    // The service being down is told, and nothing is posted.
    let down_harness = self::harness(Fake(down));
    let replies = down_harness
        .click(Click::new("linkexpand:translate:*:20"))
        .await
        .unwrap();
    assert!(
        replies
            .iter()
            .all(|r| !r.content.contains("a translated post"))
    );
}

#[test]
fn both_catalogs_define_the_same_messages() {
    assert_eq!(
        closure_i18n::missing_keys(crate::EN, crate::JA).unwrap(),
        Vec::<String>::new()
    );
    assert_eq!(
        closure_i18n::missing_keys(crate::JA, crate::EN).unwrap(),
        Vec::<String>::new()
    );
}
