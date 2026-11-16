use std::sync::Arc;

use anyhow::{Result, bail};
use async_trait::async_trait;
use closure_core::action::Action;
use closure_core::error::DiscordError;
use closure_core::event::{GuildId, UserId};
use closure_sdk::testing::{Click, Harness, Message};
use plugin_kit::net::{Get, Http, HttpStatus};
use plugin_kit::translator::{ApiKey, Translator};
use serde_json::json;
use std::sync::atomic::{AtomicUsize, Ordering};
use twilight_model::channel::Channel;
use twilight_model::channel::message::Component;
use twilight_model::guild::Role;
use twilight_model::id::Id;

use crate::Config;
use crate::builder_with;
use crate::builder_with_sites;
use crate::links::{MAX_PER_MESSAGE, PostLink};
use crate::model::Post;
use crate::pixiv::PixivProvider;
use crate::pixiv::fixtures::{
    ERROR_103, INFO_101, INFO_102, INFO_104, NULL_URLS_105, PAGES_101, PAGES_102, PAGES_104,
};
use crate::provider::Provider;
use serde::Deserialize;

const GUILD: u64 = 1;
const VIEW: u64 = (1 << 10) | (1 << 16);
const POST: &str = r#"{"code":200,"tweet":{
    "url":"https://x.com/jack/status/20","text":"just setting up my twttr",
    "author":{"name":"jack","screen_name":"jack"},"likes":10,"retweets":3,"replies":1,
    "media":{"photos":[{"url":"https://pbs.twimg.com/1.jpg"},{"url":"https://pbs.twimg.com/2.jpg"}],
             "videos":[{"url":"https://video.twimg.com/v.mp4"}]}}}"#;

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

#[async_trait]
impl plugin_kit::net::Post for Fake {
    async fn post_json(
        &self,
        _url: &str,
        _headers: &[(&str, &str)],
        _body: &serde_json::Value,
    ) -> Result<Vec<u8>> {
        bail!("the test service has no Gemini")
    }
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
    "media":{"videos":[{"url":"https://video.twimg.com/huge.mp4"}]}}}"#;

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
    "media":{"photos":[{"url":"https://pbs.twimg.com/1.jpg"}]}}}"#;

const PARENT: &str = r#"{"tweet":{"url":"https://x.com/ev/status/19","text":"the first post",
    "author":{"name":"ev","screen_name":"ev"}}}"#;

const QUOTED: &str = r#"{"tweet":{"url":"https://x.com/dom/status/5","text":"quoted words",
    "author":{"name":"dom","screen_name":"dom"},"likes":7,"retweets":8,"replies":9,
    "media":{"photos":[{"url":"https://pbs.twimg.com/q.jpg"}],
                "videos":[{"url":"https://video.twimg.com/q.mp4"}]}}}"#;

const TRANSLATED: &str =
    r#"[[["a translated post","just setting up my twttr",null,null,10]],null,"ja"]"#;

fn thread(url: &str) -> Option<&'static str> {
    if url.contains("translate_a") {
        Some(TRANSLATED)
    } else if url.contains("pbs.twimg.com") {
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

fn conversation(url: &str) -> Option<&'static str> {
    let id: u32 = url.rsplit("/status/").next()?.parse().ok()?;
    let reply = if (47..=50).contains(&id) {
        format!(r#""replying_to_status":"{}","#, id - 1)
    } else {
        String::new()
    };
    let quote = if (60..=62).contains(&id) {
        let next = id + 1;
        format!(
            r#""quote":{{"url":"https://x.com/u/status/{next}","text":"p{next}",
            "author":{{"name":"u","screen_name":"u"}}}},"#
        )
    } else {
        String::new()
    };
    let body = format!(
        r#"{{"tweet":{{{reply}{quote}"url":"https://x.com/u/status/{id}","text":"p{id}",
        "author":{{"name":"u","screen_name":"u"}}}}}}"#
    );
    Some(Box::leak(body.into_boxed_str()))
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

fn harness(service: impl Http + 'static) -> Harness {
    harness_with(Arc::new(service), Translator::free())
}

fn gemini_translator() -> Translator {
    Translator::gemini(ApiKey::new("test-key"), "gemini-test".to_owned())
}

fn harness_with(service: Arc<dyn Http>, translator: Translator) -> Harness {
    harness_of(builder_with(service, Arc::new(translator)).unwrap())
}

/// The module with `providers` in place of the default sites, its network being `service`.
fn harness_with_sites(service: Arc<dyn Http>, providers: Vec<Arc<dyn Provider>>) -> Harness {
    harness_of(builder_with_sites(service, Arc::new(Translator::free()), providers).unwrap())
}

fn harness_of(builder: closure_sdk::ModuleBuilder) -> Harness {
    let harness = Harness::new(builder).unwrap();
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
    silent: bool,
    field_values: Vec<String>,
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
                silent,
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
                silent,
                field_values: embeds
                    .iter()
                    .flat_map(|e| e.fields.iter().map(|f| f.value.clone()))
                    .collect(),
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
    assert!(
        buttons(&all[0])
            .iter()
            .any(|id| id.starts_with("linkexpand:delete:50:a")),
        "the delete button names the answer in storage"
    );
}

#[tokio::test]
async fn a_channel_the_author_cannot_read_is_not_quoted() {
    let harness = harness(Fake(down));
    say(&harness, "https://discord.com/channels/1/8/42").await;
    assert!(posted(&harness).is_empty());
}

#[tokio::test]
async fn a_message_of_an_adult_channel_is_not_quoted_in_another_channel() {
    let harness = harness(Fake(down));
    let adult: Channel = serde_json::from_value(json!({
        "id": "10", "type": 0, "guild_id": GUILD.to_string(), "name": "adults",
        "nsfw": true, "permission_overwrites": [],
    }))
    .unwrap();
    harness
        .api()
        .set_channels(Id::new(GUILD), vec![channel(7, &json!([])), adult]);
    harness
        .message(Message::new(50, 5).content("https://discord.com/channels/1/10/42"))
        .await
        .unwrap();
    assert!(posted(&harness).is_empty());
    harness
        .message(Message::new(50, 10).content("https://discord.com/channels/1/10/42"))
        .await
        .unwrap();
    assert_eq!(posted(&harness).len(), 1);
}

fn role_with(id: u64, permissions: &str) -> Role {
    serde_json::from_value(json!({
        "id": id.to_string(), "name": format!("role-{id}"), "color": 0,
        "colors": {"primary_color": 0, "secondary_color": null, "tertiary_color": null},
        "hoist": false, "managed": false, "mentionable": false,
        "permissions": permissions, "position": 1, "flags": 0,
    }))
    .unwrap()
}

#[tokio::test]
async fn a_message_of_a_channel_not_everybody_reads_is_quoted_only_in_that_channel() {
    let harness = harness(Fake(down));
    let guild: GuildId = Id::new(GUILD);
    harness.api().set_channels(
        guild,
        vec![channel(
            9,
            &json!([
                {"id": GUILD.to_string(), "type": 0, "allow": "0", "deny": "1024"},
                {"id": "77", "type": 0, "allow": "1024", "deny": "0"},
            ]),
        )],
    );
    harness
        .api()
        .set_roles(guild, vec![everyone(), role_with(77, "0")]);
    let in_channel = |channel: u64| {
        Message::new(50, channel)
            .content("https://discord.com/channels/1/9/42")
            .roles([77])
    };
    harness.message(in_channel(5)).await.unwrap();
    assert!(
        posted(&harness).is_empty(),
        "the others in channel 5 may not read it"
    );
    harness.message(in_channel(9)).await.unwrap();
    assert_eq!(posted(&harness).len(), 1, "in channel 9 they all may");
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
async fn a_post_on_x_has_its_pictures_attached_its_video_linked_and_its_preview_hidden() {
    let harness = harness(Fake(plain));
    say(&harness, "https://x.com/jack/status/20").await;
    let all = posted(&harness);
    assert_eq!(all.len(), 2, "the video alone, then the post");
    assert_eq!(all[0].embeds, 0);
    assert_eq!(
        all[0].content.as_deref(),
        Some("https://video.twimg.com/v.mp4")
    );
    assert!(all[0].components.is_empty());
    assert_eq!(all[1].embeds, 1);
    assert_eq!(all[1].files, ["photo-1.jpg", "photo-2.jpg"]);
    assert_eq!(all[1].content, None);
    assert!(
        all.iter().all(|post| post.silent),
        "the answer to a link does not notify, so the user's own message does not ping twice"
    );
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
async fn a_video_is_the_link_to_the_video_servers_of_x_however_large_it_is() {
    let harness = harness(Fake(huge));
    say(&harness, "https://x.com/jack/status/20").await;
    let all = posted(&harness);
    assert!(all[1].files.is_empty());
    assert_eq!(
        all[0].content.as_deref(),
        Some("https://video.twimg.com/huge.mp4")
    );
}

#[tokio::test]
async fn a_reply_and_a_quote_are_messages_of_their_own_each_answering_the_one_before() {
    let harness = harness(Fake(thread));
    harness.set_config(json!({"layout": "chain"}));
    say(&harness, "https://x.com/jack/status/20").await;
    let all = posted(&harness);
    assert_eq!(
        all.len(),
        4,
        "the video of the quoted post, the first post, the reply, the quoted post"
    );
    assert!(all[1..].iter().all(|p| p.embeds == 1));
    assert_eq!(
        all[0].content.as_deref(),
        Some("https://video.twimg.com/q.mp4")
    );
    assert_eq!(all[2].files, ["photo-1.jpg"]);
    assert!(all[0].reply_to.is_some());
    assert!(all[1..].iter().all(|p| p.reply_to.is_none()));
    assert!(buttons(&all[0]).is_empty() && buttons(&all[1]).is_empty());
    assert!(buttons(&all[2]).is_empty());
    let last = buttons(&all[3]);
    assert_eq!(last.len(), 3);
    assert!(last[0].starts_with("linkexpand:translate:*:k"), "{last:?}");
    assert_eq!(last[1], "https://x.com/jack/status/20");
    let delete = &last[2];
    assert!(delete.starts_with("linkexpand:delete:50:a"), "{delete}");
}

#[tokio::test]
async fn by_default_the_posts_are_the_embeds_of_one_message_named_for_what_they_are() {
    let harness = harness(Fake(thread));
    say(&harness, "https://x.com/jack/status/20").await;
    let all = posted(&harness);
    assert_eq!(
        all.len(),
        2,
        "the video of the quoted post alone, then one message"
    );
    let one = &all[1];
    assert_eq!(one.embeds, 3);
    assert!(one.titles[0].is_some());
    assert!(
        one.titles[1].is_none(),
        "the post that was linked has no label"
    );
    assert!(one.titles[2].is_some());
    assert_ne!(one.titles[0], one.titles[2]);
    assert_eq!(one.footers[2].as_deref(), Some("💬 9  🔁 8  ❤ 7"));
    assert_eq!(one.files, ["photo-1.jpg"]);
    assert_eq!(
        all[0].content.as_deref(),
        Some("https://video.twimg.com/q.mp4"),
        "the video of the quoted post is a link too"
    );
    assert_eq!(one.images, 1, "the quoted post has a picture");
    assert_eq!(
        one.fields, 3,
        "each post has its translation, as it is not in English"
    );
    assert!(
        all[0].reply_to.is_some(),
        "the first message answers the link"
    );
    assert!(one.reply_to.is_none(), "the next one does not reply");
    assert!(buttons(&all[0]).is_empty(), "the video has no buttons");
    assert_eq!(buttons(one).len(), 3);
    assert!(hid_the_original(&harness));
}

#[tokio::test]
async fn a_conversation_is_followed_up_to_its_first_post_and_the_quotes_inside_quotes() {
    let harness = harness(Fake(conversation));
    harness.set_config(json!({"auto_translate": false}));
    say(&harness, "https://x.com/u/status/50").await;
    let all = posted(&harness);
    assert_eq!(all.len(), 1);
    let titles: Vec<Option<&str>> = all[0].titles.iter().map(Option::as_deref).collect();
    assert_eq!(
        titles,
        [
            Some("↩ Original post (this replies to it) 4"),
            Some("↩ Original post (this replies to it) 3"),
            Some("↩ Original post (this replies to it) 2"),
            Some("↩ Original post (this replies to it) 1"),
            None,
        ],
        "46 is the first post"
    );
    let harness = self::harness(Fake(conversation));
    harness.set_config(json!({"auto_translate": false}));
    say(&harness, "https://x.com/u/status/60").await;
    let all = posted(&harness);
    assert_eq!(
        all[0].embeds, 4,
        "the post and three quotes inside one another"
    );
    assert_eq!(all[0].titles[1].as_deref(), Some("💬 Quoted post 1"));
    assert_eq!(all[0].titles[3].as_deref(), Some("💬 Quoted post 3"));
}

#[tokio::test]
async fn the_embeds_of_an_answer_stay_within_what_discord_takes() {
    let harness = harness(Fake(conversation));
    harness.set_config(json!({"auto_translate": false}));
    say(
        &harness,
        "https://x.com/u/status/50 https://x.com/u/status/60",
    )
    .await;
    let all = posted(&harness);
    assert_eq!(all.len(), 1);
    assert!(all[0].embeds <= 10, "{}", all[0].embeds);
    assert_eq!(all[0].embeds, 5 + 4);
}

#[tokio::test]
async fn a_chain_of_messages_keeps_to_four() {
    let harness = harness(Fake(conversation));
    harness.set_config(json!({"layout": "chain", "auto_translate": false}));
    say(&harness, "https://x.com/u/status/50").await;
    assert_eq!(posted(&harness).len(), 4);
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
    assert_eq!(posted(&harness).len(), 3, "the post, the quote, the video");
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
    let last = &all[all.len() - 1];
    let delete = buttons(last).pop().unwrap();
    assert!(
        delete.starts_with("linkexpand:delete:50:") && delete.len() > "linkexpand:delete:50:".len(),
        "the video message names the message before it: {delete}"
    );
    assert_eq!(
        buttons(last)[..2],
        [
            "linkexpand:translate:*:k".to_owned(),
            "https://x.com/jack/status/20".to_owned(),
        ]
    );
    let without = self::harness(Fake(plain));
    without.set_config(json!({"translate_button": false}));
    say(&without, "https://x.com/jack/status/20").await;
    assert_eq!(buttons(posted(&without).last().unwrap()).len(), 2);
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

fn deleted_messages(harness: &Harness) -> Vec<u64> {
    harness
        .actions()
        .iter()
        .filter_map(|a| match a {
            Action::DeleteMessage { message_id, .. } => Some(message_id.get()),
            _ => None,
        })
        .collect()
}

fn delete_button(post: &Posted) -> String {
    buttons(post)
        .into_iter()
        .find(|id| id.starts_with("linkexpand:delete:"))
        .expect("the answer's last message has a delete button")
}

#[tokio::test]
async fn a_delete_takes_the_pictures_posted_before_the_answer_too() {
    let harness = harness(Pictures {
        post: post_with(&urls(14), false),
        bytes: one_byte,
    });
    harness.set_config(json!({"auto_translate": false}));
    say(&harness, "https://x.com/jack/status/20").await;
    let all = posted(&harness);
    assert_eq!(
        all.len(),
        3,
        "the pictures in two messages, then the answer"
    );
    harness
        .click(Click::new(delete_button(&all[2])).user(50))
        .await
        .unwrap();
    // The harness clicks on a message of its own (900); the answer's messages are 1, 2 and 3.
    let gone = deleted_messages(&harness);
    assert!(
        gone.contains(&1) && gone.contains(&2),
        "the pictures go too: {gone:?}"
    );
    assert_eq!(
        gone.len(),
        4,
        "the three of the answer, and the message the button is on"
    );
}

#[tokio::test]
async fn a_delete_whose_answer_is_no_longer_stored_takes_only_its_own_message() {
    let harness = harness(Pictures {
        post: post_with(&urls(14), false),
        bytes: one_byte,
    });
    harness.set_config(json!({"auto_translate": false}));
    say(&harness, "https://x.com/jack/status/20").await;
    let all = posted(&harness);
    let button = delete_button(&all[2]);
    let key = format!(
        "answer:{}",
        button.rsplit(':').next().unwrap().trim_start_matches('a')
    );
    harness.store().delete(&key).await.unwrap();
    harness.click(Click::new(button).user(50)).await.unwrap();
    assert_eq!(
        deleted_messages(&harness).len(),
        1,
        "without the list only the message the button is on goes"
    );
}

#[tokio::test]
async fn a_delete_that_fails_says_why_in_its_own_words() {
    let cases = [
        (DiscordError::NotFound, "already deleted"),
        (
            DiscordError::Forbidden("missing permission".into()),
            "may not delete",
        ),
        (
            DiscordError::Transient("timed out".into()),
            "could not be deleted",
        ),
    ];
    for (error, said_of_it) in cases {
        let harness = harness(Fake(down));
        // The fourth request is the delete of the answer itself; the ones before it are the messages named.
        harness.api().fail_nth(4, error);
        let outcome = harness
            .click(Click::new("linkexpand:delete:50:111,222").user(50))
            .await;
        let said = format!("{outcome:?}");
        assert!(
            said.contains(said_of_it),
            "{said_of_it:?} expected, got {said}"
        );
        assert!(!said.contains("could not be translated"), "{said}");
    }
}

#[tokio::test]
async fn the_translate_button_answers_with_the_post_in_another_language() {
    let harness = harness(Fake(thread));
    let replies = harness
        .click(Click::new("linkexpand:translate:*:k"))
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
        .click(Click::new("linkexpand:translate:*:k"))
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

#[test]
fn embeds_that_are_too_long_together_are_shortened_and_not_refused() {
    let embed = |text: &str| {
        let mut e = twilight_model::channel::message::Embed {
            author: None,
            color: None,
            description: Some(text.to_owned()),
            fields: Vec::new(),
            flags: None,
            footer: None,
            image: None,
            kind: "rich".into(),
            provider: None,
            thumbnail: None,
            timestamp: None,
            title: Some("t".into()),
            url: None,
            video: None,
        };
        e.title = Some("t".into());
        e
    };
    let mut embeds: Vec<_> = (0..10).map(|_| embed(&"x".repeat(1200))).collect();
    crate::fit(&mut embeds);
    let total: usize = embeds.iter().map(crate::counted).sum();
    assert!(total <= 5_800, "{total}");
    assert_eq!(embeds.len(), 10, "all are kept, shortened");
}

#[test]
fn a_chain_that_is_too_long_keeps_what_was_linked_and_its_neighbours() {
    let unit = |main: bool| crate::Unit {
        embed: twilight_model::channel::message::Embed {
            author: None,
            color: None,
            description: None,
            fields: Vec::new(),
            flags: None,
            footer: None,
            image: None,
            kind: "rich".into(),
            provider: None,
            thumbnail: None,
            timestamp: None,
            title: Some(if main { "main" } else { "other" }.into()),
            url: None,
            video: None,
        },
        photos: Vec::new(),
        video: None,
        main,
        referer: None,
    };
    let units = vec![
        unit(false),
        unit(true),
        unit(false),
        unit(false),
        unit(false),
    ];
    let kept = crate::within(&units, 3);
    assert_eq!(kept.len(), 3);
    assert!(
        kept.iter().any(|unit| unit.main),
        "what was linked is never the one that goes"
    );
}

const SENSITIVE: &str = r#"{"code":200,"tweet":{
    "url":"https://x.com/jack/status/20","text":"a sensitive post",
    "author":{"name":"jack","screen_name":"jack"},"possibly_sensitive":true}}"#;

#[allow(
    clippy::unnecessary_wraps,
    reason = "an answerer, as the type of `Fake` says"
)]
fn sensitive(_: &str) -> Option<&'static str> {
    Some(SENSITIVE)
}

#[tokio::test]
async fn a_post_marked_sensitive_is_not_expanded_outside_an_adult_channel() {
    let harness = harness(Fake(sensitive));
    say(&harness, "https://x.com/jack/status/20").await;
    assert!(posted(&harness).is_empty());
    assert!(
        !hid_the_original(&harness),
        "the link stays as it was written"
    );
}

#[tokio::test]
async fn a_post_marked_sensitive_is_expanded_in_an_adult_channel() {
    let harness = harness(Fake(sensitive));
    let adult: Channel = serde_json::from_value(json!({
        "id": "10", "type": 0, "guild_id": GUILD.to_string(), "name": "adults",
        "nsfw": true, "permission_overwrites": [],
    }))
    .unwrap();
    harness
        .api()
        .set_channels(Id::new(GUILD), vec![channel(7, &json!([])), adult]);
    harness
        .message(Message::new(50, 10).content("https://x.com/jack/status/20"))
        .await
        .unwrap();
    let all = posted(&harness);
    assert_eq!(all.len(), 1);
    assert_eq!(all[0].embeds, 1);
}

/// A site that is not X: it has its own link, its own service and its own pictures.
struct Dummy;

impl Provider for Dummy {
    fn enabled(&self, _config: &Config) -> bool {
        true
    }

    fn detect(&self, text: &str) -> Vec<PostLink> {
        text.split_whitespace()
            .filter_map(|word| word.strip_prefix("https://fake.example/post/"))
            .filter_map(|id| id.parse().ok())
            .map(|id| PostLink {
                user: "fake".into(),
                id,
            })
            .take(MAX_PER_MESSAGE)
            .collect()
    }

    fn api_url(&self, id: u64) -> String {
        format!("https://api.fake.example/post/{id}")
    }

    fn parse(&self, bytes: &[u8]) -> Result<Post> {
        #[derive(Deserialize)]
        struct Body {
            id: u64,
            name: String,
            text: String,
            image: String,
        }
        let body: Body = serde_json::from_slice(bytes)?;
        Ok(Post {
            id: body.id,
            url: format!("https://fake.example/post/{}", body.id),
            author: crate::model::Author {
                name: body.name.clone(),
                handle: body.name.clone(),
                avatar: None,
                profile: String::new(),
            },
            text: body.text,
            images: vec![body.image],
            video: None,
            counts: Vec::new(),
            created: None,
            sensitive: false,
            color: 0x12_3456,
            reply_to: None,
            quote: None,
            reposted_by: None,
            media_referer: None,
        })
    }

    fn media_hosts(&self) -> &'static [&'static str] {
        &["media.fake.example"]
    }
}

const DUMMY_POST: &str = r#"{"id":7,"name":"Fake Author","text":"a post elsewhere",
    "image":"https://media.fake.example/1.jpg"}"#;

#[allow(
    clippy::unnecessary_wraps,
    reason = "an answerer, as the type of `Fake` says"
)]
fn dummy_site(url: &str) -> Option<&'static str> {
    if url.starts_with("https://api.fake.example/post/") {
        Some(DUMMY_POST)
    } else if url.starts_with("https://media.fake.example/") {
        Some("picture")
    } else {
        None
    }
}

#[tokio::test]
async fn a_post_of_another_site_is_expanded_by_the_same_sending() {
    let harness = Harness::new(
        builder_with_sites(
            Arc::new(Fake(dummy_site)),
            Arc::new(Translator::free()),
            vec![Arc::new(Dummy)],
        )
        .unwrap(),
    )
    .unwrap();
    harness.api().add_channel(Id::new(5), Some(Id::new(GUILD)));
    harness
        .message(Message::new(50, 5).content("see https://fake.example/post/7"))
        .await
        .unwrap();
    let all = posted(&harness);
    assert_eq!(all.len(), 1);
    assert_eq!(all[0].embeds, 1);
    assert_eq!(all[0].files, ["photo-1.jpg"]);
    assert!(all[0].reply_to.is_some());
    assert!(all[0].silent);
    assert!(hid_the_original(&harness));
}

fn urls(count: usize) -> Vec<String> {
    (1..=count)
        .map(|n| format!("https://pbs.twimg.com/{n}.jpg"))
        .collect()
}

fn post_with(photos: &[String], reply: bool) -> String {
    let photos: Vec<String> = photos
        .iter()
        .map(|url| format!(r#"{{"url":"{url}"}}"#))
        .collect();
    let reply = if reply {
        r#""replying_to_status":"19","#
    } else {
        ""
    };
    format!(
        r#"{{"tweet":{{{reply}"url":"https://x.com/jack/status/20","text":"many pictures",
        "author":{{"name":"jack","screen_name":"jack"}},"media":{{"photos":[{}]}}}}}}"#,
        photos.join(",")
    )
}

/// A post, and its pictures, each as large as `bytes` says of its address.
struct Pictures {
    post: String,
    bytes: fn(&str) -> usize,
}

#[async_trait]
impl Get for Pictures {
    async fn get(&self, url: &str) -> Result<Vec<u8>> {
        if url.contains("pbs.twimg.com") {
            Ok(vec![0; (self.bytes)(url)])
        } else if url.contains("/status/19") {
            Ok(PARENT.as_bytes().to_vec())
        } else if url.contains("/status/20") {
            Ok(self.post.clone().into_bytes())
        } else {
            bail!("down")
        }
    }
}

#[async_trait]
impl plugin_kit::net::Post for Pictures {
    async fn post_json(
        &self,
        _url: &str,
        _headers: &[(&str, &str)],
        _body: &serde_json::Value,
    ) -> Result<Vec<u8>> {
        bail!("the test service has no Gemini")
    }
}

fn one_byte(_: &str) -> usize {
    1
}

fn large(_: &str) -> usize {
    2_500_000
}

fn oversized(url: &str) -> usize {
    if url.contains("huge") { 21_000_000 } else { 1 }
}

#[tokio::test]
async fn fourteen_pictures_go_ten_then_four_before_the_answer() {
    let harness = harness(Pictures {
        post: post_with(&urls(14), false),
        bytes: one_byte,
    });
    harness.set_config(json!({"auto_translate": false}));
    say(&harness, "https://x.com/jack/status/20").await;
    let all = posted(&harness);
    assert_eq!(
        all.len(),
        3,
        "the pictures in two messages, then the answer"
    );
    assert_eq!(all[0].files.len(), 10);
    assert_eq!(
        all[1].files,
        [
            "photo-11.jpg",
            "photo-12.jpg",
            "photo-13.jpg",
            "photo-14.jpg"
        ]
    );
    assert_eq!((all[0].embeds, all[1].embeds), (0, 0));
    assert!(all[0].reply_to.is_some());
    assert!(all[1].reply_to.is_none());
    assert!(buttons(&all[0]).is_empty() && buttons(&all[1]).is_empty());
    assert_eq!(all[2].embeds, 1);
    assert!(all[2].files.is_empty());
    assert!(all[2].reply_to.is_none());
    assert!(!buttons(&all[2]).is_empty());
    assert!(all.iter().all(|post| post.silent));
}

#[tokio::test]
async fn twenty_pictures_go_ten_and_ten_before_the_answer() {
    let harness = harness(Pictures {
        post: post_with(&urls(20), false),
        bytes: one_byte,
    });
    harness.set_config(json!({"auto_translate": false}));
    say(&harness, "https://x.com/jack/status/20").await;
    let all = posted(&harness);
    assert_eq!(all.len(), 3);
    assert_eq!(all[0].files.len(), 10);
    assert_eq!(all[1].files.len(), 10);
    assert_eq!(
        all[1].files.last().map(String::as_str),
        Some("photo-20.jpg")
    );
    assert_eq!(all[2].embeds, 1);
    assert!(all[2].files.is_empty());
    assert!(!buttons(&all[2]).is_empty());
}

#[tokio::test]
async fn a_message_takes_the_pictures_that_fit_its_bytes_and_the_next_one_takes_the_rest() {
    let harness = harness(Pictures {
        post: post_with(&urls(9), false),
        bytes: large,
    });
    harness.set_config(json!({"auto_translate": false}));
    say(&harness, "https://x.com/jack/status/20").await;
    let all = posted(&harness);
    assert_eq!(
        all.len(),
        3,
        "the pictures in two messages, then the answer"
    );
    assert_eq!(all[0].files.len(), 8, "eight of 2.5 MB make 20 MB");
    assert_eq!(all[1].files, ["photo-9.jpg"]);
    assert_eq!(all[2].embeds, 1);
    assert!(all[2].files.is_empty());
}

#[tokio::test]
async fn a_picture_larger_than_a_message_is_left_out_and_the_others_are_kept() {
    let photos: Vec<String> = ["1", "huge", "3"]
        .iter()
        .map(|n| format!("https://pbs.twimg.com/{n}.jpg"))
        .collect();
    let harness = harness(Pictures {
        post: post_with(&photos, false),
        bytes: oversized,
    });
    harness.set_config(json!({"auto_translate": false}));
    say(&harness, "https://x.com/jack/status/20").await;
    let all = posted(&harness);
    assert_eq!(all.len(), 1);
    assert_eq!(all[0].files, ["photo-1.jpg", "photo-3.jpg"]);
}

#[tokio::test]
async fn a_message_of_pictures_that_fails_leaves_the_answer_and_the_request_is_handled() {
    let harness = harness(Pictures {
        post: post_with(&urls(14), false),
        bytes: one_byte,
    });
    harness.set_config(json!({"auto_translate": false}));
    // The second request is the message of the pictures that did not fit in the first.
    harness
        .api()
        .fail_nth(2, DiscordError::Transient("timed out".into()));
    say(&harness, "https://x.com/jack/status/20").await;
    let all = posted(&harness);
    assert_eq!(all.len(), 2, "the pictures that went, then the answer");
    assert_eq!(all[0].files.len(), 10);
    assert_eq!(all[1].embeds, 1);
    assert!(!buttons(&all[1]).is_empty());
}

#[tokio::test]
async fn in_a_chain_the_pictures_of_a_reply_go_before_its_message() {
    let harness = harness(Pictures {
        post: post_with(&urls(14), true),
        bytes: one_byte,
    });
    harness.set_config(json!({"layout": "chain", "auto_translate": false, "quoted_post": false}));
    say(&harness, "https://x.com/jack/status/20").await;
    let all = posted(&harness);
    assert_eq!(
        all.len(),
        4,
        "the post replied to, the pictures of the reply in two messages, then the reply"
    );
    assert_eq!(all[0].embeds, 1);
    assert!(all[0].files.is_empty());
    assert!(all[0].reply_to.is_some());
    assert_eq!(all[1].files.len(), 10);
    assert_eq!(all[2].files.len(), 4);
    assert_eq!((all[1].embeds, all[2].embeds), (0, 0));
    assert!(all[1].reply_to.is_none() && all[2].reply_to.is_none());
    assert_eq!(all[3].embeds, 1);
    assert!(all[3].files.is_empty());
    assert!(all[3].reply_to.is_none());
    assert!(!buttons(&all[3]).is_empty());
    assert!(buttons(&all[0]).is_empty());
    assert!(buttons(&all[1]).is_empty() && buttons(&all[2]).is_empty());
}

#[tokio::test]
async fn ten_pictures_stay_in_the_answer_message() {
    let harness = harness(Pictures {
        post: post_with(&urls(10), false),
        bytes: one_byte,
    });
    harness.set_config(json!({"auto_translate": false}));
    say(&harness, "https://x.com/jack/status/20").await;
    let all = posted(&harness);
    assert_eq!(
        all.len(),
        1,
        "one message holds the answer and ten pictures"
    );
    assert_eq!(all[0].files.len(), 10);
    assert_eq!(all[0].embeds, 1);
    assert!(all[0].reply_to.is_some());
    assert!(!buttons(&all[0]).is_empty());
}

#[tokio::test]
async fn a_video_goes_first_then_the_pictures_and_the_answer_last() {
    let photos: Vec<String> = urls(14)
        .iter()
        .map(|url| format!(r#"{{"url":"{url}"}}"#))
        .collect();
    let post = format!(
        r#"{{"tweet":{{"url":"https://x.com/jack/status/20","text":"a video and many pictures",
        "author":{{"name":"jack","screen_name":"jack"}},
        "media":{{"photos":[{}],"videos":[{{"url":"https://video.twimg.com/v.mp4"}}]}}}}}}"#,
        photos.join(",")
    );
    let harness = harness(Pictures {
        post,
        bytes: one_byte,
    });
    harness.set_config(json!({"auto_translate": false}));
    say(&harness, "https://x.com/jack/status/20").await;
    let all = posted(&harness);
    assert_eq!(
        all.len(),
        4,
        "the video, the pictures in two messages, then the answer"
    );
    assert!(
        all[0]
            .content
            .as_deref()
            .is_some_and(|text| text.contains("v.mp4"))
    );
    assert_eq!(all[0].embeds, 0);
    assert!(all[0].reply_to.is_some());
    assert_eq!(all[1].files.len(), 10);
    assert_eq!(all[2].files.len(), 4);
    assert!(all[1].reply_to.is_none() && all[2].reply_to.is_none());
    assert_eq!(all[3].embeds, 1);
    assert!(all[3].files.is_empty());
    assert!(!buttons(&all[3]).is_empty());
}

/// The free endpoint of `thread`, and a Gemini that answers as `gemini` says, counting its asks.
struct WithGemini {
    free: Fake,
    gemini: fn() -> Result<Vec<u8>>,
    asked: Arc<AtomicUsize>,
}

#[async_trait]
impl Get for WithGemini {
    async fn get(&self, url: &str) -> Result<Vec<u8>> {
        self.free.get(url).await
    }
}

#[async_trait]
impl plugin_kit::net::Post for WithGemini {
    async fn post_json(
        &self,
        _url: &str,
        _headers: &[(&str, &str)],
        _body: &serde_json::Value,
    ) -> Result<Vec<u8>> {
        self.asked.fetch_add(1, Ordering::Relaxed);
        (self.gemini)()
    }
}

#[allow(
    clippy::unnecessary_wraps,
    reason = "an answerer, as the type of `WithGemini` says"
)]
fn gemini_says_words() -> Result<Vec<u8>> {
    let structured = json!({"translation": "Gemini words", "source_language": "ja"}).to_string();
    Ok(
        json!({"candidates": [{"content": {"parts": [{"text": structured}]}}]})
            .to_string()
            .into_bytes(),
    )
}

fn gemini_is_down() -> Result<Vec<u8>> {
    Err(HttpStatus(503).into())
}

fn with_gemini(gemini: fn() -> Result<Vec<u8>>) -> (WithGemini, Arc<AtomicUsize>) {
    let asked = Arc::new(AtomicUsize::new(0));
    let service = WithGemini {
        free: Fake(thread),
        gemini,
        asked: Arc::clone(&asked),
    };
    (service, asked)
}

#[tokio::test]
async fn a_post_is_translated_by_gemini_first_for_each_of_its_posts() {
    let (service, asked) = with_gemini(gemini_says_words);
    let harness = harness_with(Arc::new(service), gemini_translator());
    say(&harness, "https://x.com/jack/status/20").await;
    assert_eq!(
        asked.load(Ordering::Relaxed),
        3,
        "the linked post, its reply and its quote"
    );
    let values: Vec<String> = posted(&harness)
        .into_iter()
        .flat_map(|p| p.field_values)
        .collect();
    assert_eq!(values, ["Gemini words", "Gemini words", "Gemini words"]);
}

#[tokio::test]
async fn a_gemini_that_is_down_leaves_the_translation_to_the_free_endpoint() {
    let (service, asked) = with_gemini(gemini_is_down);
    let harness = harness_with(Arc::new(service), gemini_translator());
    say(&harness, "https://x.com/jack/status/20").await;
    assert_eq!(asked.load(Ordering::Relaxed), 3);
    let values: Vec<String> = posted(&harness)
        .into_iter()
        .flat_map(|p| p.field_values)
        .collect();
    assert_eq!(
        values,
        [
            "a translated post",
            "a translated post",
            "a translated post"
        ]
    );
}

#[tokio::test]
async fn the_translate_button_is_answered_by_gemini_first() {
    let (service, asked) = with_gemini(gemini_says_words);
    let harness = harness_with(Arc::new(service), gemini_translator());
    let replies = harness
        .click(Click::new("linkexpand:translate:*:k"))
        .await
        .unwrap();
    assert_eq!(asked.load(Ordering::Relaxed), 3);
    assert!(
        replies.iter().any(|r| r.content.contains("Gemini words")),
        "{replies:?}"
    );
}

/// Pixiv's public JSON for three artworks, and the pictures of them; every other address is down.
/// It records the `Referer` each picture is fetched with.
struct PixivService {
    seen: std::sync::Mutex<Vec<(String, Option<String>)>>,
}

fn pixiv_body(url: &str) -> Option<&'static str> {
    let path = url.strip_prefix("https://www.pixiv.net/ajax/illust/")?;
    match path {
        "101?lang=ja" => Some(INFO_101),
        "101/pages?lang=ja" => Some(PAGES_101),
        "102?lang=ja" => Some(INFO_102),
        "102/pages?lang=ja" => Some(PAGES_102),
        "103?lang=ja" => Some(ERROR_103),
        "104?lang=ja" => Some(INFO_104),
        "104/pages?lang=ja" => Some(PAGES_104),
        _ => None,
    }
}

#[async_trait]
impl Get for PixivService {
    async fn get(&self, url: &str) -> Result<Vec<u8>> {
        match pixiv_body(url) {
            Some(body) => Ok(body.as_bytes().to_vec()),
            None => bail!("down"),
        }
    }

    async fn get_media(&self, url: &str, _max: usize, referer: Option<&str>) -> Result<Vec<u8>> {
        self.seen
            .lock()
            .unwrap()
            .push((url.to_owned(), referer.map(str::to_owned)));
        Ok(b"picture".to_vec())
    }
}

#[async_trait]
impl plugin_kit::net::Post for PixivService {
    async fn post_json(
        &self,
        _url: &str,
        _headers: &[(&str, &str)],
        _body: &serde_json::Value,
    ) -> Result<Vec<u8>> {
        bail!("the test service has no POST")
    }
}

fn pixiv_service() -> Arc<PixivService> {
    Arc::new(PixivService {
        seen: std::sync::Mutex::new(Vec::new()),
    })
}

#[tokio::test]
async fn a_pixiv_artwork_is_expanded_with_its_pages_fetched_with_the_pixiv_referer() {
    let service = pixiv_service();
    let harness = harness_with(service.clone(), Translator::free());
    harness.set_config(json!({"auto_translate": false}));
    say(&harness, "https://www.pixiv.net/artworks/101").await;
    let all = posted(&harness);
    assert_eq!(all.len(), 1);
    assert_eq!(all[0].embeds, 1);
    assert_eq!(all[0].footers, [Some("❤ 42  🔖 7".to_owned())]);
    assert_eq!(all[0].files, ["photo-1.png", "photo-2.png"]);
    assert!(hid_the_original(&harness));
    let seen = service.seen.lock().unwrap().clone();
    assert_eq!(seen.len(), 2);
    assert!(
        seen.iter()
            .all(|(url, referer)| url.starts_with("https://i.pximg.net/")
                && referer.as_deref() == Some("https://www.pixiv.net/")),
        "{seen:?}"
    );
}

#[tokio::test]
async fn a_pixiv_artwork_the_service_refuses_is_left_alone() {
    let harness = harness_with(pixiv_service(), Translator::free());
    say(&harness, "https://www.pixiv.net/artworks/103").await;
    assert!(posted(&harness).is_empty());
    assert!(!hid_the_original(&harness));
}

#[tokio::test]
async fn a_pixiv_artwork_for_adults_is_expanded_only_in_an_adult_channel() {
    let harness = harness_with(pixiv_service(), Translator::free());
    harness.set_config(json!({"auto_translate": false}));
    say(&harness, "https://www.pixiv.net/artworks/102").await;
    assert!(posted(&harness).is_empty());
    let adult: Channel = serde_json::from_value(json!({
        "id": "10", "type": 0, "guild_id": GUILD.to_string(), "name": "adults",
        "nsfw": true, "permission_overwrites": [],
    }))
    .unwrap();
    let harness = harness_with(pixiv_service(), Translator::free());
    harness.set_config(json!({"auto_translate": false}));
    harness
        .api()
        .set_channels(Id::new(GUILD), vec![channel(7, &json!([])), adult]);
    harness
        .message(Message::new(50, 10).content("https://www.pixiv.net/artworks/102"))
        .await
        .unwrap();
    assert_eq!(posted(&harness).len(), 1);
}

#[tokio::test]
async fn a_picture_from_another_host_is_neither_fetched_nor_attached() {
    let service = pixiv_service();
    let harness = harness_with(service.clone(), Translator::free());
    harness.set_config(json!({"auto_translate": false}));
    say(&harness, "https://www.pixiv.net/artworks/104").await;
    let all = posted(&harness);
    assert_eq!(
        all[0].files,
        ["photo-1.png"],
        "the one from pixiv, numbered after the one dropped"
    );
    let seen = service.seen.lock().unwrap().clone();
    assert!(
        seen.iter()
            .all(|(url, _)| url.starts_with("https://i.pximg.net/")),
        "{seen:?}"
    );
}

#[tokio::test]
async fn the_other_forms_of_a_pixiv_link_are_expanded_and_the_switch_turns_them_off() {
    let harness = harness_with(pixiv_service(), Translator::free());
    harness.set_config(json!({"auto_translate": false}));
    say(&harness, "https://phixiv.net/artworks/101").await;
    assert_eq!(posted(&harness).len(), 1);
    harness.set_config(json!({"pixiv_links": false}));
    say(&harness, "https://www.pixiv.net/artworks/101").await;
    assert_eq!(posted(&harness).len(), 1, "the switch is off: nothing new");
}

/// The session cookie the tests of the operator's session use.
const SESSION: &str = "PHPSESSID=1_session7";

/// Pixiv as a visitor with the session sees it: the adult artwork 102 is answered only with the
/// cookie, and artwork 105 is answered without its pictures. It records the cookie of each JSON
/// request, and the address of each picture.
struct SessionService {
    cookies: std::sync::Mutex<Vec<(String, Option<String>)>>,
    pictures: std::sync::Mutex<Vec<String>>,
}

fn session_body(url: &str, cookie: Option<&str>) -> Option<&'static str> {
    let path = url.strip_prefix("https://www.pixiv.net/ajax/illust/")?;
    let signed_in = cookie == Some(SESSION);
    Some(match path {
        "101?lang=ja" => INFO_101,
        "101/pages?lang=ja" => PAGES_101,
        "102?lang=ja" if signed_in => INFO_102,
        "102/pages?lang=ja" if signed_in => PAGES_102,
        "105?lang=ja" => NULL_URLS_105,
        "102?lang=ja" | "102/pages?lang=ja" => ERROR_103,
        _ => return None,
    })
}

#[async_trait]
impl Get for SessionService {
    async fn get(&self, url: &str) -> Result<Vec<u8>> {
        self.get_with_headers(url, &[]).await
    }

    async fn get_with_headers(&self, url: &str, headers: &[(&str, &str)]) -> Result<Vec<u8>> {
        let cookie = headers
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case("cookie"))
            .map(|(_, value)| (*value).to_owned());
        self.cookies
            .lock()
            .unwrap()
            .push((url.to_owned(), cookie.clone()));
        match session_body(url, cookie.as_deref()) {
            Some(body) => Ok(body.as_bytes().to_vec()),
            None => bail!("down"),
        }
    }

    async fn get_media(&self, url: &str, _max: usize, _referer: Option<&str>) -> Result<Vec<u8>> {
        self.pictures.lock().unwrap().push(url.to_owned());
        Ok(b"picture".to_vec())
    }
}

#[async_trait]
impl plugin_kit::net::Post for SessionService {
    async fn post_json(
        &self,
        _url: &str,
        _headers: &[(&str, &str)],
        _body: &serde_json::Value,
    ) -> Result<Vec<u8>> {
        bail!("the test service has no POST")
    }
}

fn session_service() -> Arc<SessionService> {
    Arc::new(SessionService {
        cookies: std::sync::Mutex::new(Vec::new()),
        pictures: std::sync::Mutex::new(Vec::new()),
    })
}

fn session_providers(session: Option<&str>) -> Vec<Arc<dyn Provider>> {
    vec![Arc::new(PixivProvider::new(session.map(ApiKey::new)))]
}

/// Posts `content` from a channel for adults, the only one where an adult artwork is expanded.
async fn say_in_adult_channel(harness: &Harness, content: &str) {
    let adult: Channel = serde_json::from_value(json!({
        "id": "10", "type": 0, "guild_id": GUILD.to_string(), "name": "adults",
        "nsfw": true, "permission_overwrites": [],
    }))
    .unwrap();
    harness
        .api()
        .set_channels(Id::new(GUILD), vec![channel(7, &json!([])), adult]);
    harness
        .message(Message::new(50, 10).content(content))
        .await
        .unwrap();
}

#[tokio::test]
async fn the_session_cookie_goes_to_the_pixiv_json_and_no_picture_gets_it() {
    let service = session_service();
    let harness = harness_with_sites(service.clone(), session_providers(Some("1_session7")));
    harness.set_config(json!({"auto_translate": false}));
    say(&harness, "https://www.pixiv.net/artworks/101").await;
    assert_eq!(posted(&harness)[0].files, ["photo-1.png", "photo-2.png"]);
    let cookies = service.cookies.lock().unwrap().clone();
    assert_eq!(cookies.len(), 2, "the artwork and its pages");
    assert!(
        cookies.iter().all(|(url, cookie)| {
            url.starts_with("https://www.pixiv.net/ajax/") && cookie.as_deref() == Some(SESSION)
        }),
        "the session goes to the pixiv JSON only"
    );
    let pictures = service.pictures.lock().unwrap().clone();
    assert_eq!(pictures.len(), 2);
    assert!(
        pictures
            .iter()
            .all(|url| url.starts_with("https://i.pximg.net/"))
    );
}

#[tokio::test]
async fn without_a_session_no_cookie_is_sent_and_an_adult_artwork_is_not_expanded() {
    let service = session_service();
    let harness = harness_with_sites(service.clone(), session_providers(None));
    harness.set_config(json!({"auto_translate": false}));
    say_in_adult_channel(&harness, "https://www.pixiv.net/artworks/102").await;
    assert!(posted(&harness).is_empty());
    let cookies = service.cookies.lock().unwrap().clone();
    assert!(!cookies.is_empty());
    assert!(cookies.iter().all(|(_, cookie)| cookie.is_none()));
}

#[tokio::test]
async fn an_adult_artwork_is_expanded_with_the_session_only_in_an_adult_channel() {
    let harness = harness_with_sites(session_service(), session_providers(Some("1_session7")));
    harness.set_config(json!({"auto_translate": false}));
    say(&harness, "https://www.pixiv.net/artworks/102").await;
    assert!(
        posted(&harness).is_empty(),
        "not in a channel for everybody"
    );
    say_in_adult_channel(&harness, "https://www.pixiv.net/artworks/102").await;
    assert_eq!(posted(&harness).len(), 1);
}

#[tokio::test]
async fn an_artwork_answered_without_its_pictures_is_left_alone_and_does_not_panic() {
    let harness = harness_with_sites(session_service(), session_providers(Some("1_session7")));
    harness.set_config(json!({"auto_translate": false}));
    say_in_adult_channel(&harness, "https://www.pixiv.net/artworks/105").await;
    assert!(posted(&harness).is_empty());
    assert!(!hid_the_original(&harness));
}
