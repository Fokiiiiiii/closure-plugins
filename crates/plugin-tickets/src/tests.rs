use std::time::Duration;

use chrono::{Datelike, Timelike, Utc};

use closure_core::action::{Action, MentionPolicy, OverwriteTarget};
use closure_core::event::{ChannelId, Event, GuildId, MessageId, UserInfo};
use closure_core::storage::MAX_VALUE_BYTES;
use closure_sdk::testing::{Click, Harness, Invocation, ReplyKind, Submit};
use closure_sdk::ui::CustomId;
use closure_sdk::{CommandOptionValue, Level, Locale};
use serde_json::json;
use twilight_model::channel::message::Component;
use twilight_model::guild::Permissions;
use twilight_model::id::Id;
use twilight_model::id::marker::ChannelMarker;

use crate::{
    CLOSE_REQUEST_TIMEOUT, CLOSING, CloseRequestExpiry, CloseRequestTimeout, ClosingJob, Config,
    HISTORY_PAGE_BYTES, HISTORY_SEARCH_PAGE_BUDGET, HISTORY_VALUE_BYTES, HistorySearchState,
    HistoryStatus, LABEL_LIST_UTF16_UNITS, MAX_CLOSE_REQUEST_DEADLINE_UNIX_SECONDS, NOTE_MAX_CHARS,
    NOTE_PAGE_LIMIT, NOTE_RETENTION, NOTES_PER_PAGE, NamedPanel, REPLY_CHUNK_MAX_UTF16,
    REPLY_SEND_COUNTER, RatingRequest, ReplySendAttempt, SavedReply, StaffNote, StaffNotePage,
    Ticket, TicketHistory, TicketRating, builder, close_request_expiry, close_request_timeout_key,
    closing_key, display_label, history_content_key, history_id_key, history_index_key,
    history_key, history_search_initial_state, history_search_state_valid, label_key, label_pages,
    note_page_key, note_pages_prefix, parse_rating_request, rating_button_id, reply_key,
    reply_send_attempt_key, transcript_is_incomplete, transcript_page_ranges,
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
    let channels: Vec<twilight_model::channel::Channel> = [HERE, LOG]
        .into_iter()
        .map(|channel| {
            serde_json::from_value(json!({
                "id": channel.to_string(),
                "type": 0,
                "guild_id": guild.get().to_string(),
                "name": format!("ticket-test-{channel}"),
                "topic": null,
                "parent_id": null,
                "permission_overwrites": []
            }))
            .unwrap()
        })
        .collect();
    harness.api().set_channels(guild, channels);
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

fn with_close_request_timeout(harness: &Harness, hours: u32) {
    harness.set_config(json!({
        "staff_roles": [STAFF_ROLE.to_string()],
        "log_channel": LOG.to_string(),
        "max_open": 1,
        "close_request_timeout_hours": hours
    }));
}

async fn seed_ticket(harness: &Harness, channel: u64, opener: u64) {
    let ticket = Ticket {
        number: 3,
        opener,
        claimed_by: None,
        created: 0,
        closing: None,
        close_generation: 0,
        close_request: None,
        panel: None,
        staff_roles: None,
        form_answers: Vec::new(),
        labels: Vec::new(),
        settings: None,
        form_answer_count: 0,
        thread: false,
        thread_parent: None,
        closed: false,
        pending_operation: None,
        panel_role_baselines: Vec::new(),
        participant_roles: Vec::new(),
        panel_user_baselines: Vec::new(),
        participant_users: Vec::new(),
        waiting_for_reply: false,
        staff_access: None,
        automation: crate::automation::State::default(),
        message_quote: None,
        message_quote_message_id: None,
    };
    harness
        .store()
        .set(&format!("ticket:{channel}"), &ticket)
        .await
        .unwrap();
}

async fn open_reservations(harness: &Harness, user: u64) -> Vec<crate::OpenReservation> {
    harness
        .store()
        .list(&crate::open_reservation_prefix(user), 100)
        .await
        .unwrap()
        .into_iter()
        .map(|(_, entry)| serde_json::from_value(entry.value).unwrap())
        .collect()
}

fn pending_open_reservation(opener: u64, number: i64) -> crate::OpenReservation {
    crate::OpenReservation {
        opener,
        number,
        token: number,
        created: 0,
        panel: None,
        settings: crate::Config::default(),
        staff_roles: Vec::new(),
        staff_access: None,
        form_answers: Vec::new(),
        form_modal_revision: None,
        message_quote: None,
        channel_name: String::new(),
        staff_opening: Some(false),
        generation: 1,
        until: 0,
        channel: None,
        api_started: true,
        attempts: 0,
        released_at: None,
        released_by: None,
    }
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

fn member_left(user: u64) -> Event {
    Event::MemberLeft {
        guild_id: Id::new(1),
        user: UserInfo {
            id: Id::new(user),
            name: "member".into(),
            bot: false,
        },
    }
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

fn panel_invocation(panel: Option<&str>, message: Option<&str>) -> Invocation {
    let mut invocation = Invocation::new("ticket")
        .subcommand("panel")
        .roles([MODERATOR])
        .user(MODERATOR);
    if let Some(panel) = panel {
        invocation = invocation.option("panel", CommandOptionValue::String(panel.to_owned()));
    }
    if let Some(message) = message {
        invocation = invocation.option("message", CommandOptionValue::String(message.to_owned()));
    }
    invocation
}

fn existing_panel_message(
    id: u64,
    channel: u64,
    content: &str,
) -> twilight_model::channel::Message {
    serde_json::from_value(json!({
        "id": id.to_string(),
        "channel_id": channel.to_string(),
        "content": content,
        "author": {
            "id": "9999",
            "username": "closure",
            "discriminator": "0",
            "avatar": null,
            "bot": true
        },
        "attachments": [],
        "embeds": [],
        "mention_everyone": false,
        "mention_roles": [],
        "mentions": [],
        "pinned": false,
        "timestamp": "2026-10-24T00:00:00.000000+00:00",
        "edited_timestamp": null,
        "tts": false,
        "type": 0,
        "components": [{
            "type": 1,
            "components": [{
                "type": 2,
                "custom_id": "tickets:open:*:billing",
                "disabled": false,
                "emoji": null,
                "label": "Billing",
                "style": 1,
                "url": null,
                "sku_id": null
            }]
        }]
    }))
    .unwrap()
}

type PostedPanel = (ChannelId, Option<MessageId>, Option<String>, Vec<Component>);

fn posted_panels(harness: &Harness) -> Vec<PostedPanel> {
    harness
        .actions()
        .into_iter()
        .filter_map(|action| match action {
            Action::PostMessage {
                channel_id,
                message_id,
                content,
                components,
                ..
            } => Some((channel_id, message_id, content, components)),
            _ => None,
        })
        .collect()
}

fn posted_panel(
    harness: &Harness,
) -> (ChannelId, Option<MessageId>, Option<String>, Vec<Component>) {
    posted_panels(harness).into_iter().next().unwrap()
}

async fn closed_history(harness: &Harness, channel: u64) -> TicketHistory {
    let key = harness
        .store()
        .get::<String>(&history_id_key(ChannelId::new(channel)))
        .await
        .unwrap()
        .unwrap();
    harness.store().get(&key).await.unwrap().unwrap()
}

fn closed_rating_id(history: &TicketHistory, channel: u64, score: u8) -> String {
    rating_button_id(
        &closure_core::module::ModuleId::new("tickets").unwrap(),
        closure_core::event::UserId::new(history.opener),
        ChannelId::new(channel),
        history.number,
        history.attempt,
        score,
    )
}

fn rate_invocation(ticket: i64, score: i64, user: u64) -> Invocation {
    Invocation::new("ticket")
        .subcommand("rate")
        .option("ticket", CommandOptionValue::Integer(ticket))
        .option("score", CommandOptionValue::Integer(score))
        .user(user)
}

fn rating_dm_count(harness: &Harness) -> usize {
    harness
        .actions()
        .iter()
        .filter(|action| {
            matches!(action, Action::SendDirectMessage { .. })
                && action
                    .custom_ids()
                    .into_iter()
                    .any(|custom_id| custom_id.starts_with("tickets:rate:"))
        })
        .count()
}

fn rating_dm_message(harness: &Harness) -> (closure_core::event::MessageId, ChannelId) {
    let mut next_id = 0;
    for action in harness.actions() {
        match &action {
            Action::CreateChannel { .. }
            | Action::CreateRole { .. }
            | Action::SendViaWebhook { .. }
            | Action::SendFile { .. }
            | Action::SendMessage { .. }
            | Action::SuppressEmbeds { .. }
            | Action::SendDirectMessage { .. }
            | Action::PostMessage {
                message_id: None, ..
            } => next_id += 1,
            _ => {}
        }

        if matches!(action, Action::SendDirectMessage { .. })
            && action
                .custom_ids()
                .into_iter()
                .any(|custom_id| custom_id.starts_with("tickets:rate:"))
        {
            return (
                closure_core::event::MessageId::new(next_id),
                ChannelId::new(1),
            );
        }
    }
    panic!("no rating direct message was sent");
}

fn press(route: &str, user: u64) -> Click {
    Click::new(format!("tickets:{route}:*")).user(user)
}

fn panel_press(panel: &str, user: u64) -> Click {
    Click::new(format!("tickets:open:*:{panel}")).user(user)
}

fn legacy_form_questions(questions: &[String]) -> Vec<crate::advanced_forms::FormQuestion> {
    questions
        .iter()
        .cloned()
        .map(crate::advanced_forms::FormQuestion::Legacy)
        .collect()
}

fn default_form_config(questions: &[String]) -> crate::Config {
    crate::Config {
        staff_roles: vec![STAFF_ROLE.to_string()],
        questions: legacy_form_questions(questions),
        ..crate::Config::default()
    }
}

fn named_form_config(panel_id: &str, questions: &[String], staff_role: u64) -> crate::Config {
    let panel = crate::NamedPanel {
        id: panel_id.to_owned(),
        staff_roles: vec![staff_role.to_string()],
        questions: legacy_form_questions(questions),
        ..crate::NamedPanel::default()
    };
    crate::Config::default().with_panel(&panel)
}

fn form_submit_id(_panel: &str, user: u64, _config: &crate::Config) -> String {
    form_submission_id(user, 1, 1)
}

fn form_submission_id(user: u64, number: i64, generation: i64) -> String {
    CustomId::new(crate::FORM)
        .only_for(Id::new(user))
        .data(format!("{number}:{generation}"))
        .encode(&closure_core::module::ModuleId::new("tickets").unwrap())
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

fn search_history_invocation(query: &str, staff: bool) -> Invocation {
    let command = Invocation::new("ticket")
        .subcommand("search")
        .option("query", CommandOptionValue::String(query.to_owned()));
    if staff {
        command.roles([STAFF_ROLE]).user(MODERATOR)
    } else {
        command.user(OTHER)
    }
}

fn view_history_invocation(number: i64, page: Option<u32>, staff: bool) -> Invocation {
    let command = Invocation::new("ticket")
        .subcommand("history")
        .option("number", CommandOptionValue::Integer(number));
    let command = match page {
        Some(page) => command.option("page", CommandOptionValue::Integer(i64::from(page))),
        None => command,
    };
    if staff {
        command.roles([STAFF_ROLE]).user(MODERATOR)
    } else {
        command.user(OTHER)
    }
}

async fn stage_closing_recovery(harness: &Harness) -> TicketHistory {
    seed_ticket(harness, HERE, MEMBER).await;
    harness
        .invoke(Invocation::new("ticket").subcommand("close").user(MEMBER))
        .await
        .unwrap();
    let channel = Id::new(HERE);
    let key = history_key(3, channel);
    let mut history: TicketHistory = harness.store().get(&key).await.unwrap().unwrap();
    history.status = HistoryStatus::Closing;
    let ticket = Ticket {
        number: history.number,
        opener: history.opener,
        claimed_by: history.claimed_by,
        created: history.created,
        closing: Some(history.closed),
        close_generation: history.attempt,
        close_request: None,
        panel: history.panel.clone(),
        staff_roles: Some(history.staff_roles.clone()),
        form_answers: Vec::new(),
        labels: history.labels.clone(),
        settings: None,
        form_answer_count: 0,
        thread: false,
        thread_parent: None,
        closed: false,
        pending_operation: None,
        panel_role_baselines: Vec::new(),
        participant_roles: Vec::new(),
        panel_user_baselines: Vec::new(),
        participant_users: Vec::new(),
        waiting_for_reply: false,
        staff_access: None,
        automation: crate::automation::State::default(),
        message_quote: None,
        message_quote_message_id: None,
    };
    harness
        .store()
        .set(&format!("ticket:{HERE}"), &ticket)
        .await
        .unwrap();
    harness.store().set(&key, &history).await.unwrap();
    harness
        .scheduler()
        .job(
            CLOSING,
            &ClosingJob {
                channel: HERE,
                number: history.number,
                attempt: history.attempt,
            },
        )
        .unwrap()
        .key(&closing_key(channel))
        .at(1)
        .await
        .unwrap();
    history
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

fn label_invocation(subcommand: &str, label: Option<&str>, user: u64, roles: &[u64]) -> Invocation {
    let route = subcommand.strip_prefix("label-").unwrap_or(subcommand);
    let command = Invocation::new("ticket").subcommand(&format!("label {route}"));
    let command = match label {
        Some(label) => command.option("label", CommandOptionValue::String(label.to_owned())),
        None => command,
    };
    command.roles(roles.iter().copied()).user(user)
}

fn label_page_invocation(page: i64, user: u64, roles: &[u64]) -> Invocation {
    Invocation::new("ticket")
        .subcommand("label list")
        .option("page", CommandOptionValue::Integer(page))
        .roles(roles.iter().copied())
        .user(user)
}

fn reply_search_invocation(query: &str, page: Option<i64>, user: u64, roles: &[u64]) -> Invocation {
    let command = Invocation::new("ticket")
        .subcommand("reply search")
        .option("query", CommandOptionValue::String(query.to_owned()));
    let command = match page {
        Some(page) => command.option("page", CommandOptionValue::Integer(page)),
        None => command,
    };
    command.roles(roles.iter().copied()).user(user)
}

fn reply_add_invocation(id: &str, title: &str, body: &str, user: u64, roles: &[u64]) -> Invocation {
    Invocation::new("ticket")
        .subcommand("reply add")
        .option("id", CommandOptionValue::String(id.to_owned()))
        .option("title", CommandOptionValue::String(title.to_owned()))
        .option("body", CommandOptionValue::String(body.to_owned()))
        .roles(roles.iter().copied())
        .user(user)
}

fn reply_edit_invocation(
    id: &str,
    title: &str,
    body: &str,
    user: u64,
    roles: &[u64],
) -> Invocation {
    Invocation::new("ticket")
        .subcommand("reply edit")
        .option("id", CommandOptionValue::String(id.to_owned()))
        .option("title", CommandOptionValue::String(title.to_owned()))
        .option("body", CommandOptionValue::String(body.to_owned()))
        .roles(roles.iter().copied())
        .user(user)
}

fn reply_delete_invocation(id: &str, user: u64, roles: &[u64]) -> Invocation {
    Invocation::new("ticket")
        .subcommand("reply delete")
        .option("id", CommandOptionValue::String(id.to_owned()))
        .roles(roles.iter().copied())
        .user(user)
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
    let tickets = harness.store().list("ticket:", 10).await.unwrap();
    assert_eq!(tickets.len(), 1);
    let channel_id = tickets[0]
        .0
        .strip_prefix("ticket:")
        .unwrap()
        .parse::<u64>()
        .unwrap();
    let ticket: Ticket = harness.store().get(&tickets[0].0).await.unwrap().unwrap();
    let close = |user| {
        Invocation::new("ticket")
            .subcommand("close")
            .user(user)
            .channel(channel_id)
    };

    let refused = harness.invoke(close(OTHER)).await.unwrap();
    assert!(
        refused.last().unwrap().content.contains("Only the member"),
        "{refused:?}"
    );
    assert!(deleted(&harness).is_empty());
    harness.invoke(close(MEMBER)).await.unwrap();
    assert_eq!(deleted(&harness), [channel_id]);
    let saved = transcripts(&harness);
    assert_eq!(saved.len(), 1);
    assert_eq!(saved[0].0, format!("ticket-{:04}.txt", ticket.number));
    assert!(
        saved[0].1.contains(&format!("Ticket #{}", ticket.number)),
        "{}",
        saved[0].1
    );
    let late = harness.invoke(close(MEMBER)).await.unwrap();
    assert!(
        late.last().unwrap().content.contains("not an open ticket"),
        "{late:?}"
    );
    assert_eq!(deleted(&harness), [channel_id]);
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
        .fail_nth(3, closure_core::error::DiscordError::Fatal("down".into()));
    let first = harness.click(press("close", MEMBER)).await;
    let key = format!("ticket:{HERE}");
    let kept: Option<Ticket> = harness.store().get(&key).await.unwrap();
    assert!(
        kept.is_some(),
        "the ticket is not forgotten before its channel is gone: {first:?}"
    );
    let archive_key = history_key(3, Id::new(HERE));
    let history: TicketHistory = harness.store().get(&archive_key).await.unwrap().unwrap();
    assert_eq!(history.status, HistoryStatus::Closing);
    harness.advance(Duration::from_secs(400));
    let recovery = harness.run_due_jobs().await;
    assert_eq!(recovery.len(), 1);
    assert!(recovery[0].is_ok(), "{recovery:?}");
    assert!(deleted(&harness).contains(&HERE));
    let history: TicketHistory = harness.store().get(&archive_key).await.unwrap().unwrap();
    assert_eq!(history.status, HistoryStatus::Closed);
    assert!(harness.store().get::<Ticket>(&key).await.unwrap().is_none());
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
async fn a_member_leaving_closes_only_their_ticket_when_enabled() {
    let harness = harness();
    harness.set_config(json!({
        "staff_roles": [STAFF_ROLE.to_string()],
        "close_when_opener_leaves": true
    }));
    harness.click(press("open", MEMBER)).await.unwrap();
    harness.click(press("open", OTHER)).await.unwrap();

    let mut member_channel = None;
    let mut other_key = None;
    for (key, _) in harness.store().list("ticket:", 10).await.unwrap() {
        let ticket: Ticket = harness.store().get(&key).await.unwrap().unwrap();
        if ticket.opener == MEMBER {
            member_channel = Some(key.trim_start_matches("ticket:").parse::<u64>().unwrap());
        } else if ticket.opener == OTHER {
            other_key = Some(key);
        }
    }
    let member_channel = member_channel.unwrap();
    let other_key = other_key.unwrap();

    harness.emit(member_left(MEMBER)).await.unwrap();
    harness.run_due_jobs().await;
    harness.run_due_jobs().await;

    assert_eq!(deleted(&harness), [member_channel]);
    assert!(
        harness
            .store()
            .get::<Ticket>(&other_key)
            .await
            .unwrap()
            .is_some(),
        "another member's ticket stays open"
    );
}

#[tokio::test]
async fn tickets_stay_open_on_member_leave_by_default() {
    let harness = harness();
    harness.click(press("open", MEMBER)).await.unwrap();

    harness.emit(member_left(MEMBER)).await.unwrap();
    harness.run_due_jobs().await;

    assert!(deleted(&harness).is_empty());
    assert_eq!(harness.jobs(), 0);
}

#[tokio::test]
async fn a_member_without_tickets_completes_an_empty_scan_without_changes() {
    let harness = harness();
    harness.set_config(json!({
        "staff_roles": [STAFF_ROLE.to_string()],
        "close_when_opener_leaves": true
    }));

    harness.emit(member_left(MEMBER)).await.unwrap();

    assert_eq!(harness.jobs(), 1);
    harness.run_due_jobs().await;
    assert_eq!(harness.jobs(), 0);
    assert!(
        harness
            .store()
            .list("ticket:", 10)
            .await
            .unwrap()
            .is_empty(),
        "an empty scan leaves ticket records unchanged"
    );
    assert!(
        created(&harness).is_empty(),
        "an empty scan creates no channels"
    );
    assert!(
        deleted(&harness).is_empty(),
        "an empty scan deletes no channels"
    );
    assert!(!harness.actions().iter().any(|action| matches!(
        action,
        Action::EditChannel { .. }
            | Action::SetPermissionOverwrite { .. }
            | Action::DeletePermissionOverwrite { .. }
    )));
}

#[tokio::test]
async fn a_member_leave_finds_ticket_records_past_the_store_list_limit() {
    const DELETED_CHANNEL: u64 = 999_999;
    let harness = harness();
    harness.set_config(json!({
        "staff_roles": [STAFF_ROLE.to_string()],
        "close_when_opener_leaves": true
    }));
    for channel in 10_000..11_000 {
        seed_ticket(&harness, channel, OTHER).await;
    }
    seed_ticket(&harness, DELETED_CHANNEL, MEMBER).await;
    harness
        .store()
        .score_add(crate::OPEN, MEMBER, 1)
        .await
        .unwrap();

    harness.emit(member_left(MEMBER)).await.unwrap();
    harness.run_due_jobs().await;
    harness.run_due_jobs().await;

    assert!(
        harness
            .store()
            .get::<Ticket>(&format!("ticket:{DELETED_CHANNEL}"))
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(harness.store().score(crate::OPEN, MEMBER).await.unwrap(), 0);
}

#[tokio::test]
async fn a_member_leave_close_retries_after_a_close_already_in_progress() {
    let harness = harness();
    harness.set_config(json!({
        "staff_roles": [STAFF_ROLE.to_string()],
        "close_when_opener_leaves": true
    }));
    harness.click(press("open", MEMBER)).await.unwrap();
    let (key, _) = harness.store().list("ticket:", 1).await.unwrap().remove(0);
    let mut ticket: Ticket = harness.store().get(&key).await.unwrap().unwrap();
    ticket.closing = Some(ticket.created);
    harness.store().set(&key, &ticket).await.unwrap();

    harness.emit(member_left(MEMBER)).await.unwrap();
    harness.run_due_jobs().await;
    harness.run_due_jobs().await;
    assert!(deleted(&harness).is_empty());

    harness.advance(Duration::from_secs(300));
    harness.run_due_jobs().await;
    assert_eq!(
        deleted(&harness),
        [key.trim_start_matches("ticket:").parse::<u64>().unwrap()]
    );
}

#[test]
fn old_config_and_ticket_snapshots_default_to_utc_without_hours() {
    let config: crate::Config = serde_json::from_value(json!({
        "staff_roles": [],
        "panels": [{"id": "billing"}]
    }))
    .unwrap();
    assert_eq!(config.timezone, "UTC");
    assert!(config.hours.is_empty());
    assert!(config.questions.is_empty());
    assert_eq!(config.panels[0].timezone, None);
    assert_eq!(config.panels[0].name, None);

    let ticket: Ticket = serde_json::from_value(json!({
        "number": 1,
        "opener": MEMBER,
        "claimed_by": null,
        "created": 0,
        "panel": {"id": "billing"}
    }))
    .unwrap();
    assert!(ticket.staff_roles.is_none());
    assert!(ticket.form_answers.is_empty());
    assert!(ticket.labels.is_empty());
    assert_eq!(ticket.close_generation, 0);
    assert_eq!(ticket.panel.unwrap().timezone, None);
}

#[tokio::test]
async fn closed_hours_do_not_reserve_a_ticket_or_create_a_channel() {
    const WEEKDAYS: [&str; 7] = [
        "monday",
        "tuesday",
        "wednesday",
        "thursday",
        "friday",
        "saturday",
        "sunday",
    ];
    let harness = harness();
    let day =
        WEEKDAYS[usize::try_from((Utc::now().weekday().num_days_from_monday() + 2) % 7).unwrap()];
    harness.set_config(json!({
        "staff_roles": [STAFF_ROLE.to_string()],
        "timezone": "UTC",
        "questions": ["What do you need help with?"],
        "hours": [{
            "panel": null,
            "weekday": day,
            "start_minute": 0,
            "end_minute": 1440
        }]
    }));

    let replies = harness.click(press("open", MEMBER)).await.unwrap();

    assert!(
        replies
            .last()
            .unwrap()
            .content
            .contains("Tickets are not available at this time."),
        "{replies:?}"
    );
    assert!(replies.iter().all(|reply| reply.kind != ReplyKind::Modal));
    assert!(created(&harness).is_empty());
    assert_eq!(harness.store().score(crate::OPEN, MEMBER).await.unwrap(), 0);
    assert_eq!(
        harness.store().score(crate::NUMBER, 0_u64).await.unwrap(),
        0
    );
}

#[tokio::test]
async fn invalid_opening_timezone_reports_a_config_error_before_ticket_side_effects() {
    let harness = harness();
    harness.set_config(json!({
        "staff_roles": [STAFF_ROLE.to_string()],
        "timezone": "Not/A-Timezone",
        "hours": []
    }));

    let replies = harness.click(press("open", MEMBER)).await.unwrap();

    assert!(
        replies
            .last()
            .unwrap()
            .content
            .contains("Ticket opening hours or time zone settings are invalid."),
        "{replies:?}"
    );
    assert!(created(&harness).is_empty());
    assert_eq!(harness.store().score(crate::OPEN, MEMBER).await.unwrap(), 0);
    assert_eq!(
        harness.store().score(crate::NUMBER, 0_u64).await.unwrap(),
        0
    );
}

#[tokio::test]
async fn invalid_opening_intervals_use_the_ticket_config_error_before_side_effects() {
    let harness = harness();
    harness.set_config(json!({
        "staff_roles": [STAFF_ROLE.to_string()],
        "timezone": "UTC",
        "hours": [{
            "panel": null,
            "weekday": "monday",
            "start_minute": 600,
            "end_minute": 600
        }]
    }));

    let replies = harness.click(press("open", MEMBER)).await.unwrap();

    assert!(
        replies
            .last()
            .unwrap()
            .content
            .contains("Check the ticket settings."),
        "{replies:?}"
    );
    assert!(created(&harness).is_empty());
    assert_eq!(harness.store().score(crate::OPEN, MEMBER).await.unwrap(), 0);
    assert_eq!(
        harness.store().score(crate::NUMBER, 0_u64).await.unwrap(),
        0
    );
}

#[tokio::test]
async fn opening_hours_for_an_unknown_panel_are_rejected_before_side_effects() {
    let harness = harness();
    harness.set_config(json!({
        "staff_roles": [STAFF_ROLE.to_string()],
        "timezone": "UTC",
        "hours": [{
            "panel": "missing",
            "weekday": "monday",
            "start_minute": 0,
            "end_minute": 1440
        }]
    }));

    let replies = harness.click(press("open", MEMBER)).await.unwrap();

    assert!(
        replies
            .last()
            .unwrap()
            .content
            .contains("Ticket opening hours or time zone settings are invalid."),
        "{replies:?}"
    );
    assert!(created(&harness).is_empty());
    assert_eq!(harness.store().score(crate::OPEN, MEMBER).await.unwrap(), 0);
    assert_eq!(
        harness.store().score(crate::NUMBER, 0_u64).await.unwrap(),
        0
    );
}

#[tokio::test]
async fn named_panel_hours_use_the_panel_timezone_override() {
    const WEEKDAYS: [&str; 7] = [
        "monday",
        "tuesday",
        "wednesday",
        "thursday",
        "friday",
        "saturday",
        "sunday",
    ];
    let harness = harness();
    let timezone: chrono_tz::Tz = "America/Los_Angeles".parse().unwrap();
    let local = Utc::now().with_timezone(&timezone);
    let minute = u16::try_from(local.hour() * 60 + local.minute()).unwrap();
    let weekday = local.weekday().num_days_from_monday();
    let (day, start_minute, end_minute) = if minute < 60 {
        ((weekday + 6) % 7, 1440 - (60 - minute), minute + 60)
    } else if minute >= 1380 {
        (weekday, minute - 60, minute + 60 - 1440)
    } else {
        (weekday, minute - 60, minute + 60)
    };
    let weekday = WEEKDAYS[usize::try_from(day).unwrap()];
    harness.set_config(json!({
        "staff_roles": [STAFF_ROLE.to_string()],
        "timezone": "UTC",
        "panels": [{
            "id": "billing",
            "staff_roles": [STAFF_ROLE.to_string()],
            "timezone": "America/Los_Angeles"
        }],
        "hours": [{
            "panel": "billing",
            "weekday": weekday,
            "start_minute": start_minute,
            "end_minute": end_minute
        }]
    }));

    harness.click(panel_press("billing", MEMBER)).await.unwrap();

    assert_eq!(created(&harness).len(), 1);
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
async fn named_panels_have_independent_open_limits() {
    let harness = harness();
    harness.set_config(json!({
        "staff_roles": [STAFF_ROLE.to_string()],
        "max_open": 0,
        "panels": [
            {
                "id": "billing",
                "staff_roles": [PANEL_STAFF_ROLE.to_string()],
                "max_open": 1
            },
            {
                "id": "support",
                "staff_roles": [OTHER_PANEL_STAFF_ROLE.to_string()],
                "max_open": 1
            }
        ]
    }));

    let schema = crate::schema();
    let panel_limit = &schema["properties"]["panels"]["items"]["properties"]["max_open"];
    assert_eq!(panel_limit["minimum"], 0);
    assert_eq!(panel_limit["default"], 0);

    harness.click(panel_press("billing", MEMBER)).await.unwrap();
    let tickets = harness.store().list("ticket:", 10).await.unwrap();
    let first: Ticket = harness.store().get(&tickets[0].0).await.unwrap().unwrap();
    assert_eq!(first.panel.as_ref().unwrap().max_open, 1);

    let blocked = harness.click(panel_press("billing", MEMBER)).await.unwrap();
    assert!(
        blocked.last().unwrap().content.contains("from this panel"),
        "{blocked:?}"
    );
    assert_eq!(created(&harness).len(), 1);

    harness.click(panel_press("support", MEMBER)).await.unwrap();
    assert_eq!(created(&harness).len(), 2);
    let blocked = harness.click(panel_press("support", MEMBER)).await.unwrap();
    assert!(
        blocked.last().unwrap().content.contains("from this panel"),
        "{blocked:?}"
    );
    assert_eq!(created(&harness).len(), 2);
}

#[tokio::test]
async fn concurrent_panel_opens_reserve_only_one_slot() {
    let harness = harness();
    harness.set_config(json!({
        "staff_roles": [STAFF_ROLE.to_string()],
        "max_open": 0,
        "panels": [{
            "id": "billing",
            "staff_roles": [PANEL_STAFF_ROLE.to_string()],
            "max_open": 1
        }]
    }));

    let (first, second) = tokio::join!(
        harness.click(panel_press("billing", MEMBER)),
        harness.click(panel_press("billing", MEMBER))
    );
    assert!(first.is_ok(), "{first:?}");
    assert!(second.is_ok(), "{second:?}");
    assert_eq!(created(&harness).len(), 1);
    assert_eq!(harness.store().score("open", MEMBER).await.unwrap(), 1);
}

#[tokio::test]
async fn staff_bypass_global_and_panel_open_limits() {
    let harness = harness();
    harness.set_config(json!({
        "staff_roles": [STAFF_ROLE.to_string()],
        "max_open": 1,
        "panels": [{
            "id": "billing",
            "staff_roles": [PANEL_STAFF_ROLE.to_string()],
            "max_open": 1
        }]
    }));

    harness
        .click(panel_press("billing", MEMBER).roles([PANEL_STAFF_ROLE]))
        .await
        .unwrap();
    harness
        .click(panel_press("billing", MEMBER).roles([PANEL_STAFF_ROLE]))
        .await
        .unwrap();
    assert_eq!(created(&harness).len(), 2);
}

#[tokio::test]
async fn open_limits_count_tickets_beyond_the_store_page_limit_and_repair_open_score() {
    let harness = harness();
    harness.set_config(json!({
        "staff_roles": [STAFF_ROLE.to_string()],
        "max_open": 1
    }));
    seed_ticket(&harness, 10_000_000_000_000_000_u64, MEMBER).await;
    for offset in 0..1_000_u64 {
        seed_ticket(&harness, 10_000_000_000_000_000_000_u64 + offset, MEMBER).await;
    }

    assert_eq!(harness.store().score("open", MEMBER).await.unwrap(), 0);
    let blocked = harness.click(press("open", MEMBER)).await.unwrap();
    assert!(
        blocked.last().unwrap().content.contains("open already"),
        "{blocked:?}"
    );
    assert!(created(&harness).is_empty());
    assert_eq!(harness.store().score("open", MEMBER).await.unwrap(), 1_001);
}

#[tokio::test]
async fn an_unconfirmed_transient_open_keeps_its_slot_when_no_channel_is_verified() {
    let harness = harness();
    harness.api().fail_nth(
        2,
        closure_core::error::DiscordError::Transient("connection lost".into()),
    );

    let reply = harness.click(press("open", MEMBER)).await.unwrap();
    assert!(
        reply
            .last()
            .unwrap()
            .content
            .contains("checking whether your ticket opened"),
        "{reply:?}"
    );
    let reservations = open_reservations(&harness, MEMBER).await;
    assert_eq!(reservations.len(), 1);
    assert!(reservations[0].api_started);
    assert_eq!(harness.store().score("open", MEMBER).await.unwrap(), 1);

    harness.advance(Duration::from_hours(24));
    let results = harness.run_due_jobs().await;
    assert!(results.iter().all(Result::is_ok), "{results:?}");
    assert_eq!(open_reservations(&harness, MEMBER).await.len(), 1);
    assert_eq!(harness.store().score("open", MEMBER).await.unwrap(), 1);
    assert!(created(&harness).is_empty());

    harness.advance(Duration::from_hours(24));
    let results = harness.run_due_jobs().await;
    assert!(results.iter().all(Result::is_ok), "{results:?}");
    assert_eq!(open_reservations(&harness, MEMBER).await.len(), 1);
    assert_eq!(harness.store().score("open", MEMBER).await.unwrap(), 1);
}

#[tokio::test]
async fn a_lost_create_response_is_recovered_from_the_channel_id_and_reservation_token() {
    let harness = harness();
    harness.api().fail_nth(
        2,
        closure_core::error::DiscordError::Transient("connection lost".into()),
    );
    harness.click(press("open", MEMBER)).await.unwrap();
    let reservations = open_reservations(&harness, MEMBER).await;
    let reservation = reservations[0].clone();
    let channel_id: ChannelId = Id::new(1_234_567_890_123_456_789_u64);
    let channel: twilight_model::channel::Channel = serde_json::from_value(json!({
        "id": channel_id.get().to_string(),
        "type": 0,
        "guild_id": "1",
        "name": format!("ticket-{:04}", reservation.number),
        "topic": format!("<@{MEMBER}> ticket-reservation:{}", reservation.token),
        "permission_overwrites": []
    }))
    .unwrap();
    harness.api().set_channels(Id::new(1), vec![channel]);

    harness.advance(Duration::from_secs(crate::OPEN_LEASE_SECONDS as u64));
    let results = harness.run_due_jobs().await;
    assert!(results.iter().all(Result::is_ok), "{results:?}");

    let key = format!("ticket:{}", channel_id.get());
    let ticket: Ticket = harness.store().get(&key).await.unwrap().unwrap();
    assert_eq!(ticket.number, reservation.number);
    assert_eq!(ticket.opener, MEMBER);
    assert_eq!(ticket.staff_roles, Some(vec![STAFF_ROLE]));
    assert!(open_reservations(&harness, MEMBER).await.is_empty());
    assert_eq!(harness.store().score("open", MEMBER).await.unwrap(), 1);
    assert!(deleted(&harness).is_empty());
}

#[tokio::test]
async fn a_terminal_channel_rejection_releases_a_reservation_after_a_confirmed_empty_lookup() {
    let harness = harness();
    harness.api().fail_nth(
        2,
        closure_core::error::DiscordError::Fatal("permission denied".into()),
    );

    let reply = harness.click(press("open", MEMBER)).await.unwrap();
    assert!(
        reply
            .last()
            .unwrap()
            .content
            .contains("did not work just now"),
        "{reply:?}"
    );
    assert!(open_reservations(&harness, MEMBER).await.is_empty());
    assert_eq!(harness.store().score("open", MEMBER).await.unwrap(), 0);
    assert!(created(&harness).is_empty());
}

#[tokio::test]
async fn managers_can_inspect_recheck_and_release_an_unconfirmed_opening() {
    let harness = harness();
    harness
        .store()
        .set(
            &crate::open_state_key(MEMBER),
            &crate::OpenState { lease: None },
        )
        .await
        .unwrap();
    harness
        .store()
        .set(
            &crate::open_reservation_key(MEMBER, 42),
            &crate::OpenReservation {
                opener: MEMBER,
                number: 42,
                token: 77,
                created: 0,
                panel: Some(NamedPanel {
                    id: "billing".to_owned(),
                    max_open: 1,
                    ..NamedPanel::default()
                }),
                settings: crate::Config::default(),
                staff_roles: vec![PANEL_STAFF_ROLE],
                staff_access: None,
                form_answers: Vec::new(),
                form_modal_revision: None,
                message_quote: None,
                channel_name: String::new(),
                staff_opening: Some(false),
                generation: 1,
                until: 0,
                channel: None,
                api_started: true,
                attempts: 0,
                released_at: None,
                released_by: None,
            },
        )
        .await
        .unwrap();

    let listed = harness
        .invoke(
            Invocation::new("ticket")
                .subcommand("maintenance openings")
                .roles([MODERATOR])
                .user(MODERATOR),
        )
        .await
        .unwrap();
    assert!(
        listed.last().unwrap().content.contains("token 77"),
        "{listed:?}"
    );

    let queued = harness
        .invoke(
            Invocation::new("ticket")
                .subcommand("maintenance recover-open")
                .option("number", CommandOptionValue::Integer(42))
                .roles([MODERATOR])
                .user(MODERATOR),
        )
        .await
        .unwrap();
    assert!(
        queued
            .last()
            .unwrap()
            .content
            .contains("Rechecking ticket opening"),
        "{queued:?}"
    );

    let released = harness
        .invoke(
            Invocation::new("ticket")
                .subcommand("maintenance release-open")
                .option("number", CommandOptionValue::Integer(42))
                .roles([MODERATOR])
                .user(MODERATOR),
        )
        .await
        .unwrap();
    assert!(
        released
            .last()
            .unwrap()
            .content
            .contains("released from the limit"),
        "{released:?}"
    );
    let reservations = open_reservations(&harness, MEMBER).await;
    assert_eq!(reservations.len(), 1);
    assert!(reservations[0].released_at.is_some());
    assert_eq!(reservations[0].released_by, Some(MODERATOR));
    assert_eq!(harness.store().score("open", MEMBER).await.unwrap(), 0);
}

#[tokio::test]
async fn an_unlimited_open_count_has_no_reservation_count_ceiling() {
    let harness = harness();
    harness.set_config(json!({
        "staff_roles": [STAFF_ROLE.to_string()],
        "max_open": 0
    }));
    for number in 1..=30 {
        harness
            .store()
            .set(
                &crate::open_reservation_key(MEMBER, number),
                &pending_open_reservation(MEMBER, number),
            )
            .await
            .unwrap();
    }
    harness
        .store()
        .score_add(crate::NUMBER, 0_u64, 30)
        .await
        .unwrap();

    let opened = harness.click(press("open", MEMBER)).await.unwrap();
    assert_eq!(created(&harness).len(), 1, "{opened:?}");
    assert_eq!(harness.store().score("open", MEMBER).await.unwrap(), 31);
    assert_eq!(open_reservations(&harness, MEMBER).await.len(), 30);
}

#[tokio::test]
async fn managers_can_page_through_all_reservations_beyond_the_store_list_limit() {
    let harness = harness();
    harness.set_config(json!({
        "staff_roles": [STAFF_ROLE.to_string()],
        "max_open": 1
    }));
    for number in 1..=1_001 {
        harness
            .store()
            .set(
                &crate::open_reservation_key(MEMBER, number),
                &pending_open_reservation(MEMBER, number),
            )
            .await
            .unwrap();
    }

    let blocked = harness.click(press("open", MEMBER)).await.unwrap();
    assert!(
        blocked.last().unwrap().content.contains("still checking"),
        "{blocked:?}"
    );
    assert!(created(&harness).is_empty());
    assert_eq!(harness.store().score("open", MEMBER).await.unwrap(), 1_001);

    let first_page = harness
        .invoke(
            Invocation::new("ticket")
                .subcommand("maintenance openings")
                .option("page", CommandOptionValue::Integer(1))
                .roles([MODERATOR])
                .user(MODERATOR),
        )
        .await
        .unwrap();
    let first_page = &first_page.last().unwrap().content;
    assert!(first_page.contains("page 1/126"), "{first_page}");
    assert!(first_page.contains("#1 ·"), "{first_page}");
    assert!(first_page.contains("#8 ·"), "{first_page}");
    assert!(!first_page.contains("#9 ·"), "{first_page}");

    let last_page = harness
        .invoke(
            Invocation::new("ticket")
                .subcommand("maintenance openings")
                .option("page", CommandOptionValue::Integer(126))
                .roles([MODERATOR])
                .user(MODERATOR),
        )
        .await
        .unwrap();
    let last_page = &last_page.last().unwrap().content;
    assert!(last_page.contains("page 126/126"), "{last_page}");
    assert!(last_page.contains("#1001 ·"), "{last_page}");

    let invalid_page = harness
        .invoke(
            Invocation::new("ticket")
                .subcommand("maintenance openings")
                .option("page", CommandOptionValue::Integer(127))
                .roles([MODERATOR])
                .user(MODERATOR),
        )
        .await
        .unwrap();
    assert!(
        invalid_page
            .last()
            .unwrap()
            .content
            .contains("Choose a page from 1 to 126"),
        "{invalid_page:?}"
    );
}

#[test]
fn open_channel_cleanup_requires_matching_id_and_exact_token() {
    let expected = ChannelId::new(123);
    let topic = Some("<@7> ticket-reservation:987654321");
    assert!(crate::matches_open_channel(
        expected,
        expected,
        topic,
        987_654_321
    ));
    assert!(!crate::matches_open_channel(
        ChannelId::new(124),
        expected,
        topic,
        987_654_321
    ));
    assert!(!crate::matches_open_channel(
        expected,
        expected,
        Some("<@7> ticket-reservation:9876543210"),
        987_654_321
    ));
    assert!(!crate::matches_open_channel(
        expected,
        expected,
        None,
        987_654_321
    ));
}

#[test]
fn a_recovered_ticket_keeps_its_effective_settings_roles_and_form_answers() {
    let panel = NamedPanel {
        id: "billing".to_owned(),
        category: Some("45".to_owned()),
        staff_roles: vec![PANEL_STAFF_ROLE.to_string()],
        log_channel: Some(PANEL_LOG.to_string()),
        close_after_days: 3,
        max_open: 2,
        ..NamedPanel::default()
    };
    let settings = crate::Config {
        category: panel.category.clone(),
        staff_roles: panel.staff_roles.clone(),
        log_channel: panel.log_channel.clone(),
        max_open: 1,
        close_after_days: panel.close_after_days,
        panels: Vec::new(),
        ..crate::Config::default()
    };
    let reservation = crate::OpenReservation {
        opener: MEMBER,
        number: 12,
        token: 900,
        created: 42,
        panel: Some(panel.clone()),
        settings: settings.clone(),
        staff_roles: vec![PANEL_STAFF_ROLE],
        staff_access: None,
        form_answers: vec![("Invoice number".to_owned(), "A-17".to_owned())],
        form_modal_revision: None,
        message_quote: None,
        channel_name: String::new(),
        staff_opening: Some(false),
        generation: 1,
        until: 300,
        channel: None,
        api_started: true,
        attempts: 0,
        released_at: None,
        released_by: None,
    };

    let ticket = crate::ticket_from_reservation(&reservation, MEMBER);
    assert_eq!(ticket.panel.as_ref().unwrap().max_open, 2);
    assert_eq!(ticket.staff_roles, Some(vec![PANEL_STAFF_ROLE]));
    assert_eq!(
        ticket.form_answers,
        [("Invoice number".to_owned(), "A-17".to_owned())]
    );
    assert_eq!(
        ticket.settings.as_ref().unwrap().log_channel,
        settings.log_channel
    );
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
async fn panel_command_without_named_panels_keeps_the_default_button() {
    let harness = harness();
    harness
        .invoke(
            panel_invocation(None, None)
                .option("channel", CommandOptionValue::Channel(ChannelId::new(HERE))),
        )
        .await
        .unwrap();
    let (_, message_id, _, components) = posted_panel(&harness);
    assert!(message_id.is_none());
    assert_eq!(components.len(), 1);
    let Component::ActionRow(row) = &components[0] else {
        panic!("the default button is not in an action row");
    };
    let Component::Button(button) = &row.components[0] else {
        panic!("the default panel is not a button");
    };
    assert_eq!(button.label.as_deref(), Some("Open a ticket"));
    assert_eq!(button.custom_id.as_deref(), Some("tickets:open:*"));
}

#[tokio::test]
async fn a_selected_panel_button_uses_its_name_and_configured_color() {
    let harness = harness();
    harness.set_config(json!({
        "staff_roles": [STAFF_ROLE.to_string()],
        "panels": [{
            "id": "billing",
            "name": "Billing",
            "button_style": "success",
            "staff_roles": [PANEL_STAFF_ROLE.to_string()]
        }]
    }));
    harness
        .invoke(
            panel_invocation(Some("billing"), None)
                .option("channel", CommandOptionValue::Channel(ChannelId::new(HERE))),
        )
        .await
        .unwrap();

    let (channel, message_id, _, components) = posted_panel(&harness);
    assert_eq!(channel, ChannelId::new(HERE));
    assert!(message_id.is_none());
    let Component::ActionRow(row) = &components[0] else {
        panic!("the panel is not in an action row");
    };
    let Component::Button(button) = &row.components[0] else {
        panic!("the selected panel is not a button");
    };
    assert_eq!(button.label.as_deref(), Some("Billing"));
    assert_eq!(
        button.style,
        twilight_model::channel::message::component::ButtonStyle::Success
    );
    assert!(
        button
            .custom_id
            .as_deref()
            .is_some_and(|custom_id| custom_id.ends_with(":billing"))
    );
}

#[tokio::test]
async fn a_panel_select_shows_names_descriptions_and_localized_fallbacks() {
    for (locale, fallback) in [(Locale::En, "Panel 2"), (Locale::Ja, "パネル 2")] {
        let harness = harness();
        harness.set_guild_locale(locale);
        harness.set_config(json!({
            "staff_roles": [STAFF_ROLE.to_string()],
            "panel_display": "select",
            "panels": [
                {
                    "id": "billing",
                    "name": "Billing",
                    "description": "Invoices and payments",
                    "staff_roles": [PANEL_STAFF_ROLE.to_string()]
                },
                {
                    "id": "support",
                    "name": " ",
                    "description": "General questions",
                    "staff_roles": [OTHER_PANEL_STAFF_ROLE.to_string()]
                }
            ]
        }));
        harness
            .invoke(
                panel_invocation(None, None)
                    .option("channel", CommandOptionValue::Channel(ChannelId::new(HERE)))
                    .locale(locale),
            )
            .await
            .unwrap();
        let (_, message_id, _, components) = posted_panel(&harness);
        assert!(message_id.is_none());
        assert_eq!(components.len(), 1);
        let Component::ActionRow(row) = &components[0] else {
            panic!("the selector is not in an action row");
        };
        let Component::SelectMenu(menu) = &row.components[0] else {
            panic!("the configured display is not a select menu");
        };
        assert_eq!(menu.min_values, Some(1));
        assert_eq!(menu.max_values, Some(1));
        let options = menu.options.as_deref().unwrap();
        assert_eq!(options.len(), 2);
        assert_eq!(options[0].label, "Billing");
        assert_eq!(
            options[0].description.as_deref(),
            Some("Invoices and payments")
        );
        assert_eq!(options[1].label, fallback);
        assert_eq!(options[1].description.as_deref(), Some("General questions"));
        assert_eq!(options[0].value, "billing");
        assert_eq!(options[1].value, "support");
        assert!(!options[0].label.contains("billing"));
        assert!(!options[1].label.contains("support"));
    }
}

#[tokio::test]
async fn a_panel_select_refuses_multiple_or_stale_values() {
    for (values, expected) in [
        (vec!["billing", "support"], "Choose one panel from the menu"),
        (vec!["removed"], "This panel was not found"),
    ] {
        let harness = harness();
        harness.set_config(json!({
            "staff_roles": [STAFF_ROLE.to_string()],
            "panels": [{
                "id": "billing",
                "name": "Billing",
                "staff_roles": [PANEL_STAFF_ROLE.to_string()]
            }]
        }));
        let replies = harness
            .click(
                Click::new("tickets:open-select:*")
                    .values(values)
                    .user(MEMBER),
            )
            .await
            .unwrap();
        assert!(
            replies.last().unwrap().content.contains(expected),
            "{replies:?}"
        );
        assert!(created(&harness).is_empty());
    }
}

#[tokio::test]
async fn a_selected_panel_menu_opens_only_its_panel_and_preserves_staff_access() {
    let harness = harness();
    harness.set_config(json!({
        "staff_roles": [STAFF_ROLE.to_string()],
        "panel_display": "select",
        "panels": [
            {
                "id": "billing",
                "name": "Billing",
                "staff_roles": [PANEL_STAFF_ROLE.to_string()]
            },
            {
                "id": "support",
                "name": "Support",
                "staff_roles": [OTHER_PANEL_STAFF_ROLE.to_string()]
            }
        ]
    }));
    harness
        .invoke(
            panel_invocation(None, None)
                .option("channel", CommandOptionValue::Channel(ChannelId::new(HERE))),
        )
        .await
        .unwrap();
    let (_, _, _, components) = posted_panel(&harness);
    let Component::ActionRow(row) = &components[0] else {
        panic!("the selector is not in an action row");
    };
    let Component::SelectMenu(menu) = &row.components[0] else {
        panic!("the configured display is not a select menu");
    };

    let replies = harness
        .click(
            Click::new(menu.custom_id.clone())
                .values(["billing"])
                .user(MEMBER),
        )
        .await
        .unwrap();
    assert!(
        replies
            .last()
            .unwrap()
            .content
            .contains("Your ticket is open"),
        "{replies:?}"
    );
    let made = created(&harness);
    assert_eq!(made.len(), 1);
    let overwrites = &made[0].1;
    let sees = |target: OverwriteTarget| {
        overwrites.iter().any(|(actual, allow, _)| {
            *actual == target && allow.contains(Permissions::VIEW_CHANNEL)
        })
    };
    assert!(sees(OverwriteTarget::Role(Id::new(PANEL_STAFF_ROLE))));
    assert!(!sees(OverwriteTarget::Role(Id::new(
        OTHER_PANEL_STAFF_ROLE
    ))));
    let tickets = harness.store().list("ticket:", 10).await.unwrap();
    let ticket: Ticket = harness.store().get(&tickets[0].0).await.unwrap().unwrap();
    assert_eq!(ticket.panel.as_ref().unwrap().id, "billing");
}

#[tokio::test]
async fn button_mode_splits_every_twenty_five_panels_across_messages() {
    for count in [25_usize, 26] {
        let harness = harness();
        let panels: Vec<_> = (0..count)
            .map(|number| {
                json!({
                    "id": format!("panel-{number}"),
                    "name": format!("Panel {number}"),
                    "staff_roles": [PANEL_STAFF_ROLE.to_string()]
                })
            })
            .collect();
        harness.set_config(json!({
            "staff_roles": [STAFF_ROLE.to_string()],
            "panel_display": "buttons",
            "panels": panels
        }));
        let replies = harness
            .invoke(
                panel_invocation(None, None)
                    .option("channel", CommandOptionValue::Channel(ChannelId::new(HERE))),
            )
            .await
            .unwrap();
        let posts = posted_panels(&harness);
        let expected_pages = count.div_ceil(25);
        assert_eq!(posts.len(), expected_pages);
        if count == 26 {
            assert!(
                replies.last().unwrap().content.contains("2 panel messages"),
                "{replies:?}"
            );
        }

        let mut button_count = 0;
        for (page, (channel, message_id, content, components)) in posts.iter().enumerate() {
            assert_eq!(*channel, ChannelId::new(HERE));
            assert!(message_id.is_none());
            assert!((1..=5).contains(&components.len()));
            for component in components {
                let Component::ActionRow(row) = component else {
                    panic!("the buttons are not in action rows");
                };
                assert!((1..=5).contains(&row.components.len()));
                for component in &row.components {
                    let Component::Button(button) = component else {
                        panic!("button mode contains a non-button component");
                    };
                    assert_eq!(
                        button.style,
                        twilight_model::channel::message::component::ButtonStyle::Secondary
                    );
                    button_count += 1;
                }
            }
            if count > 25 {
                assert!(
                    content
                        .as_deref()
                        .is_some_and(|text| text.contains(&format!("Panel page {} of 2", page + 1))),
                    "{content:?}"
                );
            }
        }
        assert_eq!(button_count, count);
    }
}

#[tokio::test]
async fn a_select_menu_can_hold_twenty_five_panels() {
    let harness = harness();
    let panels: Vec<_> = (0..25)
        .map(|number| {
            json!({
                "id": format!("panel-{number}"),
                "name": format!("Panel {number}"),
                "staff_roles": [PANEL_STAFF_ROLE.to_string()]
            })
        })
        .collect();
    harness.set_config(json!({
        "staff_roles": [STAFF_ROLE.to_string()],
        "panel_display": "select",
        "panels": panels
    }));
    harness
        .invoke(
            panel_invocation(None, None)
                .option("channel", CommandOptionValue::Channel(ChannelId::new(HERE))),
        )
        .await
        .unwrap();
    let (_, _, _, components) = posted_panel(&harness);
    assert_eq!(components.len(), 1);
    let Component::ActionRow(row) = &components[0] else {
        panic!("the selector is not in an action row");
    };
    let Component::SelectMenu(menu) = &row.components[0] else {
        panic!("the configured display is not a select menu");
    };
    assert_eq!(menu.options.as_deref().unwrap().len(), 25);
}

#[tokio::test]
async fn select_mode_splits_after_one_hundred_twenty_five_options_and_keeps_the_last_choice() {
    let harness = harness();
    let panels: Vec<_> = (0..126)
        .map(|number| {
            json!({
                "id": format!("panel-{number}"),
                "name": format!("Panel {number}"),
                "staff_roles": [PANEL_STAFF_ROLE.to_string()]
            })
        })
        .collect();
    harness.set_config(json!({
        "staff_roles": [STAFF_ROLE.to_string()],
        "panel_display": "select",
        "panels": panels
    }));
    let replies = harness
        .invoke(
            panel_invocation(None, None)
                .option("channel", CommandOptionValue::Channel(ChannelId::new(HERE))),
        )
        .await
        .unwrap();
    assert!(
        replies.last().unwrap().content.contains("2 panel messages"),
        "{replies:?}"
    );

    let posts = posted_panels(&harness);
    assert_eq!(posts.len(), 2);
    assert_eq!(posts[0].3.len(), 5);
    let mut option_count = 0;
    for row in &posts[0].3 {
        let Component::ActionRow(row) = row else {
            panic!("the select menus are not in action rows");
        };
        let Component::SelectMenu(menu) = &row.components[0] else {
            panic!("the configured display is not a select menu");
        };
        assert_eq!(menu.options.as_deref().unwrap().len(), 25);
        option_count += menu.options.as_deref().unwrap().len();
    }
    assert_eq!(option_count, 125);

    assert_eq!(posts[1].3.len(), 1);
    let Component::ActionRow(row) = &posts[1].3[0] else {
        panic!("the last select menu is not in an action row");
    };
    let Component::SelectMenu(menu) = &row.components[0] else {
        panic!("the configured display is not a select menu");
    };
    let options = menu.options.as_deref().unwrap();
    assert_eq!(options.len(), 1);
    assert_eq!(options[0].value, "panel-125");
    let replies = harness
        .click(
            Click::new(menu.custom_id.clone())
                .values(["panel-125"])
                .user(MEMBER),
        )
        .await
        .unwrap();
    assert!(
        replies
            .last()
            .unwrap()
            .content
            .contains("Your ticket is open"),
        "{replies:?}"
    );
    let tickets = harness.store().list("ticket:", 10).await.unwrap();
    let ticket: Ticket = harness.store().get(&tickets[0].0).await.unwrap().unwrap();
    assert_eq!(ticket.panel.as_ref().unwrap().id, "panel-125");
}

#[tokio::test]
async fn a_partial_button_publish_can_be_retried_by_page() {
    let harness = harness();
    let panels: Vec<_> = (0..26)
        .map(|number| {
            json!({
                "id": format!("panel-{number}"),
                "name": format!("Panel {number}"),
                "staff_roles": [PANEL_STAFF_ROLE.to_string()]
            })
        })
        .collect();
    harness.set_config(json!({
        "staff_roles": [STAFF_ROLE.to_string()],
        "panel_display": "buttons",
        "panels": panels
    }));
    harness
        .api()
        .fail_nth(2, closure_core::error::DiscordError::Fatal("down".into()));
    let partial = harness
        .invoke(
            panel_invocation(None, None)
                .option("channel", CommandOptionValue::Channel(ChannelId::new(HERE))),
        )
        .await
        .unwrap();
    assert!(
        partial
            .last()
            .unwrap()
            .content
            .contains("Published 1 of 2 panel messages"),
        "{partial:?}"
    );
    assert!(partial.last().unwrap().content.contains("Page 2"));

    let retry = harness
        .invoke(
            panel_invocation(None, None)
                .option("channel", CommandOptionValue::Channel(ChannelId::new(HERE)))
                .option("page", CommandOptionValue::Integer(2)),
        )
        .await
        .unwrap();
    assert!(
        retry
            .last()
            .unwrap()
            .content
            .contains("Published panel page 2 of 2"),
        "{retry:?}"
    );
    let posts = posted_panels(&harness);
    assert_eq!(posts.len(), 2);
    assert_eq!(
        posts[1].2.as_deref().unwrap(),
        "Need help? Use the panel below to open a ticket. Only you and the staff will see it.

Panel page 2 of 2"
    );
    let Component::ActionRow(row) = &posts[1].3[0] else {
        panic!("the retried page is not in an action row");
    };
    let Component::Button(button) = &row.components[0] else {
        panic!("the retried page does not contain a button");
    };
    assert_eq!(button.custom_id.as_deref(), Some("tickets:open:*:panel-25"));
}

#[tokio::test]
async fn updating_a_panel_keeps_its_text_and_a_failed_update_can_be_retried() {
    let harness = harness();
    harness.set_config(json!({
        "staff_roles": [STAFF_ROLE.to_string()],
        "panel_display": "select",
        "panels": [{
            "id": "billing",
            "name": "Billing",
            "description": "Invoices",
            "staff_roles": [PANEL_STAFF_ROLE.to_string()]
        }]
    }));
    harness
        .api()
        .set_message(existing_panel_message(99, HERE, "Keep this text"));

    let invoke = || {
        panel_invocation(None, Some("99"))
            .option("channel", CommandOptionValue::Channel(ChannelId::new(HERE)))
    };
    harness
        .api()
        .fail_nth(2, closure_core::error::DiscordError::Fatal("down".into()));
    let failed = harness.invoke(invoke()).await.unwrap();
    assert!(
        failed.last().unwrap().content.contains("did not work"),
        "{failed:?}"
    );
    assert!(!failed.last().unwrap().content.contains("was updated"));

    let updated = harness.invoke(invoke()).await.unwrap();
    assert!(
        updated.last().unwrap().content.contains("was updated"),
        "{updated:?}"
    );
    let posts = posted_panels(&harness);
    assert_eq!(posts.len(), 2);
    let (channel, message_id, content, components) = posts.last().unwrap();
    assert_eq!(*channel, ChannelId::new(HERE));
    assert_eq!(*message_id, Some(MessageId::new(99)));
    assert_eq!(content.as_deref(), Some("Keep this text"));
    let Component::ActionRow(row) = &components[0] else {
        panic!("the updated selector is not in an action row");
    };
    assert!(matches!(row.components[0], Component::SelectMenu(_)));
}

#[tokio::test]
async fn updating_a_specific_page_replaces_only_that_panel_message() {
    let harness = harness();
    let panels: Vec<_> = (0..26)
        .map(|number| {
            json!({
                "id": format!("panel-{number}"),
                "name": format!("Panel {number}"),
                "staff_roles": [PANEL_STAFF_ROLE.to_string()]
            })
        })
        .collect();
    harness.set_config(json!({
        "staff_roles": [STAFF_ROLE.to_string()],
        "panel_display": "buttons",
        "panels": panels
    }));
    harness
        .api()
        .set_message(existing_panel_message(101, HERE, "Custom panel text"));
    let updated = harness
        .invoke(
            panel_invocation(None, Some("101"))
                .option("channel", CommandOptionValue::Channel(ChannelId::new(HERE)))
                .option("page", CommandOptionValue::Integer(2)),
        )
        .await
        .unwrap();
    assert!(
        updated
            .last()
            .unwrap()
            .content
            .contains("Updated panel page 2 of 2"),
        "{updated:?}"
    );
    let posts = posted_panels(&harness);
    assert_eq!(posts.len(), 1);
    assert_eq!(posts[0].1, Some(MessageId::new(101)));
    assert_eq!(
        posts[0].2.as_deref(),
        Some("Custom panel text\n\nPanel page 2 of 2")
    );
    let Component::ActionRow(row) = &posts[0].3[0] else {
        panic!("the updated page is not in an action row");
    };
    let Component::Button(button) = &row.components[0] else {
        panic!("the updated page is not a button");
    };
    assert_eq!(button.custom_id.as_deref(), Some("tickets:open:*:panel-25"));
}

#[tokio::test]
async fn panel_update_refuses_messages_that_are_not_ticket_panels() {
    let harness = harness();
    let mut message = existing_panel_message(100, HERE, "Do not replace this");
    message.components.clear();
    harness.api().set_message(message);

    let replies = harness
        .invoke(
            panel_invocation(None, Some("100"))
                .option("channel", CommandOptionValue::Channel(ChannelId::new(HERE))),
        )
        .await
        .unwrap();
    let denied = replies.last().unwrap();
    assert!(denied.ephemeral, "{replies:?}");
    assert!(
        denied.content.contains(
            "Choose a valid ticket panel message created by this bot in the selected channel"
        ),
        "{replies:?}"
    );
    assert!(
        !harness.actions().iter().any(|action| matches!(
            action,
            Action::PostMessage { .. } | Action::EditMessage { .. }
        )),
        "the rejected update leaves the existing message unchanged"
    );
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
            staff_access: None,
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
        name: None,
        description: None,
        button_style: None,
        category: None,
        thread_channel: None,
        staff_roles: vec![PANEL_STAFF_ROLE.to_string()],
        log_channel: None,
        close_after_days: 0,
        timezone: None,
        questions: Vec::new(),
        max_open: 0,
        ..NamedPanel::default()
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
        name: None,
        description: None,
        button_style: None,
        category: None,
        thread_channel: None,
        staff_roles: vec![PANEL_STAFF_ROLE.to_string()],
        log_channel: None,
        close_after_days: 0,
        timezone: None,
        questions: Vec::new(),
        max_open: 0,
        ..NamedPanel::default()
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

#[tokio::test]
async fn ticket_stats_show_current_counts_only_to_managers() {
    let harness = harness();
    seed_ticket(&harness, HERE, MEMBER).await;
    seed_ticket(&harness, HERE + 1, OTHER).await;
    assign_ticket(&harness, HERE + 1, OTHER).await;
    seed_ticket(&harness, HERE + 2, OTHER).await;
    let closing_key = format!("ticket:{}", HERE + 2);
    let mut closing: Ticket = harness.store().get(&closing_key).await.unwrap().unwrap();
    closing.closing = Some(1);
    harness.store().set(&closing_key, &closing).await.unwrap();

    let denied = harness
        .invoke(
            Invocation::new("ticket")
                .subcommand("stats")
                .roles([STAFF_ROLE])
                .user(OTHER),
        )
        .await
        .unwrap();
    assert!(
        denied
            .last()
            .unwrap()
            .content
            .contains("Only ticket managers can view these statistics"),
        "{denied:?}"
    );

    let stats = harness
        .invoke(
            Invocation::new("ticket")
                .subcommand("stats")
                .roles([MODERATOR])
                .user(MODERATOR),
        )
        .await
        .unwrap();
    let content = &stats.last().unwrap().content;
    for expected in [
        "Total: 3",
        "Open: 2",
        "Closing: 1",
        "Assigned: 1",
        "Unassigned: 2",
    ] {
        assert!(content.contains(expected), "{stats:?}");
    }
}

#[tokio::test]
async fn ticket_stats_omit_partial_counts_at_the_store_limit() {
    let harness = harness();
    for channel in 100..1_100 {
        seed_ticket(&harness, channel, MEMBER).await;
    }

    let stats = harness
        .invoke(
            Invocation::new("ticket")
                .subcommand("stats")
                .roles([MODERATOR])
                .user(MODERATOR),
        )
        .await
        .unwrap();
    let content = &stats.last().unwrap().content;
    assert!(content.contains("At least 1,000 tickets"), "{stats:?}");
    assert!(!content.contains("Open:"), "{stats:?}");
}

#[tokio::test]
async fn a_named_panel_form_collects_answers_into_the_ticket() {
    let harness = harness();
    let questions = vec![
        "What do you need help with, and when?".to_owned(),
        "When did this start?".to_owned(),
    ];
    harness.set_config(json!({
        "staff_roles": [STAFF_ROLE.to_string()],
        "panels": [{
            "id": "support",
            "staff_roles": [PANEL_STAFF_ROLE.to_string()],
            "questions": questions.clone()
        }]
    }));

    let shown = harness.click(panel_press("support", MEMBER)).await.unwrap();
    assert_eq!(shown[0].kind, ReplyKind::Modal);
    assert_eq!(
        shown[0].custom_ids(),
        ["tickets:question-0", "tickets:question-1"]
    );
    assert!(created(&harness).is_empty());

    harness
        .submit(
            Submit::new(form_submit_id(
                "support",
                MEMBER,
                &named_form_config("support", &questions, PANEL_STAFF_ROLE),
            ))
            .user(MEMBER)
            .field("tickets:question-0", "I cannot sign in.")
            .field("tickets:question-1", "This morning."),
        )
        .await
        .unwrap();
    assert_eq!(created(&harness).len(), 1);

    let key = harness.store().list("ticket:", 10).await.unwrap()[0]
        .0
        .clone();
    let ticket: Ticket = harness.store().get(&key).await.unwrap().unwrap();
    assert_eq!(
        ticket.form_answers,
        vec![
            (questions[0].clone(), "I cannot sign in.".to_owned()),
            (questions[1].clone(), "This morning.".to_owned()),
        ]
    );
    assert_eq!(ticket.staff_roles, Some(vec![PANEL_STAFF_ROLE]));

    let ticket_embed = harness
        .actions()
        .into_iter()
        .find_map(|action| match action {
            Action::PostMessage {
                embeds,
                content: Some(_),
                ..
            } if !embeds.is_empty() => Some(serde_json::to_string(&embeds).unwrap()),
            _ => None,
        })
        .unwrap();
    assert!(ticket_embed.contains("What do you need help with, and when?"));
    assert!(ticket_embed.contains("I cannot sign in."));
    assert!(ticket_embed.contains("When did this start?"));
    assert!(ticket_embed.contains("This morning."));
}

#[tokio::test]
async fn a_ticket_form_cannot_be_submitted_by_another_member() {
    let harness = harness();
    let questions = vec!["What do you need help with?".to_owned()];
    harness.set_config(json!({
        "staff_roles": [STAFF_ROLE.to_string()],
        "panels": [{
            "id": "support",
            "staff_roles": [PANEL_STAFF_ROLE.to_string()],
            "questions": questions.clone()
        }]
    }));

    let shown = harness.click(panel_press("support", MEMBER)).await.unwrap();
    assert_eq!(shown[0].kind, ReplyKind::Modal);
    harness
        .submit(
            Submit::new(form_submit_id(
                "support",
                MEMBER,
                &named_form_config("support", &questions, PANEL_STAFF_ROLE),
            ))
            .user(OTHER)
            .field("tickets:question-0", "Not the opener."),
        )
        .await
        .unwrap();
    assert!(created(&harness).is_empty());
}

#[test]
fn panel_questions_fit_discord_modal_limits() {
    let schema = crate::schema();
    for questions in [
        &schema["properties"]["questions"],
        &schema["properties"]["panels"]["items"]["properties"]["questions"],
    ] {
        assert_eq!(questions["maxItems"], json!(5));
        assert_eq!(
            questions["items"]["oneOf"][0]["maxLength"],
            json!(crate::MAX_FORM_QUESTION_CHARS)
        );
        assert_eq!(questions["x-ui"]["level"], json!("basic"));
    }
    assert!(crate::valid_questions(&[
        crate::advanced_forms::FormQuestion::Legacy("Need help?".to_owned())
    ]));
    let too_many_questions = (0..6)
        .map(|_| crate::advanced_forms::FormQuestion::Legacy("Question".to_owned()))
        .collect::<Vec<_>>();
    assert!(!crate::valid_questions(&too_many_questions));
    assert!(!crate::valid_questions(&[
        crate::advanced_forms::FormQuestion::Legacy(" ".to_owned())
    ]));
    assert!(!crate::valid_questions(&[
        crate::advanced_forms::FormQuestion::Legacy("Q".repeat(46))
    ]));
}

#[test]
fn saved_form_answers_are_counted_toward_the_ticket_storage_limit() {
    let ticket = Ticket {
        number: 1,
        opener: MEMBER,
        claimed_by: None,
        created: 0,
        closing: None,
        close_generation: 0,
        close_request: None,
        panel: None,
        staff_roles: Some(Vec::new()),
        form_answers: vec![("Question".to_owned(), "x".repeat(crate::MAX_TICKET_BYTES))],
        labels: Vec::new(),
        settings: None,
        form_answer_count: 0,
        thread: false,
        thread_parent: None,
        closed: false,
        pending_operation: None,
        panel_role_baselines: Vec::new(),
        participant_roles: Vec::new(),
        panel_user_baselines: Vec::new(),
        participant_users: Vec::new(),
        waiting_for_reply: false,
        staff_access: None,
        automation: crate::automation::State::default(),
        message_quote: None,
        message_quote_message_id: None,
    };
    assert!(crate::ticket_storage_size(&ticket).unwrap() > crate::MAX_TICKET_BYTES);
}

#[test]
fn old_ticket_records_load_without_form_answers() {
    let ticket: Ticket = serde_json::from_value(json!({
        "number": 1,
        "opener": MEMBER,
        "claimed_by": null,
        "created": 0,
        "closing": null,
        "close_request": null,
        "panel": null
    }))
    .unwrap();
    assert!(ticket.form_answers.is_empty());
    assert!(ticket.staff_roles.is_none());
}

#[tokio::test]
async fn a_changed_question_config_requires_a_new_form() {
    let harness = harness();
    let old_questions = vec!["What do you need help with?".to_owned()];
    let new_questions = vec!["What changed?".to_owned()];
    harness.set_config(json!({
        "staff_roles": [STAFF_ROLE.to_string()],
        "questions": old_questions.clone()
    }));

    let shown = harness.click(panel_press("", MEMBER)).await.unwrap();
    assert_eq!(shown[0].kind, ReplyKind::Modal);
    let custom_id = form_submit_id("", MEMBER, &default_form_config(&old_questions));
    harness.set_config(json!({
        "staff_roles": [STAFF_ROLE.to_string()],
        "questions": new_questions
    }));
    let replies = harness
        .submit(
            Submit::new(custom_id)
                .user(MEMBER)
                .field("tickets:question-0", "My answer."),
        )
        .await
        .unwrap();

    assert!(replies.last().unwrap().ephemeral);
    assert!(created(&harness).is_empty());
}

#[test]
fn a_maximum_ticket_form_custom_id_fits_within_one_hundred_utf8_bytes() {
    let custom_id = form_submission_id(u64::MAX, i64::MAX, i64::MAX);
    assert!(
        custom_id.len() <= crate::MAX_TICKET_CUSTOM_ID_BYTES,
        "custom ID is {} bytes: {custom_id}",
        custom_id.len()
    );
}

#[tokio::test]
async fn a_stale_form_generation_cannot_change_its_reservation_or_open_a_ticket() {
    let harness = harness();
    let questions = vec!["What happened?".to_owned()];
    harness.set_config(json!({
        "staff_roles": [STAFF_ROLE.to_string()],
        "questions": questions
    }));

    let shown = harness.click(press("open", MEMBER)).await.unwrap();
    assert_eq!(shown[0].kind, ReplyKind::Modal);
    let mut reservations = open_reservations(&harness, MEMBER).await;
    assert_eq!(reservations.len(), 1);
    let reservation = reservations.remove(0);
    let stale_custom_id = form_submission_id(MEMBER, reservation.number, reservation.generation);

    let mut current_reservation = reservation;
    current_reservation.generation += 1;
    let expected = serde_json::to_value(&current_reservation).unwrap();
    harness
        .store()
        .set(
            &crate::open_reservation_key(MEMBER, current_reservation.number),
            &current_reservation,
        )
        .await
        .unwrap();

    let replies = harness
        .submit(
            Submit::new(stale_custom_id)
                .user(MEMBER)
                .field("tickets:question-0", "My answer."),
        )
        .await
        .unwrap();
    assert!(
        replies
            .last()
            .unwrap()
            .content
            .contains("checking whether your ticket opened"),
        "{replies:?}"
    );

    let after = open_reservations(&harness, MEMBER).await;
    assert_eq!(after.len(), 1);
    assert_eq!(serde_json::to_value(&after[0]).unwrap(), expected);
    assert!(created(&harness).is_empty());
    assert!(
        harness
            .store()
            .list("ticket:", 10)
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(harness.store().score(crate::OPEN, MEMBER).await.unwrap(), 1);
    assert_eq!(
        harness.store().score(crate::NUMBER, 0_u64).await.unwrap(),
        1
    );
}

#[tokio::test]
async fn a_changed_panel_staff_role_requires_a_new_form() {
    let harness = harness();
    let questions = vec!["What do you need help with?".to_owned()];
    harness.set_config(json!({
        "staff_roles": [STAFF_ROLE.to_string()],
        "panels": [{
            "id": "support",
            "staff_roles": [PANEL_STAFF_ROLE.to_string()],
            "questions": questions.clone()
        }]
    }));

    let shown = harness.click(panel_press("support", MEMBER)).await.unwrap();
    assert_eq!(shown[0].kind, ReplyKind::Modal);
    let custom_id = form_submit_id(
        "support",
        MEMBER,
        &named_form_config("support", &questions, PANEL_STAFF_ROLE),
    );
    harness.set_config(json!({
        "staff_roles": [STAFF_ROLE.to_string()],
        "panels": [{
            "id": "support",
            "staff_roles": [OTHER_PANEL_STAFF_ROLE.to_string()],
            "questions": questions
        }]
    }));
    let replies = harness
        .submit(
            Submit::new(custom_id)
                .user(MEMBER)
                .field("tickets:question-0", "My answer."),
        )
        .await
        .unwrap();

    assert!(replies.last().unwrap().ephemeral);
    assert!(created(&harness).is_empty());
}

#[tokio::test]
async fn saved_form_answers_can_be_resent_from_the_ticket() {
    let harness = harness();
    seed_ticket(&harness, HERE, MEMBER).await;
    let key = format!("ticket:{HERE}");
    let mut ticket: Ticket = harness.store().get(&key).await.unwrap().unwrap();
    ticket.form_answers = vec![("What happened?".to_owned(), "I cannot sign in.".to_owned())];
    harness.store().set(&key, &ticket).await.unwrap();

    let replies = harness
        .invoke(Invocation::new("ticket").subcommand("answers").user(MEMBER))
        .await
        .unwrap();
    assert!(replies.last().unwrap().ephemeral);
    let posted = harness
        .actions()
        .into_iter()
        .find_map(|action| match action {
            Action::SendMessage {
                channel_id,
                embeds,
                content: None,
                ..
            } if channel_id.get() == HERE && !embeds.is_empty() => {
                Some(serde_json::to_string(&embeds).unwrap())
            }
            _ => None,
        })
        .unwrap();
    assert!(posted.contains("What happened?"));
    assert!(posted.contains("I cannot sign in."));
}

#[tokio::test]
async fn saved_answers_remain_available_to_ticket_staff_after_config_changes() {
    let harness = harness();
    seed_ticket(&harness, HERE, MEMBER).await;
    let key = format!("ticket:{HERE}");
    let mut ticket: Ticket = harness.store().get(&key).await.unwrap().unwrap();
    ticket.form_answers = vec![("Question".to_owned(), "Saved answer.".to_owned())];
    ticket.staff_roles = Some(vec![PANEL_STAFF_ROLE]);
    harness.store().set(&key, &ticket).await.unwrap();
    harness.set_config(json!({ "staff_roles": [] }));

    let replies = harness
        .invoke(
            Invocation::new("ticket")
                .subcommand("answers")
                .roles([PANEL_STAFF_ROLE])
                .user(OTHER),
        )
        .await
        .unwrap();
    assert!(replies.last().unwrap().ephemeral);
    assert!(harness.actions().into_iter().any(|action| matches!(
        action,
        Action::SendMessage { embeds, .. } if !embeds.is_empty()
    )));
}

#[tokio::test]
async fn another_member_cannot_resend_ticket_form_answers() {
    let harness = harness();
    seed_ticket(&harness, HERE, MEMBER).await;
    let key = format!("ticket:{HERE}");
    let mut ticket: Ticket = harness.store().get(&key).await.unwrap().unwrap();
    ticket.form_answers = vec![("Question".to_owned(), "Private answer.".to_owned())];
    harness.store().set(&key, &ticket).await.unwrap();

    let replies = harness
        .invoke(Invocation::new("ticket").subcommand("answers").user(OTHER))
        .await
        .unwrap();
    assert!(replies.last().unwrap().ephemeral);
    assert!(harness.actions().into_iter().all(|action| !matches!(
        action,
        Action::SendMessage { embeds, .. } if !embeds.is_empty()
    )));
}

#[tokio::test]
async fn answers_are_saved_and_reposted_when_the_ticket_welcome_fails() {
    let harness = harness();
    let questions = vec!["What happened?".to_owned()];
    harness.set_config(json!({
        "staff_roles": [STAFF_ROLE.to_string()],
        "questions": questions.clone()
    }));

    harness.click(panel_press("", MEMBER)).await.unwrap();
    // CreateChannel and channel verification precede the welcome PostMessage.
    harness
        .api()
        .fail_nth(3, closure_core::error::DiscordError::Fatal("down".into()));
    harness
        .submit(
            Submit::new(form_submit_id("", MEMBER, &default_form_config(&questions)))
                .user(MEMBER)
                .field("tickets:question-0", "I cannot sign in."),
        )
        .await
        .unwrap();

    let entries = harness.store().list("ticket:", 10).await.unwrap();
    let ticket: Ticket = harness.store().get(&entries[0].0).await.unwrap().unwrap();
    assert_eq!(
        ticket.form_answers,
        vec![("What happened?".to_owned(), "I cannot sign in.".to_owned())]
    );
    let fallback = harness
        .actions()
        .into_iter()
        .find_map(|action| match action {
            Action::SendMessage {
                embeds,
                content: None,
                ..
            } if !embeds.is_empty() => Some(serde_json::to_string(&embeds).unwrap()),
            _ => None,
        })
        .unwrap();
    assert!(fallback.contains("What happened?"));
    assert!(fallback.contains("I cannot sign in."));
}

#[tokio::test]
async fn form_submission_after_hours_close_does_not_create_a_ticket() {
    const WEEKDAYS: [&str; 7] = [
        "monday",
        "tuesday",
        "wednesday",
        "thursday",
        "friday",
        "saturday",
        "sunday",
    ];
    let harness = harness();
    let now = Utc::now();
    let day = WEEKDAYS[usize::try_from(now.weekday().num_days_from_monday()).unwrap()];
    let current_minute = now.hour() * 60 + now.minute();
    let end_minute = (current_minute + 2).min(1_440);
    let questions = vec!["What happened?".to_owned()];
    let config_json = json!({
        "staff_roles": [STAFF_ROLE.to_string()],
        "timezone": "UTC",
        "questions": questions.clone(),
        "hours": [{
            "panel": null,
            "weekday": day,
            "start_minute": 0,
            "end_minute": end_minute
        }]
    });
    let config: crate::Config = serde_json::from_value(config_json.clone()).unwrap();
    harness.set_config(config_json);

    let shown = harness.click(press("open", MEMBER)).await.unwrap();
    assert_eq!(shown[0].kind, ReplyKind::Modal);
    let custom_id = form_submit_id("", MEMBER, &config);
    harness.advance(Duration::from_secs(180));

    let replies = harness
        .submit(
            Submit::new(custom_id)
                .user(MEMBER)
                .field("tickets:question-0", "I cannot sign in."),
        )
        .await
        .unwrap();

    assert!(
        replies
            .last()
            .unwrap()
            .content
            .contains("Tickets are not available at this time.")
    );
    assert!(created(&harness).is_empty());
    assert_eq!(harness.store().score(crate::OPEN, MEMBER).await.unwrap(), 0);
    assert_eq!(
        harness.store().score(crate::NUMBER, 0_u64).await.unwrap(),
        0
    );
}

#[tokio::test]
async fn a_changed_hours_config_requires_a_new_form() {
    const WEEKDAYS: [&str; 7] = [
        "monday",
        "tuesday",
        "wednesday",
        "thursday",
        "friday",
        "saturday",
        "sunday",
    ];
    let harness = harness();
    let questions = vec!["What happened?".to_owned()];
    harness.set_config(json!({
        "staff_roles": [STAFF_ROLE.to_string()],
        "questions": questions.clone()
    }));
    let shown = harness.click(press("open", MEMBER)).await.unwrap();
    assert_eq!(shown[0].kind, ReplyKind::Modal);
    let custom_id = form_submit_id("", MEMBER, &default_form_config(&questions));
    let day = WEEKDAYS[usize::try_from(Utc::now().weekday().num_days_from_monday()).unwrap()];
    harness.set_config(json!({
        "staff_roles": [STAFF_ROLE.to_string()],
        "questions": questions,
        "hours": [{
            "panel": null,
            "weekday": day,
            "start_minute": 0,
            "end_minute": 1440
        }]
    }));

    let replies = harness
        .submit(
            Submit::new(custom_id)
                .user(MEMBER)
                .field("tickets:question-0", "My answer."),
        )
        .await
        .unwrap();

    assert!(replies.last().unwrap().ephemeral);
    assert!(created(&harness).is_empty());
}

#[test]
fn named_panel_name_is_optional_and_role_widgets_are_advertised() {
    let config: crate::Config = serde_json::from_value(json!({
        "staff_roles": [],
        "panels": [
            {"id": "support"},
            {"id": "billing", "name": "Billing help"}
        ]
    }))
    .unwrap();
    assert_eq!(config.panels[0].name, None);
    assert_eq!(config.panels[0].id, "support");
    assert_eq!(config.panels[1].name.as_deref(), Some("Billing help"));
    assert_eq!(config.panels[1].id, "billing");

    let schema = crate::schema();
    let name = &schema["properties"]["panels"]["items"]["properties"]["name"];
    assert!(name["anyOf"].is_array());
    assert_eq!(
        schema["properties"]["staff_roles"]["x-ui"]["widget"],
        json!("roles")
    );
    assert_eq!(
        schema["properties"]["panels"]["items"]["properties"]["staff_roles"]["x-ui"]["widget"],
        json!("roles")
    );
}

#[test]
fn old_ticket_records_load_without_new_snapshot_fields() {
    let ticket: Ticket = serde_json::from_value(json!({
        "number": 3,
        "opener": MEMBER,
        "claimed_by": null,
        "created": 0,
        "closing": null,
        "close_request": null,
        "panel": null
    }))
    .unwrap();
    assert!(ticket.staff_roles.is_none());
    assert!(ticket.form_answers.is_empty());
    assert!(ticket.labels.is_empty());
    assert_eq!(ticket.close_generation, 0);
}

#[test]
fn transcript_cut_requires_an_older_message() {
    assert!(!transcript_is_incomplete(5000, false));
    assert!(transcript_is_incomplete(5000, true));
    assert!(!transcript_is_incomplete(4999, true));
}

// shortcut: keep this 103-line regression case intact; split if it grows past 110 lines.
#[allow(clippy::too_many_lines)]
#[tokio::test]
async fn staff_can_search_and_read_closed_ticket_history_privately() {
    let harness = harness();
    seed_ticket(&harness, HERE, MEMBER).await;
    let ticket_key = format!("ticket:{HERE}");
    let mut ticket: Ticket = harness.store().get(&ticket_key).await.unwrap().unwrap();
    ticket.form_answers = vec![("Order number".to_owned(), "42".to_owned())];
    ticket.labels = vec!["Urgent".to_owned()];
    harness.store().set(&ticket_key, &ticket).await.unwrap();

    harness
        .invoke(
            Invocation::new("ticket")
                .subcommand("close")
                .option(
                    "reason",
                    CommandOptionValue::String("billing follow up".to_owned()),
                )
                .user(MEMBER),
        )
        .await
        .unwrap();

    let logged = transcripts(&harness);
    assert_eq!(logged.len(), 1);
    assert!(!logged[0].1.contains("Order number"));
    let key = history_key(3, Id::new(HERE));
    let mut history: TicketHistory = harness.store().get(&key).await.unwrap().unwrap();
    assert_eq!(history.status, HistoryStatus::Closed);
    assert_eq!(history.staff_roles, [STAFF_ROLE]);
    history.rating = Some(TicketRating {
        score: 5,
        created: history.closed,
    });
    harness.store().set(&key, &history).await.unwrap();
    assert_eq!(history.labels, ["Urgent"]);
    assert!(history.transcript_pages > 0);

    let search = harness
        .invoke(search_history_invocation("billing", true))
        .await
        .unwrap();
    assert!(search.last().unwrap().content.contains("#3"), "{search:?}");
    let label_search = harness
        .invoke(search_history_invocation("urgent", true))
        .await
        .unwrap();
    assert!(
        label_search.last().unwrap().content.contains("#3"),
        "{label_search:?}"
    );
    let answer_search = harness
        .invoke(search_history_invocation("42", true))
        .await
        .unwrap();
    assert!(
        answer_search.last().unwrap().content.contains("#3"),
        "{answer_search:?}"
    );
    assert!(
        answer_search
            .last()
            .unwrap()
            .content
            .contains("order number: 42"),
        "{answer_search:?}"
    );
    let denied_search = harness
        .invoke(search_history_invocation("42", false))
        .await
        .unwrap();
    assert!(
        !denied_search.last().unwrap().content.contains("#3"),
        "{denied_search:?}"
    );
    assert!(
        !denied_search
            .last()
            .unwrap()
            .content
            .contains("order number: 42"),
        "{denied_search:?}"
    );

    let view = harness
        .invoke(view_history_invocation(3, None, true))
        .await
        .unwrap();
    assert!(
        view.last().unwrap().content.contains("Order number: 42"),
        "{view:?}"
    );
    assert!(view.last().unwrap().content.contains("Staff rating: 5/5"));
    let denied = harness
        .invoke(view_history_invocation(3, None, false))
        .await
        .unwrap();
    assert!(
        denied
            .last()
            .unwrap()
            .content
            .contains("not available to you"),
        "{denied:?}"
    );
    assert!(
        !denied.last().unwrap().content.contains("Order number"),
        "{denied:?}"
    );
}

#[test]
fn history_search_cursors_bind_to_the_user_and_expire() {
    let state = history_search_initial_state(42, "search term".to_owned(), 100, 7).unwrap();
    assert!(history_search_state_valid(&state, 42, &state.token, 100));
    assert!(!history_search_state_valid(&state, 43, &state.token, 100));
    assert!(!history_search_state_valid(
        &state,
        42,
        &state.token,
        state.expires_at
    ));
}

#[tokio::test]
async fn history_search_is_bounded_resumable_and_private() {
    let harness = harness();
    seed_ticket(&harness, HERE, MEMBER).await;
    harness
        .invoke(
            Invocation::new("ticket")
                .subcommand("close")
                .option("reason", CommandOptionValue::String("routine".to_owned()))
                .user(MEMBER),
        )
        .await
        .unwrap();

    let meta_key = history_key(3, Id::new(HERE));
    let mut history: TicketHistory = harness.store().get(&meta_key).await.unwrap().unwrap();
    let pages = HISTORY_SEARCH_PAGE_BUDGET + 1;
    let mut counts = vec![1_u32; pages];
    counts[HISTORY_SEARCH_PAGE_BUDGET - 1] = 4;
    counts[HISTORY_SEARCH_PAGE_BUDGET] = 2;
    history.transcript_pages = u32::try_from(pages).unwrap();
    history.transcript_index_pages = 1;
    history.transcript_char_count = counts.iter().map(|count| u64::from(*count)).sum();
    history.transcript_truncated = false;
    harness.store().set(&meta_key, &history).await.unwrap();
    harness
        .store()
        .set_for(
            &history_index_key(HERE, history.transcript_attempt, 0),
            &counts,
            Duration::from_secs(60),
        )
        .await
        .unwrap();
    for page in 0..pages {
        let text = if page == HISTORY_SEARCH_PAGE_BUDGET - 1 {
            "need"
        } else if page == HISTORY_SEARCH_PAGE_BUDGET {
            "le"
        } else {
            "x"
        };
        harness
            .store()
            .set_for(
                &history_content_key(HERE, history.transcript_attempt, page),
                &text,
                Duration::from_secs(60),
            )
            .await
            .unwrap();
    }

    let first = harness
        .invoke(search_history_invocation("needle", true))
        .await
        .unwrap();
    let first = first.last().unwrap();
    assert!(!first.content.contains("#3"), "{first:?}");
    let old_button = first.custom_ids().into_iter().next().unwrap();
    let token = old_button.rsplit(':').next().unwrap();
    let session_key = format!("ticket-history-search-session:{MODERATOR}:{token}");
    let saved: HistorySearchState = harness.store().get(&session_key).await.unwrap().unwrap();
    let resume = saved.current.as_ref().unwrap();
    assert_eq!(
        resume.next_page,
        u32::try_from(HISTORY_SEARCH_PAGE_BUDGET).unwrap()
    );
    assert!(resume.carry.ends_with("need"));

    let denied = harness
        .click(
            Click::new(old_button.clone())
                .user(OTHER)
                .roles([STAFF_ROLE]),
        )
        .await
        .unwrap();
    assert!(
        !denied.last().unwrap().content.contains("needle"),
        "{denied:?}"
    );
    let still_available: HistorySearchState =
        harness.store().get(&session_key).await.unwrap().unwrap();
    assert!(!still_available.consumed);

    let continued = harness
        .click(
            Click::new(old_button.clone())
                .user(MODERATOR)
                .roles([STAFF_ROLE]),
        )
        .await
        .unwrap();
    let result = continued.last().unwrap();
    assert!(result.content.contains("#3"), "{continued:?}");
    assert!(result.content.contains("needle"), "{continued:?}");
    assert!(result.custom_ids().is_empty(), "{continued:?}");
    let consumed: HistorySearchState = harness.store().get(&session_key).await.unwrap().unwrap();
    assert!(consumed.consumed);

    let stale = harness
        .click(Click::new(old_button).user(MODERATOR).roles([STAFF_ROLE]))
        .await
        .unwrap();
    assert!(!stale.last().unwrap().content.contains("#3"), "{stale:?}");
}

#[tokio::test]
async fn full_text_search_reports_missing_pages_and_checks_access_first() {
    let harness = harness();
    seed_ticket(&harness, HERE, MEMBER).await;
    let ticket_key = format!("ticket:{HERE}");
    let mut ticket: Ticket = harness.store().get(&ticket_key).await.unwrap().unwrap();
    ticket.form_answers = vec![("Request".to_owned(), "private phrase".to_owned())];
    harness.store().set(&ticket_key, &ticket).await.unwrap();
    harness
        .invoke(
            Invocation::new("ticket")
                .subcommand("close")
                .option("reason", CommandOptionValue::String("routine".to_owned()))
                .user(MEMBER),
        )
        .await
        .unwrap();

    let history: TicketHistory = harness
        .store()
        .get(&history_key(3, Id::new(HERE)))
        .await
        .unwrap()
        .unwrap();
    let content_key = history_content_key(HERE, history.transcript_attempt, 0);
    harness.store().delete(&content_key).await.unwrap();

    let denied = harness
        .invoke(search_history_invocation("private phrase", false))
        .await
        .unwrap();
    assert!(!denied.last().unwrap().content.contains("private phrase"));
    assert!(
        !denied
            .last()
            .unwrap()
            .content
            .contains("Results may be incomplete")
    );

    let staff = harness
        .invoke(search_history_invocation("private phrase", true))
        .await
        .unwrap();
    assert!(
        staff
            .last()
            .unwrap()
            .content
            .contains("Results may be incomplete"),
        "{staff:?}"
    );
}

#[tokio::test]
async fn history_is_available_without_a_log_channel() {
    let harness = harness();
    harness.set_config(json!({
        "staff_roles": [STAFF_ROLE.to_string()],
        "log_channel": null
    }));
    seed_ticket(&harness, HERE, MEMBER).await;
    let ticket_key = format!("ticket:{HERE}");
    let mut ticket: Ticket = harness.store().get(&ticket_key).await.unwrap().unwrap();
    ticket.form_answers = vec![("Order number".to_owned(), "42".to_owned())];
    ticket.labels = vec!["Urgent".to_owned()];
    harness.store().set(&ticket_key, &ticket).await.unwrap();

    harness
        .invoke(Invocation::new("ticket").subcommand("close").user(MEMBER))
        .await
        .unwrap();

    let history: TicketHistory = harness
        .store()
        .get(&history_key(3, Id::new(HERE)))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(history.status, HistoryStatus::Closed);
    assert!(history.transcript_pages > 0);
    assert_eq!(history.transcript_messages, 0);
    assert!(transcripts(&harness).is_empty());
    let view = harness
        .invoke(view_history_invocation(3, None, true))
        .await
        .unwrap();
    assert!(
        view.last().unwrap().content.contains("Order number: 42"),
        "{view:?}"
    );
}

#[tokio::test]
async fn retries_within_one_second_get_distinct_close_generations() {
    let harness = harness();
    seed_ticket(&harness, HERE, MEMBER).await;
    harness.api().fail_nth(
        2,
        closure_core::error::DiscordError::Fatal("log unavailable".into()),
    );
    let first = harness
        .invoke(Invocation::new("ticket").subcommand("close").user(MEMBER))
        .await;
    assert!(first.is_err(), "{first:?}");
    let ticket: Ticket = harness
        .store()
        .get(&format!("ticket:{HERE}"))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(ticket.close_generation, 1);
    assert!(ticket.closing.is_none());

    harness
        .invoke(Invocation::new("ticket").subcommand("close").user(MEMBER))
        .await
        .unwrap();
    let history: TicketHistory = harness
        .store()
        .get(&history_key(3, Id::new(HERE)))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(history.attempt, 2);
    assert_eq!(history.status, HistoryStatus::Closed);
}

#[tokio::test]
async fn closed_history_and_its_pages_expire_after_one_year() {
    let harness = harness();
    seed_ticket(&harness, HERE, MEMBER).await;
    harness
        .invoke(Invocation::new("ticket").subcommand("close").user(MEMBER))
        .await
        .unwrap();
    let channel = Id::new(HERE);
    let key = history_key(3, channel);
    let history: TicketHistory = harness.store().get(&key).await.unwrap().unwrap();

    harness.advance(Duration::from_secs(365 * 24 * 60 * 60 + 1));

    assert!(
        harness
            .store()
            .get::<TicketHistory>(&key)
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(
        harness
            .store()
            .get::<String>(&history_id_key(channel))
            .await
            .unwrap(),
        None
    );
    assert_eq!(
        harness
            .store()
            .get::<String>(&history_content_key(HERE, history.transcript_attempt, 0))
            .await
            .unwrap(),
        None
    );
}

#[test]
fn transcript_pages_preserve_text_and_fit_the_storage_value_limit() {
    let text = format!(
        "{}{}",
        "日本語😀".repeat(HISTORY_PAGE_BYTES),
        "\u{0001}".repeat(HISTORY_PAGE_BYTES)
    );
    let pages = transcript_page_ranges(&text);
    assert!(pages.len() > 2);
    let mut rebuilt = String::new();
    for (start, end) in &pages {
        let page = &text[*start..*end];
        rebuilt.push_str(page);
        assert!(serde_json::to_vec(page).unwrap().len() <= HISTORY_VALUE_BYTES);
    }
    assert_eq!(rebuilt, text);
}

#[tokio::test]
async fn a_recovery_job_finalizes_not_found_without_posting_a_second_log() {
    let harness = harness();
    let staged = stage_closing_recovery(&harness).await;
    assert_eq!(transcripts(&harness).len(), 1);
    harness
        .api()
        .fail_nth(1, closure_core::error::DiscordError::NotFound);
    harness.advance(Duration::from_secs(400));

    let results = harness.run_due_jobs().await;
    assert_eq!(results.len(), 1);
    assert!(results[0].is_ok(), "{results:?}");
    let history: TicketHistory = harness
        .store()
        .get(&history_key(staged.number, Id::new(HERE)))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(history.status, HistoryStatus::Closed);
    assert!(
        harness
            .store()
            .get::<Ticket>(&format!("ticket:{HERE}"))
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(transcripts(&harness).len(), 1);
}

#[tokio::test]
async fn a_recovery_job_does_not_close_when_a_transcript_page_is_missing() {
    let harness = harness();
    let staged = stage_closing_recovery(&harness).await;
    let deleted_before = deleted(&harness);
    assert!(
        harness
            .store()
            .delete(&history_content_key(HERE, staged.transcript_attempt, 0))
            .await
            .unwrap()
    );
    harness.advance(Duration::from_secs(400));

    let results = harness.run_due_jobs().await;
    assert_eq!(results.len(), 1);
    assert!(results[0].is_err(), "{results:?}");
    let history: TicketHistory = harness
        .store()
        .get(&history_key(staged.number, Id::new(HERE)))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(history.status, HistoryStatus::Closing);
    assert!(
        harness
            .store()
            .get::<Ticket>(&format!("ticket:{HERE}"))
            .await
            .unwrap()
            .is_some()
    );
    assert_eq!(deleted(&harness), deleted_before);
}

#[tokio::test]
async fn a_recovery_job_retries_when_closing_history_cannot_be_written() {
    let harness = harness();
    let staged = stage_closing_recovery(&harness).await;
    let key = history_key(staged.number, Id::new(HERE));
    let mut history: TicketHistory = harness.store().get(&key).await.unwrap().unwrap();
    history.closed = 1;
    history.reason.clear();
    let base = serde_json::to_vec(&history).unwrap().len();
    assert!(base < MAX_VALUE_BYTES);
    history.reason = "x".repeat(MAX_VALUE_BYTES - base - 1);
    assert_eq!(
        serde_json::to_vec(&history).unwrap().len(),
        MAX_VALUE_BYTES - 1
    );
    harness.store().set(&key, &history).await.unwrap();

    harness
        .api()
        .fail_nth(1, closure_core::error::DiscordError::NotFound);
    harness.advance(Duration::from_secs(400));
    let first = harness.run_due_jobs().await;
    assert_eq!(first.len(), 1);
    assert!(first[0].is_err(), "{first:?}");
    let still_closing: TicketHistory = harness.store().get(&key).await.unwrap().unwrap();
    assert_eq!(still_closing.status, HistoryStatus::Closing);
    assert!(
        harness
            .store()
            .get::<Ticket>(&format!("ticket:{HERE}"))
            .await
            .unwrap()
            .is_some()
    );

    history.reason = "recovered".to_owned();
    harness.store().set(&key, &history).await.unwrap();
    harness.advance(Duration::from_secs(3600));
    let retry = harness.run_due_jobs().await;
    assert_eq!(retry.len(), 1);
    assert!(retry[0].is_ok(), "{retry:?}");
    let closed: TicketHistory = harness.store().get(&key).await.unwrap().unwrap();
    assert_eq!(closed.status, HistoryStatus::Closed);
}

#[test]
fn legacy_staff_snapshot_falls_back_but_explicit_empty_does_not() {
    let config = crate::Config {
        staff_roles: vec![STAFF_ROLE.to_string()],
        ..crate::Config::default()
    };
    let record = json!({
        "number": 3,
        "opener": MEMBER,
        "claimed_by": null,
        "created": 0
    });
    let legacy: Ticket = serde_json::from_value(record.clone()).unwrap();
    let mut explicit_record = record;
    explicit_record["staff_roles"] = json!([]);
    let explicit: Ticket = serde_json::from_value(explicit_record).unwrap();

    assert_eq!(
        crate::history_staff_roles(&legacy, &config),
        vec![STAFF_ROLE]
    );
    assert!(crate::history_staff_roles(&explicit, &config).is_empty());
}

// Keep this panel ACL regression case intact; split only if it grows further.
#[allow(clippy::too_many_lines)]
#[tokio::test]
async fn panel_staff_manage_labels_with_display_names_only_for_their_panel() {
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

    let empty = harness
        .invoke(label_invocation(
            "label-list",
            None,
            OTHER,
            &[PANEL_STAFF_ROLE],
        ))
        .await
        .unwrap();
    assert!(
        empty
            .last()
            .unwrap()
            .content
            .contains("This ticket has no labels"),
        "{empty:?}"
    );

    let added = harness
        .invoke(label_invocation(
            "label-add",
            Some("  Urgent 日本語  "),
            OTHER,
            &[PANEL_STAFF_ROLE],
        ))
        .await
        .unwrap();
    assert!(added.last().unwrap().ephemeral, "{added:?}");
    let listed = harness
        .invoke(label_invocation(
            "label-list",
            None,
            OTHER,
            &[PANEL_STAFF_ROLE],
        ))
        .await
        .unwrap();
    assert!(
        listed.last().unwrap().content.contains("Urgent 日本語"),
        "{listed:?}"
    );

    let denied_add = harness
        .invoke(label_invocation(
            "label-add",
            Some("other-panel"),
            OTHER,
            &[OTHER_PANEL_STAFF_ROLE],
        ))
        .await
        .unwrap();
    assert!(denied_add.last().unwrap().ephemeral, "{denied_add:?}");
    assert!(
        denied_add
            .last()
            .unwrap()
            .content
            .contains("Only the staff can do that"),
        "{denied_add:?}"
    );
    let ticket: Ticket = harness
        .store()
        .get(&format!("ticket:{HERE}"))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(ticket.labels, vec!["Urgent 日本語".to_owned()]);
    let denied_list = harness
        .invoke(label_invocation(
            "label-list",
            None,
            OTHER,
            &[OTHER_PANEL_STAFF_ROLE],
        ))
        .await
        .unwrap();
    assert!(denied_list.last().unwrap().ephemeral, "{denied_list:?}");
    assert!(
        denied_list
            .last()
            .unwrap()
            .content
            .contains("Only the staff can do that"),
        "{denied_list:?}"
    );
    assert!(
        !denied_list
            .last()
            .unwrap()
            .content
            .contains("Urgent 日本語"),
        "{denied_list:?}"
    );

    let removed = harness
        .invoke(label_invocation(
            "label-remove",
            Some("urgent 日本語"),
            OTHER,
            &[PANEL_STAFF_ROLE],
        ))
        .await
        .unwrap();
    assert!(removed.last().unwrap().ephemeral, "{removed:?}");
    let ticket: Ticket = harness
        .store()
        .get(&format!("ticket:{HERE}"))
        .await
        .unwrap()
        .unwrap();
    assert!(ticket.labels.is_empty());
}

#[tokio::test]
async fn ticket_managers_can_manage_labels() {
    let harness = harness();
    seed_ticket(&harness, HERE, MEMBER).await;

    harness
        .invoke(label_invocation(
            "label-add",
            Some("Needs Review"),
            MODERATOR,
            &[MODERATOR],
        ))
        .await
        .unwrap();
    let duplicate = harness
        .invoke(label_invocation(
            "label-add",
            Some(" needs review "),
            MODERATOR,
            &[MODERATOR],
        ))
        .await
        .unwrap();
    assert!(
        duplicate
            .last()
            .unwrap()
            .content
            .contains("already has that label"),
        "{duplicate:?}"
    );
    let listed = harness
        .invoke(label_invocation(
            "label-list",
            None,
            MODERATOR,
            &[MODERATOR],
        ))
        .await
        .unwrap();
    assert!(
        listed.last().unwrap().content.contains("Needs Review"),
        "{listed:?}"
    );
    harness
        .invoke(label_invocation(
            "label-remove",
            Some("needs review"),
            MODERATOR,
            &[MODERATOR],
        ))
        .await
        .unwrap();
    let ticket: Ticket = harness
        .store()
        .get(&format!("ticket:{HERE}"))
        .await
        .unwrap()
        .unwrap();
    assert!(ticket.labels.is_empty());
}

#[tokio::test]
async fn label_list_paginates_within_discord_message_limits() {
    let harness = harness();
    seed_ticket(&harness, HERE, MEMBER).await;
    let key = format!("ticket:{HERE}");
    let mut ticket: Ticket = harness.store().get(&key).await.unwrap().unwrap();
    ticket.labels = (0..40)
        .map(|index| format!("label-{index}-{}", "日".repeat(50)))
        .collect();
    harness.store().set(&key, &ticket).await.unwrap();

    let first = harness
        .invoke(label_page_invocation(1, MODERATOR, &[MODERATOR]))
        .await
        .unwrap();
    let second = harness
        .invoke(label_page_invocation(2, MODERATOR, &[MODERATOR]))
        .await
        .unwrap();
    let first_content = &first.last().unwrap().content;
    let second_content = &second.last().unwrap().content;
    assert!(first_content.contains("page 1 of 2"), "{first:?}");
    assert!(second_content.contains("page 2 of 2"), "{second:?}");
    assert!(first_content.contains("label-0-"), "{first:?}");
    assert!(second_content.contains("label-39-"), "{second:?}");
    assert!(first_content.encode_utf16().count() <= 2_000, "{first:?}");
    assert!(second_content.encode_utf16().count() <= 2_000, "{second:?}");
}

#[tokio::test]
async fn ticket_labels_report_the_store_size_limit_without_partial_write() {
    let harness = harness();
    let key = format!("ticket:{HERE}");
    let ticket_with_labels = |labels: Vec<String>| Ticket {
        number: 3,
        opener: MEMBER,
        claimed_by: None,
        created: 0,
        closing: None,
        close_generation: 0,
        close_request: None,
        panel: None,
        staff_roles: None,
        form_answers: Vec::new(),
        labels,
        settings: None,
        form_answer_count: 0,
        thread: false,
        thread_parent: None,
        closed: false,
        pending_operation: None,
        panel_role_baselines: Vec::new(),
        participant_roles: Vec::new(),
        panel_user_baselines: Vec::new(),
        participant_users: Vec::new(),
        waiting_for_reply: false,
        staff_access: None,
        automation: crate::automation::State::default(),
        message_quote: None,
        message_quote_message_id: None,
    };
    let mut labels = Vec::new();
    for index in 0..MAX_VALUE_BYTES {
        labels.push(format!("{index:04}-{}", "x".repeat(59)));
        if serde_json::to_vec(&ticket_with_labels(labels.clone()))
            .unwrap()
            .len()
            > MAX_VALUE_BYTES
        {
            labels.pop();
            break;
        }
    }
    let ticket = ticket_with_labels(labels);
    assert!(ticket.labels.len() > 20);
    let size = serde_json::to_vec(&ticket).unwrap().len();
    assert!(size <= MAX_VALUE_BYTES);
    let expected_labels = ticket.labels.clone();
    let mut too_large = ticket_with_labels(expected_labels.clone());
    let extra = "z".repeat(64);
    too_large.labels.push(extra.clone());
    assert!(serde_json::to_vec(&too_large).unwrap().len() > MAX_VALUE_BYTES);
    harness.store().set(&key, &ticket).await.unwrap();

    let response = harness
        .invoke(label_invocation(
            "label-add",
            Some(&extra),
            MODERATOR,
            &[MODERATOR],
        ))
        .await
        .unwrap();
    assert!(
        response
            .last()
            .unwrap()
            .content
            .contains("cannot store any more labels"),
        "{response:?}"
    );
    let ticket: Ticket = harness.store().get(&key).await.unwrap().unwrap();
    assert_eq!(ticket.labels, expected_labels);
}

#[tokio::test]
async fn concurrent_label_changes_are_preserved_and_closing_blocks_changes() {
    let harness = harness();
    seed_ticket(&harness, HERE, MEMBER).await;
    let key = format!("ticket:{HERE}");
    let mut ticket: Ticket = harness.store().get(&key).await.unwrap().unwrap();
    ticket.labels.push("Urgent".to_owned());
    harness.store().set(&key, &ticket).await.unwrap();

    let (added, removed) = tokio::join!(
        harness.invoke(label_invocation(
            "label-add",
            Some("Needs Review"),
            MODERATOR,
            &[MODERATOR],
        )),
        harness.invoke(label_invocation(
            "label-remove",
            Some("urgent"),
            MODERATOR,
            &[MODERATOR],
        ))
    );
    assert!(added.is_ok(), "{added:?}");
    assert!(removed.is_ok(), "{removed:?}");
    let mut ticket: Ticket = harness.store().get(&key).await.unwrap().unwrap();
    assert_eq!(ticket.labels, vec!["Needs Review".to_owned()]);

    ticket.closing = Some(1);
    harness.store().set(&key, &ticket).await.unwrap();
    let blocked = harness
        .invoke(label_invocation(
            "label-add",
            Some("after-close"),
            MODERATOR,
            &[MODERATOR],
        ))
        .await
        .unwrap();
    assert!(
        blocked.last().is_some_and(|reply| {
            reply.ephemeral && reply.content.to_lowercase().contains("closing")
        }),
        "{blocked:?}"
    );
    let ticket: Ticket = harness.store().get(&key).await.unwrap().unwrap();
    assert_eq!(ticket.labels, vec!["Needs Review".to_owned()]);
}

#[test]
fn legacy_tickets_default_to_no_labels_and_display_names_are_validated() {
    let ticket: Ticket = serde_json::from_value(json!({
        "number": 3,
        "opener": MEMBER,
        "claimed_by": null,
        "created": 0
    }))
    .unwrap();
    assert!(ticket.labels.is_empty());

    assert_eq!(
        display_label("  Urgent 日本語  "),
        Some("Urgent 日本語".to_owned())
    );
    assert_eq!(label_key("Urgent 日本語"), label_key(" urgent 日本語 "));
    assert_eq!(display_label(""), None);
    assert_eq!(display_label(&"日".repeat(65)), None);
    assert_eq!(display_label("line\nbreak"), None);
}

#[test]
fn label_pages_fit_the_discord_budget_and_break_mentions() {
    let mut labels: Vec<String> = (0..40)
        .map(|index| format!("label-{index}-{}", "😀".repeat(40)))
        .collect();
    labels.push("@everyone".to_owned());

    let pages = label_pages(&labels);
    assert!(pages.len() > 1);
    assert!(
        pages
            .iter()
            .all(|page| page.encode_utf16().count() <= LABEL_LIST_UTF16_UNITS)
    );
    assert!(pages.iter().any(|page| page.contains("@\u{200b}everyone")));
}

#[tokio::test]
async fn ticket_labels_are_saved_to_history_and_searchable() {
    let harness = harness();
    seed_ticket(&harness, HERE, MEMBER).await;
    harness
        .invoke(label_invocation(
            "label-add",
            Some("Urgent"),
            MODERATOR,
            &[MODERATOR],
        ))
        .await
        .unwrap();

    harness.click(press("close", MEMBER)).await.unwrap();

    let history: TicketHistory = harness
        .store()
        .get(&history_key(3, Id::new(HERE)))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(history.labels, ["Urgent"]);
    let results = harness
        .invoke(search_history_invocation("urgent", true))
        .await
        .unwrap();
    assert!(results.last().unwrap().ephemeral);
    assert!(results.last().unwrap().content.contains("#3"));
}

#[tokio::test]
async fn an_unanswered_close_request_closes_at_its_deadline_after_restart() {
    let harness = harness();
    with_close_request_timeout(&harness, 1);
    seed_ticket(&harness, HERE, MEMBER).await;

    let sent = harness
        .invoke(close_request_command(OTHER, Some("The issue is resolved")))
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
    let deadline = ticket.close_request.as_ref().unwrap().expires_at.unwrap();
    assert!(deadline > 0);
    assert_eq!(harness.jobs(), 1);
    let message = harness
        .actions()
        .into_iter()
        .find_map(|action| match action {
            Action::PostMessage {
                channel_id,
                content: Some(content),
                ..
            } if channel_id.get() == HERE => Some(content),
            _ => None,
        })
        .unwrap();
    assert!(message.contains("Reply within 60 min"), "{message}");

    harness
        .invoke(close_request_command(OTHER, Some("The issue is resolved")))
        .await
        .unwrap();
    assert_eq!(harness.jobs(), 1, "resending replaces the same timeout job");
    let ticket: Ticket = harness.store().get(&key).await.unwrap().unwrap();
    assert_eq!(
        ticket.close_request.as_ref().unwrap().expires_at,
        Some(deadline)
    );

    let restarted = harness.sibling(builder().unwrap()).unwrap();
    with_close_request_timeout(&restarted, 1);
    restarted.advance(Duration::from_secs(3_599));
    restarted.run_due_jobs().await;
    assert!(deleted(&restarted).is_empty());
    restarted.advance(Duration::from_secs(1));
    let results = restarted.run_due_jobs().await;
    assert!(results.iter().all(Result::is_ok), "{results:?}");
    assert_eq!(deleted(&restarted), [HERE]);
    assert_eq!(transcripts(&restarted).len(), 1);
    let ticket: Option<Ticket> = restarted.store().get(&key).await.unwrap();
    assert!(ticket.is_none());
}

#[tokio::test]
async fn rejecting_a_request_cancels_its_timeout_and_stale_jobs_cannot_close_a_new_request() {
    let harness = harness();
    with_close_request_timeout(&harness, 1);
    seed_ticket(&harness, HERE, MEMBER).await;

    harness
        .invoke(close_request_command(OTHER, Some("The first request")))
        .await
        .unwrap();
    let key = format!("ticket:{HERE}");
    let ticket: Ticket = harness.store().get(&key).await.unwrap().unwrap();
    let first = ticket.close_request.as_ref().unwrap().clone();
    let first_deadline = first.expires_at.unwrap();
    assert_eq!(harness.jobs(), 1);

    harness.advance(Duration::from_mins(30));
    let rejected = harness
        .click(request_click("close-request-reject", first.id, MEMBER))
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
    assert_eq!(harness.jobs(), 0, "rejecting cancels the waiting timeout");

    harness
        .invoke(close_request_command(OTHER, Some("The second request")))
        .await
        .unwrap();
    let ticket: Ticket = harness.store().get(&key).await.unwrap().unwrap();
    let second = ticket.close_request.as_ref().unwrap().clone();
    assert_ne!(second.id, first.id);
    assert_eq!(harness.jobs(), 1);

    let stale = CloseRequestTimeout {
        channel: HERE,
        request_id: first.id,
        expires_at: first_deadline,
    };
    let scheduler = harness.scheduler();
    scheduler
        .job(CLOSE_REQUEST_TIMEOUT, &stale)
        .unwrap()
        .key(&close_request_timeout_key(ChannelId::new(HERE), first.id))
        .at(first_deadline)
        .await
        .unwrap();
    assert_eq!(harness.jobs(), 2);

    harness.advance(Duration::from_mins(30));
    let stale_job = harness.run_due_jobs().await;
    assert!(stale_job.iter().all(Result::is_ok), "{stale_job:?}");
    assert!(deleted(&harness).is_empty());
    let ticket: Ticket = harness.store().get(&key).await.unwrap().unwrap();
    assert_eq!(ticket.close_request.as_ref().unwrap().id, second.id);
    assert_eq!(harness.jobs(), 1);

    harness.advance(Duration::from_mins(30));
    let second_job = harness.run_due_jobs().await;
    assert!(second_job.iter().all(Result::is_ok), "{second_job:?}");
    assert_eq!(deleted(&harness), [HERE]);
}

#[tokio::test]
async fn a_timeout_and_concurrent_approval_close_the_ticket_once() {
    let harness = harness();
    with_close_request_timeout(&harness, 1);
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
    let request_id = ticket.close_request.as_ref().unwrap().id;

    harness.advance(Duration::from_secs(3_600));
    let (jobs, approval) = tokio::join!(
        harness.run_due_jobs(),
        harness.click(request_click("close-request-approve", request_id, MEMBER))
    );
    assert!(jobs.iter().all(Result::is_ok), "{jobs:?}");
    assert!(approval.is_ok(), "{approval:?}");
    assert_eq!(deleted(&harness), [HERE]);
    assert_eq!(transcripts(&harness).len(), 1);
    let ticket: Option<Ticket> = harness
        .store()
        .get(&format!("ticket:{HERE}"))
        .await
        .unwrap();
    assert!(ticket.is_none());

    harness.advance(Duration::from_secs(600));
    let stale_job = harness.run_due_jobs().await;
    assert!(stale_job.iter().all(Result::is_ok), "{stale_job:?}");
    assert_eq!(deleted(&harness), [HERE]);
    assert_eq!(transcripts(&harness).len(), 1);
    assert_eq!(harness.jobs(), 0);
}

#[tokio::test]
async fn a_late_rejection_cannot_cancel_an_expired_close_request() {
    let harness = harness();
    with_close_request_timeout(&harness, 1);
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
    let request_id = ticket.close_request.as_ref().unwrap().id;

    harness.advance(Duration::from_secs(3_600));
    let rejected = harness
        .click(request_click("close-request-reject", request_id, MEMBER))
        .await
        .unwrap();
    assert!(
        rejected
            .last()
            .unwrap()
            .content
            .contains("expired and the ticket is closing"),
        "{rejected:?}"
    );
    assert_eq!(deleted(&harness), [HERE]);
    assert_eq!(transcripts(&harness).len(), 1);
    assert_eq!(harness.jobs(), 0);
    harness.run_due_jobs().await;
    assert_eq!(deleted(&harness), [HERE]);
}

#[tokio::test]
async fn zero_disables_close_request_timeouts() {
    let harness = harness();
    with_close_request_timeout(&harness, 0);
    seed_ticket(&harness, HERE, MEMBER).await;
    harness
        .invoke(close_request_command(OTHER, Some("The issue is resolved")))
        .await
        .unwrap();
    let key = format!("ticket:{HERE}");
    let ticket: Ticket = harness.store().get(&key).await.unwrap().unwrap();
    let request_id = ticket.close_request.as_ref().unwrap().id;
    assert!(ticket.close_request.as_ref().unwrap().expires_at.is_none());
    assert_eq!(harness.jobs(), 0);

    harness.advance(Duration::from_hours(48));
    harness.run_due_jobs().await;
    assert!(deleted(&harness).is_empty());
    let rejected = harness
        .click(request_click("close-request-reject", request_id, MEMBER))
        .await
        .unwrap();
    assert!(
        rejected
            .last()
            .unwrap()
            .content
            .contains("kept the ticket open")
    );
    let ticket: Ticket = harness.store().get(&key).await.unwrap().unwrap();
    assert!(ticket.close_request.is_none());
    assert_eq!(harness.jobs(), 0);
}

#[tokio::test]
async fn an_invalid_close_request_timeout_does_not_create_a_request_or_job() {
    let harness = harness();
    seed_ticket(&harness, HERE, MEMBER).await;

    for timeout in [json!(-1), json!(u32::MAX)] {
        harness.set_config(json!({
            "staff_roles": [STAFF_ROLE.to_string()],
            "log_channel": LOG.to_string(),
            "max_open": 1,
            "close_request_timeout_hours": timeout
        }));
        let result = harness
            .invoke(close_request_command(OTHER, Some("The issue is resolved")))
            .await;
        if let Ok(replies) = result {
            assert!(
                replies
                    .last()
                    .unwrap()
                    .content
                    .contains("Check the ticket settings"),
                "{replies:?}"
            );
        }
        let ticket: Ticket = harness
            .store()
            .get(&format!("ticket:{HERE}"))
            .await
            .unwrap()
            .unwrap();
        assert!(ticket.close_request.is_none());
        assert_eq!(harness.jobs(), 0);
    }
}

#[test]
fn old_configs_and_close_requests_default_to_no_timeout() {
    let config: Config = serde_json::from_value(json!({
        "category": null,
        "staff_roles": [STAFF_ROLE.to_string()],
        "log_channel": LOG.to_string(),
        "max_open": 1,
        "close_after_days": 0,
        "panels": []
    }))
    .unwrap();
    assert_eq!(config.close_request_timeout_hours, 0);
    assert_eq!(
        close_request_expiry(100, 0),
        Some(CloseRequestExpiry::Disabled)
    );
    assert_eq!(
        close_request_expiry(100, 1),
        Some(CloseRequestExpiry::At(3_700))
    );
    assert_eq!(close_request_expiry(100, u32::MAX), None);
    let max_hours = u32::try_from((MAX_CLOSE_REQUEST_DEADLINE_UNIX_SECONDS - 100) / 3_600).unwrap();
    assert!(matches!(
        close_request_expiry(100, max_hours),
        Some(CloseRequestExpiry::At(_))
    ));
    assert_eq!(close_request_expiry(100, max_hours + 1), None);
    assert_eq!(close_request_expiry(i64::MAX - 3_599, 1), None);
    let mut enabled = config.clone();
    enabled.close_request_timeout_hours = 24;
    assert_eq!(
        enabled
            .with_panel(&NamedPanel::default())
            .close_request_timeout_hours,
        24
    );

    let ticket: Ticket = serde_json::from_value(json!({
        "number": 3,
        "opener": MEMBER,
        "claimed_by": null,
        "created": 0,
        "close_request": {
            "id": 9,
            "requested_by": OTHER,
            "reason": null
        }
    }))
    .unwrap();
    assert!(ticket.close_request.unwrap().expires_at.is_none());
}

#[tokio::test]
async fn pending_close_request_ticket_survives_refreshes_until_a_long_deadline() {
    let harness = harness();
    with_close_request_timeout(&harness, 24 * 365 * 4);
    seed_ticket(&harness, HERE, MEMBER).await;

    harness
        .invoke(close_request_command(OTHER, Some("The issue is resolved")))
        .await
        .unwrap();
    let key = format!("ticket:{HERE}");
    let ticket: Ticket = harness.store().get(&key).await.unwrap().unwrap();
    let deadline = ticket.close_request.as_ref().unwrap().expires_at.unwrap();

    harness.advance(Duration::from_hours(24 * 365 * 2 + 48));
    let before_refresh: Option<Ticket> = harness.store().get(&key).await.unwrap();
    assert!(
        before_refresh.is_some(),
        "the request must survive past the original one-year TTL"
    );
    harness
        .click(press("claim", OTHER).roles([STAFF_ROLE]))
        .await
        .unwrap();
    let refreshed: Ticket = harness.store().get(&key).await.unwrap().unwrap();
    assert_eq!(refreshed.claimed_by, Some(OTHER));
    assert_eq!(
        refreshed.close_request.as_ref().unwrap().expires_at,
        Some(deadline)
    );

    harness.advance(Duration::from_hours(24 * 365 * 2 - 72));
    assert!(deleted(&harness).is_empty());
    let before_deadline: Option<Ticket> = harness.store().get(&key).await.unwrap();
    assert!(
        before_deadline.is_some(),
        "a later Ticket write must retain it through the deadline"
    );
    harness.advance(Duration::from_hours(24));
    let results = harness.run_due_jobs().await;
    assert!(results.iter().all(Result::is_ok), "{results:?}");
    assert_eq!(deleted(&harness), [HERE]);
}

#[tokio::test]
async fn rejection_cancels_its_timeout_and_a_stale_job_cannot_close_a_new_request() {
    let harness = harness();
    with_close_request_timeout(&harness, 1);
    seed_ticket(&harness, HERE, MEMBER).await;

    harness
        .invoke(close_request_command(OTHER, Some("The first request")))
        .await
        .unwrap();
    let key = format!("ticket:{HERE}");
    let ticket: Ticket = harness.store().get(&key).await.unwrap().unwrap();
    let first = ticket.close_request.as_ref().unwrap().clone();
    let first_deadline = first.expires_at.unwrap();
    assert_eq!(harness.jobs(), 1);

    harness.advance(Duration::from_mins(30));
    let rejected = harness
        .click(request_click("close-request-reject", first.id, MEMBER))
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
    assert_eq!(harness.jobs(), 0, "rejecting cancels the waiting timeout");

    harness
        .invoke(close_request_command(OTHER, Some("The second request")))
        .await
        .unwrap();
    let ticket: Ticket = harness.store().get(&key).await.unwrap().unwrap();
    let second = ticket.close_request.as_ref().unwrap().clone();
    assert_ne!(second.id, first.id);
    assert_eq!(harness.jobs(), 1);

    harness
        .scheduler()
        .job(
            CLOSE_REQUEST_TIMEOUT,
            &CloseRequestTimeout {
                channel: HERE,
                request_id: first.id,
                expires_at: first_deadline,
            },
        )
        .unwrap()
        .key(&close_request_timeout_key(ChannelId::new(HERE), first.id))
        .at(first_deadline)
        .await
        .unwrap();
    assert_eq!(harness.jobs(), 2);

    harness.advance(Duration::from_mins(30));
    let stale_job = harness.run_due_jobs().await;
    assert!(stale_job.iter().all(Result::is_ok), "{stale_job:?}");
    assert!(deleted(&harness).is_empty());
    let ticket: Ticket = harness.store().get(&key).await.unwrap().unwrap();
    assert_eq!(ticket.close_request.as_ref().unwrap().id, second.id);
    assert_eq!(harness.jobs(), 1);

    harness.advance(Duration::from_mins(30));
    let second_job = harness.run_due_jobs().await;
    assert!(second_job.iter().all(Result::is_ok), "{second_job:?}");
    assert_eq!(deleted(&harness), [HERE]);
    assert_eq!(harness.jobs(), 0);
}

// shortcut: keep this 144-line regression case intact; split if it grows further.
#[allow(clippy::too_many_lines)]
#[tokio::test]
async fn saved_replies_use_the_ticket_panels_staff_access_and_safe_mentions() {
    let harness = harness();
    harness.set_config(json!({
        "staff_roles": [STAFF_ROLE.to_string()],
        "panels": [{
            "id": "support",
            "staff_roles": [PANEL_STAFF_ROLE.to_string()]
        }]
    }));
    seed_ticket(&harness, HERE, MEMBER).await;
    let ticket_key = format!("ticket:{HERE}");
    let mut ticket: Ticket = harness.store().get(&ticket_key).await.unwrap().unwrap();
    ticket.panel = Some(NamedPanel {
        id: "support".to_owned(),
        category: None,
        thread_channel: None,
        staff_roles: vec![PANEL_STAFF_ROLE.to_string()],
        log_channel: None,
        close_after_days: 0,
        max_open: 0,
        name: None,
        description: None,
        button_style: None,
        timezone: None,
        questions: Vec::new(),
        ..NamedPanel::default()
    });
    harness.store().set(&ticket_key, &ticket).await.unwrap();
    let default_ticket = Ticket {
        number: 3,
        opener: MEMBER,
        claimed_by: None,
        created: 0,
        closing: None,
        close_request: None,
        panel: None,
        close_generation: 0,
        staff_roles: None,
        form_answers: Vec::new(),
        labels: Vec::new(),
        settings: None,
        form_answer_count: 0,
        thread: false,
        thread_parent: None,
        closed: false,
        pending_operation: None,
        panel_role_baselines: Vec::new(),
        participant_roles: Vec::new(),
        panel_user_baselines: Vec::new(),
        participant_users: Vec::new(),
        waiting_for_reply: false,
        staff_access: None,
        automation: crate::automation::State::default(),
        message_quote: None,
        message_quote_message_id: None,
    };
    assert_ne!(
        reply_key(&ticket, "welcome"),
        reply_key(&default_ticket, "welcome")
    );

    let body = "@everyone <@7> please try signing in again.";
    let added = harness
        .invoke(reply_add_invocation(
            "welcome",
            "Welcome",
            body,
            OTHER,
            &[PANEL_STAFF_ROLE],
        ))
        .await
        .unwrap();
    assert!(added.last().unwrap().ephemeral, "{added:?}");

    let search = harness
        .invoke(reply_search_invocation(
            "wel",
            None,
            OTHER,
            &[PANEL_STAFF_ROLE],
        ))
        .await
        .unwrap();
    assert!(search.last().unwrap().ephemeral, "{search:?}");
    assert_eq!(
        search.last().unwrap().custom_ids(),
        ["tickets:reply-select:*:"]
    );
    assert!(!search.last().unwrap().content.contains(body));

    let denied = harness
        .invoke(reply_search_invocation(
            "welcome",
            None,
            OTHER,
            &[OTHER_PANEL_STAFF_ROLE],
        ))
        .await
        .unwrap();
    assert!(
        denied
            .last()
            .unwrap()
            .content
            .contains("Only the staff can do that"),
        "{denied:?}"
    );

    let denied_pick = harness
        .click(
            Click::new("tickets:reply-select:*:")
                .values(["welcome"])
                .roles([OTHER_PANEL_STAFF_ROLE])
                .user(OTHER),
        )
        .await
        .unwrap();
    assert!(
        denied_pick
            .last()
            .unwrap()
            .content
            .contains("Only the staff can do that"),
        "{denied_pick:?}"
    );
    assert!(ticket_messages(&harness).is_empty());

    harness
        .click(
            Click::new("tickets:reply-select:*:")
                .values(["welcome"])
                .roles([PANEL_STAFF_ROLE])
                .user(OTHER),
        )
        .await
        .unwrap();
    let sent: Vec<_> = harness
        .actions()
        .into_iter()
        .filter_map(|action| match action {
            Action::SendMessage {
                channel_id,
                content: Some(content),
                mentions,
                ..
            } if channel_id.get() == HERE => Some((content, mentions)),
            _ => None,
        })
        .collect();
    assert_eq!(sent, [(body.to_owned(), MentionPolicy::None)]);
}

#[tokio::test]
async fn long_saved_replies_are_split_and_never_ping_mentions() {
    let harness = harness();
    seed_ticket(&harness, HERE, MEMBER).await;
    let body = format!(
        "{}{}@everyone <@{MEMBER}>",
        "x".repeat(1799),
        "💬".repeat(200)
    );
    harness
        .invoke(reply_add_invocation(
            "long",
            "Long answer",
            &body,
            OTHER,
            &[STAFF_ROLE],
        ))
        .await
        .unwrap();
    harness
        .invoke(reply_search_invocation("long", None, OTHER, &[STAFF_ROLE]))
        .await
        .unwrap();
    harness
        .click(
            Click::new("tickets:reply-select:*:")
                .values(["long"])
                .roles([STAFF_ROLE])
                .user(OTHER),
        )
        .await
        .unwrap();

    let sent: Vec<_> = harness
        .actions()
        .into_iter()
        .filter_map(|action| match action {
            Action::SendMessage {
                channel_id,
                content: Some(content),
                mentions,
                ..
            } if channel_id.get() == HERE => Some((content, mentions)),
            _ => None,
        })
        .collect();
    assert!(sent.len() > 1, "{sent:?}");
    assert_eq!(
        sent.iter()
            .map(|(content, _)| content.as_str())
            .collect::<Vec<_>>()
            .join(""),
        body
    );
    assert!(sent.iter().all(|(content, mentions)| {
        content.encode_utf16().count() <= REPLY_CHUNK_MAX_UTF16 && *mentions == MentionPolicy::None
    }));
}

#[tokio::test]
async fn concurrent_saved_reply_edits_keep_a_complete_revision() {
    let harness = harness();
    seed_ticket(&harness, HERE, MEMBER).await;
    harness
        .invoke(reply_add_invocation(
            "faq",
            "FAQ",
            "Original answer",
            OTHER,
            &[STAFF_ROLE],
        ))
        .await
        .unwrap();

    let (first, second) = tokio::join!(
        harness.invoke(reply_edit_invocation(
            "faq",
            "FAQ",
            "First complete answer",
            OTHER,
            &[STAFF_ROLE],
        )),
        harness.invoke(reply_edit_invocation(
            "faq",
            "FAQ",
            "Second complete answer",
            MEMBER,
            &[STAFF_ROLE],
        ))
    );
    for result in [first.unwrap(), second.unwrap()] {
        let response = &result.last().unwrap().content;
        assert!(
            response.contains("Saved reply updated")
                || response.contains("This reply changed at the same time"),
            "{result:?}"
        );
    }

    let ticket: Ticket = harness
        .store()
        .get(&format!("ticket:{HERE}"))
        .await
        .unwrap()
        .unwrap();
    let saved: SavedReply = harness
        .store()
        .get(&reply_key(&ticket, "faq"))
        .await
        .unwrap()
        .unwrap();
    assert!(
        saved.body == "First complete answer" || saved.body == "Second complete answer",
        "{saved:?}"
    );
}

#[tokio::test]
async fn a_deleted_saved_reply_can_be_recreated_without_its_old_body() {
    let harness = harness();
    seed_ticket(&harness, HERE, MEMBER).await;
    harness
        .invoke(reply_add_invocation(
            "policy",
            "Policy",
            "Old answer",
            OTHER,
            &[STAFF_ROLE],
        ))
        .await
        .unwrap();
    let deleted = harness
        .invoke(reply_delete_invocation("policy", OTHER, &[STAFF_ROLE]))
        .await
        .unwrap();
    assert!(
        deleted
            .last()
            .unwrap()
            .content
            .contains("Saved reply removed"),
        "{deleted:?}"
    );

    harness
        .invoke(reply_add_invocation(
            "policy",
            "Policy",
            "New answer",
            OTHER,
            &[STAFF_ROLE],
        ))
        .await
        .unwrap();
    let ticket: Ticket = harness
        .store()
        .get(&format!("ticket:{HERE}"))
        .await
        .unwrap()
        .unwrap();
    let saved: SavedReply = harness
        .store()
        .get(&reply_key(&ticket, "policy"))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(saved.body, "New answer");
    assert!(!saved.deleted);
}

#[tokio::test]
async fn saved_reply_search_reaches_entries_after_the_first_storage_page() {
    let harness = harness();
    seed_ticket(&harness, HERE, MEMBER).await;
    let ticket: Ticket = harness
        .store()
        .get(&format!("ticket:{HERE}"))
        .await
        .unwrap()
        .unwrap();
    for number in 0..1_005 {
        let reply = SavedReply {
            id: format!("entry-{number:04}"),
            title: format!("Entry {number:04}"),
            body: format!("Body {number:04}"),
            deleted: false,
        };
        harness
            .store()
            .set(&reply_key(&ticket, &reply.id), &reply)
            .await
            .unwrap();
    }

    let results = harness
        .invoke(reply_search_invocation(
            "entry",
            Some(41),
            OTHER,
            &[STAFF_ROLE],
        ))
        .await
        .unwrap();
    assert_eq!(
        results.last().unwrap().custom_ids(),
        ["tickets:reply-select:*:"]
    );
    harness
        .click(
            Click::new("tickets:reply-select:*:")
                .values(["entry-1004"])
                .roles([STAFF_ROLE])
                .user(OTHER),
        )
        .await
        .unwrap();
    assert_eq!(ticket_messages(&harness), ["Body 1004".to_owned()]);
}

#[tokio::test]
async fn retrying_a_partial_reply_skips_confirmed_parts_and_flags_the_uncertain_one() {
    let harness = harness();
    seed_ticket(&harness, HERE, MEMBER).await;
    let first = "a".repeat(2_000);
    let second = "b".repeat(2_000);
    let third = "c".repeat(1_500);
    let body = format!("{first}{second}{third}");
    harness
        .invoke(reply_add_invocation(
            "long",
            "Long reply",
            &body,
            OTHER,
            &[STAFF_ROLE],
        ))
        .await
        .unwrap();
    harness
        .invoke(reply_search_invocation("long", None, OTHER, &[STAFF_ROLE]))
        .await
        .unwrap();

    // defer, progress edit, and part 1 precede the intended part 2 failure.
    harness.api().fail_nth(
        4,
        closure_core::error::DiscordError::Fatal("connection lost".into()),
    );
    let stopped = harness
        .click(
            Click::new("tickets:reply-select:*:")
                .values(["long"])
                .roles([STAFF_ROLE])
                .user(OTHER),
        )
        .await
        .unwrap();
    let status = &stopped.last().unwrap().content;
    assert!(status.contains("1 of 3 parts are confirmed"), "{stopped:?}");
    assert!(status.contains("may have arrived"), "{stopped:?}");
    assert_eq!(ticket_messages(&harness), std::slice::from_ref(&first));

    let attempt_id = harness
        .store()
        .counter_total(REPLY_SEND_COUNTER, OTHER)
        .await
        .unwrap();
    let ticket: Ticket = harness
        .store()
        .get(&format!("ticket:{HERE}"))
        .await
        .unwrap()
        .unwrap();
    let key = reply_send_attempt_key(ChannelId::new(HERE), OTHER, attempt_id);
    let attempt: ReplySendAttempt = harness.store().get(&key).await.unwrap().unwrap();
    assert_eq!(attempt.confirmed, 1);
    assert_eq!(attempt.in_flight, Some(1));

    let uncertain = harness
        .click(
            Click::new(format!("tickets:reply-retry:*:{attempt_id}"))
                .roles([STAFF_ROLE])
                .user(OTHER),
        )
        .await
        .unwrap();
    assert!(
        uncertain
            .last()
            .unwrap()
            .custom_ids()
            .iter()
            .any(|id| id.contains("reply-retry-uncertain")),
        "{uncertain:?}"
    );
    assert_eq!(ticket_messages(&harness), std::slice::from_ref(&first));

    let resumed = harness
        .click(
            Click::new(format!("tickets:reply-retry-uncertain:*:{attempt_id}"))
                .roles([STAFF_ROLE])
                .user(OTHER),
        )
        .await
        .unwrap();
    assert!(
        resumed
            .last()
            .unwrap()
            .content
            .contains("Posted reply long in 3 message parts"),
        "{resumed:?}"
    );
    assert_eq!(ticket_messages(&harness), [first, second, third]);
    let saved_attempt: ReplySendAttempt = harness.store().get(&key).await.unwrap().unwrap();
    assert_eq!(saved_attempt.confirmed, 3);
    assert_eq!(saved_attempt.in_flight, None);
    assert_eq!(ticket.number, saved_attempt.ticket_number);
}

#[tokio::test]
async fn selecting_a_saved_reply_again_creates_a_new_send_attempt() {
    let harness = harness();
    seed_ticket(&harness, HERE, MEMBER).await;
    harness
        .invoke(reply_add_invocation(
            "greeting",
            "Greeting",
            "Hello there.",
            OTHER,
            &[STAFF_ROLE],
        ))
        .await
        .unwrap();

    harness
        .invoke(reply_search_invocation(
            "greeting",
            None,
            OTHER,
            &[STAFF_ROLE],
        ))
        .await
        .unwrap();
    harness
        .click(
            Click::new("tickets:reply-select:*:")
                .values(["greeting"])
                .roles([STAFF_ROLE])
                .user(OTHER),
        )
        .await
        .unwrap();
    let first_attempt = harness
        .store()
        .counter_total(REPLY_SEND_COUNTER, OTHER)
        .await
        .unwrap();
    assert_eq!(first_attempt, 1);

    let completed_retry = harness
        .click(
            Click::new(format!("tickets:reply-retry:*:{first_attempt}"))
                .roles([STAFF_ROLE])
                .user(OTHER),
        )
        .await
        .unwrap();
    assert!(
        completed_retry
            .last()
            .unwrap()
            .content
            .contains("already complete"),
        "{completed_retry:?}"
    );
    assert_eq!(ticket_messages(&harness), ["Hello there.".to_owned()]);

    harness
        .invoke(reply_search_invocation(
            "greeting",
            None,
            OTHER,
            &[STAFF_ROLE],
        ))
        .await
        .unwrap();
    harness
        .click(
            Click::new("tickets:reply-select:*:")
                .values(["greeting"])
                .roles([STAFF_ROLE])
                .user(OTHER),
        )
        .await
        .unwrap();
    let second_attempt = harness
        .store()
        .counter_total(REPLY_SEND_COUNTER, OTHER)
        .await
        .unwrap();
    assert_eq!(second_attempt, 2);
    assert_eq!(
        ticket_messages(&harness),
        ["Hello there.".to_owned(), "Hello there.".to_owned()]
    );
}

#[test]
fn rating_button_ids_fit_the_custom_id_limit_at_maximum_ids() {
    let module = closure_core::module::ModuleId::new("tickets").unwrap();
    let owner = closure_core::event::UserId::new(u64::MAX);
    let custom_id = rating_button_id(
        &module,
        owner,
        ChannelId::new(u64::MAX),
        i64::MAX,
        i64::MAX,
        5,
    );
    assert!(
        custom_id.len() <= 100,
        "{} bytes: {custom_id}",
        custom_id.len()
    );

    let (_, parsed) = CustomId::parse(&custom_id).unwrap();
    assert_eq!(parsed.owner, Some(owner));
    assert_eq!(parsed.route, "rate");
    assert_eq!(
        parse_rating_request(&parsed.data),
        Some(RatingRequest {
            channel: u64::MAX,
            number: i64::MAX,
            attempt: i64::MAX,
            score: 5,
        })
    );
    assert!(parse_rating_request("0:1:1:1").is_none());
    assert!(parse_rating_request("1:1:0:1").is_none());
    assert!(parse_rating_request("1:1:1:6").is_none());
    assert!(parse_rating_request("1:1:1:5:extra").is_none());
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
async fn only_the_opener_can_rate_and_old_buttons_are_fenced() {
    let harness = harness();
    seed_ticket(&harness, HERE, MEMBER).await;
    harness.click(press("close", MEMBER)).await.unwrap();
    let history = closed_history(&harness, HERE).await;
    let old_button = closed_rating_id(&history, HERE, 5);
    let (rating_message_id, rating_channel_id) = rating_dm_message(&harness);

    let _ = harness
        .click_direct_message(
            rating_message_id,
            rating_channel_id,
            Click::new(old_button.clone()).user(OTHER),
        )
        .await;
    assert!(closed_history(&harness, HERE).await.rating.is_none());

    let pointer: String = harness
        .store()
        .get(&history_id_key(ChannelId::new(HERE)))
        .await
        .unwrap()
        .unwrap();
    let mut newer = closed_history(&harness, HERE).await;
    newer.attempt += 1;
    harness.store().set(&pointer, &newer).await.unwrap();
    let stale = harness
        .click_direct_message(
            rating_message_id,
            rating_channel_id,
            Click::new(old_button).user(MEMBER),
        )
        .await
        .unwrap();
    assert!(
        stale
            .last()
            .unwrap()
            .content
            .contains("no longer available"),
        "{stale:?}"
    );
    assert!(closed_history(&harness, HERE).await.rating.is_none());
    let outsider = harness.invoke(rate_invocation(3, 4, OTHER)).await.unwrap();
    assert!(
        outsider
            .last()
            .unwrap()
            .content
            .contains("no longer available"),
        "{outsider:?}"
    );
    assert!(closed_history(&harness, HERE).await.rating.is_none());
    let invalid = harness.invoke(rate_invocation(3, 6, MEMBER)).await.unwrap();
    assert!(invalid.last().unwrap().content.contains("1 to 5"));
    assert!(closed_history(&harness, HERE).await.rating.is_none());

    let saved = harness.invoke(rate_invocation(3, 4, MEMBER)).await.unwrap();
    assert!(
        saved.last().unwrap().content.contains("was saved"),
        "{saved:?}"
    );
    assert_eq!(
        closed_history(&harness, HERE).await.rating.unwrap().score,
        4
    );
    let duplicate = harness.invoke(rate_invocation(3, 2, MEMBER)).await.unwrap();
    assert!(
        duplicate
            .last()
            .unwrap()
            .content
            .contains("already been rated")
    );
    assert_eq!(
        closed_history(&harness, HERE).await.rating.unwrap().score,
        4
    );
}

#[tokio::test]
async fn concurrent_rating_buttons_save_only_one_score() {
    let harness = harness();
    seed_ticket(&harness, HERE, MEMBER).await;
    harness.click(press("close", MEMBER)).await.unwrap();
    let history = closed_history(&harness, HERE).await;
    let lower = closed_rating_id(&history, HERE, 2);
    let higher = closed_rating_id(&history, HERE, 5);
    let (rating_message_id, rating_channel_id) = rating_dm_message(&harness);

    let (left, right) = tokio::join!(
        harness.click_direct_message(
            rating_message_id,
            rating_channel_id,
            Click::new(lower).user(MEMBER),
        ),
        harness.click_direct_message(
            rating_message_id,
            rating_channel_id,
            Click::new(higher).user(MEMBER),
        ),
    );
    let left = left.unwrap();
    let right = right.unwrap();
    let contents = [
        left.last().unwrap().content.as_str(),
        right.last().unwrap().content.as_str(),
    ];
    assert_eq!(
        contents
            .iter()
            .filter(|text| text.contains("was saved"))
            .count(),
        1
    );
    assert_eq!(
        contents
            .iter()
            .filter(|text| text.contains("already been rated"))
            .count(),
        1
    );
    assert!(matches!(
        closed_history(&harness, HERE).await.rating.unwrap().score,
        2 | 5
    ));
}

#[tokio::test]
async fn a_failed_rating_dm_keeps_the_guild_command_fallback() {
    let harness = harness();
    harness.set_config(json!({
        "staff_roles": [STAFF_ROLE.to_string()],
        "max_open": 1
    }));
    seed_ticket(&harness, HERE, MEMBER).await;
    harness.api().fail_nth(
        4,
        closure_core::error::DiscordError::Forbidden("DMs are disabled".into()),
    );

    let closed = harness.click(press("close", MEMBER)).await.unwrap();
    assert!(
        closed.last().unwrap().content.contains("couldn't confirm"),
        "{closed:?}"
    );
    assert_eq!(deleted(&harness), [HERE]);
    let pending = closed_history(&harness, HERE).await;
    assert!(pending.rating.is_none());
    assert!(pending.rating_prompt_attempted);
    let saved = harness.invoke(rate_invocation(3, 5, MEMBER)).await.unwrap();
    assert!(
        saved.last().unwrap().content.contains("was saved"),
        "{saved:?}"
    );
    assert_eq!(
        closed_history(&harness, HERE).await.rating.unwrap().score,
        5
    );
}

#[tokio::test]
async fn a_closed_ticket_retry_does_not_send_the_rating_dm_twice() {
    let harness = harness();
    seed_ticket(&harness, HERE, MEMBER).await;
    harness.click(press("close", MEMBER)).await.unwrap();
    assert_eq!(rating_dm_count(&harness), 1);
    assert!(closed_history(&harness, HERE).await.rating_prompt_attempted);

    // Simulate a ticket row that remained after the archive was committed.
    seed_ticket(&harness, HERE, MEMBER).await;
    let (first, second) = tokio::join!(
        harness.click(press("close", MEMBER)),
        harness.click(press("close", MEMBER)),
    );
    assert!(first.is_ok());
    assert!(second.is_ok());
    assert_eq!(rating_dm_count(&harness), 1);
    assert!(closed_history(&harness, HERE).await.rating_prompt_attempted);
}

#[tokio::test]
async fn a_recovery_job_and_close_retry_send_one_rating_dm() {
    let harness = harness();
    harness.set_config(json!({
        "staff_roles": [STAFF_ROLE.to_string()],
        "max_open": 1
    }));
    seed_ticket(&harness, HERE, MEMBER).await;
    harness.api().fail_nth(
        3,
        closure_core::error::DiscordError::Fatal("temporary delete failure".into()),
    );
    let _ = harness.click(press("close", MEMBER)).await;
    assert_eq!(rating_dm_count(&harness), 0);
    assert!(matches!(
        closed_history(&harness, HERE).await.status,
        crate::HistoryStatus::Closing
    ));

    harness.advance(Duration::from_secs(300));
    let (jobs, retry) = tokio::join!(
        harness.run_due_jobs(),
        harness.click(press("close", MEMBER)),
    );
    let _ = jobs;
    let _ = retry;
    assert_eq!(rating_dm_count(&harness), 1);
    let history = closed_history(&harness, HERE).await;
    assert!(matches!(history.status, crate::HistoryStatus::Closed));
    assert!(history.rating_prompt_attempted);
}

#[tokio::test]
async fn a_staff_closer_is_told_to_relay_the_rating_command_when_dm_is_unconfirmed() {
    let harness = harness();
    harness.set_config(json!({
        "staff_roles": [STAFF_ROLE.to_string()],
        "max_open": 1
    }));
    seed_ticket(&harness, HERE, MEMBER).await;
    harness.api().fail_nth(
        4,
        closure_core::error::DiscordError::Forbidden("DMs are disabled".into()),
    );

    let closed = harness
        .invoke(
            Invocation::new("ticket")
                .subcommand("close")
                .roles([STAFF_ROLE])
                .user(MODERATOR),
        )
        .await
        .unwrap();
    assert!(
        closed
            .last()
            .unwrap()
            .content
            .contains("pass these instructions"),
        "{closed:?}"
    );
    assert_eq!(deleted(&harness), [HERE]);
    assert!(closed_history(&harness, HERE).await.rating_prompt_attempted);
}
