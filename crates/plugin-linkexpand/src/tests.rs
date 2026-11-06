use std::sync::Arc;

use anyhow::{Result, bail};
use async_trait::async_trait;
use closure_core::action::Action;
use closure_core::event::{GuildId, UserId};
use closure_sdk::testing::{Click, Harness, Message};
use plugin_kit::net::Get;
use serde_json::json;
use twilight_model::channel::Channel;
use twilight_model::channel::message::Component;
use twilight_model::guild::Role;
use twilight_model::id::Id;

use crate::builder_with;

const GUILD: u64 = 1;
const VIEW: u64 = (1 << 10) | (1 << 16);
const POST: &str = r#"{"code":200,"tweet":{
    "url":"https://x.com/jack/status/20","text":"just setting up my twttr",
    "author":{"name":"jack","screen_name":"jack"},"likes":10,"retweets":3,"replies":1,
    "media":{"photos":[{"url":"https://p.example/1.jpg"},{"url":"https://p.example/2.jpg"}],
             "videos":[{"url":"https://v.example/v.mp4"}]}}}"#;

/// The services, answering what `answer` says for an address, or failing when it says nothing. A
/// file whose address says `huge` is too large to fetch.
struct Fake(fn(&str) -> Option<&'static str>);

#[async_trait]
impl Get for Fake {
    async fn get(&self, url: &str) -> Result<Vec<u8>> {
        match (self.0)(url) {
            Some(body) => Ok(body.as_bytes().to_vec()),
            None => bail!("down"),
        }
    }

    async fn get_large(&self, url: &str, _max: usize) -> Result<Vec<u8>> {
        if url.contains("huge") {
            bail!("too large");
        }
        self.get(url).await
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

const HUGE: &str = r#"{"code":200,"tweet":{
    "url":"https://x.com/jack/status/20","text":"a long video",
    "author":{"name":"jack","screen_name":"jack"},
    "media":{"videos":[{"url":"https://v.example/huge.mp4"}]}}}"#;

#[allow(
    clippy::unnecessary_wraps,
    reason = "an answerer, as the type of `Fake` says"
)]
fn huge(_: &str) -> Option<&'static str> {
    Some(HUGE)
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

const QUOTED: &str = r#"{"tweet":{"url":"https://x.com/dom/status/5","text":"quoted words",
    "author":{"name":"dom","screen_name":"dom"},"likes":7,"retweets":8,"replies":9,
    "media":{"photos":[{"url":"https://p.example/q.jpg"}],
                "videos":[{"url":"https://v.example/q.mp4"}]}}}"#;

const TRANSLATED: &str =
    r#"[[["a translated post","just setting up my twttr",null,null,10]],null,"ja"]"#;

fn thread(url: &str) -> Option<&'static str> {
    if url.contains("translate_a") {
        Some(TRANSLATED)
    } else if url.contains("p.example") {
        Some("picture")
    } else if url.contains("/status/19") {
        Some(PARENT)
    } else if url.contains("/status/20") {
        Some(REPLY)
    } else if url.contains("/status/5") {
        Some(QUOTED)
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

/// What was posted, in order.
struct Posted {
    content: Option<String>,
    embeds: usize,
    titles: Vec<Option<String>>,
    footers: Vec<Option<String>>,
    images: usize,
    fields: usize,
    reply_to: Option<u64>,
    files: Vec<String>,
    components: Vec<Component>,
}

fn posted(harness: &Harness) -> Vec<Posted> {
    harness
        .actions()
        .into_iter()
        .filter_map(|a| match a {
            Action::PostMessage {
                content,
                embeds,
                reply_to,
                files,
                components,
                ..
            } => Some(Posted {
                content,
                embeds: embeds.len(),
                titles: embeds.iter().map(|e| e.title.clone()).collect(),
                footers: embeds
                    .iter()
                    .map(|e| e.footer.as_ref().map(|f| f.text.clone()))
                    .collect(),
                images: embeds.iter().filter(|e| e.image.is_some()).count(),
                fields: embeds.iter().map(|e| e.fields.len()).sum(),
                reply_to: reply_to.map(twilight_model::id::Id::get),
                files: files.into_iter().map(|f| f.filename).collect(),
                components,
            }),
            _ => None,
        })
        .collect()
}

fn hid_the_original(harness: &Harness) -> bool {
    harness
        .actions()
        .iter()
        .any(|a| matches!(a, Action::SuppressEmbeds { .. }))
}

/// The `custom_id` or the link of each button of a message.
fn buttons(post: &Posted) -> Vec<String> {
    post.components
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

async fn say(harness: &Harness, content: &str) {
    harness
        .message(Message::new(50, 5).content(content))
        .await
        .unwrap();
}

#[tokio::test]
async fn a_link_to_a_readable_message_is_answered_with_a_quote_and_the_preview_stays() {
    let harness = harness(Fake(down));
    say(&harness, "look https://discord.com/channels/1/7/42").await;
    let all = posted(&harness);
    assert_eq!(all.len(), 1);
    assert_eq!((all[0].embeds, all[0].reply_to.is_some()), (1, true));
    assert!(all[0].files.is_empty());
    assert!(
        !hid_the_original(&harness),
        "Discord shows nothing of such a link"
    );
    assert_eq!(buttons(&all[0]), ["linkexpand:delete:50:".to_owned()]);
}

#[tokio::test]
async fn a_channel_the_author_cannot_read_is_not_quoted() {
    let harness = harness(Fake(down));
    say(&harness, "https://discord.com/channels/1/8/42").await;
    assert!(posted(&harness).is_empty());
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
    assert!(posted(&harness).is_empty());
}

#[tokio::test]
async fn a_post_on_x_has_its_pictures_and_video_attached_and_its_preview_hidden() {
    let harness = harness(Fake(plain));
    say(&harness, "https://x.com/jack/status/20").await;
    let all = posted(&harness);
    assert_eq!(all.len(), 1);
    assert_eq!(all[0].embeds, 1);
    assert_eq!(all[0].files, ["photo-1.jpg", "photo-2.jpg", "video.mp4"]);
    assert_eq!(all[0].content, None);
    assert!(hid_the_original(&harness));
    harness.set_config(json!({"hide_original": false}));
    let before = harness.actions().len();
    say(&harness, "https://x.com/jack/status/20").await;
    let hid_again = harness.actions()[before..]
        .iter()
        .any(|a| matches!(a, Action::SuppressEmbeds { .. }));
    assert!(!hid_again);
}

#[tokio::test]
async fn a_video_too_large_to_attach_is_shown_through_a_player_link() {
    let harness = harness(Fake(huge));
    say(&harness, "https://x.com/jack/status/20").await;
    let all = posted(&harness);
    assert!(all[0].files.is_empty());
    assert_eq!(
        all[0].content.as_deref(),
        Some("https://fixupx.com/jack/status/20")
    );
}

#[tokio::test]
async fn a_reply_and_a_quote_are_messages_of_their_own_each_answering_the_one_before() {
    let harness = harness(Fake(thread));
    harness.set_config(json!({"layout": "chain"}));
    say(&harness, "https://x.com/jack/status/20").await;
    let all = posted(&harness);
    assert_eq!(all.len(), 3, "the first post, the reply, the quoted post");
    assert!(all.iter().all(|p| p.embeds == 1));
    assert_eq!(all[1].files, ["photo-1.jpg"]);
    // Each answers the one before: the first the link, the others a message of the bot.
    assert!(all[0].reply_to.is_some() && all[1].reply_to.is_some() && all[2].reply_to.is_some());
    // The buttons are under the last one, and take all of them away.
    assert!(buttons(&all[0]).is_empty() && buttons(&all[1]).is_empty());
    let last = buttons(&all[2]);
    assert_eq!(last.len(), 3);
    assert!(
        last[0].starts_with("linkexpand:translate:*:j,k,5"),
        "{last:?}"
    );
    assert_eq!(last[1], "https://x.com/jack/status/20");
    let delete = &last[2];
    assert!(
        delete.starts_with("linkexpand:delete:50:") && delete.contains(','),
        "{delete}"
    );
}

#[tokio::test]
async fn by_default_the_posts_are_the_embeds_of_one_message_named_for_what_they_are() {
    let harness = harness(Fake(thread));
    say(&harness, "https://x.com/jack/status/20").await;
    let all = posted(&harness);
    assert_eq!(all.len(), 1, "one message");
    let one = &all[0];
    assert_eq!(one.embeds, 3);
    assert!(one.titles[0].is_some());
    assert!(
        one.titles[1].is_none(),
        "the post that was linked has no label"
    );
    assert!(one.titles[2].is_some());
    assert_ne!(one.titles[0], one.titles[2]);
    // The quoted post was fetched whole, so it has its counts.
    assert_eq!(one.footers[2].as_deref(), Some("💬 9  🔁 8  ❤ 7"));
    // What was linked has its picture attached; the others show theirs small.
    assert_eq!(one.files, ["photo-1.jpg"]);
    assert_eq!(
        one.content.as_deref(),
        Some("https://fixupx.com/dom/status/5"),
        "the video of the quoted post is a player link, it is not attached"
    );
    assert_eq!(one.images, 1, "the quoted post has a picture");
    assert_eq!(
        one.fields, 3,
        "each post has its translation, as it is not in English"
    );
    assert!(one.reply_to.is_some());
    assert_eq!(buttons(one).len(), 3);
    assert!(hid_the_original(&harness));
}

#[tokio::test]
async fn the_translation_can_be_left_out() {
    let harness = harness(Fake(thread));
    harness.set_config(json!({"auto_translate": false}));
    say(&harness, "https://x.com/jack/status/20").await;
    assert_eq!(posted(&harness)[0].fields, 0);
}

#[tokio::test]
async fn the_post_answered_and_the_post_quoted_can_each_be_left_out() {
    let harness = harness(Fake(thread));
    harness.set_config(json!({"layout": "chain", "replied_post": false}));
    say(&harness, "https://x.com/jack/status/20").await;
    assert_eq!(posted(&harness).len(), 2);
    let harness = self::harness(Fake(thread));
    harness.set_config(json!({"layout": "chain", "quoted_post": false, "replied_post": false}));
    say(&harness, "https://x.com/jack/status/20").await;
    assert_eq!(posted(&harness).len(), 1);
}

#[tokio::test]
async fn a_service_that_is_down_leaves_the_link_alone() {
    let harness = harness(Fake(down));
    say(&harness, "https://x.com/jack/status/20").await;
    assert!(posted(&harness).is_empty() && !hid_the_original(&harness));
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
    assert!(posted(&harness).is_empty());
    harness.set_config(json!({"x_links": false}));
    say(
        &harness,
        "https://x.com/jack/status/20 https://discord.com/channels/1/7/42",
    )
    .await;
    assert_eq!(posted(&harness).len(), 1);
}

#[tokio::test]
async fn the_last_message_has_buttons_to_translate_to_open_and_to_delete() {
    let harness = harness(Fake(plain));
    say(&harness, "https://x.com/jack/status/20").await;
    let all = posted(&harness);
    assert_eq!(
        buttons(&all[0]),
        [
            "linkexpand:translate:*:k".to_owned(),
            "https://x.com/jack/status/20".to_owned(),
            "linkexpand:delete:50:".to_owned(),
        ]
    );
    let without = self::harness(Fake(plain));
    without.set_config(json!({"translate_button": false}));
    say(&without, "https://x.com/jack/status/20").await;
    assert_eq!(buttons(&posted(&without)[0]).len(), 2);
}

#[tokio::test]
async fn only_the_one_who_posted_the_link_deletes_the_answer_all_of_it() {
    let harness = harness(Fake(down));
    let deleted = |h: &Harness| -> Vec<u64> {
        h.actions()
            .iter()
            .filter_map(|a| match a {
                Action::DeleteMessage { message_id, .. } => Some(message_id.get()),
                _ => None,
            })
            .collect()
    };
    let _ = harness
        .click(Click::new("linkexpand:delete:50:111,222").user(51))
        .await;
    assert!(deleted(&harness).is_empty(), "somebody else cannot");
    harness
        .click(Click::new("linkexpand:delete:50:111,222").user(50))
        .await
        .unwrap();
    let gone = deleted(&harness);
    assert_eq!(
        gone.len(),
        3,
        "the two before it, and the one the button is on"
    );
    assert!(gone.contains(&111) && gone.contains(&222));
}

#[tokio::test]
async fn the_translate_button_answers_with_the_post_in_another_language() {
    let harness = harness(Fake(thread));
    let replies = harness
        .click(Click::new("linkexpand:translate:*:j,k,5"))
        .await
        .unwrap();
    assert!(
        replies
            .iter()
            .any(|r| r.content.contains("a translated post")),
        "{replies:?}"
    );
    let down_harness = self::harness(Fake(down));
    let replies = down_harness
        .click(Click::new("linkexpand:translate:*:j,k,5"))
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
