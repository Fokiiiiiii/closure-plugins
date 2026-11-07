use std::time::Duration;

use closure_core::action::{Action, OverwriteTarget};
use closure_core::event::GuildId;
use closure_sdk::testing::{Click, Harness, Invocation};
use closure_sdk::{CommandOptionValue, Level};
use serde_json::json;
use twilight_model::guild::Permissions;
use twilight_model::id::Id;

use crate::{Ticket, builder};

const STAFF_ROLE: u64 = 30;
const MODERATOR: u64 = 200;
const LOG: u64 = 6;
const MEMBER: u64 = 7;
const OTHER: u64 = 8;
/// The channel every click of the test harness comes from.
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
    };
    harness
        .store()
        .set(&format!("ticket:{channel}"), &ticket)
        .await
        .unwrap();
}

/// What a channel was made with: a target, and what it may and may not do.
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
    // A second ticket is turned away.
    let again = harness.click(press("open", MEMBER)).await.unwrap();
    assert!(
        again.last().unwrap().content.contains("open already"),
        "{again:?}"
    );
    assert_eq!(created(&harness).len(), 1);
    // Somebody else has a ticket of their own, with the next number.
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

#[tokio::test]
async fn closing_saves_the_transcript_then_deletes_the_channel_and_gives_the_place_back() {
    let harness = harness();
    harness.click(press("open", MEMBER)).await.unwrap();
    seed_ticket(&harness, HERE, MEMBER).await;
    // Somebody who is neither the opener nor staff cannot close it.
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
    // A second press finds nothing, and the member can open another ticket.
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
