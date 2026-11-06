use std::sync::Arc;

use anyhow::{Result, bail};
use async_trait::async_trait;
use closure_core::action::Action;
use closure_core::event::{GuildId, UserId};
use closure_sdk::testing::{Harness, Message};
use serde_json::json;
use twilight_model::channel::Channel;
use twilight_model::guild::Role;
use twilight_model::id::Id;

use crate::builder_with;
use crate::fetch::Get;

const GUILD: u64 = 1;
const VIEW: u64 = (1 << 10) | (1 << 16);
const POST: &str = r#"{"code":200,"tweet":{
    "url":"https://x.com/jack/status/20","text":"just setting up my twttr",
    "author":{"name":"jack","screen_name":"jack"},"likes":10,"retweets":3,"replies":1,
    "media":{"photos":[{"url":"https://p.example/1.jpg"},{"url":"https://p.example/2.jpg"}],
             "videos":[{"url":"https://v.example/v.mp4"}]}}}"#;

/// The service, answering with `body`, or failing when there is none.
struct Fake(Option<&'static str>);

#[async_trait]
impl Get for Fake {
    async fn get(&self, _: &str) -> Result<Vec<u8>> {
        match self.0 {
            Some(body) => Ok(body.as_bytes().to_vec()),
            None => bail!("down"),
        }
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
            Action::SendMessage {
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
    let harness = harness(Fake(None));
    say(&harness, "look https://discord.com/channels/1/7/42").await;
    assert_eq!(sent(&harness), [(None, 1, true)]);
    let Some(Action::SendMessage { embeds, .. }) = harness
        .actions()
        .into_iter()
        .find(|a| matches!(a, Action::SendMessage { .. }))
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
    let harness = harness(Fake(None));
    say(&harness, "https://discord.com/channels/1/8/42").await;
    assert!(sent(&harness).is_empty());
}

#[tokio::test]
async fn other_servers_unwrapped_requests_and_bots_are_left_alone() {
    let harness = harness(Fake(None));
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
    let harness = harness(Fake(Some(POST)));
    say(&harness, "https://x.com/jack/status/20").await;
    assert_eq!(
        sent(&harness),
        [(Some("https://v.example/v.mp4".to_owned()), 2, true)]
    );
}

#[tokio::test]
async fn a_service_that_is_down_leaves_the_link_alone() {
    let harness = harness(Fake(None));
    say(&harness, "https://x.com/jack/status/20").await;
    assert!(sent(&harness).is_empty());
}

#[tokio::test]
async fn each_kind_of_link_can_be_turned_off() {
    let harness = harness(Fake(Some(POST)));
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
