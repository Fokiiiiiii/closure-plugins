use std::time::Duration;

use closure_core::action::Action;
use closure_core::event::{ChannelId, Event, GuildId, UserId, UserInfo};
use closure_sdk::testing::{Harness, Invocation, Message};
use closure_sdk::{CommandOptionValue, Level};
use serde_json::json;
use twilight_model::id::Id;

use crate::builder;

const MODERATOR: u64 = 200;
const REWARD_ROLE: u64 = 9;

fn harness(config: &serde_json::Value) -> Harness {
    let harness = Harness::new(builder().unwrap()).unwrap();
    let guild: GuildId = Id::new(1);
    harness.api().add_channel(Id::new(5), Some(guild));
    harness.api().add_channel(Id::new(6), Some(guild));
    harness.permissions(|permissions| {
        permissions
            .role_levels
            .insert(Id::new(MODERATOR), Level::MODERATOR);
    });
    harness.set_config(config.clone());
    harness
}

async fn say(harness: &Harness, user: u64, channel: u64) {
    harness
        .message(Message::new(user, channel).content("hello"))
        .await
        .unwrap();
}

async fn xp(harness: &Harness, user: u64) -> u64 {
    u64::try_from(harness.store().score("xp", user).await.unwrap()).unwrap()
}

fn user(id: u64) -> CommandOptionValue {
    CommandOptionValue::User(Id::new(id))
}

fn announcements(harness: &Harness) -> Vec<String> {
    harness
        .actions()
        .iter()
        .filter_map(|a| match a {
            Action::SendMessage {
                content: Some(text),
                ..
            } => Some(text.clone()),
            _ => None,
        })
        .collect()
}

fn rewarded(harness: &Harness, user: u64) -> bool {
    harness.actions().iter().any(|a| {
        matches!(a, Action::AddRole { user_id, role_id, .. }
            if user_id.get() == user && role_id.get() == REWARD_ROLE)
    })
}

fn voice(user: u64, before: Option<u64>, after: Option<u64>) -> Event {
    Event::VoiceStateChanged {
        guild_id: Id::new(1),
        user: UserInfo {
            id: UserId::new(user),
            name: "member".into(),
            bot: false,
        },
        before: before.map(ChannelId::new),
        after: after.map(ChannelId::new),
    }
}

/// `user` is in `channel` for `minutes`.
async fn sit(harness: &Harness, user: u64, channel: u64, minutes: u64) {
    harness
        .emit(voice(user, None, Some(channel)))
        .await
        .unwrap();
    harness.advance(Duration::from_mins(minutes));
    harness
        .emit(voice(user, Some(channel), None))
        .await
        .unwrap();
}

fn member(user: u64, roles: &[u64]) -> twilight_model::guild::Member {
    serde_json::from_value(json!({
        "user": {"id": user.to_string(), "username": "member", "discriminator": "0",
                 "avatar": null, "bot": false},
        "roles": roles.iter().map(ToString::to_string).collect::<Vec<_>>(),
        "joined_at": "2020-01-01T00:00:00.000000+00:00",
        "deaf": false, "mute": false, "flags": 0,
    }))
    .unwrap()
}

#[tokio::test]
async fn time_in_voice_gives_experience_by_the_minute_when_the_member_leaves() {
    let harness = harness(&json!({"voice_xp_per_minute": 4}));
    harness.emit(voice(50, None, Some(5))).await.unwrap();
    harness.advance(Duration::from_secs(620));
    assert_eq!(xp(&harness, 50).await, 0, "nothing until they leave");
    harness.emit(voice(50, Some(5), None)).await.unwrap();
    assert_eq!(xp(&harness, 50).await, 40, "ten whole minutes");
    // A member who is left there for days is rewarded for half a day.
    sit(&harness, 51, 5, 3 * 24 * 60).await;
    assert_eq!(xp(&harness, 51).await, 12 * 60 * 4);
}

#[tokio::test]
async fn voice_can_be_off_and_its_ignored_channels_and_bots_give_nothing() {
    let off = harness(&json!({"voice_xp_per_minute": 0}));
    sit(&off, 50, 5, 30).await;
    assert_eq!(xp(&off, 50).await, 0);
    let ignored = harness(&json!({"ignored_channels": ["6"]}));
    sit(&ignored, 50, 6, 30).await;
    assert_eq!(xp(&ignored, 50).await, 0);
    let bots = harness(&json!({}));
    bots.emit(Event::VoiceStateChanged {
        guild_id: Id::new(1),
        user: UserInfo {
            id: UserId::new(52),
            name: "beep".into(),
            bot: true,
        },
        before: None,
        after: Some(ChannelId::new(5)),
    })
    .await
    .unwrap();
    bots.advance(Duration::from_mins(10));
    bots.emit(voice(52, Some(5), None)).await.unwrap();
    assert_eq!(xp(&bots, 52).await, 0);
}

#[tokio::test]
async fn moving_between_channels_counts_each_stay_in_its_own_channel() {
    let harness = harness(&json!({
        "voice_xp_per_minute": 2, "ignored_channels": ["6"],
    }));
    harness.emit(voice(50, None, Some(5))).await.unwrap();
    harness.advance(Duration::from_mins(10));
    harness.emit(voice(50, Some(5), Some(6))).await.unwrap();
    harness.advance(Duration::from_mins(10));
    harness.emit(voice(50, Some(6), None)).await.unwrap();
    assert_eq!(xp(&harness, 50).await, 20, "only the stay in channel 5");
}

#[tokio::test]
async fn roles_and_channels_make_experience_worth_more_or_less() {
    let harness = harness(&json!({
        "xp_per_message": 20, "cooldown_seconds": 0, "voice_xp_per_minute": 10,
        "role_multipliers": [
            {"role": "70", "percent": 200},
            {"role": "71", "percent": 150},
        ],
        "channel_multipliers": [{"channel": "6", "percent": 50}],
    }));
    let say_as = |user: u64, channel: u64, roles: Vec<u64>| {
        harness.message(Message::new(user, channel).content("hi").roles(roles))
    };
    say_as(50, 5, vec![]).await.unwrap();
    say_as(51, 5, vec![70, 71]).await.unwrap();
    say_as(52, 6, vec![70]).await.unwrap();
    assert_eq!(xp(&harness, 50).await, 20);
    assert_eq!(
        xp(&harness, 51).await,
        40,
        "the best role counts, they do not add up"
    );
    assert_eq!(xp(&harness, 52).await, 20, "twice, then half");
    // In voice the roles are asked for.
    harness.api().set_member(Id::new(1), member(53, &[70]));
    sit(&harness, 53, 5, 3).await;
    assert_eq!(xp(&harness, 53).await, 60);
}

#[tokio::test]
async fn a_reward_can_take_the_rewards_before_it_away() {
    let rewards = json!([
        {"level": 1, "role": "9"}, {"level": 2, "role": "10"},
    ]);
    let harness = harness(&json!({
        "xp_per_message": 1000, "cooldown_seconds": 0, "announce": false,
        "rewards": rewards, "replace_rewards": true,
    }));
    say(&harness, 50, 5).await;
    let gone: Vec<u64> = harness
        .actions()
        .iter()
        .filter_map(|a| match a {
            Action::RemoveRole { role_id, .. } => Some(role_id.get()),
            _ => None,
        })
        .collect();
    assert_eq!(gone, [9], "level 2 and 1 were reached at once: 1 goes");
    let keeps = self::harness(&json!({
        "xp_per_message": 1000, "cooldown_seconds": 0, "announce": false, "rewards": rewards,
    }));
    say(&keeps, 50, 5).await;
    assert!(
        !keeps
            .actions()
            .iter()
            .any(|a| matches!(a, Action::RemoveRole { .. }))
    );
}

#[tokio::test]
async fn the_place_and_the_board_are_right_beyond_a_thousand_members() {
    let harness = harness(&json!({}));
    for member in 1..=1500_u64 {
        harness
            .store()
            .score_add("xp", member, i64::try_from(member).unwrap())
            .await
            .unwrap();
    }
    let reply = harness
        .invoke(Invocation::new("rank").option("user", user(1)).user(70))
        .await
        .unwrap();
    let fields: Vec<String> = reply[0].embeds[0]
        .fields
        .iter()
        .map(|f| f.value.clone())
        .collect();
    assert_eq!(fields[2], "#1500 / 1500");
    let board = harness
        .invoke(Invocation::new("leaderboard").user(70))
        .await
        .unwrap();
    let text = board[0].embeds[0].description.clone().unwrap();
    assert!(text.starts_with("1. <@1500>"), "{text}");
}

#[tokio::test]
async fn a_message_gives_experience_once_in_a_cooldown() {
    let harness = harness(&json!({"xp_per_message": 20, "cooldown_seconds": 60}));
    say(&harness, 50, 5).await;
    say(&harness, 50, 5).await;
    assert_eq!(xp(&harness, 50).await, 20, "the second came too soon");
    harness.advance(Duration::from_secs(61));
    say(&harness, 50, 5).await;
    assert_eq!(xp(&harness, 50).await, 40);
    say(&harness, 51, 5).await;
    assert_eq!(xp(&harness, 51).await, 20, "members are counted apart");
}

#[tokio::test]
async fn bots_and_ignored_channels_give_nothing() {
    let harness = harness(&json!({"ignored_channels": ["6"]}));
    harness
        .message(Message::new(52, 5).content("beep").bot())
        .await
        .unwrap();
    say(&harness, 50, 6).await;
    assert_eq!(xp(&harness, 52).await + xp(&harness, 50).await, 0);
}

#[tokio::test]
async fn a_new_level_is_announced_and_its_reward_is_given_once() {
    let harness = harness(&json!({
        "xp_per_message": 100, "cooldown_seconds": 0,
        "rewards": [{"level": 1, "role": REWARD_ROLE.to_string()}],
    }));
    say(&harness, 50, 5).await;
    let said = announcements(&harness);
    assert_eq!(said.len(), 1, "{said:?}");
    assert!(
        said[0].contains("<@50>") && said[0].contains('1'),
        "{said:?}"
    );
    assert!(rewarded(&harness, 50));
    // Level 2 needs more: the next message is no new level and no new reward.
    say(&harness, 50, 5).await;
    assert_eq!(announcements(&harness).len(), 1);
}

#[tokio::test]
async fn the_announcement_can_go_to_a_channel_of_its_own_or_nowhere() {
    let harness = harness(&json!({
        "xp_per_message": 100, "cooldown_seconds": 0, "announce_channel": "6",
    }));
    say(&harness, 50, 5).await;
    let to_six = harness
        .actions()
        .iter()
        .any(|a| matches!(a, Action::SendMessage { channel_id, .. } if channel_id.get() == 6));
    assert!(to_six);
    let quiet =
        self::harness(&json!({"xp_per_message": 100, "cooldown_seconds": 0, "announce": false}));
    say(&quiet, 50, 5).await;
    assert!(announcements(&quiet).is_empty());
}

#[tokio::test]
async fn rank_shows_the_level_and_the_place_and_the_leaderboard_the_order() {
    let harness = harness(&json!({"xp_per_message": 50, "cooldown_seconds": 0}));
    say(&harness, 50, 5).await;
    say(&harness, 51, 5).await;
    say(&harness, 51, 5).await;
    let reply = harness
        .invoke(Invocation::new("rank").option("user", user(50)).user(70))
        .await
        .unwrap();
    let fields: Vec<String> = reply[0].embeds[0]
        .fields
        .iter()
        .map(|f| f.value.clone())
        .collect();
    assert_eq!(fields, ["0", "50", "#2 / 2"]);
    let board = harness
        .invoke(Invocation::new("leaderboard").user(70))
        .await
        .unwrap();
    let text = board[0].embeds[0].description.clone().unwrap();
    let first = text.find("<@51>").unwrap();
    let second = text.find("<@50>").unwrap();
    assert!(first < second, "{text}");
}

#[tokio::test]
async fn a_moderator_changes_experience_and_a_member_cannot() {
    let harness = harness(&json!({
        "rewards": [{"level": 1, "role": REWARD_ROLE.to_string()}],
    }));
    let add = |user_id: u64, amount: i64, roles: &[u64]| {
        Invocation::new("xp")
            .subcommand("add")
            .option("user", user(user_id))
            .option("amount", CommandOptionValue::Integer(amount))
            .roles(roles.iter().copied())
            .user(70)
    };
    let _ = harness.invoke(add(50, 500, &[])).await;
    assert_eq!(xp(&harness, 50).await, 0, "a member is refused");
    harness.invoke(add(50, 500, &[MODERATOR])).await.unwrap();
    assert_eq!(xp(&harness, 50).await, 500);
    assert!(rewarded(&harness, 50), "level 1 was passed");
    harness
        .invoke(add(50, -10_000, &[MODERATOR]))
        .await
        .unwrap();
    assert_eq!(xp(&harness, 50).await, 0, "it does not go below nothing");
    let reset = Invocation::new("xp")
        .subcommand("reset")
        .option("user", user(50))
        .roles([MODERATOR])
        .user(70);
    harness.invoke(reset).await.unwrap();
    assert_eq!(xp(&harness, 50).await, 0);
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
