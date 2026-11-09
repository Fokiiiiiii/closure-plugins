use std::time::Duration;

use closure_core::action::{Action, MentionPolicy};
use closure_core::event::GuildId;
use closure_sdk::testing::{Click, Harness, Invocation};
use closure_sdk::{CommandOptionValue, Level};
use twilight_model::id::Id;

use crate::{Giveaway, builder, span};

const MODERATOR: u64 = 200;

fn harness() -> Harness {
    let harness = Harness::new(builder().unwrap()).unwrap();
    let guild: GuildId = Id::new(1);
    harness.api().add_channel(Id::new(5), Some(guild));
    harness.permissions(|permissions| {
        permissions
            .role_levels
            .insert(Id::new(MODERATOR), Level::MODERATOR);
    });
    harness
}

fn text(value: &str) -> CommandOptionValue {
    CommandOptionValue::String(value.to_owned())
}

fn start(prize: &str, duration: &str) -> Invocation {
    Invocation::new("giveaway")
        .subcommand("start")
        .option("prize", text(prize))
        .option("duration", text(duration))
        .roles([MODERATOR])
}

fn of_message(sub: &str, message: u64) -> Invocation {
    Invocation::new("giveaway")
        .subcommand(sub)
        .option("message", text(&message.to_string()))
        .roles([MODERATOR])
}

async fn enter(harness: &Harness, message: u64, user: u64) {
    harness
        .store()
        .set_add(&format!("e-{message}"), user)
        .await
        .unwrap();
}

fn posted(harness: &Harness) -> Vec<Action> {
    harness
        .actions()
        .into_iter()
        .filter(|a| matches!(a, Action::PostMessage { .. }))
        .collect()
}

fn announced(harness: &Harness) -> Vec<(String, Vec<u64>)> {
    harness
        .actions()
        .into_iter()
        .filter_map(|a| match a {
            Action::SendMessage {
                content: Some(text),
                mentions: MentionPolicy::Users(users),
                ..
            } => Some((text, users.iter().map(|u| u.get()).collect())),
            _ => None,
        })
        .collect()
}

#[test]
fn a_time_is_minutes_hours_and_days_from_a_minute_up() {
    assert_eq!(span("30m"), Some(1800));
    assert_eq!(span("2h"), Some(7200));
    assert_eq!(span("1d"), Some(86_400));
    assert_eq!(span("1h30m"), Some(5400));
    for bad in ["", "m", "30", "30s", "0m", "ab", "1h x"] {
        assert_eq!(span(bad), None, "{bad}");
    }
}

#[tokio::test]
async fn a_giveaway_is_a_message_with_a_button_and_an_end_that_is_scheduled() {
    let harness = harness();
    let replies = harness.invoke(start("a prize", "30m")).await.unwrap();
    assert_eq!(replies.len(), 1);
    let posts = posted(&harness);
    assert_eq!(posts.len(), 1);
    let Action::PostMessage {
        embeds, components, ..
    } = &posts[0]
    else {
        unreachable!()
    };
    assert_eq!(embeds[0].title.as_deref(), Some("🎉 a prize"));
    assert_eq!(components.len(), 1);
    assert_eq!(harness.jobs(), 1);
    let stored: Option<Giveaway> = harness.store().get("g:1").await.unwrap();
    assert_eq!(stored.unwrap().winners, 1);

    let bad = self::harness();
    bad.invoke(start("x", "tomorrow")).await.unwrap();
    assert!(posted(&bad).is_empty());
    assert_eq!(bad.jobs(), 0);
    let nobody = self::harness();
    let _ = nobody
        .invoke(start("x", "30m").roles(Vec::<u64>::new()))
        .await;
    assert!(posted(&nobody).is_empty(), "a member cannot start one");
}

#[tokio::test]
async fn the_button_enters_and_a_second_press_leaves() {
    let harness = harness();
    let record = Giveaway {
        channel: 5,
        prize: "p".into(),
        winners: 1,
        ends: 9_999_999_999,
        host: 1,
        ended: false,
        announced: false,
        drawn: None,
        counted: false,
    };
    harness.store().set("g:900", &record).await.unwrap();
    let click = |user: u64| Click::new("giveaway:join:*").user(user);
    let first = harness.click(click(70)).await.unwrap();
    assert!(first[0].content.contains('1'), "{first:?}");
    let second = harness.click(click(71)).await.unwrap();
    assert!(second[0].content.contains('2'), "{second:?}");
    harness.click(click(70)).await.unwrap();
    assert!(!harness.store().set_has("e-900", 70_u64).await.unwrap());
    assert!(harness.store().set_has("e-900", 71_u64).await.unwrap());
}

#[tokio::test]
async fn at_the_end_the_winners_are_told_and_the_button_goes_and_it_ends_only_once() {
    let harness = harness();
    harness.invoke(start("a prize", "1m")).await.unwrap();
    for user in [70, 71, 72] {
        enter(&harness, 1, user).await;
    }
    harness.advance(Duration::from_secs(61));
    let ended = harness.run_due_jobs().await;
    assert!(ended.iter().all(Result::is_ok), "{ended:?}");
    let told = announced(&harness);
    assert_eq!(told.len(), 1);
    assert!([70, 71, 72].contains(&told[0].1[0]));
    assert!(told[0].0.contains("a prize"));
    let edits: Vec<Action> = posted(&harness)
        .into_iter()
        .filter(|a| {
            matches!(
                a,
                Action::PostMessage {
                    message_id: Some(_),
                    ..
                }
            )
        })
        .collect();
    assert_eq!(edits.len(), 1);
    let Action::PostMessage { components, .. } = &edits[0] else {
        unreachable!()
    };
    assert!(components.is_empty());
    let stored: Giveaway = harness.store().get("g:1").await.unwrap().unwrap();
    assert!(stored.ended);
    harness.invoke(of_message("end", 1)).await.unwrap();
    assert_eq!(announced(&harness).len(), 1);
}

#[tokio::test]
async fn a_telling_that_failed_is_tried_again_for_the_same_winners() {
    let harness = harness();
    harness.invoke(start("a prize", "1m")).await.unwrap();
    for user in 1..=20_u64 {
        enter(&harness, 1, user).await;
    }
    harness
        .api()
        .fail_nth(2, closure_core::error::DiscordError::Fatal("down".into()));
    harness.advance(Duration::from_secs(61));
    let first = harness.run_due_jobs().await;
    assert!(first.iter().any(Result::is_err), "{first:?}");
    let stored: Giveaway = harness.store().get("g:1").await.unwrap().unwrap();
    assert!(
        !stored.announced,
        "it is not done until the winners were told"
    );
    let drawn = stored.drawn.clone().expect("the draw is kept");
    assert_eq!(drawn.len(), 1);

    for _ in 0..3 {
        harness.advance(Duration::from_secs(600));
        harness.run_due_jobs().await;
    }
    let told = announced(&harness);
    assert_eq!(told.len(), 1, "told once, in the end");
    assert_eq!(
        told[0].1, drawn,
        "the same winner as the draw that was kept"
    );
    let stored: Giveaway = harness.store().get("g:1").await.unwrap().unwrap();
    assert!(stored.announced);
}

#[tokio::test]
async fn a_giveaway_of_many_winners_tells_them_in_as_many_messages_as_they_need() {
    let harness = harness();
    harness
        .invoke(start("a prize", "1m").option("winners", CommandOptionValue::Integer(120)))
        .await
        .unwrap();
    for user in 1..=300_u64 {
        enter(&harness, 1, user).await;
    }
    harness.invoke(of_message("end", 1)).await.unwrap();
    let told = announced(&harness);
    assert_eq!(told.len(), 3, "50, 50 and 20 names");
    let mut all: Vec<u64> = told.iter().flat_map(|(_, users)| users.clone()).collect();
    assert_eq!(all.len(), 120);
    all.sort_unstable();
    all.dedup();
    assert_eq!(all.len(), 120, "each winner once");
}

#[tokio::test]
async fn nobody_entering_is_said_and_a_giveaway_can_be_ended_and_redrawn_by_hand() {
    let harness = harness();
    harness.invoke(start("p", "1d")).await.unwrap();
    harness.invoke(of_message("end", 1)).await.unwrap();
    assert!(announced(&harness).is_empty(), "nobody to tell");

    let second = self::harness();
    second.invoke(start("p", "1d")).await.unwrap();
    let early = second.invoke(of_message("reroll", 1)).await.unwrap();
    assert!(announced(&second).is_empty(), "{early:?}");
    enter(&second, 1, 70).await;
    second.invoke(of_message("end", 1)).await.unwrap();
    assert_eq!(announced(&second).len(), 1);
    second.invoke(of_message("reroll", 1)).await.unwrap();
    let told = announced(&second);
    assert_eq!(told.len(), 2);
    assert_eq!(told[1].1, [70]);
    let unknown = second.invoke(of_message("end", 12345)).await.unwrap();
    assert!(!unknown.is_empty());
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
async fn winners_that_one_stored_value_cannot_hold_are_refused() {
    let harness = harness();
    let replies = harness
        .invoke(start("a prize", "1m").option("winners", CommandOptionValue::Integer(10_000)))
        .await
        .unwrap();
    assert!(
        posted(&harness).is_empty(),
        "a giveaway that cannot be drawn is not posted"
    );
    assert!(
        replies
            .last()
            .unwrap()
            .content
            .contains(&crate::MAX_WINNERS.to_string()),
        "{replies:?}"
    );
}

#[tokio::test]
async fn the_most_winners_drawn_from_19_digit_ids_are_all_kept() {
    let harness = harness();
    let most = crate::MAX_WINNERS;
    let count = usize::try_from(most).unwrap();
    harness
        .invoke(start("a prize", "1m").option("winners", CommandOptionValue::Integer(most)))
        .await
        .unwrap();
    for offset in 0..u64::try_from(most).unwrap() {
        enter(&harness, 1, 1_000_000_000_000_000_000 + offset).await;
    }
    harness.invoke(of_message("end", 1)).await.unwrap();
    let stored: Giveaway = harness.store().get("g:1").await.unwrap().unwrap();
    assert_eq!(stored.drawn.map(|drawn| drawn.len()), Some(count));
    let told: usize = announced(&harness)
        .iter()
        .map(|(_, users)| users.len())
        .sum();
    assert_eq!(told, count);
}

#[tokio::test]
async fn a_count_started_with_the_giveaway_stays_exact_through_entries_and_leaves() {
    let harness = harness();
    let record = Giveaway {
        channel: 5,
        prize: "p".into(),
        winners: 1,
        ends: 9_999_999_999,
        host: 1,
        ended: false,
        announced: false,
        drawn: None,
        counted: true,
    };
    harness.store().set("g:900", &record).await.unwrap();
    let click = |user: u64| Click::new("giveaway:join:*").user(user);
    for user in [70, 71, 72] {
        harness.click(click(user)).await.unwrap();
    }
    harness.click(click(70)).await.unwrap();
    assert_eq!(harness.store().score("n-900", 0_u64).await.unwrap(), 2);
    assert_eq!(harness.store().set_count("e-900").await.unwrap(), 2);
}

#[tokio::test]
async fn a_giveaway_without_a_kept_count_is_counted_from_its_entries_and_nothing_is_written() {
    let harness = harness();
    let record = Giveaway {
        channel: 5,
        prize: "p".into(),
        winners: 1,
        ends: 9_999_999_999,
        host: 1,
        ended: false,
        announced: false,
        drawn: None,
        counted: false,
    };
    harness.store().set("g:900", &record).await.unwrap();
    enter(&harness, 900, 70).await;
    enter(&harness, 900, 71).await;
    let joined = harness
        .click(Click::new("giveaway:join:*").user(72))
        .await
        .unwrap();
    assert!(joined[0].content.contains('3'), "{joined:?}");
    assert_eq!(harness.store().score("n-900", 0_u64).await.unwrap(), 0);
}
