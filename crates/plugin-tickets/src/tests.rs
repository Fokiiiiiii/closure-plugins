use std::time::Duration;

use closure_core::action::{Action, OverwriteTarget};
use closure_core::event::GuildId;
use closure_sdk::testing::{Click, Harness, Invocation};
use closure_sdk::{CommandOptionValue, Level};
use serde_json::json;
use twilight_model::guild::Permissions;
use twilight_model::id::Id;
use twilight_model::id::marker::ChannelMarker;

use crate::{Ticket, builder};

const STAFF_ROLE: u64 = 30;
const MODERATOR: u64 = 200;
const LOG: u64 = 6;
const MEMBER: u64 = 7;
const OTHER: u64 = 8;
const HERE: u64 = 5;

fn harness() -> Harness {
    let harness = Harness::new(builder().unwrap()).unwrap();
    let guild: GuildId = Id::new(1);
    for channel in [HERE, LOG] {
        harness.api().add_channel(Id::new(channel), Some(guild));
    }
    harness.set_config(json!({
        "staff_roles": [STAFF_ROLE.to_string()],
        "log_channel": LOG.to_string(),
        "max_open": 1
    }));
    harness.permissions(|permissions| {
        permissions
            .role_levels
            .insert(Id::new(MODERATOR), Level::MODERATOR);
    });
    harness
}

async fn seed_ticket(harness: &Harness, channel: u64, opener: u64) {
    let ticket = Ticket {
        number: 3,
        opener,
        claimed_by: None,
        created: 0,
        closing: None,
    };
    harness
        .store()
        .set(&format!("ticket:{channel}"), &ticket)
        .await
        .unwrap();
}

type Rights = (OverwriteTarget, Permissions, Permissions);

fn created(harness: &Harness) -> Vec<(String, Vec<Rights>)> {
    harness
        .actions()
        .into_iter()
        .filter_map(|action| match action {
            Action::CreateChannel { spec, .. } => Some((
                spec.name,
                spec.overwrites
                    .into_iter()
                    .map(|o| (o.target, o.allow, o.deny))
                    .collect(),
            )),
            _ => None,
        })
        .collect()
}

fn deleted(harness: &Harness) -> Vec<u64> {
    harness
        .actions()
        .into_iter()
        .filter_map(|action| match action {
            Action::DeleteChannel { channel_id, .. } => Some(channel_id.get()),
            _ => None,
        })
        .collect()
}

fn transcripts(harness: &Harness) -> Vec<(String, String)> {
    harness
        .actions()
        .into_iter()
        .filter_map(|action| match action {
            Action::PostMessage {
                channel_id, files, ..
            } if channel_id.get() == LOG && !files.is_empty() => Some((
                files[0].filename.clone(),
                String::from_utf8_lossy(&files[0].data).into_owned(),
            )),
            _ => None,
        })
        .collect()
}

fn press(route: &str, user: u64) -> Click {
    Click::new(format!("tickets:{route}:*")).user(user)
}

#[tokio::test]
async fn a_press_opens_a_private_channel_for_the_member_and_the_staff_and_only_one() {
    let harness = harness();
    let replies = harness.click(press("open", MEMBER)).await.unwrap();
    assert!(
        replies.last().unwrap().content.contains("<#"),
        "{replies:?}"
    );
    let made = created(&harness);
    assert_eq!(made.len(), 1);
    assert_eq!(made[0].0, "ticket-0001");
    let overwrites = &made[0].1;
    let everyone = overwrites
        .iter()
        .find(|(target, _, _)| *target == OverwriteTarget::Role(Id::new(1)))
        .unwrap();
    assert!(everyone.2.contains(Permissions::VIEW_CHANNEL));
    let sees = |target: OverwriteTarget| {
        overwrites
            .iter()
            .any(|(t, allow, _)| *t == target && allow.contains(Permissions::VIEW_CHANNEL))
    };
    assert!(sees(OverwriteTarget::Member(Id::new(MEMBER))));
    assert!(sees(OverwriteTarget::Role(Id::new(STAFF_ROLE))));
    let again = harness.click(press("open", MEMBER)).await.unwrap();
    assert!(
        again.last().unwrap().content.contains("open already"),
        "{again:?}"
    );
    assert_eq!(created(&harness).len(), 1);
    harness.click(press("open", OTHER)).await.unwrap();
    assert_eq!(created(&harness)[1].0, "ticket-0002");
}

#[tokio::test]
async fn tickets_are_off_until_a_staff_role_is_chosen() {
    let harness = harness();
    harness.set_config(json!({ "staff_roles": [] }));
    let replies = harness.click(press("open", MEMBER)).await.unwrap();
    assert!(
        replies.last().unwrap().content.contains("not set up"),
        "{replies:?}"
    );
    assert!(created(&harness).is_empty());
}

/// A guild whose channel 9 is the ticket category, or some other kind of channel when `kind` says so.
fn with_category(harness: &Harness, kind: u8) {
    let parent = serde_json::from_value(
        json!({ "id": "9", "type": kind, "name": "parent", "parent_id": null }),
    )
    .unwrap();
    harness.api().set_channels(Id::new(1), vec![parent]);
    harness.set_config(json!({
        "staff_roles": [STAFF_ROLE.to_string()],
        "log_channel": LOG.to_string(),
        "max_open": 1,
        "category": "9"
    }));
}

fn parents(harness: &Harness) -> Vec<Option<Id<ChannelMarker>>> {
    harness
        .actions()
        .into_iter()
        .filter_map(|action| match action {
            Action::CreateChannel { spec, .. } => Some(spec.parent_id),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn a_ticket_is_made_under_a_category() {
    let harness = harness();
    with_category(&harness, 4);
    harness.click(press("open", MEMBER)).await.unwrap();
    assert_eq!(parents(&harness), [Some(Id::new(9))]);
}

#[tokio::test]
async fn a_category_setting_that_is_not_a_category_is_refused_before_any_place_is_taken() {
    let harness = harness();
    with_category(&harness, 0);
    let _ = harness.click(press("open", MEMBER)).await;
    assert!(created(&harness).is_empty());
    assert_eq!(harness.store().score("open", MEMBER).await.unwrap(), 0);
}

#[tokio::test]
async fn closing_saves_the_transcript_then_deletes_the_channel_and_gives_the_place_back() {
    let harness = harness();
    harness.click(press("open", MEMBER)).await.unwrap();
    seed_ticket(&harness, HERE, MEMBER).await;
    let refused = harness.click(press("close", OTHER)).await.unwrap();
    assert!(
        refused.last().unwrap().content.contains("Only the member"),
        "{refused:?}"
    );
    assert!(deleted(&harness).is_empty());
    harness.click(press("close", MEMBER)).await.unwrap();
    assert_eq!(deleted(&harness), [HERE]);
    let saved = transcripts(&harness);
    assert_eq!(saved.len(), 1);
    assert_eq!(saved[0].0, "ticket-0003.txt");
    assert!(saved[0].1.contains("Ticket #3"), "{}", saved[0].1);
    let late = harness.click(press("close", MEMBER)).await.unwrap();
    assert!(
        late.last().unwrap().content.contains("not an open ticket"),
        "{late:?}"
    );
    assert_eq!(deleted(&harness), [HERE]);
    harness.click(press("open", MEMBER)).await.unwrap();
    assert_eq!(created(&harness).len(), 2);
}

#[tokio::test]
async fn the_staff_close_and_claim_and_a_claim_is_taken_once() {
    let harness = harness();
    seed_ticket(&harness, HERE, MEMBER).await;
    let plain = harness.click(press("claim", OTHER)).await.unwrap();
    assert!(
        plain.last().unwrap().content.contains("Only the staff"),
        "{plain:?}"
    );
    let staff = |user| press("claim", user).roles([STAFF_ROLE]);
    harness.click(staff(OTHER)).await.unwrap();
    let again = harness.click(staff(MODERATOR)).await.unwrap();
    assert!(
        again
            .last()
            .unwrap()
            .content
            .contains("took this ticket already"),
        "{again:?}"
    );
    let command = Invocation::new("ticket")
        .subcommand("close")
        .roles([STAFF_ROLE])
        .user(OTHER);
    harness.invoke(command).await.unwrap();
    assert_eq!(deleted(&harness), [HERE]);
}

#[tokio::test]
async fn a_channel_that_could_not_be_deleted_is_still_a_ticket_and_closes_when_tried_again() {
    let harness = harness();
    harness.click(press("open", MEMBER)).await.unwrap();
    seed_ticket(&harness, HERE, MEMBER).await;
    harness
        .api()
        .fail_nth(2, closure_core::error::DiscordError::Fatal("down".into()));
    let first = harness.click(press("close", MEMBER)).await;
    let key = format!("ticket:{HERE}");
    let kept: Option<Ticket> = harness.store().get(&key).await.unwrap();
    assert!(
        kept.is_some(),
        "the ticket is not forgotten before its channel is gone: {first:?}"
    );
    harness.advance(Duration::from_secs(400));
    harness.click(press("close", MEMBER)).await.unwrap();
    assert!(deleted(&harness).contains(&HERE));
    let gone: Option<Ticket> = harness.store().get(&key).await.unwrap();
    assert!(gone.is_none());
}

#[tokio::test]
async fn a_member_who_may_not_use_tickets_cannot_open_one_by_the_button() {
    let harness = harness();
    harness.permissions(|permissions| {
        permissions
            .keys
            .entry(closure_core::permission::PermissionKey::new("tickets.use").unwrap())
            .or_default()
            .deny_users
            .push(Id::new(MEMBER));
    });
    let _ = harness.click(press("open", MEMBER)).await;
    assert!(created(&harness).is_empty());
}

#[tokio::test]
async fn a_claim_and_a_close_at_once_leave_the_close() {
    let harness = harness();
    seed_ticket(&harness, HERE, MEMBER).await;
    let staff = |user| press("claim", user).roles([STAFF_ROLE]);
    harness.click(press("close", MEMBER)).await.unwrap();
    let late = harness.click(staff(OTHER)).await.unwrap();
    assert!(
        late.last().unwrap().content.contains("not an open ticket"),
        "{late:?}"
    );
}

#[tokio::test]
async fn a_ticket_nobody_writes_in_is_closed_after_the_days_of_the_settings() {
    let harness = harness();
    harness.set_config(json!({
        "staff_roles": [STAFF_ROLE.to_string()],
        "log_channel": LOG.to_string(),
        "close_after_days": 2
    }));
    harness.click(press("open", MEMBER)).await.unwrap();
    harness.advance(Duration::from_hours(24));
    harness.run_due_jobs().await;
    assert!(deleted(&harness).is_empty(), "a day is too soon");
    harness.advance(Duration::from_hours(48));
    harness.run_due_jobs().await;
    assert_eq!(deleted(&harness).len(), 1);
    assert_eq!(transcripts(&harness).len(), 1);
}

#[tokio::test]
async fn staff_let_a_member_in_and_take_them_out_and_others_cannot() {
    let harness = harness();
    seed_ticket(&harness, HERE, MEMBER).await;
    let change = |sub: &str, roles: &[u64]| {
        Invocation::new("ticket")
            .subcommand(sub)
            .option("user", CommandOptionValue::User(Id::new(OTHER)))
            .roles(roles.iter().copied())
            .user(MODERATOR)
    };
    let _ = harness.invoke(change("add", &[])).await;
    let touched = |h: &Harness| {
        h.actions()
            .iter()
            .filter(|a| {
                matches!(
                    a,
                    Action::SetPermissionOverwrite { .. }
                        | Action::DeletePermissionOverwrite { .. }
                )
            })
            .count()
    };
    assert_eq!(touched(&harness), 0);
    harness.invoke(change("add", &[MODERATOR])).await.unwrap();
    harness
        .invoke(change("remove", &[MODERATOR]))
        .await
        .unwrap();
    assert_eq!(touched(&harness), 2);
}

#[tokio::test]
async fn an_idle_close_that_meets_a_close_under_way_is_tried_again_after_it() {
    let harness = harness();
    harness.set_config(json!({
        "staff_roles": [STAFF_ROLE.to_string()],
        "close_after_days": 1
    }));
    harness.click(press("open", MEMBER)).await.unwrap();
    let tickets = harness.store().list("ticket:", 10).await.unwrap();
    let channel: u64 = tickets[0].0.trim_start_matches("ticket:").parse().unwrap();
    harness.advance(Duration::from_hours(24));
    harness
        .api()
        .fail_nth(2, closure_core::error::DiscordError::Fatal("down".into()));
    let first = harness.run_due_jobs().await;
    assert!(first.iter().any(Result::is_err), "{first:?}");
    assert!(deleted(&harness).is_empty());

    harness.advance(Duration::from_secs(5));
    harness.run_due_jobs().await;
    assert!(deleted(&harness).is_empty(), "the close still runs");
    assert_eq!(harness.jobs(), 1, "the idle close waits for the other one");

    harness.advance(Duration::from_secs(295));
    harness.run_due_jobs().await;
    assert_eq!(deleted(&harness), [channel]);
}
