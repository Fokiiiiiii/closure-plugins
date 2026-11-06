use std::time::Duration;

use closure_core::action::Action;
use closure_core::event::GuildId;
use closure_sdk::testing::{Harness, Invocation, Message};
use closure_sdk::{CommandOptionValue, Level};
use serde_json::json;
use twilight_model::id::Id;

use crate::{Profile, builder};

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
    harness
        .store()
        .get::<Profile>(&format!("xp:{user}"))
        .await
        .unwrap()
        .map_or(0, |p| p.xp)
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
