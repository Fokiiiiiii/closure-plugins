use std::time::Duration;

use closure_core::action::{Action, MentionPolicy, OverwriteTarget};
use closure_core::event::{ChannelId, GuildId};
use closure_sdk::testing::{Click, Harness, Invocation};
use closure_sdk::{CommandOptionValue, Level};
use serde_json::json;
use twilight_model::guild::Permissions;
use twilight_model::id::Id;
use twilight_model::id::marker::ChannelMarker;

use crate::{
    NOTE_MAX_CHARS, NOTE_PAGE_LIMIT, NOTE_RETENTION, NOTES_PER_PAGE, NamedPanel, StaffNote,
    StaffNotePage, Ticket, builder, note_page_key, note_pages_prefix,
};

const STAFF_ROLE: u64 = 30;
const MODERATOR: u64 = 200;
const LOG: u64 = 6;
const MEMBER: u64 = 7;
const OTHER: u64 = 8;
const ADMIN_ROLE: u64 = 31;
const ORDINARY_ROLE: u64 = 32;
const GUILD_OWNER: u64 = 999_999_999;
const HERE: u64 = 5;
const PANEL_STAFF_ROLE: u64 = 33;
const OTHER_PANEL_STAFF_ROLE: u64 = 34;
const PANEL_LOG: u64 = 9;

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
        close_request: None,
        panel: None,
    };
    harness
        .store()
        .set(&format!("ticket:{channel}"), &ticket)
        .await
        .unwrap();
}

async fn assign_ticket(harness: &Harness, channel: u64, user: u64) {
    let key = format!("ticket:{channel}");
    let mut ticket: Ticket = harness.store().get(&key).await.unwrap().unwrap();
    ticket.claimed_by = Some(user);
    harness.store().set(&key, &ticket).await.unwrap();
}

fn member(id: u64, roles: &[u64]) -> twilight_model::guild::Member {
    serde_json::from_value(json!({
        "deaf": false,
        "mute": false,
        "flags": 0,
        "roles": roles.iter().map(u64::to_string).collect::<Vec<_>>(),
        "user": {
            "id": id.to_string(),
            "username": "member",
            "discriminator": "0",
            "avatar": null,
            "bot": false
        }
    }))
    .unwrap()
}

fn guild_role(id: u64, permissions: Permissions) -> twilight_model::guild::Role {
    serde_json::from_value(json!({
        "id": id.to_string(),
        "name": format!("role-{id}"),
        "color": 0,
        "hoist": false,
        "managed": false,
        "mentionable": false,
        "permissions": permissions.bits().to_string(),
        "position": 1,
        "flags": 0,
        "icon": null,
        "unicode_emoji": null,
        "tags": null,
        "colors": {
            "primary_color": 0,
            "secondary_color": null,
            "tertiary_color": null
        }
    }))
    .unwrap()
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

fn role_can_see(rights: &[Rights], role: u64) -> bool {
    rights.iter().any(|(target, allow, _)| {
        *target == OverwriteTarget::Role(Id::new(role)) && allow.contains(Permissions::VIEW_CHANNEL)
    })
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
    transcripts_at(harness, LOG)
}

fn transcripts_at(harness: &Harness, log: u64) -> Vec<(String, String)> {
    harness
        .actions()
        .into_iter()
        .filter_map(|action| match action {
            Action::PostMessage {
                channel_id, files, ..
            } if channel_id.get() == log && !files.is_empty() => Some((
                files[0].filename.clone(),
                String::from_utf8_lossy(&files[0].data).into_owned(),
            )),
            _ => None,
        })
        .collect()
}

fn ticket_messages(harness: &Harness) -> Vec<String> {
    harness
        .actions()
        .into_iter()
        .filter_map(|action| match action {
            Action::SendMessage {
                channel_id,
                content: Some(content),
                ..
            } if channel_id.get() == HERE => Some(content),
            _ => None,
        })
        .collect()
}

fn press(route: &str, user: u64) -> Click {
    Click::new(format!("tickets:{route}:*")).user(user)
}

fn panel_press(panel: &str, user: u64) -> Click {
    Click::new(format!("tickets:open:*:{panel}")).user(user)
}

fn close_request_command(user: u64, reason: Option<&str>) -> Invocation {
    let command = Invocation::new("ticket")
        .subcommand("close-request")
        .roles([STAFF_ROLE])
        .user(user);
    match reason {
        Some(reason) => command.option("reason", CommandOptionValue::String(reason.to_owned())),
        None => command,
    }
}

fn request_click(route: &str, request_id: i64, user: u64) -> Click {
    Click::new(format!("tickets:{route}:*:{HERE}:{request_id}")).user(user)
}

fn transfer_invocation(target: u64, actor: u64) -> Invocation {
    Invocation::new("ticket")
        .subcommand("transfer")
        .option("user", CommandOptionValue::User(Id::new(target)))
        .roles([STAFF_ROLE])
        .user(actor)
}

fn rename_invocation(name: &str, staff: bool) -> Invocation {
    let command = Invocation::new("ticket")
        .subcommand("rename")
        .option("name", CommandOptionValue::String(name.to_owned()));
    if staff {
        command.roles([STAFF_ROLE]).user(MODERATOR)
    } else {
        command.user(OTHER)
    }
}

fn note_invocation(text: &str, user: u64, roles: &[u64]) -> Invocation {
    Invocation::new("ticket")
        .subcommand("note")
        .option("text", CommandOptionValue::String(text.to_owned()))
        .roles(roles.iter().copied())
        .user(user)
}

fn notes_invocation(ticket: u64, page: Option<i64>, user: u64, roles: &[u64]) -> Invocation {
    let command = Invocation::new("ticket")
        .subcommand("notes")
        .option("ticket", CommandOptionValue::String(ticket.to_string()));
    let command = match page {
        Some(page) => command.option("page", CommandOptionValue::Integer(page)),
        None => command,
    };
    command.roles(roles.iter().copied()).user(user)
}

#[tokio::test]
async fn a_press_opens_a_private_channel_for_the_member_and_the_staff_and_only_one() {
    let harness = harness();
    harness.set_config(json!({
        "staff_roles": [STAFF_ROLE.to_string()],
        "log_channel": LOG.to_string(),
        "max_open": 1,
        "panels": [{
            "id": "billing",
            "staff_roles": [PANEL_STAFF_ROLE.to_string()]
        }]
    }));
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
    assert!(!sees(OverwriteTarget::Role(Id::new(PANEL_STAFF_ROLE))));
    let tickets = harness.store().list("ticket:", 10).await.unwrap();
    let ticket: Ticket = harness.store().get(&tickets[0].0).await.unwrap().unwrap();
    assert!(ticket.panel.is_none());
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
async fn staff_can_transfer_and_unclaim_an_assigned_ticket() {
    let harness = harness();
    seed_ticket(&harness, HERE, MEMBER).await;
    assign_ticket(&harness, HERE, OTHER).await;
    harness
        .api()
        .set_member(Id::new(1), member(MEMBER, &[STAFF_ROLE]));
    let transfer = Invocation::new("ticket")
        .subcommand("transfer")
        .option("user", CommandOptionValue::User(Id::new(MEMBER)))
        .roles([STAFF_ROLE])
        .user(MODERATOR);
    harness.invoke(transfer).await.unwrap();
    let key = format!("ticket:{HERE}");
    let ticket: Ticket = harness.store().get(&key).await.unwrap().unwrap();
    assert_eq!(ticket.claimed_by, Some(MEMBER));
    let unclaim = Invocation::new("ticket")
        .subcommand("unclaim")
        .roles([STAFF_ROLE])
        .user(MODERATOR);
    harness.invoke(unclaim).await.unwrap();
    let ticket: Ticket = harness.store().get(&key).await.unwrap().unwrap();
    assert_eq!(ticket.claimed_by, None);
    let messages = ticket_messages(&harness);
    assert_eq!(messages.len(), 2, "{messages:?}");
    assert!(
        messages[0].contains(&format!("<@{MEMBER}>")),
        "{messages:?}"
    );
    assert!(messages[1].contains("unassigned"), "{messages:?}");
}

#[tokio::test]
async fn unclaim_and_transfer_require_staff_and_an_existing_assignee() {
    let harness = harness();
    seed_ticket(&harness, HERE, MEMBER).await;
    harness
        .api()
        .set_member(Id::new(1), member(MEMBER, &[STAFF_ROLE]));
    let unauthorized = harness
        .invoke(Invocation::new("ticket").subcommand("unclaim").user(OTHER))
        .await
        .unwrap();
    assert!(
        unauthorized
            .last()
            .unwrap()
            .content
            .contains("Only the staff"),
        "{unauthorized:?}"
    );
    let unclaim = Invocation::new("ticket")
        .subcommand("unclaim")
        .roles([STAFF_ROLE])
        .user(MODERATOR);
    let unclaimed = harness.invoke(unclaim).await.unwrap();
    assert!(
        unclaimed.last().unwrap().content.contains("no assignee"),
        "{unclaimed:?}"
    );
    let transfer = Invocation::new("ticket")
        .subcommand("transfer")
        .option("user", CommandOptionValue::User(Id::new(MEMBER)))
        .roles([STAFF_ROLE])
        .user(MODERATOR);
    let transferred = harness.invoke(transfer).await.unwrap();
    assert!(
        transferred.last().unwrap().content.contains("no assignee"),
        "{transferred:?}"
    );
    let ticket: Ticket = harness
        .store()
        .get(&format!("ticket:{HERE}"))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(ticket.claimed_by, None);
    assert!(ticket_messages(&harness).is_empty());
}

#[tokio::test]
async fn a_transfer_is_rejected_when_the_target_is_not_ticket_staff() {
    let harness = harness();
    seed_ticket(&harness, HERE, MEMBER).await;
    assign_ticket(&harness, HERE, OTHER).await;
    harness.api().set_guild_name(Id::new(1), "support");
    harness.api().set_member(Id::new(1), member(MEMBER, &[]));
    let refused = harness
        .invoke(transfer_invocation(MEMBER, MODERATOR))
        .await
        .unwrap();
    assert!(
        refused.last().unwrap().content.contains("not ticket staff"),
        "{refused:?}"
    );
    let ticket: Ticket = harness
        .store()
        .get(&format!("ticket:{HERE}"))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(ticket.claimed_by, Some(OTHER));
    assert!(ticket_messages(&harness).is_empty());
}

#[tokio::test]
async fn a_guild_owner_can_receive_a_ticket_transfer() {
    let harness = harness();
    seed_ticket(&harness, HERE, MEMBER).await;
    assign_ticket(&harness, HERE, OTHER).await;
    let guild = Id::new(1);
    harness.api().set_guild_name(guild, "support");
    harness.api().set_member(guild, member(GUILD_OWNER, &[]));
    let replies = harness
        .invoke(transfer_invocation(GUILD_OWNER, OTHER))
        .await
        .unwrap();
    assert!(
        replies
            .last()
            .unwrap()
            .content
            .contains(&format!("<@{GUILD_OWNER}>")),
        "{replies:?}"
    );
    let ticket: Ticket = harness
        .store()
        .get(&format!("ticket:{HERE}"))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(ticket.claimed_by, Some(GUILD_OWNER));
}

#[tokio::test]
async fn a_discord_administrator_can_receive_a_ticket_transfer() {
    let harness = harness();
    seed_ticket(&harness, HERE, MEMBER).await;
    assign_ticket(&harness, HERE, OTHER).await;
    let guild = Id::new(1);
    harness.api().set_guild_name(guild, "support");
    harness
        .api()
        .set_member(guild, member(MEMBER, &[ADMIN_ROLE]));
    harness.api().set_roles(
        guild,
        vec![guild_role(ADMIN_ROLE, Permissions::ADMINISTRATOR)],
    );
    let replies = harness
        .invoke(transfer_invocation(MEMBER, OTHER))
        .await
        .unwrap();
    assert!(
        replies
            .last()
            .unwrap()
            .content
            .contains(&format!("<@{MEMBER}>")),
        "{replies:?}"
    );
    let ticket: Ticket = harness
        .store()
        .get(&format!("ticket:{HERE}"))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(ticket.claimed_by, Some(MEMBER));
}

#[tokio::test]
async fn administrator_permission_on_everyone_allows_a_ticket_transfer() {
    let harness = harness();
    seed_ticket(&harness, HERE, MEMBER).await;
    assign_ticket(&harness, HERE, OTHER).await;
    let guild = Id::new(1);
    harness.api().set_guild_name(guild, "support");
    harness.api().set_member(guild, member(MEMBER, &[]));
    harness.api().set_roles(
        guild,
        vec![guild_role(guild.get(), Permissions::ADMINISTRATOR)],
    );
    let replies = harness
        .invoke(transfer_invocation(MEMBER, OTHER))
        .await
        .unwrap();
    assert!(
        replies
            .last()
            .unwrap()
            .content
            .contains(&format!("<@{MEMBER}>")),
        "{replies:?}"
    );
    let ticket: Ticket = harness
        .store()
        .get(&format!("ticket:{HERE}"))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(ticket.claimed_by, Some(MEMBER));
}

#[tokio::test]
async fn a_tickets_manage_holder_without_discord_staff_rights_cannot_receive_a_ticket() {
    let harness = harness();
    seed_ticket(&harness, HERE, MEMBER).await;
    assign_ticket(&harness, HERE, OTHER).await;
    let guild = Id::new(1);
    harness.api().set_guild_name(guild, "support");
    harness
        .api()
        .set_member(guild, member(MODERATOR, &[MODERATOR]));
    harness
        .api()
        .set_roles(guild, vec![guild_role(MODERATOR, Permissions::empty())]);
    let replies = harness
        .invoke(transfer_invocation(MODERATOR, OTHER))
        .await
        .unwrap();
    assert!(
        replies.last().unwrap().content.contains("not ticket staff"),
        "{replies:?}"
    );
    let ticket: Ticket = harness
        .store()
        .get(&format!("ticket:{HERE}"))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(ticket.claimed_by, Some(OTHER));
}

#[tokio::test]
async fn a_nonmember_cannot_receive_a_ticket_transfer() {
    let harness = harness();
    seed_ticket(&harness, HERE, MEMBER).await;
    assign_ticket(&harness, HERE, OTHER).await;
    harness.api().set_not_a_member(Id::new(1), Id::new(MEMBER));
    let replies = harness
        .invoke(transfer_invocation(MEMBER, OTHER))
        .await
        .unwrap();
    assert!(
        replies.last().unwrap().content.contains("not ticket staff"),
        "{replies:?}"
    );
    let ticket: Ticket = harness
        .store()
        .get(&format!("ticket:{HERE}"))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(ticket.claimed_by, Some(OTHER));
}

#[tokio::test]
async fn transfer_does_not_change_a_ticket_when_staff_authority_cannot_be_verified() {
    for call in 1..=4 {
        let harness = harness();
        seed_ticket(&harness, HERE, MEMBER).await;
        assign_ticket(&harness, HERE, OTHER).await;
        let guild = Id::new(1);
        harness.api().set_guild_name(guild, "support");
        harness
            .api()
            .set_member(guild, member(MEMBER, &[ORDINARY_ROLE]));
        harness
            .api()
            .set_roles(guild, vec![guild_role(ORDINARY_ROLE, Permissions::empty())]);
        harness.api().fail_nth(
            call,
            closure_core::error::DiscordError::Fatal("down".into()),
        );
        let result = harness.invoke(transfer_invocation(MEMBER, OTHER)).await;
        if call == 1 {
            assert!(result.is_err(), "defer call {call}");
        } else {
            let replies = result.unwrap();
            assert!(
                replies.last().unwrap().ephemeral,
                "call {call}: {replies:?}"
            );
            assert!(
                replies.last().unwrap().content.contains("did not work"),
                "call {call}: {replies:?}"
            );
        }
        let ticket: Ticket = harness
            .store()
            .get(&format!("ticket:{HERE}"))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(ticket.claimed_by, Some(OTHER));
        assert!(ticket_messages(&harness).is_empty());
    }
}

#[tokio::test]
async fn a_transfer_with_invalid_configuration_reports_that_it_is_not_set_up() {
    let harness = harness();
    seed_ticket(&harness, HERE, MEMBER).await;
    assign_ticket(&harness, HERE, OTHER).await;
    harness.set_config(json!({"staff_roles": "not-a-role-list"}));

    let replies = harness
        .invoke(transfer_invocation(MEMBER, OTHER))
        .await
        .unwrap();
    assert!(replies.last().unwrap().ephemeral, "{replies:?}");
    assert!(
        replies
            .last()
            .unwrap()
            .content
            .contains("Check the ticket settings"),
        "{replies:?}"
    );

    let ticket: Ticket = harness
        .store()
        .get(&format!("ticket:{HERE}"))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(ticket.claimed_by, Some(OTHER));
    assert!(ticket_messages(&harness).is_empty());
}

#[tokio::test]
async fn a_staff_role_target_does_not_need_extra_authority_lookups() {
    let harness = harness();
    seed_ticket(&harness, HERE, MEMBER).await;
    assign_ticket(&harness, HERE, OTHER).await;
    harness
        .api()
        .set_member(Id::new(1), member(MEMBER, &[STAFF_ROLE]));
    harness
        .api()
        .fail_nth(3, closure_core::error::DiscordError::Fatal("down".into()));
    let replies = harness
        .invoke(transfer_invocation(MEMBER, OTHER))
        .await
        .unwrap();
    assert!(
        replies
            .last()
            .unwrap()
            .content
            .contains(&format!("<@{MEMBER}>")),
        "{replies:?}"
    );
    let ticket: Ticket = harness
        .store()
        .get(&format!("ticket:{HERE}"))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(ticket.claimed_by, Some(MEMBER));
}

#[tokio::test]
async fn a_ticket_closing_cannot_be_unclaimed_or_transferred() {
    let harness = harness();
    seed_ticket(&harness, HERE, MEMBER).await;
    assign_ticket(&harness, HERE, OTHER).await;
    let key = format!("ticket:{HERE}");
    let mut ticket: Ticket = harness.store().get(&key).await.unwrap().unwrap();
    ticket.closing = Some(1);
    harness.store().set(&key, &ticket).await.unwrap();
    harness
        .api()
        .set_member(Id::new(1), member(MEMBER, &[STAFF_ROLE]));
    for command in [
        Invocation::new("ticket")
            .subcommand("unclaim")
            .roles([STAFF_ROLE])
            .user(MODERATOR),
        Invocation::new("ticket")
            .subcommand("transfer")
            .option("user", CommandOptionValue::User(Id::new(MEMBER)))
            .roles([STAFF_ROLE])
            .user(MODERATOR),
    ] {
        let refused = harness.invoke(command).await.unwrap();
        assert!(
            refused
                .last()
                .unwrap()
                .content
                .contains("not an open ticket"),
            "{refused:?}"
        );
    }
    let ticket: Ticket = harness.store().get(&key).await.unwrap().unwrap();
    assert_eq!(ticket.claimed_by, Some(OTHER));
    assert_eq!(ticket.closing, Some(1));
    assert!(ticket_messages(&harness).is_empty());
}

#[tokio::test]
async fn a_close_and_transfer_at_once_leave_the_close() {
    let harness = harness();
    seed_ticket(&harness, HERE, MEMBER).await;
    assign_ticket(&harness, HERE, OTHER).await;
    harness
        .api()
        .set_member(Id::new(1), member(MEMBER, &[STAFF_ROLE]));
    let close = harness.click(press("close", MEMBER));
    let transfer = harness.invoke(
        Invocation::new("ticket")
            .subcommand("transfer")
            .option("user", CommandOptionValue::User(Id::new(MEMBER)))
            .roles([STAFF_ROLE])
            .user(MODERATOR),
    );
    let (closed, transferred) = tokio::join!(close, transfer);
    assert!(closed.is_ok(), "{closed:?}");
    let _ = transferred;
    assert_eq!(deleted(&harness), [HERE]);
    let ticket: Option<Ticket> = harness
        .store()
        .get(&format!("ticket:{HERE}"))
        .await
        .unwrap();
    assert!(ticket.is_none());
}

#[tokio::test]
async fn a_staff_member_renames_only_the_ticket_channel() {
    let harness = harness();
    seed_ticket(&harness, HERE, MEMBER).await;
    let replies = harness
        .invoke(rename_invocation("billing-help", true))
        .await
        .unwrap();
    assert!(
        replies.last().unwrap().content.contains("billing-help"),
        "{replies:?}"
    );
    let edits: Vec<_> = harness
        .actions()
        .into_iter()
        .filter_map(|action| match action {
            Action::EditChannel {
                channel_id, patch, ..
            } => Some((channel_id.get(), patch)),
            _ => None,
        })
        .collect();
    assert_eq!(edits.len(), 1);
    assert_eq!(edits[0].0, HERE);
    assert_eq!(edits[0].1.name.as_deref(), Some("billing-help"));
    assert_eq!(edits[0].1.topic, None);
    assert_eq!(edits[0].1.nsfw, None);
    assert_eq!(edits[0].1.slowmode_seconds, None);
    let ticket: Ticket = harness
        .store()
        .get(&format!("ticket:{HERE}"))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(ticket.number, 3);
    assert_eq!(ticket.claimed_by, None);
}

#[tokio::test]
async fn a_general_member_cannot_rename_a_ticket() {
    let harness = harness();
    seed_ticket(&harness, HERE, MEMBER).await;
    let replies = harness
        .invoke(rename_invocation("should-not-change", false))
        .await
        .unwrap();
    assert!(
        replies.last().unwrap().content.contains("Only the staff"),
        "{replies:?}"
    );
    assert!(
        harness
            .actions()
            .iter()
            .all(|action| !matches!(action, Action::EditChannel { .. }))
    );
}

#[tokio::test]
async fn renaming_requires_an_open_ticket() {
    let harness = harness();
    let not_a_ticket = harness
        .invoke(rename_invocation("should-not-change", true))
        .await
        .unwrap();
    assert!(
        not_a_ticket
            .last()
            .unwrap()
            .content
            .contains("not an open ticket"),
        "{not_a_ticket:?}"
    );
    seed_ticket(&harness, HERE, MEMBER).await;
    let key = format!("ticket:{HERE}");
    let mut ticket: Ticket = harness.store().get(&key).await.unwrap().unwrap();
    ticket.closing = Some(1);
    harness.store().set(&key, &ticket).await.unwrap();
    let closing = harness
        .invoke(rename_invocation("should-not-change", true))
        .await
        .unwrap();
    assert!(
        closing
            .last()
            .unwrap()
            .content
            .contains("not an open ticket"),
        "{closing:?}"
    );
    let ticket: Ticket = harness.store().get(&key).await.unwrap().unwrap();
    assert_eq!(ticket.closing, Some(1));
    assert!(
        harness
            .actions()
            .iter()
            .all(|action| !matches!(action, Action::EditChannel { .. }))
    );
}

#[tokio::test]
async fn a_name_outside_action_limits_is_rejected_before_the_channel_edit() {
    let harness = harness();
    seed_ticket(&harness, HERE, MEMBER).await;
    let invalid_name = "x".repeat(101);
    let replies = harness
        .invoke(rename_invocation(&invalid_name, true))
        .await
        .unwrap();
    assert!(
        replies
            .last()
            .unwrap()
            .content
            .contains("1 to 100 characters"),
        "{replies:?}"
    );
    assert!(
        harness
            .actions()
            .iter()
            .all(|action| !matches!(action, Action::EditChannel { .. }))
    );
}

#[tokio::test]
async fn a_channel_edit_failure_does_not_return_rename_success() {
    let harness = harness();
    seed_ticket(&harness, HERE, MEMBER).await;
    harness
        .api()
        .fail_nth(1, closure_core::error::DiscordError::Fatal("down".into()));
    let result = harness
        .invoke(rename_invocation("billing-help", true))
        .await;
    assert!(result.is_err());
    assert!(
        harness
            .actions()
            .iter()
            .all(|action| !matches!(action, Action::EditChannel { .. }))
    );
}

#[tokio::test]
async fn staff_close_request_can_only_be_rejected_by_the_opener() {
    let harness = harness();
    seed_ticket(&harness, HERE, MEMBER).await;
    let unauthorized = harness
        .invoke(
            Invocation::new("ticket")
                .subcommand("close-request")
                .user(OTHER),
        )
        .await
        .unwrap();
    assert!(
        unauthorized
            .last()
            .unwrap()
            .content
            .contains("Only the staff"),
        "{unauthorized:?}"
    );
    let reason = Some("The issue is resolved");
    let sent = harness
        .invoke(close_request_command(OTHER, reason))
        .await
        .unwrap();
    assert!(
        sent.last()
            .unwrap()
            .content
            .contains("sent to the ticket opener"),
        "{sent:?}"
    );
    let key = format!("ticket:{HERE}");
    let ticket: Ticket = harness.store().get(&key).await.unwrap().unwrap();
    let request_id = ticket.close_request.as_ref().unwrap().id;
    assert_eq!(ticket.close_request.as_ref().unwrap().requested_by, OTHER);
    assert_eq!(
        ticket.close_request.as_ref().unwrap().reason.as_deref(),
        reason
    );
    let notify = harness
        .actions()
        .into_iter()
        .find_map(|action| match action {
            Action::SendMessage {
                channel_id,
                content: Some(content),
                mentions,
                ..
            } if channel_id.get() == HERE && content == format!("<@{MEMBER}>") => Some(mentions),
            _ => None,
        })
        .unwrap();
    assert_eq!(notify, MentionPolicy::Users(vec![Id::new(MEMBER)]));
    let resent = harness
        .invoke(close_request_command(OTHER, reason))
        .await
        .unwrap();
    assert!(
        resent
            .last()
            .unwrap()
            .content
            .contains("sent to the ticket opener"),
        "{resent:?}"
    );
    let changed = harness
        .invoke(close_request_command(MODERATOR, Some("A different reason")))
        .await
        .unwrap();
    assert!(
        changed.last().unwrap().content.contains("already waiting"),
        "{changed:?}"
    );
    let ticket: Ticket = harness.store().get(&key).await.unwrap().unwrap();
    assert_eq!(ticket.close_request.as_ref().unwrap().id, request_id);
    assert_eq!(
        ticket.close_request.as_ref().unwrap().reason.as_deref(),
        reason
    );
}

#[tokio::test]
async fn old_close_request_buttons_cannot_change_a_new_request() {
    let harness = harness();
    seed_ticket(&harness, HERE, MEMBER).await;
    harness
        .invoke(close_request_command(OTHER, Some("The issue is resolved")))
        .await
        .unwrap();
    let key = format!("ticket:{HERE}");
    let ticket: Ticket = harness.store().get(&key).await.unwrap().unwrap();
    let request_id = ticket.close_request.as_ref().unwrap().id;
    for route in ["close-request-approve", "close-request-reject"] {
        let denied = harness
            .click(request_click(route, request_id, OTHER).roles([STAFF_ROLE]))
            .await
            .unwrap();
        assert!(
            denied
                .last()
                .unwrap()
                .content
                .contains("Only the member who opened"),
            "{denied:?}"
        );
    }
    let rejected = harness
        .click(request_click("close-request-reject", request_id, MEMBER))
        .await
        .unwrap();
    assert!(
        rejected
            .last()
            .unwrap()
            .content
            .contains("kept the ticket open"),
        "{rejected:?}"
    );
    let ticket: Ticket = harness.store().get(&key).await.unwrap().unwrap();
    assert!(ticket.close_request.is_none());
    assert!(deleted(&harness).is_empty());
    let stale = harness
        .click(request_click("close-request-approve", request_id, MEMBER))
        .await
        .unwrap();
    assert!(
        stale.last().unwrap().content.contains("no longer active"),
        "{stale:?}"
    );
    let new_request = harness
        .invoke(close_request_command(MODERATOR, Some("A new reason")))
        .await
        .unwrap();
    assert!(
        new_request
            .last()
            .unwrap()
            .content
            .contains("sent to the ticket opener"),
        "{new_request:?}"
    );
    let ticket: Ticket = harness.store().get(&key).await.unwrap().unwrap();
    let new_id = ticket.close_request.as_ref().unwrap().id;
    assert_ne!(new_id, request_id);
    let old_button = harness
        .click(request_click("close-request-reject", request_id, MEMBER))
        .await
        .unwrap();
    assert!(
        old_button
            .last()
            .unwrap()
            .content
            .contains("no longer active"),
        "{old_button:?}"
    );
    let ticket: Ticket = harness.store().get(&key).await.unwrap().unwrap();
    assert_eq!(ticket.close_request.as_ref().unwrap().id, new_id);
    assert!(deleted(&harness).is_empty());
}

#[tokio::test]
async fn concurrent_close_request_approvals_close_the_ticket_once() {
    let harness = harness();
    seed_ticket(&harness, HERE, MEMBER).await;
    harness
        .invoke(close_request_command(OTHER, Some("The issue is resolved")))
        .await
        .unwrap();
    let ticket: Ticket = harness
        .store()
        .get(&format!("ticket:{HERE}"))
        .await
        .unwrap()
        .unwrap();
    let request_id = ticket.close_request.unwrap().id;
    let (first, second) = tokio::join!(
        harness.click(request_click("close-request-approve", request_id, MEMBER)),
        harness.click(request_click("close-request-approve", request_id, MEMBER)),
    );
    assert!(first.is_ok(), "{first:?}");
    assert!(second.is_ok(), "{second:?}");
    assert_eq!(deleted(&harness), [HERE]);
    assert_eq!(transcripts(&harness).len(), 1);
    let ticket: Option<Ticket> = harness
        .store()
        .get(&format!("ticket:{HERE}"))
        .await
        .unwrap();
    assert!(ticket.is_none());
}

#[tokio::test]
async fn a_failed_close_request_post_can_be_resent_with_the_same_id() {
    let harness = harness();
    seed_ticket(&harness, HERE, MEMBER).await;
    harness
        .api()
        .fail_nth(2, closure_core::error::DiscordError::Fatal("down".into()));
    let first = harness
        .invoke(close_request_command(OTHER, Some("The issue is resolved")))
        .await
        .unwrap();
    assert!(
        first.last().unwrap().content.contains("did not work"),
        "{first:?}"
    );
    let key = format!("ticket:{HERE}");
    let ticket: Ticket = harness.store().get(&key).await.unwrap().unwrap();
    let request_id = ticket.close_request.as_ref().unwrap().id;
    let second = harness
        .invoke(close_request_command(
            MODERATOR,
            Some("The issue is resolved"),
        ))
        .await
        .unwrap();
    assert!(
        second
            .last()
            .unwrap()
            .content
            .contains("sent to the ticket opener"),
        "{second:?}"
    );
    let ticket: Ticket = harness.store().get(&key).await.unwrap().unwrap();
    assert_eq!(ticket.close_request.as_ref().unwrap().id, request_id);
    let posts: Vec<_> = harness
        .actions()
        .iter()
        .filter(|action| {
            matches!(
                action,
                Action::PostMessage { channel_id, .. } if channel_id.get() == HERE
            )
        })
        .map(|action| serde_json::to_string(action).unwrap())
        .collect();
    assert_eq!(posts.len(), 1, "{posts:?}");
    assert!(
        posts[0].contains(&format!(
            "tickets:close-request-approve:*:{HERE}:{request_id}"
        )),
        "{posts:?}"
    );
    assert!(
        posts[0].contains(&format!(
            "tickets:close-request-reject:*:{HERE}:{request_id}"
        )),
        "{posts:?}"
    );
    let approved = harness
        .click(request_click("close-request-approve", request_id, MEMBER))
        .await
        .unwrap();
    assert!(
        approved
            .last()
            .unwrap()
            .content
            .contains("approved the closure request"),
        "{approved:?}"
    );
    assert_eq!(deleted(&harness), [HERE]);
    assert_eq!(transcripts(&harness).len(), 1);
}

#[tokio::test]
async fn a_close_request_can_be_added_to_a_legacy_ticket() {
    let harness = harness();
    harness
        .store()
        .set(
            &format!("ticket:{HERE}"),
            &json!({
                "number": 3,
                "opener": MEMBER,
                "claimed_by": null,
                "created": 0
            }),
        )
        .await
        .unwrap();
    harness
        .invoke(close_request_command(OTHER, None))
        .await
        .unwrap();
    let ticket: Ticket = harness
        .store()
        .get(&format!("ticket:{HERE}"))
        .await
        .unwrap()
        .unwrap();
    assert!(ticket.panel.is_none());
    assert_eq!(ticket.close_request.unwrap().id, 1);
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

#[tokio::test]
async fn a_named_panel_snapshot_keeps_its_staff_access_after_config_removal() {
    let harness = harness();
    let guild: GuildId = Id::new(1);
    harness.api().add_channel(Id::new(PANEL_LOG), Some(guild));
    harness.set_config(json!({
        "staff_roles": [STAFF_ROLE.to_string()],
        "panels": [
            {
                "id": "billing",
                "staff_roles": [PANEL_STAFF_ROLE.to_string()],
                "log_channel": PANEL_LOG.to_string(),
                "close_after_days": 2
            },
            {
                "id": "support",
                "staff_roles": [OTHER_PANEL_STAFF_ROLE.to_string()]
            }
        ]
    }));

    harness.click(panel_press("billing", MEMBER)).await.unwrap();
    let made = created(&harness);
    assert_eq!(made.len(), 1);
    assert!(role_can_see(&made[0].1, PANEL_STAFF_ROLE));
    assert!(!role_can_see(&made[0].1, STAFF_ROLE));

    let tickets = harness.store().list("ticket:", 10).await.unwrap();
    let ticket: Ticket = harness.store().get(&tickets[0].0).await.unwrap().unwrap();
    let snapshot = ticket.panel.as_ref().unwrap();
    assert_eq!(snapshot.id, "billing");
    assert_eq!(snapshot.staff_roles, [PANEL_STAFF_ROLE.to_string()]);
    assert_eq!(
        snapshot.log_channel.as_deref(),
        Some(PANEL_LOG.to_string().as_str())
    );
    assert_eq!(snapshot.close_after_days, 2);

    harness.set_config(json!({
        "staff_roles": [STAFF_ROLE.to_string()],
        "log_channel": LOG.to_string(),
        "panels": []
    }));
    // shortcut: Click targets HERE only, use channel-aware clicks if the harness adds them.
    harness
        .store()
        .set(&format!("ticket:{HERE}"), &ticket)
        .await
        .unwrap();
    let other_panel_staff = harness
        .click(press("claim", OTHER).roles([OTHER_PANEL_STAFF_ROLE]))
        .await
        .unwrap();
    assert!(
        other_panel_staff
            .last()
            .unwrap()
            .content
            .contains("Only the staff"),
        "{other_panel_staff:?}"
    );
    harness
        .click(press("claim", OTHER).roles([PANEL_STAFF_ROLE]))
        .await
        .unwrap();
    let ticket: Ticket = harness
        .store()
        .get(&format!("ticket:{HERE}"))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(ticket.claimed_by, Some(OTHER));
    harness
        .click(press("close", OTHER).roles([PANEL_STAFF_ROLE]))
        .await
        .unwrap();
    assert_eq!(deleted(&harness), [HERE]);
    assert_eq!(transcripts_at(&harness, PANEL_LOG).len(), 1);
    assert!(transcripts(&harness).is_empty());
}

#[tokio::test]
async fn an_open_ticket_limit_is_shared_across_named_panels_per_user() {
    let harness = harness();
    harness.set_config(json!({
        "staff_roles": [STAFF_ROLE.to_string()],
        "max_open": 1,
        "panels": [
            {
                "id": "billing",
                "staff_roles": [PANEL_STAFF_ROLE.to_string()]
            },
            {
                "id": "support",
                "staff_roles": [OTHER_PANEL_STAFF_ROLE.to_string()]
            }
        ]
    }));

    harness.click(panel_press("billing", MEMBER)).await.unwrap();
    assert_eq!(created(&harness).len(), 1);
    let blocked = harness.click(panel_press("support", MEMBER)).await.unwrap();
    assert!(
        blocked.last().unwrap().content.contains("open already"),
        "{blocked:?}"
    );
    assert_eq!(created(&harness).len(), 1);

    harness.click(panel_press("support", OTHER)).await.unwrap();
    assert_eq!(created(&harness).len(), 2);
}

#[tokio::test]
async fn zero_max_open_is_unlimited_but_existing_open_tickets_count_when_limited() {
    let harness = harness();
    let schema = crate::schema();
    assert_eq!(schema["properties"]["max_open"]["minimum"], 0);
    assert_eq!(schema["properties"]["max_open"]["maximum"], u32::MAX);
    harness.set_config(json!({
        "staff_roles": [STAFF_ROLE.to_string()],
        "max_open": 0
    }));

    harness.click(press("open", MEMBER)).await.unwrap();
    harness.click(press("open", MEMBER)).await.unwrap();
    assert_eq!(created(&harness).len(), 2);

    harness.set_config(json!({
        "staff_roles": [STAFF_ROLE.to_string()],
        "max_open": 1
    }));
    let blocked = harness.click(press("open", MEMBER)).await.unwrap();
    assert!(
        blocked.last().unwrap().content.contains("open already"),
        "{blocked:?}"
    );
    assert_eq!(created(&harness).len(), 2);

    harness.click(press("open", OTHER)).await.unwrap();
    assert_eq!(created(&harness).len(), 3);
}

#[tokio::test]
async fn staff_can_open_over_the_limit_but_are_limited_after_losing_staff_status() {
    let harness = harness();
    harness.set_config(json!({
        "staff_roles": [STAFF_ROLE.to_string()],
        "max_open": 1
    }));

    harness
        .click(press("open", MEMBER).roles([STAFF_ROLE]))
        .await
        .unwrap();
    harness
        .click(press("open", MEMBER).roles([STAFF_ROLE]))
        .await
        .unwrap();
    assert_eq!(created(&harness).len(), 2);

    harness
        .click(press("open", MEMBER).roles([MODERATOR]))
        .await
        .unwrap();
    assert_eq!(created(&harness).len(), 3);

    let blocked = harness.click(press("open", MEMBER)).await.unwrap();
    assert!(
        blocked.last().unwrap().content.contains("open already"),
        "{blocked:?}"
    );
    assert_eq!(created(&harness).len(), 3);
}

#[tokio::test]
async fn a_named_panel_ticket_uses_its_staff_roles_for_member_changes() {
    let harness = harness();
    harness.set_config(json!({
        "staff_roles": [STAFF_ROLE.to_string()],
        "panels": [
            {
                "id": "billing",
                "staff_roles": [PANEL_STAFF_ROLE.to_string()]
            },
            {
                "id": "support",
                "staff_roles": [OTHER_PANEL_STAFF_ROLE.to_string()]
            }
        ]
    }));

    harness.click(panel_press("billing", MEMBER)).await.unwrap();
    let tickets = harness.store().list("ticket:", 10).await.unwrap();
    let ticket: Ticket = harness.store().get(&tickets[0].0).await.unwrap().unwrap();
    assert_eq!(
        ticket.panel.as_ref().map(|panel| panel.id.as_str()),
        Some("billing")
    );
    harness
        .store()
        .set(&format!("ticket:{HERE}"), &ticket)
        .await
        .unwrap();

    let change = |sub: &str, role: u64| {
        Invocation::new("ticket")
            .subcommand(sub)
            .option("user", CommandOptionValue::User(Id::new(MODERATOR)))
            .roles([role])
            .user(OTHER)
    };
    let touched = |h: &Harness| {
        h.actions()
            .iter()
            .filter(|action| {
                matches!(
                    action,
                    Action::SetPermissionOverwrite { .. }
                        | Action::DeletePermissionOverwrite { .. }
                )
            })
            .count()
    };

    for sub in ["add", "remove"] {
        let denied = harness
            .invoke(change(sub, OTHER_PANEL_STAFF_ROLE))
            .await
            .unwrap();
        assert!(
            denied.last().unwrap().content.contains("Only the staff"),
            "{denied:?}"
        );
    }
    assert_eq!(touched(&harness), 0);

    for sub in ["add", "remove"] {
        harness.invoke(change(sub, PANEL_STAFF_ROLE)).await.unwrap();
    }
    assert_eq!(touched(&harness), 2);
}

#[tokio::test]
async fn invalid_panel_references_and_everyone_staff_are_rejected_without_creating_channels() {
    let cases = [
        ("missing", json!([]), "This panel was not found."),
        (
            "duplicate",
            json!([
                {"id": "duplicate", "staff_roles": [PANEL_STAFF_ROLE.to_string()]},
                {"id": "duplicate", "staff_roles": [OTHER_PANEL_STAFF_ROLE.to_string()]}
            ]),
            "This panel ID is used more than once.",
        ),
        (
            "everyone",
            json!([{"id": "everyone", "staff_roles": ["1"]}]),
            "The @everyone role cannot be used as a staff role.",
        ),
    ];
    for (panel_id, panels, expected) in cases {
        let harness = harness();
        harness.set_config(json!({
            "staff_roles": [STAFF_ROLE.to_string()],
            "panels": panels
        }));
        let replies = harness.click(panel_press(panel_id, MEMBER)).await.unwrap();
        assert!(
            replies.last().unwrap().content.contains(expected),
            "{replies:?}"
        );
        assert!(created(&harness).is_empty(), "{panel_id}");
    }
}

#[tokio::test]
async fn panel_command_button_contains_the_selected_panel_id() {
    let harness = harness();
    harness.set_config(json!({
        "staff_roles": [STAFF_ROLE.to_string()],
        "panels": [{
            "id": "billing",
            "staff_roles": [PANEL_STAFF_ROLE.to_string()]
        }]
    }));
    harness
        .invoke(
            Invocation::new("ticket")
                .subcommand("panel")
                .option("panel", CommandOptionValue::String("billing".to_owned()))
                .roles([MODERATOR])
                .user(MODERATOR),
        )
        .await
        .unwrap();
    let post = harness
        .actions()
        .into_iter()
        .find_map(|action| match action {
            Action::PostMessage { components, .. } => {
                Some(serde_json::to_string(&components).unwrap())
            }
            _ => None,
        })
        .unwrap();
    assert!(post.contains("tickets:open:*:billing"), "{post}");
}

#[tokio::test]
async fn idle_close_uses_panel_settings_after_config_removal() {
    let harness = harness();
    let guild: GuildId = Id::new(1);
    harness.api().add_channel(Id::new(PANEL_LOG), Some(guild));
    harness.set_config(json!({
        "staff_roles": [STAFF_ROLE.to_string()],
        "log_channel": LOG.to_string(),
        "close_after_days": 0,
        "panels": [{
            "id": "billing",
            "staff_roles": [PANEL_STAFF_ROLE.to_string()],
            "log_channel": PANEL_LOG.to_string(),
            "close_after_days": 2
        }]
    }));
    harness.click(panel_press("billing", MEMBER)).await.unwrap();
    let tickets = harness.store().list("ticket:", 10).await.unwrap();
    let channel: u64 = tickets[0].0.trim_start_matches("ticket:").parse().unwrap();
    let ticket: Ticket = harness.store().get(&tickets[0].0).await.unwrap().unwrap();
    assert_eq!(ticket.panel.as_ref().unwrap().close_after_days, 2);
    harness.set_config(json!({
        "staff_roles": [STAFF_ROLE.to_string()],
        "log_channel": LOG.to_string(),
        "close_after_days": 0,
        "panels": []
    }));

    harness.advance(Duration::from_hours(24));
    harness.run_due_jobs().await;
    assert!(deleted(&harness).is_empty());
    harness.advance(Duration::from_hours(48));
    harness.run_due_jobs().await;
    assert_eq!(deleted(&harness), [channel]);
    assert_eq!(transcripts_at(&harness, PANEL_LOG).len(), 1);
    assert!(transcripts(&harness).is_empty());
}

#[tokio::test]
async fn ticket_notes_are_private_and_staff_only_after_close() {
    let harness = harness();
    seed_ticket(&harness, HERE, MEMBER).await;

    let denied = harness
        .invoke(note_invocation("private note", OTHER, &[]))
        .await
        .unwrap();
    assert!(denied.last().unwrap().ephemeral, "{denied:?}");
    assert!(!denied.last().unwrap().content.contains("private note"));
    assert!(
        harness
            .store()
            .list(&note_pages_prefix(ChannelId::new(HERE)), NOTE_PAGE_LIMIT)
            .await
            .unwrap()
            .is_empty()
    );

    let saved = harness
        .invoke(note_invocation("private note", MODERATOR, &[STAFF_ROLE]))
        .await
        .unwrap();
    assert!(saved.last().unwrap().ephemeral, "{saved:?}");
    assert!(saved.last().unwrap().content.contains("Staff note saved"));
    assert!(ticket_messages(&harness).is_empty());

    harness
        .store()
        .delete(&format!("ticket:{HERE}"))
        .await
        .unwrap();
    let shown = harness
        .invoke(notes_invocation(HERE, Some(1), MODERATOR, &[STAFF_ROLE]))
        .await
        .unwrap();
    assert!(shown.last().unwrap().ephemeral, "{shown:?}");
    assert!(shown.last().unwrap().content.contains("private note"));

    let denied = harness
        .invoke(notes_invocation(HERE, Some(1), OTHER, &[]))
        .await
        .unwrap();
    assert!(denied.last().unwrap().ephemeral, "{denied:?}");
    assert!(denied.last().unwrap().content.contains("Only the staff"));
    assert!(!denied.last().unwrap().content.contains("private note"));
}

#[tokio::test]
async fn staff_note_pages_fit_storage_and_embed_limits_at_the_utf8_boundary() {
    let harness = harness();
    seed_ticket(&harness, HERE, MEMBER).await;
    let text = "🧭".repeat(NOTE_MAX_CHARS);

    for _ in 0..NOTES_PER_PAGE {
        harness
            .invoke(note_invocation(&text, MODERATOR, &[STAFF_ROLE]))
            .await
            .unwrap();
    }

    let key = note_page_key(ChannelId::new(HERE), 1);
    let page: StaffNotePage = harness.store().get(&key).await.unwrap().unwrap();
    assert_eq!(page.notes.len(), NOTES_PER_PAGE);
    let encoded = serde_json::to_vec(&page).unwrap();
    assert!(encoded.len() <= closure_core::storage::MAX_VALUE_BYTES);

    let listed = harness
        .invoke(notes_invocation(HERE, Some(1), MODERATOR, &[STAFF_ROLE]))
        .await
        .unwrap();
    let content = &listed.last().unwrap().content;
    assert!(listed.last().unwrap().ephemeral, "{listed:?}");
    assert!(content.chars().count() <= 4096);
    assert!(content.contains(&text));

    let too_long = "x".repeat(NOTE_MAX_CHARS + 1);
    harness
        .invoke(note_invocation(&too_long, MODERATOR, &[STAFF_ROLE]))
        .await
        .unwrap();
    harness
        .invoke(note_invocation("bad\0note", MODERATOR, &[STAFF_ROLE]))
        .await
        .unwrap();
    let unchanged: StaffNotePage = harness.store().get(&key).await.unwrap().unwrap();
    assert_eq!(unchanged.notes.len(), NOTES_PER_PAGE);
}

#[tokio::test]
async fn a_stale_staff_note_page_write_cannot_replace_a_newer_revision() {
    let harness = harness();
    seed_ticket(&harness, HERE, MEMBER).await;
    harness
        .invoke(note_invocation("first", MODERATOR, &[STAFF_ROLE]))
        .await
        .unwrap();

    let key = note_page_key(ChannelId::new(HERE), 1);
    let entry = harness.store().entry(&key).await.unwrap().unwrap();
    let original: StaffNotePage = serde_json::from_value(entry.value).unwrap();
    let mut newer = original.clone();
    newer.notes.push(StaffNote {
        author: OTHER,
        created: 1,
        text: "newer".to_owned(),
    });
    harness
        .store()
        .replace(
            &key,
            &newer,
            closure_core::storage::Expect::Revision(entry.revision),
            Some(NOTE_RETENTION),
        )
        .await
        .unwrap();

    let mut stale = original;
    stale.notes.push(StaffNote {
        author: MEMBER,
        created: 2,
        text: "stale".to_owned(),
    });
    let conflict = harness
        .store()
        .replace(
            &key,
            &stale,
            closure_core::storage::Expect::Revision(entry.revision),
            Some(NOTE_RETENTION),
        )
        .await;
    assert!(matches!(
        conflict,
        Err(closure_core::storage::StorageError::Conflict)
    ));

    let stored: StaffNotePage = harness.store().get(&key).await.unwrap().unwrap();
    assert!(stored.notes.iter().any(|note| note.text == "newer"));
    assert!(!stored.notes.iter().any(|note| note.text == "stale"));
}

#[tokio::test]
async fn concurrent_staff_notes_survive_a_page_revision_conflict() {
    let harness = harness();
    seed_ticket(&harness, HERE, MEMBER).await;

    let (first, second) = tokio::join!(
        harness.invoke(note_invocation("first", MODERATOR, &[STAFF_ROLE])),
        harness.invoke(note_invocation("second", MODERATOR, &[STAFF_ROLE]))
    );
    assert!(first.unwrap().last().unwrap().ephemeral);
    assert!(second.unwrap().last().unwrap().ephemeral);

    let page: StaffNotePage = harness
        .store()
        .get(&note_page_key(ChannelId::new(HERE), 1))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(page.notes.len(), 2);
    assert!(page.notes.iter().any(|note| note.text == "first"));
    assert!(page.notes.iter().any(|note| note.text == "second"));
}

#[tokio::test]
async fn note_history_has_no_small_per_ticket_count_limit() {
    let harness = harness();
    seed_ticket(&harness, HERE, MEMBER).await;

    for page_number in 1..=13 {
        let note_count = if page_number == 13 { 4 } else { NOTES_PER_PAGE };
        let page = StaffNotePage {
            ticket_number: 3,
            panel: None,
            notes: (0..note_count)
                .map(|index| StaffNote {
                    author: MODERATOR,
                    created: i64::try_from(index).unwrap(),
                    text: format!("old note {page_number}-{index}"),
                })
                .collect(),
        };
        harness
            .store()
            .set_for(
                &note_page_key(ChannelId::new(HERE), page_number),
                &page,
                NOTE_RETENTION,
            )
            .await
            .unwrap();
    }

    harness
        .invoke(note_invocation("note 101", MODERATOR, &[STAFF_ROLE]))
        .await
        .unwrap();
    let last: StaffNotePage = harness
        .store()
        .get(&note_page_key(ChannelId::new(HERE), 13))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(last.notes.len(), 5);
    assert_eq!(last.notes.last().unwrap().text, "note 101");

    let listed = harness
        .invoke(notes_invocation(HERE, Some(13), MODERATOR, &[STAFF_ROLE]))
        .await
        .unwrap();
    assert!(listed.last().unwrap().content.contains("note 101"));
    assert!(listed.last().unwrap().content.contains("13 of 13"));
}

#[tokio::test]
async fn mismatched_note_page_snapshots_never_authorize_a_mixed_archive() {
    let harness = harness();
    seed_ticket(&harness, HERE, MEMBER).await;
    for number in 1..=9 {
        harness
            .invoke(note_invocation(
                &format!("note {number}"),
                MODERATOR,
                &[STAFF_ROLE],
            ))
            .await
            .unwrap();
    }

    let second_key = note_page_key(ChannelId::new(HERE), 2);
    let mut second: StaffNotePage = harness.store().get(&second_key).await.unwrap().unwrap();
    second.panel = Some(NamedPanel {
        id: "different".to_owned(),
        category: None,
        staff_roles: vec![PANEL_STAFF_ROLE.to_string()],
        log_channel: None,
        close_after_days: 0,
    });
    harness.store().set(&second_key, &second).await.unwrap();
    harness
        .store()
        .delete(&format!("ticket:{HERE}"))
        .await
        .unwrap();

    let replies = harness
        .invoke(notes_invocation(HERE, Some(1), MODERATOR, &[STAFF_ROLE]))
        .await
        .unwrap();
    assert!(replies.last().unwrap().ephemeral, "{replies:?}");
    assert!(replies.last().unwrap().content.contains("did not work"));
    assert!(!replies.last().unwrap().content.contains("note 1"));
    assert!(!replies.last().unwrap().content.contains("note 9"));
}

#[tokio::test]
async fn a_closed_named_ticket_keeps_staff_access_when_an_older_page_is_missing() {
    let harness = harness();
    seed_ticket(&harness, HERE, MEMBER).await;
    let key = format!("ticket:{HERE}");
    let mut ticket: Ticket = harness.store().get(&key).await.unwrap().unwrap();
    ticket.panel = Some(NamedPanel {
        id: "support".to_owned(),
        category: None,
        staff_roles: vec![PANEL_STAFF_ROLE.to_string()],
        log_channel: None,
        close_after_days: 0,
    });
    harness.store().set(&key, &ticket).await.unwrap();

    for number in 1..=9 {
        harness
            .invoke(note_invocation(
                &format!("note {number}"),
                OTHER,
                &[PANEL_STAFF_ROLE],
            ))
            .await
            .unwrap();
    }
    let first_page_key = note_page_key(ChannelId::new(HERE), 1);
    // Store.list omits expired entries; deletion makes that result deterministic.
    harness.store().delete(&first_page_key).await.unwrap();

    let (raced_note, closed) = tokio::join!(
        harness.invoke(note_invocation("note 10", OTHER, &[PANEL_STAFF_ROLE])),
        harness.click(press("close", MEMBER))
    );
    let raced_note = raced_note.unwrap();
    closed.unwrap();
    assert_eq!(deleted(&harness), [HERE]);
    let note_was_saved = raced_note
        .last()
        .unwrap()
        .content
        .contains("Staff note saved");
    assert!(
        note_was_saved
            || raced_note
                .last()
                .unwrap()
                .content
                .contains("not an open ticket"),
        "{raced_note:?}"
    );
    harness.set_config(json!({"staff_roles": [STAFF_ROLE.to_string()], "panels": []}));

    let allowed = harness
        .invoke(notes_invocation(HERE, Some(1), OTHER, &[PANEL_STAFF_ROLE]))
        .await
        .unwrap();
    assert!(allowed.last().unwrap().ephemeral, "{allowed:?}");
    assert!(allowed.last().unwrap().content.contains("note 9"));
    assert!(allowed.last().unwrap().content.contains("1 of 1"));
    assert_eq!(
        allowed.last().unwrap().content.contains("note 10"),
        note_was_saved
    );

    let wrong_staff_role = harness
        .invoke(notes_invocation(HERE, Some(1), MEMBER, &[STAFF_ROLE]))
        .await
        .unwrap();
    assert!(
        wrong_staff_role.last().unwrap().ephemeral,
        "{wrong_staff_role:?}"
    );
    assert!(
        wrong_staff_role
            .last()
            .unwrap()
            .content
            .contains("Only the staff")
    );
    assert!(!wrong_staff_role.last().unwrap().content.contains("note 9"));
}

#[tokio::test]
async fn staff_notes_can_be_added_to_a_ticket_with_the_older_saved_shape() {
    let harness = harness();
    harness
        .store()
        .set(
            &format!("ticket:{HERE}"),
            &json!({
                "number": 3,
                "opener": MEMBER,
                "claimed_by": null,
                "created": 0
            }),
        )
        .await
        .unwrap();

    let saved = harness
        .invoke(note_invocation(
            "legacy ticket note",
            MODERATOR,
            &[STAFF_ROLE],
        ))
        .await
        .unwrap();
    assert!(saved.last().unwrap().ephemeral, "{saved:?}");
    let page: StaffNotePage = harness
        .store()
        .get(&note_page_key(ChannelId::new(HERE), 1))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(page.notes[0].text, "legacy ticket note");
}
