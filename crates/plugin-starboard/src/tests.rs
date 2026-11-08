use closure_core::action::{Action, Emoji};
use closure_core::event::{ChannelId, Event, GuildId, MessageId, ReactionInfo, UserId};
use closure_sdk::testing::Harness;
use serde_json::json;
use twilight_model::channel::{Channel, Message};
use twilight_model::guild::Role;
use twilight_model::id::Id;

use crate::builder;

const GUILD: u64 = 1;
const SOURCE_CHANNEL: u64 = 7;
const PRIVATE_CHANNEL: u64 = 8;
const ADULT_CHANNEL: u64 = 9;
const BOARD: u64 = 20;
const VIEW: u64 = (1 << 10) | (1 << 16);

fn channel(id: u64, overwrites: &serde_json::Value, nsfw: bool) -> Channel {
    serde_json::from_value(json!({
        "id": id.to_string(), "type": 0, "guild_id": GUILD.to_string(),
        "name": format!("channel-{id}"), "nsfw": nsfw, "permission_overwrites": overwrites,
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

fn message(channel: u64, id: u64, stars: u64, emoji: &str) -> Message {
    let reactions = if stars == 0 {
        json!([])
    } else {
        json!([{"count": stars, "count_details": {"burst": 0, "normal": stars}, "me": false,
            "me_burst": false, "burst_colors": [], "emoji": {"id": null, "name": emoji}}])
    };
    serde_json::from_value(json!({
        "id": id.to_string(), "channel_id": channel.to_string(), "content": "great",
        "author": {"id": "60", "username": "alice", "discriminator": "0", "avatar": null},
        "attachments": [], "embeds": [], "mention_everyone": false, "mention_roles": [],
        "mentions": [], "pinned": false, "timestamp": "2026-10-24T00:00:00.000000+00:00",
        "edited_timestamp": null, "tts": false, "type": 0, "reactions": reactions,
    }))
    .unwrap()
}

fn harness() -> Harness {
    let harness = Harness::new(builder().unwrap()).unwrap();
    let guild: GuildId = Id::new(GUILD);
    harness.api().set_channels(
        guild,
        vec![
            channel(SOURCE_CHANNEL, &json!([]), false),
            channel(
                PRIVATE_CHANNEL,
                &json!([{"id": GUILD.to_string(), "type": 0, "allow": "0", "deny": "1024"}]),
                false,
            ),
            channel(ADULT_CHANNEL, &json!([]), true),
            channel(BOARD, &json!([]), false),
        ],
    );
    harness.api().set_roles(guild, vec![everyone()]);
    harness.set_config(json!({"channel": BOARD.to_string(), "threshold": 3}));
    harness
}

async fn react(harness: &Harness, channel: u64, id: u64, emoji: &str, add: bool) {
    let info = ReactionInfo {
        guild_id: Id::new(GUILD),
        channel_id: ChannelId::new(channel),
        message_id: MessageId::new(id),
        user_id: UserId::new(70),
        emoji: Emoji::Unicode {
            value: emoji.to_owned(),
        },
    };
    let event = if add {
        Event::ReactionAdded(info)
    } else {
        Event::ReactionRemoved(info)
    };
    harness.emit(event).await.unwrap();
}

fn posts(harness: &Harness) -> Vec<String> {
    harness
        .actions()
        .iter()
        .filter_map(|a| match a {
            Action::SendMessage {
                channel_id,
                content: Some(text),
                ..
            } if channel_id.get() == BOARD => Some(text.clone()),
            _ => None,
        })
        .collect()
}

fn edits(harness: &Harness) -> Vec<String> {
    harness
        .actions()
        .iter()
        .filter_map(|a| match a {
            Action::EditMessage {
                content: Some(text),
                ..
            } => Some(text.clone()),
            _ => None,
        })
        .collect()
}

fn deleted(harness: &Harness) -> usize {
    harness
        .actions()
        .iter()
        .filter(|a| matches!(a, Action::DeleteMessage { .. }))
        .count()
}

#[tokio::test]
async fn a_message_is_posted_once_it_has_enough_stars() {
    let harness = harness();
    harness
        .api()
        .set_message(message(SOURCE_CHANNEL, 100, 2, "⭐"));
    react(&harness, SOURCE_CHANNEL, 100, "⭐", true).await;
    assert!(posts(&harness).is_empty(), "two are not enough");
    harness
        .api()
        .set_message(message(SOURCE_CHANNEL, 100, 3, "⭐"));
    react(&harness, SOURCE_CHANNEL, 100, "⭐", true).await;
    assert_eq!(posts(&harness), [format!("⭐ **3** | <#{SOURCE_CHANNEL}>")]);
}

#[tokio::test]
async fn the_post_follows_the_count_and_goes_when_no_star_is_left() {
    let harness = harness();
    harness
        .api()
        .set_message(message(SOURCE_CHANNEL, 100, 3, "⭐"));
    react(&harness, SOURCE_CHANNEL, 100, "⭐", true).await;
    harness
        .api()
        .set_message(message(SOURCE_CHANNEL, 100, 5, "⭐"));
    react(&harness, SOURCE_CHANNEL, 100, "⭐", true).await;
    assert_eq!(posts(&harness).len(), 1, "it is not posted twice");
    assert_eq!(edits(&harness), [format!("⭐ **5** | <#{SOURCE_CHANNEL}>")]);
    harness
        .api()
        .set_message(message(SOURCE_CHANNEL, 100, 0, "⭐"));
    react(&harness, SOURCE_CHANNEL, 100, "⭐", false).await;
    assert_eq!(deleted(&harness), 1);
    harness
        .api()
        .set_message(message(SOURCE_CHANNEL, 100, 3, "⭐"));
    react(&harness, SOURCE_CHANNEL, 100, "⭐", true).await;
    assert_eq!(posts(&harness).len(), 2);
}

#[tokio::test]
async fn other_emoji_the_starboard_itself_and_an_unset_channel_change_nothing() {
    let harness = harness();
    harness
        .api()
        .set_message(message(SOURCE_CHANNEL, 100, 5, "🔥"));
    react(&harness, SOURCE_CHANNEL, 100, "🔥", true).await;
    harness.api().set_message(message(BOARD, 101, 5, "⭐"));
    react(&harness, BOARD, 101, "⭐", true).await;
    harness
        .api()
        .set_message(message(SOURCE_CHANNEL, 102, 5, "⭐"));
    harness.set_config(json!({"threshold": 3}));
    react(&harness, SOURCE_CHANNEL, 102, "⭐", true).await;
    assert!(posts(&harness).is_empty());
}

#[tokio::test]
async fn what_not_everybody_can_read_or_is_for_adults_is_never_posted() {
    let harness = harness();
    for (channel, id) in [(PRIVATE_CHANNEL, 110), (ADULT_CHANNEL, 111)] {
        harness.api().set_message(message(channel, id, 5, "⭐"));
        react(&harness, channel, id, "⭐", true).await;
    }
    assert!(posts(&harness).is_empty());
}

#[tokio::test]
async fn the_threshold_and_the_emoji_are_the_servers_to_choose() {
    let harness = harness();
    harness.set_config(json!({"channel": BOARD.to_string(), "threshold": 1, "emoji": "🌟"}));
    harness
        .api()
        .set_message(message(SOURCE_CHANNEL, 120, 1, "🌟"));
    react(&harness, SOURCE_CHANNEL, 120, "🌟", true).await;
    assert_eq!(posts(&harness), [format!("🌟 **1** | <#{SOURCE_CHANNEL}>")]);
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

#[tokio::test]
async fn the_post_goes_with_its_source_and_follows_an_edit_of_it() {
    let harness = harness();
    harness
        .api()
        .set_message(message(SOURCE_CHANNEL, 100, 3, "⭐"));
    react(&harness, SOURCE_CHANNEL, 100, "⭐", true).await;
    assert_eq!(posts(&harness).len(), 1);
    let edited = serde_json::from_value(json!({
        "id": "100", "channel_id": SOURCE_CHANNEL.to_string(), "guild_id": GUILD.to_string(),
        "author": {"id": "60", "name": "alice", "bot": false}, "author_roles": [],
        "content": "changed", "mentioned_users": [], "mentions_everyone": false,
        "from_webhook": false, "created_at": 0,
    }));
    {
        let info = edited.expect("a message");
        harness.emit(Event::MessageUpdated(info)).await.unwrap();
        let quotes = harness
            .actions()
            .iter()
            .filter(|a| {
                matches!(
                    a,
                    Action::EditMessage {
                        embeds: Some(_),
                        ..
                    }
                )
            })
            .count();
        assert_eq!(quotes, 1);
    }
    harness
        .emit(Event::MessageDeleted {
            guild_id: Id::new(GUILD),
            channel_id: ChannelId::new(SOURCE_CHANNEL),
            message_id: MessageId::new(100),
        })
        .await
        .unwrap();
    assert_eq!(deleted(&harness), 1);
}

#[tokio::test]
async fn an_edit_made_after_the_channel_went_private_is_not_put_on_the_board() {
    let harness = harness();
    harness
        .api()
        .set_message(message(SOURCE_CHANNEL, 100, 3, "⭐"));
    react(&harness, SOURCE_CHANNEL, 100, "⭐", true).await;
    assert_eq!(posts(&harness).len(), 1);
    harness.api().set_channels(
        Id::new(GUILD),
        vec![
            channel(
                SOURCE_CHANNEL,
                &json!([{"id": GUILD.to_string(), "type": 0, "allow": "0", "deny": "1024"}]),
                false,
            ),
            channel(BOARD, &json!([]), false),
        ],
    );
    let info = serde_json::from_value(json!({
        "id": "100", "channel_id": SOURCE_CHANNEL.to_string(), "guild_id": GUILD.to_string(),
        "author": {"id": "60", "name": "alice", "bot": false}, "author_roles": [],
        "content": "something only some may read", "mentioned_users": [],
        "mentions_everyone": false, "from_webhook": false, "created_at": 0,
    }))
    .expect("a message");
    harness.emit(Event::MessageUpdated(info)).await.unwrap();
    assert!(
        !harness.actions().iter().any(|a| matches!(
            a,
            Action::EditMessage {
                embeds: Some(_),
                ..
            }
        )),
        "the new text is not quoted"
    );
    assert_eq!(deleted(&harness), 1, "what was quoted comes off the board");
}

#[tokio::test]
async fn a_post_that_could_not_be_taken_off_is_still_recorded_and_not_made_twice() {
    let harness = harness();
    harness
        .api()
        .set_message(message(SOURCE_CHANNEL, 100, 3, "⭐"));
    react(&harness, SOURCE_CHANNEL, 100, "⭐", true).await;
    assert_eq!(posts(&harness).len(), 1);
    harness
        .api()
        .set_message(message(SOURCE_CHANNEL, 100, 0, "⭐"));
    harness
        .api()
        .fail_nth(2, closure_core::error::DiscordError::Fatal("down".into()));
    let _ = harness
        .emit(Event::ReactionRemoved(ReactionInfo {
            guild_id: Id::new(GUILD),
            channel_id: ChannelId::new(SOURCE_CHANNEL),
            message_id: MessageId::new(100),
            user_id: UserId::new(70),
            emoji: Emoji::Unicode {
                value: "⭐".into()
            },
        }))
        .await;
    harness
        .api()
        .set_message(message(SOURCE_CHANNEL, 100, 3, "⭐"));
    react(&harness, SOURCE_CHANNEL, 100, "⭐", true).await;
    assert_eq!(posts(&harness).len(), 1, "no second post beside the first");
}

#[tokio::test]
async fn a_post_stays_managed_on_the_board_it_was_made_on() {
    let harness = harness();
    harness
        .api()
        .set_message(message(SOURCE_CHANNEL, 100, 3, "⭐"));
    react(&harness, SOURCE_CHANNEL, 100, "⭐", true).await;
    harness.set_config(json!({"channel": "21", "threshold": 3}));
    harness
        .emit(Event::MessageDeleted {
            guild_id: Id::new(GUILD),
            channel_id: ChannelId::new(SOURCE_CHANNEL),
            message_id: MessageId::new(100),
        })
        .await
        .unwrap();
    let deleted_on: Vec<u64> = harness
        .actions()
        .iter()
        .filter_map(|a| match a {
            Action::DeleteMessage { channel_id, .. } => Some(channel_id.get()),
            _ => None,
        })
        .collect();
    assert_eq!(
        deleted_on,
        [BOARD],
        "the old post is taken off the old board"
    );
}

#[tokio::test]
async fn the_authors_own_star_is_left_out_however_many_people_reacted() {
    let harness = harness();
    let mut starred = serde_json::to_value(message(SOURCE_CHANNEL, 100, 1_201, "⭐")).unwrap();
    starred["author"]["id"] = json!("5000");
    harness
        .api()
        .set_message(serde_json::from_value(starred).unwrap());
    let mut reactors: Vec<UserId> = (1_000..2_200).map(UserId::new).collect();
    reactors.push(UserId::new(5_000));
    harness.api().set_reactors(
        ChannelId::new(SOURCE_CHANNEL),
        MessageId::new(100),
        reactors,
    );
    react(&harness, SOURCE_CHANNEL, 100, "⭐", true).await;
    assert_eq!(
        posts(&harness),
        [format!("⭐ **1200** | <#{SOURCE_CHANNEL}>")]
    );
}
