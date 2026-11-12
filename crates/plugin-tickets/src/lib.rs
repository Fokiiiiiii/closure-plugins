//! Tickets: a button opens a private channel for the member and the staff, and closing it saves
//! a transcript.
//!
//! `/ticket panel` posts the button; a press opens `ticket-0001` for the member and the staff.
//! Closing writes the channel's messages to the log channel as text, then deletes the channel.

#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

use std::collections::HashSet;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::time::Duration;

use closure_core::action::{
    Action, ChannelKind, ChannelPatch, FileUpload, MentionPolicy, NewChannel, Outcome,
    OverwriteSpec, OverwriteTarget, Reason, ThreadKind, ThreadPatch,
};
use closure_core::error::{DiscordError, ModuleError};
use closure_core::event::{ChannelId, Event, EventKind, MessageId, RoleId, UserId};
use closure_core::module::{EventContext, ModuleId};
use closure_core::storage::{Entry, Expect, StorageError};
use closure_core::subscription::Flow;
use closure_sdk::ui::{self, CustomId};
use closure_sdk::{
    Args, Command, CommandArgs, CommandCx, ComponentCx, CoreError, Cx, Intents, InteractionRef,
    InteractionReply, JobCx, Level, Locale, ModalCx, ModuleBuilder, PermissionSpec, SdkModule,
    SubcommandGroup, Subscription, UserError,
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use twilight_model::channel::ChannelType;
use twilight_model::channel::message::Component;
use twilight_model::channel::message::component::ButtonStyle;
use twilight_model::guild::Permissions;

const EN: &str = include_str!("../locales/en.ftl");
const JA: &str = include_str!("../locales/ja.ftl");
/// The job that closes a ticket nobody writes in.
const IDLE: &str = "idle";
const CLOSE_REQUEST_TIMEOUT: &str = "close-request-timeout";
const LEFT_SCAN: &str = "left-scan";
const LEFT_CLOSE: &str = "left-close";
/// How many tickets a member has open, and the last number given.
const OPEN: &str = "open";
const OPEN_LEASE: &str = "open-lease";
const OPEN_RECOVERY: &str = "open-recovery";
const OPEN_LEASE_SECONDS: i64 = 300;
const OPEN_RECOVERY_SECONDS: i64 = 300;
const OPEN_STATE_RETENTION: Duration = Duration::from_hours(24 * 365);
const OPEN_RESERVATION_MAX_BYTES: usize = 32 * 1024;
const OPENINGS_PAGE_SIZE: usize = 8;
const NUMBER: &str = "number";
const CLOSE_REQUESTS: &str = "close-requests";
const CLOSING: &str = "closing";
const FORM: &str = "form";
const MAX_TICKET_CUSTOM_ID_BYTES: usize = 100;
#[cfg(test)]
const MAX_FORM_QUESTION_CHARS: usize = advanced_forms::MAX_FORM_QUESTION_CHARS;
const MAX_TICKET_BYTES: usize = 32 * 1024;
const HISTORY_PREFIX: &str = "ticket-history:";
const HISTORY_LINK_PREFIX: &str = "ticket-history-link:";
const HISTORY_CONTENT_PREFIX: &str = "ticket-history-content:";
const HISTORY_INDEX_PREFIX: &str = "ticket-history-index:";
const HISTORY_ID_PREFIX: &str = "ticket-history-id:";
const HISTORY_TTL: Duration = Duration::from_hours(24 * 365);
const HISTORY_SEARCH_SESSION_PREFIX: &str = "ticket-history-search-session:";
const HISTORY_SEARCH_TTL: Duration = Duration::from_mins(15);
const HISTORY_SEARCH_BATCH: usize = 25;
const HISTORY_SEARCH_PAGE_BUDGET: usize = 32;
const HISTORY_SEARCH_NORMALIZED_CHARS: usize = 300;
const HISTORY_RESULTS: usize = 8;
const HISTORY_PAGE_BYTES: usize = 24 * 1024;
const HISTORY_VALUE_BYTES: usize = 30 * 1024;
const HISTORY_INDEX_SIZE: usize = 1000;
const HISTORY_DISPLAY_CHARS: usize = 700;
const NOTES_PER_PAGE: usize = 8;
const NOTE_MAX_CHARS: usize = 300;
const NOTE_PAGE_LIMIT: usize = 1_000;
const NOTE_RETENTION: Duration = Duration::from_hours(24 * 365);
const POSTGRES_TIMESTAMP_END_UNIX_SECONDS: i64 = 9_224_318_016_000;
const MAX_CLOSE_REQUEST_DEADLINE_UNIX_SECONDS: i64 =
    POSTGRES_TIMESTAMP_END_UNIX_SECONDS - 31_536_000 - 2;
const TICKET_RETENTION: Duration = Duration::from_hours(24 * 365);
const STATS_LIMIT: usize = 1_000;
const LABEL_MAX_CHARS: usize = 64;
const LABEL_LIST_UTF16_UNITS: usize = 1_700;
const REPLY_SCAN_BATCH_SIZE: usize = 1_000;
const REPLY_PAGE_SIZE: usize = 25;
const REPLY_ID_MAX_CHARS: usize = 76;
const REPLY_TITLE_MAX_CHARS: usize = 100;
const REPLY_BODY_MAX_CHARS: usize = 6_000;
const REPLY_CHUNK_MAX_UTF16: usize = 2_000;
const REPLY_SEND_COUNTER: &str = "ticket-reply-send";
const REPLY_SEND_RETENTION: Duration = Duration::from_hours(24 * 30);
/// How many messages a transcript holds at most: 100 for each of 50 pages.
/// A ticket nobody writes in is looked at again after at most this long.
const DAY: i64 = 86_400;
/// `Store::list` caps results; ticket keys can be partitioned by channel ID digits.
const TICKET_LIST_LIMIT: usize = 1000;
const CHANNEL_ID_DIGITS: usize = 20;
/// What the member and the staff may do in a ticket.
const TICKET_PERMISSIONS: Permissions = Permissions::VIEW_CHANNEL
    .union(Permissions::SEND_MESSAGES)
    .union(Permissions::READ_MESSAGE_HISTORY)
    .union(Permissions::ATTACH_FILES)
    .union(Permissions::EMBED_LINKS);

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
struct Config {
    /// The category the tickets are made in; without one they are made at the top.
    category: Option<String>,
    /// The text channel where private ticket threads are created; unset uses private channels.
    thread_channel: Option<String>,
    /// The roles that see every ticket. Tickets are off without one.
    staff_roles: Vec<String>,
    #[serde(default)]
    staff_teams: Vec<teams_survey::StaffTeam>,
    /// The channel the transcripts go to. Without one no transcript is kept.
    log_channel: Option<String>,
    timezone: String,
    hours: Vec<hours::Window>,
    max_open: u32,
    /// A ticket nobody writes in for this many days is closed; 0 never closes one.
    close_after_days: u32,
    waiting_after_hours: u32,
    first_response_timeout_hours: u32,
    last_message_timeout_hours: u32,
    waiting_category: Option<String>,
    /// Hours before an unanswered close request is closed; 0 disables it.
    close_request_timeout_hours: u32,
    questions: Vec<advanced_forms::FormQuestion>,
    forms: Vec<form_library::FormDefinition>,
    integrations: Vec<integrations::Integration>,
    /// Close the member's open tickets when they leave the server.
    close_when_opener_leaves: bool,
    panel_display: PanelDisplay,
    panels: Vec<NamedPanel>,
}

#[derive(Debug, Clone, Copy, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum PanelDisplay {
    #[default]
    Buttons,
    Select,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum PanelButtonStyle {
    Primary,
    Secondary,
    Success,
    Danger,
}

impl PanelButtonStyle {
    fn discord(self) -> ButtonStyle {
        match self {
            Self::Primary => ButtonStyle::Primary,
            Self::Secondary => ButtonStyle::Secondary,
            Self::Success => ButtonStyle::Success,
            Self::Danger => ButtonStyle::Danger,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
struct NamedPanel {
    id: String,
    name: Option<String>,
    description: Option<String>,
    button_style: Option<PanelButtonStyle>,
    category: Option<String>,
    thread_channel: Option<String>,
    staff_roles: Vec<String>,
    staff_teams: Vec<String>,
    log_channel: Option<String>,
    timezone: Option<String>,
    close_after_days: u32,
    waiting_after_hours: u32,
    first_response_timeout_hours: u32,
    last_message_timeout_hours: u32,
    waiting_category: Option<String>,
    form_id: Option<String>,
    questions: Vec<advanced_forms::FormQuestion>,
    exit_survey: Vec<advanced_forms::FormQuestion>,
    #[serde(flatten)]
    customization: panel_customization::PanelCustomization,
    max_open: u32,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            category: None,
            thread_channel: None,
            staff_roles: Vec::new(),
            staff_teams: Vec::new(),
            log_channel: None,
            timezone: "UTC".to_owned(),
            hours: Vec::new(),
            max_open: 1,
            close_after_days: 0,
            waiting_after_hours: 0,
            first_response_timeout_hours: 0,
            last_message_timeout_hours: 0,
            waiting_category: None,
            close_request_timeout_hours: 0,
            questions: Vec::new(),
            forms: Vec::new(),
            integrations: Vec::new(),
            close_when_opener_leaves: false,
            panel_display: PanelDisplay::Buttons,
            panels: Vec::new(),
        }
    }
}

fn id_of(text: Option<&String>) -> Option<u64> {
    text.and_then(|text| text.parse::<u64>().ok())
        .filter(|id| *id > 0)
}

impl Config {
    fn staff(&self) -> Vec<RoleId> {
        self.staff_roles
            .iter()
            .filter_map(|text| id_of(Some(text)))
            .map(RoleId::new)
            .collect()
    }

    fn category(&self) -> Option<ChannelId> {
        id_of(self.category.as_ref()).map(ChannelId::new)
    }

    fn thread_channel(&self) -> Result<Option<ChannelId>, UserError> {
        self.thread_channel
            .as_ref()
            .map(|text| {
                id_of(Some(text))
                    .map(ChannelId::new)
                    .ok_or_else(|| UserError::message("tickets-thread-channel-invalid"))
            })
            .transpose()
    }

    fn with_panel(&self, panel: &NamedPanel) -> Self {
        let questions = panel
            .form_id
            .as_deref()
            .and_then(|id| form_library::resolve_form(&self.forms, id))
            .map_or_else(|| panel.questions.clone(), |form| form.questions.clone());
        Self {
            category: panel.category.clone(),
            thread_channel: panel.thread_channel.clone(),
            staff_roles: panel.staff_roles.clone(),
            staff_teams: self.staff_teams.clone(),
            log_channel: panel.log_channel.clone(),
            timezone: panel
                .timezone
                .clone()
                .unwrap_or_else(|| self.timezone.clone()),
            hours: self.hours.clone(),
            max_open: self.max_open,
            close_after_days: panel.close_after_days,
            waiting_after_hours: panel.waiting_after_hours,
            first_response_timeout_hours: panel.first_response_timeout_hours,
            last_message_timeout_hours: panel.last_message_timeout_hours,
            waiting_category: panel.waiting_category.clone(),
            close_request_timeout_hours: self.close_request_timeout_hours,
            questions,
            forms: Vec::new(),
            integrations: self.integrations.clone(),
            close_when_opener_leaves: self.close_when_opener_leaves,
            panel_display: self.panel_display,
            panels: Vec::new(),
        }
    }

    fn for_ticket(&self, ticket: &Ticket) -> Self {
        ticket.settings.clone().unwrap_or_else(|| {
            ticket
                .panel
                .as_ref()
                .map_or_else(|| self.clone(), |panel| self.with_panel(panel))
        })
    }

    fn named(&self, id: &str) -> Result<&NamedPanel, UserError> {
        let mut matches = self.panels.iter().filter(|panel| panel.id == id);
        let panel = matches
            .next()
            .ok_or_else(|| UserError::message("tickets-panel-unavailable"))?;
        if matches.next().is_some() {
            return Err(UserError::message("tickets-panel-duplicate"));
        }
        Ok(panel)
    }

    fn log(&self) -> Option<ChannelId> {
        id_of(self.log_channel.as_ref()).map(ChannelId::new)
    }
}

/// An open ticket.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Ticket {
    number: i64,
    opener: u64,
    claimed_by: Option<u64>,
    created: i64,
    /// When somebody began to close it (unix seconds): a claim made now is refused, and a second
    /// close waits for the first, unless that one stopped long ago.
    #[serde(default)]
    closing: Option<i64>,
    /// A monotonic fence for close retries; independent of the closing timestamp.
    #[serde(default)]
    close_generation: i64,
    #[serde(default)]
    close_request: Option<CloseRequest>,
    #[serde(default)]
    panel: Option<NamedPanel>,
    #[serde(default)]
    staff_roles: Option<Vec<u64>>,
    #[serde(default)]
    staff_access: Option<teams_survey::StaffAccessSnapshot>,
    #[serde(default)]
    form_answers: Vec<(String, String)>,
    #[serde(default)]
    form_answer_count: u8,
    #[serde(default)]
    labels: Vec<String>,
    #[serde(default)]
    settings: Option<Config>,
    #[serde(default)]
    thread: bool,
    #[serde(default)]
    thread_parent: Option<u64>,
    #[serde(default)]
    closed: bool,
    #[serde(default)]
    pending_operation: Option<operations::PendingOperation>,
    #[serde(default)]
    panel_role_baselines: Vec<operations::RoleAccess>,
    #[serde(default)]
    participant_roles: Vec<operations::RoleAccess>,
    #[serde(default)]
    panel_user_baselines: Vec<operations::UserAccess>,
    #[serde(default)]
    participant_users: Vec<u64>,
    #[serde(default)]
    waiting_for_reply: bool,
    #[serde(default)]
    automation: automation::State,
    #[serde(default)]
    message_quote: Option<message_ticket::MessageQuote>,
    #[serde(default)]
    message_quote_message_id: Option<u64>,
}

impl Ticket {
    fn is_open(&self) -> bool {
        !self.closed && self.closing.is_none()
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum HistoryStatus {
    Closing,
    Anonymizing,
    Reopening,
    Closed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct TicketRating {
    score: u8,
    created: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[allow(clippy::struct_excessive_bools)]
struct TicketHistory {
    version: u8,
    #[serde(default)]
    expires_at: i64,
    channel_id: u64,
    number: i64,
    opener: u64,
    claimed_by: Option<u64>,
    created: i64,
    closed: i64,
    closer: Option<u64>,
    reason: String,
    panel: Option<NamedPanel>,
    #[serde(default)]
    survey_integrations: Vec<integrations::Integration>,
    #[serde(default)]
    labels: Vec<String>,
    #[serde(default)]
    rating: Option<TicketRating>,
    /// Claimed before sending so recovery cannot duplicate a DM with an unknown outcome.
    #[serde(default)]
    rating_prompt_attempted: bool,
    #[serde(default)]
    opener_erased: bool,
    #[serde(default)]
    privacy_generation: u64,
    #[serde(default)]
    anonymizing_user: Option<u64>,
    #[serde(default)]
    operation_token: Option<u64>,
    #[serde(default)]
    operation_until: i64,
    #[serde(default)]
    log_copy_channel: Option<u64>,
    #[serde(default)]
    log_copy_message: Option<u64>,
    #[serde(default)]
    transcript_span_pages: u32,
    #[serde(default)]
    staff_access: Option<teams_survey::StaffAccessSnapshot>,
    staff_roles: Vec<u64>,
    status: HistoryStatus,
    /// Prevents recovery from posting the configured transcript copy more than once.
    #[serde(default)]
    log_copy_sent: bool,
    /// The close generation used to fence retries and ratings.
    attempt: i64,
    /// The close generation that identifies the stored transcript pages.
    transcript_attempt: i64,
    transcript_pages: u32,
    transcript_index_pages: u32,
    transcript_char_count: u64,
    transcript_bytes: u64,
    transcript_messages: u32,
    transcript_truncated: bool,
}

const HISTORY_OPERATION_LEASE_SECONDS: i64 = 300;

#[derive(Debug)]
pub(crate) struct HistoryReopenLease {
    key: String,
    token: u64,
    generation: u64,
}

pub(crate) enum HistoryReopenAcquire {
    Legacy,
    Acquired(HistoryReopenLease),
    Busy,
    Erased,
}

pub(crate) async fn acquire_history_reopen_lease(
    ctx: &EventContext,
    channel: ChannelId,
    ticket: &Ticket,
    token: u64,
) -> Result<HistoryReopenAcquire, ModuleError> {
    let key = history_key(ticket.number, channel);
    for _ in 0..5 {
        let Some(entry) = ctx
            .store()
            .entry(&key)
            .await
            .map_err(|error| stored(&error))?
        else {
            return Ok(HistoryReopenAcquire::Legacy);
        };
        let mut history: TicketHistory = serde_json::from_value(entry.value)
            .map_err(|error| ModuleError::other(error.to_string()))?;
        if history.channel_id != channel.get() || history.number != ticket.number {
            return Err(ModuleError::other(
                "ticket history does not match its ticket",
            ));
        }
        if history.opener_erased {
            return Ok(HistoryReopenAcquire::Erased);
        }
        let now = ctx.now();
        let expired = history.operation_until <= now;
        let available = match history.status {
            HistoryStatus::Closed => history.operation_token.is_none() || expired,
            HistoryStatus::Reopening => expired,
            HistoryStatus::Anonymizing | HistoryStatus::Closing => false,
        };
        if !available {
            return Ok(HistoryReopenAcquire::Busy);
        }
        let generation = history
            .privacy_generation
            .checked_add(1)
            .ok_or_else(|| ModuleError::other("ticket privacy generation is exhausted"))?;
        let operation_until = now
            .checked_add(HISTORY_OPERATION_LEASE_SECONDS)
            .ok_or_else(|| ModuleError::other("ticket history operation deadline overflowed"))?;
        history.status = HistoryStatus::Reopening;
        history.privacy_generation = generation;
        history.operation_token = Some(token);
        history.operation_until = operation_until;
        history.anonymizing_user = None;
        let ttl = history_ttl(&history, now)?;
        match ctx
            .store()
            .replace(&key, &history, Expect::Revision(entry.revision), Some(ttl))
            .await
        {
            Ok(_) => {
                return Ok(HistoryReopenAcquire::Acquired(HistoryReopenLease {
                    key,
                    token,
                    generation,
                }));
            }
            Err(StorageError::Conflict) => {}
            Err(error) => return Err(stored(&error)),
        }
    }
    Ok(HistoryReopenAcquire::Busy)
}

pub(crate) async fn renew_history_reopen_lease(
    ctx: &EventContext,
    lease: &HistoryReopenLease,
) -> Result<bool, ModuleError> {
    for _ in 0..5 {
        let Some(entry) = ctx
            .store()
            .entry(&lease.key)
            .await
            .map_err(|error| stored(&error))?
        else {
            return Ok(false);
        };
        let mut history: TicketHistory = serde_json::from_value(entry.value)
            .map_err(|error| ModuleError::other(error.to_string()))?;
        if history.status != HistoryStatus::Reopening
            || history.operation_token != Some(lease.token)
            || history.privacy_generation != lease.generation
            || history.operation_until <= ctx.now()
            || history.opener_erased
        {
            return Ok(false);
        }
        history.operation_until = ctx
            .now()
            .checked_add(HISTORY_OPERATION_LEASE_SECONDS)
            .ok_or_else(|| ModuleError::other("ticket history operation deadline overflowed"))?;
        let ttl = history_ttl(&history, ctx.now())?;
        match ctx
            .store()
            .replace(
                &lease.key,
                &history,
                Expect::Revision(entry.revision),
                Some(ttl),
            )
            .await
        {
            Ok(_) => return Ok(true),
            Err(StorageError::Conflict) => {}
            Err(error) => return Err(stored(&error)),
        }
    }
    Ok(false)
}

pub(crate) async fn release_history_reopen_lease(
    ctx: &EventContext,
    lease: &HistoryReopenLease,
) -> Result<bool, ModuleError> {
    for _ in 0..5 {
        let Some(entry) = ctx
            .store()
            .entry(&lease.key)
            .await
            .map_err(|error| stored(&error))?
        else {
            return Ok(false);
        };
        let mut history: TicketHistory = serde_json::from_value(entry.value)
            .map_err(|error| ModuleError::other(error.to_string()))?;
        if history.status != HistoryStatus::Reopening
            || history.operation_token != Some(lease.token)
            || history.privacy_generation != lease.generation
        {
            return Ok(false);
        }
        history.status = HistoryStatus::Closed;
        history.operation_token = None;
        history.operation_until = 0;
        history.anonymizing_user = None;
        let ttl = history_ttl(&history, ctx.now())?;
        match ctx
            .store()
            .replace(
                &lease.key,
                &history,
                Expect::Revision(entry.revision),
                Some(ttl),
            )
            .await
        {
            Ok(_) => return Ok(true),
            Err(StorageError::Conflict) => {}
            Err(error) => return Err(stored(&error)),
        }
    }
    Ok(false)
}

struct CloseAttempt<'a> {
    channel: ChannelId,
    ticket: &'a Ticket,
    config: &'a Config,
    closer: Option<UserId>,
    reason: &'a str,
    attempt: i64,
}

#[derive(Clone, Copy)]
struct TranscriptInfo {
    pages: u32,
    index_pages: u32,
    span_pages: u32,
    chars: u64,
    bytes: u64,
    messages: u32,
    truncated: bool,
}

#[derive(Serialize, Deserialize)]
struct ClosingJob {
    channel: u64,
    number: i64,
    attempt: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct StaffNote {
    author: u64,
    created: i64,
    text: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct StaffNotePage {
    ticket_number: i64,
    panel: Option<NamedPanel>,
    #[serde(default)]
    staff_access: Option<teams_survey::StaffAccessSnapshot>,
    notes: Vec<StaffNote>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CloseRequest {
    id: i64,
    requested_by: u64,
    reason: Option<String>,
    #[serde(default)]
    expires_at: Option<i64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct RatingRequest {
    channel: u64,
    number: i64,
    attempt: i64,
    score: u8,
}

#[derive(Serialize, Deserialize)]
struct Idle {
    channel: u64,
}

#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(default)]
struct OpenState {
    lease: Option<OpenLease>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct OpenLease {
    token: i64,
    until: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct OpenReservation {
    opener: u64,
    number: i64,
    token: i64,
    created: i64,
    panel: Option<NamedPanel>,
    settings: Config,
    staff_roles: Vec<u64>,
    #[serde(default)]
    staff_access: Option<teams_survey::StaffAccessSnapshot>,
    form_answers: Vec<(String, String)>,
    #[serde(default)]
    form_modal_revision: Option<u64>,
    #[serde(default)]
    message_quote: Option<message_ticket::MessageQuote>,
    #[serde(default)]
    channel_name: String,
    #[serde(default)]
    staff_opening: Option<bool>,
    generation: i64,
    until: i64,
    channel: Option<u64>,
    #[serde(default)]
    api_started: bool,
    #[serde(default)]
    attempts: u32,
    #[serde(default)]
    released_at: Option<i64>,
    #[serde(default)]
    released_by: Option<u64>,
}

#[derive(Serialize, Deserialize)]
struct OpenRecovery {
    opener: u64,
    number: i64,
    #[serde(default)]
    force: bool,
}

#[derive(Debug)]
#[allow(clippy::large_enum_variant)]
enum OpenReservationResult {
    Reserved(OpenReservation),
    GlobalLimit(u32),
    PanelLimit(u32),
    Cooldown(u32),
    TooLarge,
    Pending,
    Busy,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OpenChannelLookup {
    Missing,
    Found(ChannelId),
    Ambiguous,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OpenChannelStatus {
    Missing,
    Verified,
    Mismatch,
}

#[derive(Serialize, Deserialize)]
struct CloseRequestTimeout {
    channel: u64,
    request_id: i64,
    expires_at: i64,
}

#[derive(Debug, PartialEq, Eq)]
enum CloseRequestExpiry {
    Disabled,
    At(i64),
}
fn ticket_ttl(ticket: &Ticket, now: i64) -> Duration {
    let remaining = ticket
        .close_request
        .as_ref()
        .and_then(|request| request.expires_at)
        .map_or(0, |deadline| {
            u64::try_from(deadline.saturating_sub(now).max(0)).unwrap_or_default()
        });
    let max = u64::try_from(
        POSTGRES_TIMESTAMP_END_UNIX_SECONDS
            .saturating_sub(now)
            .saturating_sub(2),
    )
    .unwrap_or_default();
    Duration::from_secs(
        TICKET_RETENTION
            .as_secs()
            .saturating_add(remaining)
            .min(max),
    )
}

fn close_request_expiry(now: i64, timeout_hours: u32) -> Option<CloseRequestExpiry> {
    if timeout_hours == 0 {
        Some(CloseRequestExpiry::Disabled)
    } else {
        let seconds = i64::from(timeout_hours).checked_mul(3_600)?;
        let deadline = now.checked_add(seconds)?;
        (deadline <= MAX_CLOSE_REQUEST_DEADLINE_UNIX_SECONDS)
            .then_some(CloseRequestExpiry::At(deadline))
    }
}
#[derive(Serialize, Deserialize)]
struct LeftScan {
    opener: u64,
    at: i64,
}

#[derive(Serialize, Deserialize)]
struct LeftClose {
    channel: u64,
}

#[derive(CommandArgs)]
struct PanelArgs {
    /// A panel ID to publish alone; omit to publish all configured panels
    #[arg(max_length = 32)]
    panel: Option<String>,
    /// Where the published message goes; this channel when left out
    channel: Option<ChannelId>,
    /// What the message says
    #[arg(max_length = 1000)]
    text: Option<String>,
    /// An existing panel message to update; omit to publish new messages
    #[arg(max_length = 20)]
    message: Option<String>,
    /// Which panel page to publish or update
    page: Option<i64>,
}

#[derive(CommandArgs)]
struct Close {
    /// Why it is closed
    #[arg(max_length = 300)]
    reason: Option<String>,
}

#[derive(CommandArgs)]
struct CloseRequestArgs {
    /// Why the staff member asks to close it
    #[arg(max_length = 300)]
    reason: Option<String>,
}

#[derive(CommandArgs)]
struct Unclaim {}

#[derive(CommandArgs)]
struct StatsArgs {}

#[derive(CommandArgs)]
struct Member {
    /// The member
    user: UserId,
}

#[derive(CommandArgs)]
struct Rename {
    /// The new channel name
    name: String,
}

#[derive(CommandArgs)]
struct LabelArgs {
    /// The label name
    #[arg(max_length = 64)]
    label: String,
}

#[derive(CommandArgs)]
struct LabelListArgs {
    /// The page number
    page: Option<i64>,
}

#[derive(CommandArgs)]
struct SearchHistory {
    /// Ticket number, member ID, or close reason
    #[arg(max_length = 100)]
    query: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct HistorySearchResume {
    key: String,
    channel_id: u64,
    number: i64,
    transcript_attempt: i64,
    #[serde(default)]
    privacy_generation: u64,
    next_page: u32,
    carry: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct HistorySearchState {
    owner: u64,
    token: String,
    query: String,
    after_key: String,
    current: Option<HistorySearchResume>,
    expires_at: i64,
    incomplete: bool,
    complete: bool,
    consumed: bool,
}

struct HistorySearchHit {
    history: TicketHistory,
    excerpt: String,
}

struct HistorySearchAccess<'a> {
    actor_id: u64,
    roles: &'a [RoleId],
    can_manage: bool,
}

enum HistoryTextSearch {
    Match(String),
    NoMatch,
    Partial { next_page: u32, carry: String },
    Incomplete,
}

#[derive(CommandArgs)]
struct ViewHistory {
    /// Ticket number
    number: i64,
    /// Transcript page
    page: Option<i64>,
}

#[derive(CommandArgs)]
struct NoteArgs {
    /// The note text
    #[arg(max_length = 300)]
    text: String,
}

#[derive(CommandArgs)]
struct NotesArgs {
    /// The ticket channel ID
    #[arg(max_length = 20)]
    ticket: String,
    /// The page number
    page: Option<i64>,
}

#[derive(CommandArgs)]
struct OpeningsArgs {
    /// The page number, starting at 1
    page: Option<i64>,
}

#[derive(CommandArgs)]
struct OpenReservationArgs {
    /// The number of the pending ticket opening
    number: i64,
}

#[derive(Debug, Deserialize, CommandArgs)]
struct RateArgs {
    /// The closed ticket number
    ticket: i64,
    /// A rating from 1 to 5
    score: i64,
}

#[derive(Debug, Deserialize, CommandArgs)]
struct SurveyArgs {
    /// The closed ticket number
    number: i64,
}

#[derive(Debug, Deserialize, CommandArgs)]
struct KnowledgeSendArgs {
    /// The knowledge article ID
    #[arg(max_length = 32)]
    id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct SavedReply {
    id: String,
    title: String,
    body: String,
    #[serde(default)]
    deleted: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ReplySendAttempt {
    channel: u64,
    owner: u64,
    ticket_number: i64,
    panel: Option<NamedPanel>,
    reply_id: String,
    chunks: Vec<String>,
    confirmed: usize,
    in_flight: Option<usize>,
}

#[derive(CommandArgs)]
struct ReplySearchArgs {
    /// Search response titles and IDs
    #[arg(max_length = 100)]
    query: Option<String>,
    /// The result page
    page: Option<i64>,
}

#[derive(CommandArgs)]
struct ReplyAddArgs {
    /// A unique ID with letters, numbers, underscores, or hyphens
    #[arg(max_length = 76)]
    id: String,
    /// The title shown in search
    #[arg(max_length = 100)]
    title: String,
    /// The response body
    #[arg(max_length = 6000)]
    body: String,
}

#[derive(CommandArgs)]
struct ReplyEditArgs {
    /// The response ID
    #[arg(max_length = 76)]
    id: String,
    /// The title shown in search
    #[arg(max_length = 100)]
    title: String,
    /// The response body
    #[arg(max_length = 6000)]
    body: String,
}

#[derive(CommandArgs)]
struct ReplyDeleteArgs {
    /// The response ID
    #[arg(max_length = 76)]
    id: String,
}

fn ticket_key(channel: ChannelId) -> String {
    format!("ticket:{}", channel.get())
}

fn open_state_key(user: u64) -> String {
    format!("open-state:{user}")
}

fn open_recovery_key(user: u64, number: i64) -> String {
    format!("open-recovery-{user}-{number}")
}

fn open_recovery_delay(attempts: u32) -> i64 {
    match attempts {
        0 => OPEN_RECOVERY_SECONDS,
        1 => OPEN_RECOVERY_SECONDS * 2,
        2 => OPEN_RECOVERY_SECONDS * 4,
        3 => OPEN_RECOVERY_SECONDS * 8,
        4 => 60 * 60,
        _ => 24 * 60 * 60,
    }
}

async fn all_tickets(ctx: &EventContext) -> Result<Vec<(ChannelId, Ticket)>, ModuleError> {
    let mut prefixes = vec!["ticket:".to_owned()];
    let mut tickets = Vec::new();
    while let Some(prefix) = prefixes.pop() {
        let entries = ctx
            .store()
            .list(&prefix, TICKET_LIST_LIMIT)
            .await
            .map_err(|error| stored(&error))?;
        if entries.len() == TICKET_LIST_LIMIT {
            if let Some((key, entry)) = entries
                .iter()
                .find(|entry| entry.0.as_str() == prefix.as_str())
            {
                let suffix = key
                    .strip_prefix("ticket:")
                    .ok_or_else(|| ModuleError::other("ticket listing returned an invalid key"))?;
                let channel = suffix
                    .parse::<u64>()
                    .map_err(|error| ModuleError::other(error.to_string()))?;
                let ticket = serde_json::from_value(entry.value.clone())
                    .map_err(|error| ModuleError::other(error.to_string()))?;
                tickets.push((ChannelId::new(channel), ticket));
            }
            if prefix.len() - "ticket:".len() >= CHANNEL_ID_DIGITS {
                return Err(ModuleError::other(
                    "ticket listing exceeded a channel ID prefix",
                ));
            }
            prefixes.extend((b'0'..=b'9').map(|digit| format!("{prefix}{}", char::from(digit))));
            continue;
        }
        for (key, entry) in entries {
            let suffix = key
                .strip_prefix("ticket:")
                .ok_or_else(|| ModuleError::other("ticket listing returned an invalid key"))?;
            let channel = suffix
                .parse::<u64>()
                .map_err(|error| ModuleError::other(error.to_string()))?;
            let ticket = serde_json::from_value(entry.value)
                .map_err(|error| ModuleError::other(error.to_string()))?;
            tickets.push((ChannelId::new(channel), ticket));
        }
    }
    Ok(tickets)
}

async fn remove_missing_closing_tickets(
    ctx: &EventContext,
    user: u64,
    tickets: &mut Vec<(ChannelId, Ticket)>,
) -> Result<(), ModuleError> {
    let mut missing = Vec::new();
    for (channel, _) in tickets
        .iter()
        .filter(|(_, ticket)| ticket.opener == user && ticket.closing.is_some())
    {
        match ctx
            .api
            .execute(Action::GetChannel {
                channel_id: *channel,
            })
            .await
        {
            Ok(Outcome::Channel(_)) => {}
            Err(DiscordError::NotFound) => {
                ctx.store()
                    .delete(&ticket_key(*channel))
                    .await
                    .map_err(|error| stored(&error))?;
                missing.push(*channel);
            }
            Ok(_) => {
                return Err(ModuleError::other(
                    "ticket channel lookup returned no channel",
                ));
            }
            Err(error) => return Err(error.into()),
        }
    }
    tickets.retain(|(channel, _)| !missing.contains(channel));
    Ok(())
}

fn open_reservation_key(user: u64, number: i64) -> String {
    format!("open-reservation:{user}:{number:020}")
}

fn open_reservation_prefix(user: u64) -> String {
    format!("open-reservation:{user}:")
}

async fn read_open_reservation(
    ctx: &EventContext,
    user: u64,
    number: i64,
) -> Result<(Option<Entry>, Option<OpenReservation>), ModuleError> {
    let entry = ctx
        .store()
        .entry(&open_reservation_key(user, number))
        .await
        .map_err(|error| stored(&error))?;
    let reservation = entry
        .as_ref()
        .map(|entry| serde_json::from_value(entry.value.clone()))
        .transpose()
        .map_err(|error| ModuleError::other(error.to_string()))?;
    Ok((entry, reservation))
}

async fn list_user_reservations(
    ctx: &EventContext,
    user: u64,
) -> Result<Vec<OpenReservation>, ModuleError> {
    let root = open_reservation_prefix(user);
    let mut prefixes = vec![root.clone()];
    let mut reservations = Vec::new();
    while let Some(prefix) = prefixes.pop() {
        let entries = ctx
            .store()
            .list(&prefix, TICKET_LIST_LIMIT)
            .await
            .map_err(|error| stored(&error))?;
        if entries.len() == TICKET_LIST_LIMIT {
            if prefix.len() - root.len() >= 20 {
                return Err(ModuleError::other(
                    "reservation listing exceeded its numeric ID",
                ));
            }
            prefixes.extend((b'0'..=b'9').map(|digit| format!("{prefix}{}", char::from(digit))));
            continue;
        }
        for (key, entry) in entries {
            let suffix = key
                .strip_prefix(&root)
                .ok_or_else(|| ModuleError::other("reservation listing returned an invalid key"))?;
            let number = suffix
                .parse::<i64>()
                .map_err(|error| ModuleError::other(error.to_string()))?;
            let reservation: OpenReservation = serde_json::from_value(entry.value)
                .map_err(|error| ModuleError::other(error.to_string()))?;
            if reservation.opener != user || reservation.number != number {
                return Err(ModuleError::other(
                    "reservation key does not match its value",
                ));
            }
            reservations.push(reservation);
        }
    }
    Ok(reservations)
}

async fn all_open_reservations(
    ctx: &EventContext,
) -> Result<Vec<(u64, OpenReservation)>, ModuleError> {
    let root = "open-reservation:";
    let mut prefixes = vec![root.to_owned()];
    let mut reservations = Vec::new();
    while let Some(prefix) = prefixes.pop() {
        let entries = ctx
            .store()
            .list(&prefix, TICKET_LIST_LIMIT)
            .await
            .map_err(|error| stored(&error))?;
        if entries.len() == TICKET_LIST_LIMIT {
            if let Some((key, entry)) = entries.iter().find(|entry| entry.0.as_str() == prefix) {
                let suffix = key.strip_prefix(root).ok_or_else(|| {
                    ModuleError::other("reservation listing returned an invalid key")
                })?;
                let (user, number) = suffix.split_once(':').ok_or_else(|| {
                    ModuleError::other("reservation key is missing its separator")
                })?;
                let user = user
                    .parse::<u64>()
                    .map_err(|error| ModuleError::other(error.to_string()))?;
                let number = number
                    .parse::<i64>()
                    .map_err(|error| ModuleError::other(error.to_string()))?;
                let reservation: OpenReservation = serde_json::from_value(entry.value.clone())
                    .map_err(|error| ModuleError::other(error.to_string()))?;
                if reservation.opener != user || reservation.number != number {
                    return Err(ModuleError::other(
                        "reservation key does not match its value",
                    ));
                }
                reservations.push((user, reservation));
            }
            if prefix.len() - root.len() >= 41 {
                return Err(ModuleError::other("reservation listing exceeded its key"));
            }
            prefixes.extend(
                (b'0'..=b'9')
                    .chain(std::iter::once(b':'))
                    .map(|character| format!("{prefix}{}", char::from(character))),
            );
            continue;
        }
        for (key, entry) in entries {
            let suffix = key
                .strip_prefix(root)
                .ok_or_else(|| ModuleError::other("reservation listing returned an invalid key"))?;
            let (user, number) = suffix
                .split_once(':')
                .ok_or_else(|| ModuleError::other("reservation key is missing its separator"))?;
            let user = user
                .parse::<u64>()
                .map_err(|error| ModuleError::other(error.to_string()))?;
            let number = number
                .parse::<i64>()
                .map_err(|error| ModuleError::other(error.to_string()))?;
            let reservation: OpenReservation = serde_json::from_value(entry.value)
                .map_err(|error| ModuleError::other(error.to_string()))?;
            if reservation.opener != user || reservation.number != number {
                return Err(ModuleError::other(
                    "reservation key does not match its value",
                ));
            }
            reservations.push((user, reservation));
        }
    }
    Ok(reservations)
}

async fn find_open_reservation(
    ctx: &EventContext,
    number: i64,
) -> Result<Option<(u64, OpenReservation)>, ModuleError> {
    let mut found = None;
    for (user, reservation) in all_open_reservations(ctx)
        .await?
        .into_iter()
        .filter(|(_, reservation)| reservation.number == number)
    {
        if found.is_some() {
            return Err(ModuleError::other("ticket opening number is ambiguous"));
        }
        found = Some((user, reservation));
    }
    Ok(found)
}

fn open_counts(
    user: u64,
    panel_id: Option<&str>,
    tickets: &[(ChannelId, Ticket)],
    reservations: &[OpenReservation],
) -> (u64, u64) {
    let mut numbers = Vec::new();
    let mut all = 0_u64;
    let mut panel = 0_u64;
    for (_, ticket) in tickets
        .iter()
        .filter(|(_, ticket)| ticket.opener == user && !ticket.closed)
    {
        all += 1;
        numbers.push(ticket.number);
        if ticket.panel.as_ref().map(|saved| saved.id.as_str()) == panel_id {
            panel += 1;
        }
    }
    for reservation in reservations
        .iter()
        .filter(|reservation| reservation.opener == user)
    {
        if reservation.released_at.is_some() || numbers.contains(&reservation.number) {
            continue;
        }
        all += 1;
        if reservation.panel.as_ref().map(|saved| saved.id.as_str()) == panel_id {
            panel += 1;
        }
    }
    (all, panel)
}

async fn read_open_state(
    ctx: &EventContext,
    user: u64,
) -> Result<(Option<Entry>, OpenState), ModuleError> {
    let entry = ctx
        .store()
        .entry(&open_state_key(user))
        .await
        .map_err(|error| stored(&error))?;
    let state = entry
        .as_ref()
        .map(|entry| serde_json::from_value(entry.value.clone()))
        .transpose()
        .map_err(|error| ModuleError::other(error.to_string()))?
        .unwrap_or_default();
    Ok((entry, state))
}

fn owns_open_lock(state: &OpenState, token: i64, now: i64) -> bool {
    state
        .lease
        .as_ref()
        .is_some_and(|lease| lease.token == token && lease.until > now)
}

/// A short lease protects quota checks and counter repair. Every write checks its fencing token.
async fn acquire_open_lock(ctx: &EventContext, user: u64) -> Result<Option<i64>, ModuleError> {
    let token = ctx
        .store()
        .score_add(OPEN_LEASE, 0_u64, 1)
        .await
        .map_err(|error| stored(&error))?;
    let key = open_state_key(user);
    for _ in 0..5 {
        let (entry, mut state) = read_open_state(ctx, user).await?;
        if state
            .lease
            .as_ref()
            .is_some_and(|lease| lease.until > ctx.now())
        {
            return Ok(None);
        }
        state.lease = Some(OpenLease {
            token,
            until: ctx.now() + OPEN_LEASE_SECONDS,
        });
        let expect = entry
            .as_ref()
            .map_or(Expect::Missing, |entry| Expect::Revision(entry.revision));
        match ctx.store().replace(&key, &state, expect, None).await {
            Ok(_) => return Ok(Some(token)),
            Err(StorageError::Conflict) => {}
            Err(error) => return Err(stored(&error)),
        }
    }
    Ok(None)
}

async fn release_open_lock(ctx: &EventContext, user: u64, token: i64) -> Result<(), ModuleError> {
    let key = open_state_key(user);
    for _ in 0..5 {
        let (Some(entry), mut state) = read_open_state(ctx, user).await? else {
            return Ok(());
        };
        if state.lease.as_ref().map(|lease| lease.token) != Some(token) {
            return Ok(());
        }
        state.lease = None;
        match ctx
            .store()
            .replace(
                &key,
                &state,
                Expect::Revision(entry.revision),
                Some(OPEN_STATE_RETENTION),
            )
            .await
        {
            Ok(_) => return Ok(()),
            Err(StorageError::Conflict) => {}
            Err(error) => return Err(stored(&error)),
        }
    }
    Err(ModuleError::other(
        "the ticket opening lock changed too often",
    ))
}

async fn set_open_score(ctx: &EventContext, user: u64, count: u64) -> Result<(), ModuleError> {
    let count =
        i64::try_from(count).map_err(|_| ModuleError::other("too many open tickets to count"))?;
    let current = ctx
        .store()
        .score(OPEN, user)
        .await
        .map_err(|error| stored(&error))?;
    let delta = count
        .checked_sub(current)
        .ok_or_else(|| ModuleError::other("open ticket counter overflowed"))?;
    if delta != 0 {
        ctx.store()
            .score_add(OPEN, user, delta)
            .await
            .map_err(|error| stored(&error))?;
    }
    Ok(())
}

async fn reconcile_open_counter(ctx: &EventContext, user: u64) -> Result<bool, ModuleError> {
    let Some(token) = acquire_open_lock(ctx, user).await? else {
        return Ok(false);
    };
    let result = async {
        let mut tickets = all_tickets(ctx).await?;
        remove_missing_closing_tickets(ctx, user, &mut tickets).await?;
        let reservations = list_user_reservations(ctx, user).await?;
        let (_, state) = read_open_state(ctx, user).await?;
        if !owns_open_lock(&state, token, ctx.now()) {
            return Ok(false);
        }
        let (open_count, _) = open_counts(user, None, &tickets, &reservations);
        set_open_score(ctx, user, open_count).await?;
        Ok(true)
    }
    .await;
    release_open_lock(ctx, user, token).await?;
    result
}

async fn schedule_open_recovery(
    ctx: &EventContext,
    user: u64,
    number: i64,
    at: i64,
) -> Result<(), ModuleError> {
    ctx.scheduler()
        .job(
            OPEN_RECOVERY,
            &OpenRecovery {
                opener: user,
                number,
                force: false,
            },
        )
        .map_err(|error| stored(&error))?
        .key(&open_recovery_key(user, number))
        .at(at)
        .await
        .map_err(|error| stored(&error))
}

async fn force_open_recovery(
    ctx: &EventContext,
    user: u64,
    number: i64,
) -> Result<(), ModuleError> {
    ctx.scheduler()
        .job(
            OPEN_RECOVERY,
            &OpenRecovery {
                opener: user,
                number,
                force: true,
            },
        )
        .map_err(|error| stored(&error))?
        .key(&open_recovery_key(user, number))
        .at(ctx.now())
        .await
        .map_err(|error| stored(&error))
}

async fn cancel_open_recovery(ctx: &EventContext, user: u64, number: i64) {
    let _ = ctx
        .scheduler()
        .cancel(&open_recovery_key(user, number))
        .await;
}

fn ticket_from_reservation(reservation: &OpenReservation, user: u64) -> Ticket {
    let thread_parent = reservation
        .settings
        .thread_channel
        .as_ref()
        .and_then(|text| id_of(Some(text)));
    Ticket {
        number: reservation.number,
        opener: user,
        claimed_by: None,
        created: reservation.created,
        closing: None,
        close_generation: 0,
        close_request: None,
        panel: reservation.panel.clone(),
        staff_roles: Some(reservation.staff_roles.clone()),
        staff_access: reservation.staff_access.clone(),
        form_answers: reservation.form_answers.clone(),
        form_answer_count: 0,
        labels: Vec::new(),
        settings: Some(reservation.settings.clone()),
        thread: thread_parent.is_some(),
        thread_parent,
        closed: false,
        pending_operation: None,
        panel_role_baselines: Vec::new(),
        participant_roles: Vec::new(),
        panel_user_baselines: Vec::new(),
        participant_users: Vec::new(),
        waiting_for_reply: false,
        automation: automation::State::default(),
        message_quote: reservation.message_quote.clone(),
        message_quote_message_id: None,
    }
}

fn reservation_channel_name(reservation: &OpenReservation) -> String {
    if !reservation.channel_name.trim().is_empty() {
        return reservation.channel_name.clone();
    }
    if reservation
        .settings
        .thread_channel
        .as_ref()
        .and_then(|parent| id_of(Some(parent)))
        .is_some()
    {
        format!("ticket-{:04}-r{}", reservation.number, reservation.token)
    } else {
        format!("ticket-{:04}", reservation.number)
    }
}

fn has_open_reservation_marker(topic: Option<&str>, token: i64) -> bool {
    let marker = format!("ticket-reservation:{token}");
    topic.is_some_and(|topic| topic.split_whitespace().any(|part| part == marker))
}

fn matches_open_channel(
    found: ChannelId,
    expected: ChannelId,
    topic: Option<&str>,
    token: i64,
) -> bool {
    found == expected && has_open_reservation_marker(topic, token)
}

async fn find_open_channels(
    ctx: &EventContext,
    reservation: &OpenReservation,
) -> Result<Vec<ChannelId>, ModuleError> {
    if reservation
        .settings
        .thread_channel()
        .map_err(|_| ModuleError::Config("invalid ticket thread parent".into()))?
        .is_some()
    {
        return Ok(Vec::new());
    }
    match ctx
        .api
        .execute(Action::ListChannels {
            guild_id: ctx.guild_id,
        })
        .await
        .map_err(|error| stored(&error))?
    {
        Outcome::Channels(channels) => Ok(channels
            .into_iter()
            .filter(|channel| {
                has_open_reservation_marker(channel.topic.as_deref(), reservation.token)
            })
            .map(|channel| channel.id)
            .collect()),
        _ => Ok(Vec::new()),
    }
}

async fn verify_open_channel(
    ctx: &EventContext,
    reservation: &OpenReservation,
    channel: ChannelId,
) -> Result<OpenChannelStatus, ModuleError> {
    let thread_parent = reservation
        .settings
        .thread_channel()
        .map_err(|_| ModuleError::Config("invalid ticket thread parent".into()))?;
    match ctx
        .api
        .execute(Action::GetChannel {
            channel_id: channel,
        })
        .await
    {
        Ok(Outcome::Channel(found)) => {
            let verified = if let Some(parent) = thread_parent {
                found.id == channel
                    && found.kind == ChannelType::PrivateThread
                    && found.parent_id == Some(parent)
                    && found.name.as_deref()
                        == Some(
                            format!("ticket-{:04}-r{}", reservation.number, reservation.token)
                                .as_str(),
                        )
            } else {
                matches_open_channel(found.id, channel, found.topic.as_deref(), reservation.token)
            };
            if verified {
                Ok(OpenChannelStatus::Verified)
            } else {
                Ok(OpenChannelStatus::Mismatch)
            }
        }
        Err(DiscordError::NotFound) => Ok(OpenChannelStatus::Missing),
        Ok(_) => Ok(OpenChannelStatus::Mismatch),
        Err(error) => Err(error.into()),
    }
}

async fn delete_open_channel(
    ctx: &EventContext,
    reservation: &OpenReservation,
    channel: ChannelId,
) -> Result<bool, ModuleError> {
    if verify_open_channel(ctx, reservation, channel).await? != OpenChannelStatus::Verified {
        return Ok(false);
    }
    match ctx
        .api
        .execute(Action::DeleteChannel {
            channel_id: channel,
            reason: Reason::truncated(&ctx.t_in(ctx.locale, "tickets-reason-open", &Args::new())),
        })
        .await
    {
        Ok(_) | Err(DiscordError::NotFound) => Ok(true),
        Err(error) => Err(stored(&error)),
    }
}

async fn remember_open_channel(
    ctx: &EventContext,
    user: u64,
    reservation: &OpenReservation,
    channel: ChannelId,
) -> Result<bool, ModuleError> {
    if verify_open_channel(ctx, reservation, channel).await? != OpenChannelStatus::Verified {
        return Ok(false);
    }
    let Some(token) = acquire_open_lock(ctx, user).await? else {
        return Ok(false);
    };
    let result = async {
        let (_, state) = read_open_state(ctx, user).await?;
        if !owns_open_lock(&state, token, ctx.now()) {
            return Ok(false);
        }
        let (Some(entry), Some(mut current)) =
            read_open_reservation(ctx, user, reservation.number).await?
        else {
            return Ok(false);
        };
        if current.opener != user
            || current.token != reservation.token
            || current.generation != reservation.generation
        {
            return Ok(false);
        }
        current.channel = Some(channel.get());
        match ctx
            .store()
            .replace(
                &open_reservation_key(user, reservation.number),
                &current,
                Expect::Revision(entry.revision),
                None,
            )
            .await
        {
            Ok(_) => Ok(true),
            Err(StorageError::Conflict) => Ok(false),
            Err(error) => Err(stored(&error)),
        }
    }
    .await;
    release_open_lock(ctx, user, token).await?;
    result
}

fn create_channel_error_is_definitive(error: &DiscordError) -> bool {
    matches!(
        error,
        DiscordError::Invalid(_)
            | DiscordError::CapabilityDenied(_)
            | DiscordError::Forbidden(_)
            | DiscordError::HierarchyViolation(_)
            | DiscordError::NotFound
            | DiscordError::RateLimited { .. }
            | DiscordError::Fatal(_)
    )
}

async fn cleanup_open_orphan(
    ctx: &EventContext,
    reservation: &OpenReservation,
    channel: ChannelId,
) {
    match delete_open_channel(ctx, reservation, channel).await {
        Ok(true) => {}
        Ok(false) => tracing::warn!(
            guild = %ctx.guild_id,
            channel = %channel,
            number = reservation.number,
            "ticket channel was not deleted because its reservation token did not match"
        ),
        Err(error) => tracing::warn!(
            ?error,
            guild = %ctx.guild_id,
            channel = %channel,
            number = reservation.number,
            "ticket channel cleanup will be retried"
        ),
    }
}

async fn recover_open_channel(
    ctx: &EventContext,
    reservation: &OpenReservation,
) -> Result<OpenChannelLookup, ModuleError> {
    let channels = find_open_channels(ctx, reservation).await?;
    match channels.as_slice() {
        [] => Ok(OpenChannelLookup::Missing),
        [channel] => {
            if verify_open_channel(ctx, reservation, *channel).await? == OpenChannelStatus::Verified
            {
                Ok(OpenChannelLookup::Found(*channel))
            } else {
                Ok(OpenChannelLookup::Ambiguous)
            }
        }
        _ => {
            tracing::warn!(
                guild = %ctx.guild_id,
                number = reservation.number,
                "ticket recovery found multiple channels with the same reservation token"
            );
            Ok(OpenChannelLookup::Ambiguous)
        }
    }
}

async fn open_reservation_exists(
    ctx: &EventContext,
    user: u64,
    number: i64,
) -> Result<bool, ModuleError> {
    Ok(read_open_reservation(ctx, user, number).await?.1.is_some())
}

async fn find_ticket_by_number(
    ctx: &EventContext,
    number: i64,
) -> Result<Option<(ChannelId, Ticket)>, ModuleError> {
    Ok(all_tickets(ctx)
        .await?
        .into_iter()
        .find(|(_, ticket)| ticket.number == number))
}

async fn save_open_ticket(
    ctx: &EventContext,
    user: u64,
    reservation: &OpenReservation,
    channel: ChannelId,
) -> Result<bool, ModuleError> {
    let Some(token) = acquire_open_lock(ctx, user).await? else {
        return Ok(false);
    };
    let result = save_open_ticket_locked(ctx, user, token, reservation, channel).await;
    release_open_lock(ctx, user, token).await?;
    result
}

#[allow(clippy::too_many_lines)]
async fn save_open_ticket_locked(
    ctx: &EventContext,
    user: u64,
    token: i64,
    reservation: &OpenReservation,
    channel: ChannelId,
) -> Result<bool, ModuleError> {
    let (_, state) = read_open_state(ctx, user).await?;
    if !owns_open_lock(&state, token, ctx.now()) {
        return Ok(false);
    }
    let (Some(reservation_entry), Some(mut pending)) =
        read_open_reservation(ctx, user, reservation.number).await?
    else {
        return Ok(false);
    };
    if pending.opener != user
        || pending.token != reservation.token
        || pending.generation != reservation.generation
        || !pending.api_started
        || pending.until <= ctx.now()
        || verify_open_channel(ctx, reservation, channel).await? != OpenChannelStatus::Verified
    {
        return Ok(false);
    }
    pending.channel = Some(channel.get());
    ctx.store()
        .replace(
            &open_reservation_key(user, reservation.number),
            &pending,
            Expect::Revision(reservation_entry.revision),
            None,
        )
        .await
        .map_err(|error| stored(&error))?;

    if reservation
        .settings
        .thread_channel()
        .map_err(|_| ModuleError::Config("invalid ticket thread parent".into()))?
        .is_some()
    {
        thread::add_member(ctx, channel, UserId::new(user)).await?;
    }

    let key = ticket_key(channel);
    match ctx
        .store()
        .get::<Ticket>(&key)
        .await
        .map_err(|error| stored(&error))?
    {
        Some(existing) if existing.number != reservation.number || existing.opener != user => {
            return Err(ModuleError::other("ticket channel was already assigned"));
        }
        Some(_) => {}
        None => {
            let mut ticket = ticket_from_reservation(reservation, user);
            let answers = ticket.form_answers.clone();
            let externalized = advanced_forms::externalize_form_answers(&mut ticket, &answers)
                .map_err(|error| stored(&error))?;
            if externalized {
                advanced_forms::write_ticket_form_answers(ctx, &ticket, &answers)
                    .await
                    .map_err(|error| stored(&error))?;
            }
            let ticket_size = serde_json::to_vec(&ticket)
                .map_err(|error| ModuleError::other(error.to_string()))?
                .len();
            if ticket_size > MAX_TICKET_BYTES {
                if externalized {
                    let _ = advanced_forms::delete_ticket_form_answers(ctx, &ticket).await;
                }
                return Err(ModuleError::other(
                    "ticket settings exceed the Store value limit",
                ));
            }
            if let Err(error) = ctx
                .store()
                .set_for(&key, &ticket, ticket_ttl(&ticket, ctx.now()))
                .await
            {
                if externalized
                    && let Err(cleanup_error) =
                        advanced_forms::delete_ticket_form_answers(ctx, &ticket).await
                {
                    tracing::warn!(?cleanup_error, channel = %channel, number = ticket.number, "could not clean up external ticket form answers");
                }
                return Err(stored(&error));
            }
        }
    }

    if reservation.staff_opening == Some(false)
        && let Some(panel) = reservation.panel.as_ref()
        && panel.customization.open_cooldown_seconds > 0
    {
        let cooldown_key = panel_customization::open_cooldown_key(user, &panel.id);
        let previous = ctx
            .store()
            .get::<i64>(&cooldown_key)
            .await
            .map_err(|error| stored(&error))?;
        if previous.is_none_or(|opened| opened < reservation.created) {
            let opened = ctx.now();
            ctx.store()
                .set_for(
                    &cooldown_key,
                    &opened,
                    Duration::from_secs(u64::from(panel.customization.open_cooldown_seconds)),
                )
                .await
                .map_err(|error| stored(&error))?;
        }
    }

    let (_, state) = read_open_state(ctx, user).await?;
    if !owns_open_lock(&state, token, ctx.now()) {
        return Ok(false);
    }
    let (Some(current_entry), Some(current_reservation)) =
        read_open_reservation(ctx, user, reservation.number).await?
    else {
        return Ok(false);
    };
    if current_reservation.generation != reservation.generation
        || current_reservation.token != reservation.token
    {
        return Ok(false);
    }
    ctx.store()
        .delete(&open_reservation_key(user, reservation.number))
        .await
        .map_err(|error| stored(&error))?;
    let mut tickets = all_tickets(ctx).await?;
    remove_missing_closing_tickets(ctx, user, &mut tickets).await?;
    let reservations = list_user_reservations(ctx, user).await?;
    let (open_count, _) = open_counts(user, None, &tickets, &reservations);
    set_open_score(ctx, user, open_count).await?;
    let _ = current_entry;
    cancel_open_recovery(ctx, user, reservation.number).await;
    Ok(true)
}

#[allow(clippy::too_many_lines)]
async fn announce_ticket(ctx: &EventContext, channel: ChannelId, ticket: &Ticket) {
    if let Err(error) = add_team_users_to_thread(ctx, channel, ticket).await {
        tracing::warn!(?error, %channel, "ticket team members could not be added to the thread");
    }
    if let Err(error) = post_ticket_message_quote(ctx, channel, ticket).await {
        tracing::warn!(?error, %channel, "ticket source quote could not be posted");
    }
    let config: Config = match ctx.parse_config::<Config>() {
        Ok(config) => config.for_ticket(ticket),
        Err(error) => {
            tracing::warn!(?error, guild = %ctx.guild_id, "could not read ticket settings for greeting");
            return;
        }
    };
    let saved_answers = match advanced_forms::load_ticket_form_answers(ctx, ticket).await {
        Ok(answers) => answers,
        Err(error) => {
            tracing::warn!(?error, guild = %ctx.guild_id, number = ticket.number, "could not load saved ticket answers");
            Vec::new()
        }
    };
    let answer_embeds = match advanced_forms::form_answers_embeds_in(ctx, &saved_answers) {
        Ok(embeds) => embeds,
        Err(error) => {
            tracing::warn!(?error, guild = %ctx.guild_id, number = ticket.number, "saved ticket answers could not be rendered");
            Vec::new()
        }
    };
    let mut welcome_embeds = answer_embeds.clone();
    match ticket_welcome_embed(ctx, channel, ticket, &config, &saved_answers).await {
        Ok(Some(embed)) => welcome_embeds.insert(0, embed),
        Ok(None) => {}
        Err(error) => {
            tracing::warn!(?error, channel = %channel, "custom ticket welcome embed could not be built");
        }
    }
    let welcome = ctx
        .api
        .execute(Action::PostMessage {
            channel_id: channel,
            message_id: None,
            reply_to: None,
            content: Some(
                ctx.t_in(
                    ctx.locale,
                    "tickets-welcome",
                    &Args::new()
                        .with("user", format!("<@{}>", ticket.opener))
                        .with("number", ticket.number),
                ),
            ),
            embeds: welcome_embeds,
            components: vec![ui::row(vec![
                button(ctx, "claim", ButtonStyle::Secondary, "tickets-claim-button"),
                button(ctx, "close", ButtonStyle::Danger, "tickets-close-button"),
            ])],
            files: Vec::new(),
        })
        .await;
    if let Err(error) = welcome {
        tracing::warn!(?error, guild = %ctx.guild_id, "could not greet in a new ticket");
        if !answer_embeds.is_empty()
            && let Err(error) =
                advanced_forms::send_form_answers(ctx, channel, &answer_embeds).await
        {
            tracing::warn!(?error, guild = %ctx.guild_id, "could not post saved ticket answers");
        }
    }
    if let Err(error) =
        post_ticket_knowledge_suggestions(ctx, channel, ticket, &saved_answers).await
    {
        tracing::warn!(?error, channel = %channel, "published ticket knowledge suggestions could not be posted");
    }
    let mut staff = ticket.staff_roles.as_ref().map_or_else(
        || config.staff(),
        |roles| roles.iter().copied().map(RoleId::new).collect(),
    );
    staff.sort_unstable();
    staff.dedup();
    let _ = ctx
        .api
        .execute(Action::SendMessage {
            channel_id: channel,
            content: Some(
                staff
                    .iter()
                    .map(|role| format!("<@&{role}>"))
                    .collect::<Vec<_>>()
                    .join(" "),
            ),
            embeds: Vec::new(),
            reply_to: None,
            mentions: MentionPolicy::Roles(staff),
        })
        .await;
    if config.close_after_days > 0 {
        let _ = watch(
            ctx,
            channel,
            ticket.created + i64::from(config.close_after_days) * DAY,
        )
        .await;
    }
}

async fn ticket_welcome_embed(
    ctx: &EventContext,
    channel: ChannelId,
    ticket: &Ticket,
    config: &Config,
    form_data: &[(String, String)],
) -> Result<Option<twilight_model::channel::message::Embed>, ModuleError> {
    let Some(panel) = ticket.panel.as_ref() else {
        return Ok(None);
    };
    let ticket_id =
        u64::try_from(ticket.number).map_err(|error| ModuleError::other(error.to_string()))?;
    let placeholders = integrations::resolve_welcome_placeholders(
        ctx,
        &config.integrations,
        &integrations::WelcomeContext {
            guild_id: ctx.guild_id.get(),
            user_id: ticket.opener,
            ticket_id,
            ticket_channel_id: channel.get(),
            form_data,
        },
    )
    .await;
    let mut customization = panel.customization.clone();
    let apply = |value: &mut Option<String>| {
        if let Some(value) = value.as_mut() {
            *value = integrations::apply_welcome_placeholders(value, &placeholders);
        }
    };
    apply(&mut customization.welcome_title);
    apply(&mut customization.welcome_description);
    apply(&mut customization.welcome_author_name);
    apply(&mut customization.welcome_footer_text);
    for field in &mut customization.welcome_fields {
        field.name = integrations::apply_welcome_placeholders(&field.name, &placeholders);
        field.value = integrations::apply_welcome_placeholders(&field.value, &placeholders);
    }
    customization
        .welcome_embed(ctx.now())
        .map_err(|error| ModuleError::other(error.to_string()))
}

async fn post_ticket_knowledge_suggestions(
    ctx: &EventContext,
    channel: ChannelId,
    ticket: &Ticket,
    answers: &[(String, String)],
) -> Result<(), ModuleError> {
    knowledge::schedule_ticket_suggestions(
        ctx,
        channel,
        ticket.number,
        ticket.panel.as_ref().map(|panel| panel.id.as_str()),
        answers,
    )
    .await
}

async fn add_team_users_to_thread(
    ctx: &EventContext,
    channel: ChannelId,
    ticket: &Ticket,
) -> Result<(), ModuleError> {
    if !ticket.thread {
        return Ok(());
    }
    let bot = ctx.bot();
    for user in team_staff_users(ticket.staff_access.as_ref()) {
        if user.get() == ticket.opener || Some(user) == bot {
            continue;
        }
        thread::add_member(ctx, channel, user).await?;
    }
    Ok(())
}

#[allow(clippy::too_many_lines)]
async fn post_ticket_message_quote(
    ctx: &EventContext,
    channel: ChannelId,
    ticket: &Ticket,
) -> Result<(), ModuleError> {
    let Some(quote) = ticket.message_quote.as_ref() else {
        return Ok(());
    };
    let key = ticket_key(channel);
    let current = ctx
        .store()
        .get::<Ticket>(&key)
        .await
        .map_err(|error| stored(&error))?;
    let Some(current) = current else {
        return Ok(());
    };
    if !current.is_open()
        || current.message_quote.as_ref().is_none_or(|saved| {
            saved.guild_id != quote.guild_id
                || saved.channel_id != quote.channel_id
                || saved.message_id != quote.message_id
                || saved.author_id != quote.author_id
        })
    {
        return Ok(());
    }
    if let Some(message_id) = current.message_quote_message_id {
        match ctx
            .api
            .execute(Action::GetMessage {
                channel_id: channel,
                message_id: MessageId::new(message_id),
            })
            .await
        {
            Ok(Outcome::Message(message)) if message.id.get() == message_id => return Ok(()),
            Err(DiscordError::NotFound) | Ok(_) => {}
            Err(error) => return Err(stored(&error)),
        }
    }
    let source_url = message_ticket::source_url(quote);
    let bot = ctx.bot();
    let recent = ctx
        .api
        .get_messages(channel, None, 100)
        .await
        .map_err(|error| stored(&error))?;
    let existing = recent.iter().find(|message| {
        bot.is_some_and(|bot| message.author.id.get() == bot.get())
            && message.embeds.iter().any(|embed| {
                embed
                    .description
                    .as_deref()
                    .is_some_and(|description| description.contains(&source_url))
            })
    });
    let posted_id = if let Some(message) = existing {
        message.id.get()
    } else {
        let embeds = message_ticket::quote_embeds(ctx, quote)?;
        match ctx
            .api
            .execute(Action::PostMessage {
                channel_id: channel,
                message_id: None,
                reply_to: None,
                content: None,
                embeds,
                components: Vec::new(),
                files: Vec::new(),
            })
            .await
        {
            Ok(Outcome::Message(message)) => message.id.get(),
            Ok(_) => return Err(ModuleError::other("ticket quote post returned no message")),
            Err(error) => return Err(stored(&error)),
        }
    };
    for _ in 0..5 {
        let Some(entry) = ctx
            .store()
            .entry(&key)
            .await
            .map_err(|error| stored(&error))?
        else {
            return Ok(());
        };
        let mut latest: Ticket =
            serde_json::from_value(entry.value).map_err(|error| stored(&error))?;
        if !latest.is_open()
            || latest.message_quote.as_ref().is_none_or(|saved| {
                saved.guild_id != quote.guild_id
                    || saved.channel_id != quote.channel_id
                    || saved.message_id != quote.message_id
                    || saved.author_id != quote.author_id
            })
        {
            return Ok(());
        }
        if latest.message_quote_message_id.is_some() {
            return Ok(());
        }
        latest.message_quote_message_id = Some(posted_id);
        advanced_forms::refresh_ticket_form_answer_ttl(ctx, &latest)
            .await
            .map_err(|error| stored(&error))?;
        match ctx
            .store()
            .replace(
                &key,
                &latest,
                Expect::Revision(entry.revision),
                Some(ticket_ttl(&latest, ctx.now())),
            )
            .await
        {
            Ok(_) => return Ok(()),
            Err(StorageError::Conflict) => {}
            Err(error) => return Err(stored(&error)),
        }
    }
    Err(ModuleError::other(
        "ticket quote message could not be recorded",
    ))
}

async fn release_open_reservation(
    ctx: &EventContext,
    user: u64,
    number: i64,
    generation: i64,
) -> Result<bool, ModuleError> {
    let Some(token) = acquire_open_lock(ctx, user).await? else {
        return Ok(false);
    };
    let result = async {
        let (_, state) = read_open_state(ctx, user).await?;
        if !owns_open_lock(&state, token, ctx.now()) {
            return Ok(false);
        }
        let (Some(_entry), Some(reservation)) = read_open_reservation(ctx, user, number).await?
        else {
            return Ok(false);
        };
        if reservation.generation != generation || reservation.opener != user {
            return Ok(false);
        }
        ctx.store()
            .delete(&open_reservation_key(user, number))
            .await
            .map_err(|error| stored(&error))?;
        let mut tickets = all_tickets(ctx).await?;
        remove_missing_closing_tickets(ctx, user, &mut tickets).await?;
        let reservations = list_user_reservations(ctx, user).await?;
        let (_, state) = read_open_state(ctx, user).await?;
        if !owns_open_lock(&state, token, ctx.now()) {
            return Ok(false);
        }
        let (open_count, _) = open_counts(user, None, &tickets, &reservations);
        set_open_score(ctx, user, open_count).await?;
        cancel_open_recovery(ctx, user, number).await;
        Ok(true)
    }
    .await;
    release_open_lock(ctx, user, token).await?;
    result
}

async fn release_unstarted_open_reservation(
    ctx: &EventContext,
    user: u64,
    number: i64,
    generation: i64,
) -> Result<bool, ModuleError> {
    let Some(token) = acquire_open_lock(ctx, user).await? else {
        return Ok(false);
    };
    let result = async {
        let (_, state) = read_open_state(ctx, user).await?;
        if !owns_open_lock(&state, token, ctx.now()) {
            return Ok(false);
        }
        let (Some(_entry), Some(reservation)) = read_open_reservation(ctx, user, number).await?
        else {
            return Ok(false);
        };
        if reservation.opener != user
            || reservation.generation != generation
            || reservation.api_started
            || reservation.released_at.is_some()
        {
            return Ok(false);
        }
        ctx.store()
            .delete(&open_reservation_key(user, number))
            .await
            .map_err(|error| stored(&error))?;
        let mut tickets = all_tickets(ctx).await?;
        remove_missing_closing_tickets(ctx, user, &mut tickets).await?;
        let reservations = list_user_reservations(ctx, user).await?;
        let (open_count, _) = open_counts(user, None, &tickets, &reservations);
        set_open_score(ctx, user, open_count).await?;
        cancel_open_recovery(ctx, user, number).await;
        Ok(true)
    }
    .await;
    release_open_lock(ctx, user, token).await?;
    result
}

async fn mark_open_reservation_released(
    ctx: &EventContext,
    user: u64,
    number: i64,
    generation: i64,
    actor: u64,
) -> Result<bool, ModuleError> {
    let Some(token) = acquire_open_lock(ctx, user).await? else {
        return Ok(false);
    };
    let result = async {
        let (_, state) = read_open_state(ctx, user).await?;
        if !owns_open_lock(&state, token, ctx.now()) {
            return Ok(false);
        }
        let (Some(entry), Some(mut reservation)) = read_open_reservation(ctx, user, number).await?
        else {
            return Ok(false);
        };
        if reservation.opener != user
            || reservation.generation != generation
            || reservation.released_at.is_some()
        {
            return Ok(false);
        }
        reservation.released_at = Some(ctx.now());
        reservation.released_by = Some(actor);
        reservation.panel = reservation.panel.as_ref().map(|panel| NamedPanel {
            id: panel.id.clone(),
            ..NamedPanel::default()
        });
        reservation.settings = Config::default();
        reservation.staff_roles.clear();
        reservation.form_answers.clear();
        schedule_open_recovery(ctx, user, number, ctx.now() + OPEN_RECOVERY_SECONDS).await?;
        ctx.store()
            .replace(
                &open_reservation_key(user, number),
                &reservation,
                Expect::Revision(entry.revision),
                None,
            )
            .await
            .map_err(|error| stored(&error))?;
        let mut tickets = all_tickets(ctx).await?;
        remove_missing_closing_tickets(ctx, user, &mut tickets).await?;
        let reservations = list_user_reservations(ctx, user).await?;
        let (_, state) = read_open_state(ctx, user).await?;
        if !owns_open_lock(&state, token, ctx.now()) {
            return Ok(false);
        }
        let (open_count, _) = open_counts(user, None, &tickets, &reservations);
        set_open_score(ctx, user, open_count).await?;
        Ok(true)
    }
    .await;
    release_open_lock(ctx, user, token).await?;
    result
}

async fn claim_open_recovery(
    ctx: &EventContext,
    user: u64,
    number: i64,
    force: bool,
) -> Result<Option<OpenReservation>, ModuleError> {
    let Some(token) = acquire_open_lock(ctx, user).await? else {
        if let Some(reservation) = read_open_reservation(ctx, user, number).await?.1 {
            schedule_open_recovery(
                ctx,
                user,
                number,
                (ctx.now() + OPEN_RECOVERY_SECONDS).max(reservation.until),
            )
            .await?;
        }
        return Ok(None);
    };
    let result = async {
        let (_, state) = read_open_state(ctx, user).await?;
        if !owns_open_lock(&state, token, ctx.now()) {
            schedule_open_recovery(ctx, user, number, ctx.now() + OPEN_RECOVERY_SECONDS).await?;
            return Ok(None);
        }
        let (Some(_entry), Some(mut claimed)) = read_open_reservation(ctx, user, number).await?
        else {
            cancel_open_recovery(ctx, user, number).await;
            return Ok(None);
        };
        if claimed.opener != user {
            return Err(ModuleError::other(
                "ticket opening belongs to another member",
            ));
        }
        if claimed.until > ctx.now() && !force {
            schedule_open_recovery(ctx, user, number, claimed.until).await?;
            return Ok(None);
        }
        let old_generation = claimed.generation;
        claimed.generation = claimed
            .generation
            .checked_add(1)
            .ok_or_else(|| ModuleError::other("ticket recovery generation overflowed"))?;
        claimed.attempts = claimed.attempts.saturating_add(1);
        claimed.until = ctx.now() + OPEN_LEASE_SECONDS;
        schedule_open_recovery(
            ctx,
            user,
            number,
            ctx.now() + open_recovery_delay(claimed.attempts),
        )
        .await?;
        let (_, latest_state) = read_open_state(ctx, user).await?;
        if !owns_open_lock(&latest_state, token, ctx.now()) {
            return Ok(None);
        }
        let (Some(latest_entry), Some(latest)) = read_open_reservation(ctx, user, number).await?
        else {
            return Ok(None);
        };
        if latest.opener != user || latest.generation != old_generation {
            return Ok(None);
        }
        match ctx
            .store()
            .replace(
                &open_reservation_key(user, number),
                &claimed,
                Expect::Revision(latest_entry.revision),
                None,
            )
            .await
        {
            Ok(_) => Ok(Some(claimed)),
            Err(StorageError::Conflict) => Ok(None),
            Err(error) => Err(stored(&error)),
        }
    }
    .await;
    release_open_lock(ctx, user, token).await?;
    result
}

#[allow(clippy::too_many_lines)]
async fn open_recovery_job(cx: JobCx) -> Result<(), ModuleError> {
    let job: OpenRecovery = cx.payload()?;
    let ctx = cx.ctx();
    let Some(reservation) = claim_open_recovery(ctx, job.opener, job.number, job.force).await?
    else {
        schedule_open_recovery(
            ctx,
            job.opener,
            job.number,
            ctx.now() + OPEN_RECOVERY_SECONDS,
        )
        .await?;
        if reconcile_open_counter(ctx, job.opener).await? {
            cancel_open_recovery(ctx, job.opener, job.number).await;
        }
        return Ok(());
    };
    if !reservation.api_started && reservation.released_at.is_none() {
        let _ =
            release_open_reservation(ctx, job.opener, reservation.number, reservation.generation)
                .await?;
        return Ok(());
    }
    if reservation.released_at.is_some() {
        let mut channel_confirmed_gone = false;
        if let Some(channel) = reservation.channel.map(ChannelId::new) {
            match verify_open_channel(ctx, &reservation, channel).await? {
                OpenChannelStatus::Verified => {
                    if delete_open_channel(ctx, &reservation, channel).await? {
                        let _ = release_open_reservation(
                            ctx,
                            job.opener,
                            reservation.number,
                            reservation.generation,
                        )
                        .await?;
                    }
                    return Ok(());
                }
                OpenChannelStatus::Mismatch => tracing::warn!(
                    guild = %ctx.guild_id,
                    channel = %channel,
                    number = reservation.number,
                    "released ticket reservation no longer matches its saved channel"
                ),
                OpenChannelStatus::Missing => channel_confirmed_gone = true,
            }
        }
        match recover_open_channel(ctx, &reservation).await? {
            OpenChannelLookup::Found(channel) => {
                if remember_open_channel(ctx, job.opener, &reservation, channel).await?
                    && delete_open_channel(ctx, &reservation, channel).await?
                {
                    let _ = release_open_reservation(
                        ctx,
                        job.opener,
                        reservation.number,
                        reservation.generation,
                    )
                    .await?;
                }
            }
            OpenChannelLookup::Missing if channel_confirmed_gone => {
                let _ = release_open_reservation(
                    ctx,
                    job.opener,
                    reservation.number,
                    reservation.generation,
                )
                .await?;
            }
            OpenChannelLookup::Missing | OpenChannelLookup::Ambiguous => {}
        }
        return Ok(());
    }
    if let Some(channel) = reservation.channel.map(ChannelId::new) {
        match verify_open_channel(ctx, &reservation, channel).await? {
            OpenChannelStatus::Verified => {
                if save_open_ticket(ctx, job.opener, &reservation, channel).await? {
                    let ticket = ticket_from_reservation(&reservation, job.opener);
                    announce_ticket(ctx, channel, &ticket).await;
                }
                return Ok(());
            }
            OpenChannelStatus::Missing => {
                let _ = release_open_reservation(
                    ctx,
                    job.opener,
                    reservation.number,
                    reservation.generation,
                )
                .await?;
                return Ok(());
            }
            OpenChannelStatus::Mismatch => {
                tracing::warn!(
                    guild = %ctx.guild_id,
                    channel = %channel,
                    number = reservation.number,
                    "ticket channel ID no longer matches its reservation token"
                );
            }
        }
    }
    if let OpenChannelLookup::Found(channel) = recover_open_channel(ctx, &reservation).await?
        && save_open_ticket(ctx, job.opener, &reservation, channel).await?
    {
        let ticket = ticket_from_reservation(&reservation, job.opener);
        announce_ticket(ctx, channel, &ticket).await;
    }
    Ok(())
}

async fn openings_command(cx: CommandCx, args: OpeningsArgs) -> Result<(), UserError> {
    if cx.require("tickets.manage").await.is_err() {
        return cx
            .reply_private_t("tickets-openings-manage-only", &Args::new())
            .await;
    }
    let mut reservations = all_open_reservations(cx.ctx())
        .await
        .map_err(|error| failed(&error))?;
    reservations.sort_by_key(|(user, reservation)| (reservation.number, *user));
    if reservations.is_empty() {
        return cx
            .reply_private_t("tickets-openings-empty", &Args::new())
            .await;
    }

    let pages = reservations.len().div_ceil(OPENINGS_PAGE_SIZE);
    let page_count = i64::try_from(pages).map_err(|error| failed(&error))?;
    let total = i64::try_from(reservations.len()).map_err(|error| failed(&error))?;
    let requested_page = args.page.unwrap_or(1);
    let start = if requested_page > 0 {
        usize::try_from(requested_page - 1)
            .ok()
            .and_then(|index| index.checked_mul(OPENINGS_PAGE_SIZE))
    } else {
        None
    };
    let Some(start) = start.filter(|start| *start < reservations.len()) else {
        return cx
            .reply_private_t(
                "tickets-openings-invalid-page",
                &Args::new().with("pages", page_count),
            )
            .await;
    };
    let end = start
        .saturating_add(OPENINGS_PAGE_SIZE)
        .min(reservations.len());
    let entries = reservations[start..end]
        .iter()
        .map(|(user, reservation)| {
            let panel = reservation
                .panel
                .as_ref()
                .map_or("default", |panel| panel.id.as_str());
            let status = reservation.released_at.map_or_else(
                || {
                    if reservation.api_started {
                        "checking".to_owned()
                    } else {
                        "not started".to_owned()
                    }
                },
                |released| {
                    let actor = reservation.released_by.unwrap_or_default();
                    if actor == 0 {
                        "released".to_owned()
                    } else {
                        format!("released by <@{actor}> at <t:{released}:R>")
                    }
                },
            );
            let channel = reservation
                .channel
                .map_or_else(|| "-".to_owned(), |channel| channel.to_string());
            format!(
                "#{} · <@{}> · {} · {} · channel {} · token {}",
                reservation.number, user, panel, status, channel, reservation.token
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    let shown = i64::try_from(end - start).map_err(|error| failed(&error))?;
    cx.reply_private_t(
        "tickets-openings-list",
        &Args::new()
            .with("shown", shown)
            .with("total", total)
            .with("page", requested_page)
            .with("pages", page_count)
            .with("entries", entries),
    )
    .await
}

async fn recover_open_command(cx: CommandCx, args: OpenReservationArgs) -> Result<(), UserError> {
    if cx.require("tickets.manage").await.is_err() {
        return cx
            .reply_private_t("tickets-openings-manage-only", &Args::new())
            .await;
    }
    if args.number <= 0 {
        return cx
            .reply_private_t("tickets-openings-not-found", &Args::new())
            .await;
    }
    let Some((user, _)) = find_open_reservation(cx.ctx(), args.number)
        .await
        .map_err(|error| failed(&error))?
    else {
        return cx
            .reply_private_t("tickets-openings-not-found", &Args::new())
            .await;
    };
    force_open_recovery(cx.ctx(), user, args.number)
        .await
        .map_err(|error| failed(&error))?;
    cx.reply_private_t(
        "tickets-openings-retry-queued",
        &Args::new().with("number", args.number),
    )
    .await
}

#[allow(
    clippy::too_many_lines,
    reason = "keeps manager verification and release outcomes together"
)]
async fn release_open_command(cx: CommandCx, args: OpenReservationArgs) -> Result<(), UserError> {
    if cx.require("tickets.manage").await.is_err() {
        return cx
            .reply_private_t("tickets-openings-manage-only", &Args::new())
            .await;
    }
    if args.number <= 0 {
        return cx
            .reply_private_t("tickets-openings-not-found", &Args::new())
            .await;
    }
    let ctx = cx.ctx();
    let Some((user, reservation)) = find_open_reservation(ctx, args.number)
        .await
        .map_err(|error| failed(&error))?
    else {
        return cx
            .reply_private_t("tickets-openings-not-found", &Args::new())
            .await;
    };
    if reservation.released_at.is_some() {
        return cx
            .reply_private_t(
                "tickets-openings-already-released",
                &Args::new().with("number", args.number),
            )
            .await;
    }
    if !reservation.api_started {
        if release_unstarted_open_reservation(ctx, user, args.number, reservation.generation)
            .await
            .map_err(|error| failed(&error))?
        {
            return cx
                .reply_private_t(
                    "tickets-openings-released-safe",
                    &Args::new().with("number", args.number),
                )
                .await;
        }
        return cx
            .reply_private_t("tickets-openings-release-refused", &Args::new())
            .await;
    }
    if reservation.channel.is_none() && reservation.settings.thread_channel()?.is_some() {
        return cx
            .reply_private_t("tickets-openings-release-refused", &Args::new())
            .await;
    }

    let mut channel_confirmed_gone = false;
    let mut channel_mismatch = false;
    if let Some(channel) = reservation.channel.map(ChannelId::new) {
        match verify_open_channel(ctx, &reservation, channel)
            .await
            .map_err(|error| failed(&error))?
        {
            OpenChannelStatus::Verified => {
                let saved = save_open_ticket(ctx, user, &reservation, channel)
                    .await
                    .map_err(|error| failed(&error))?;
                if saved {
                    let ticket = ticket_from_reservation(&reservation, user);
                    announce_ticket(ctx, channel, &ticket).await;
                }
                return cx
                    .reply_private_t(
                        "tickets-openings-channel-found",
                        &Args::new()
                            .with("number", args.number)
                            .with("channel", format!("<#{channel}>")),
                    )
                    .await;
            }
            OpenChannelStatus::Mismatch => channel_mismatch = true,
            OpenChannelStatus::Missing => channel_confirmed_gone = true,
        }
    }

    match recover_open_channel(ctx, &reservation)
        .await
        .map_err(|error| failed(&error))?
    {
        OpenChannelLookup::Found(channel) => {
            let saved = save_open_ticket(ctx, user, &reservation, channel)
                .await
                .map_err(|error| failed(&error))?;
            if saved {
                let ticket = ticket_from_reservation(&reservation, user);
                announce_ticket(ctx, channel, &ticket).await;
            }
            cx.reply_private_t(
                "tickets-openings-channel-found",
                &Args::new()
                    .with("number", args.number)
                    .with("channel", format!("<#{channel}>")),
            )
            .await
        }
        OpenChannelLookup::Missing if channel_confirmed_gone => {
            if release_open_reservation(ctx, user, reservation.number, reservation.generation)
                .await
                .map_err(|error| failed(&error))?
            {
                return cx
                    .reply_private_t(
                        "tickets-openings-released-safe",
                        &Args::new().with("number", reservation.number),
                    )
                    .await;
            }
            cx.reply_private_t("tickets-openings-release-refused", &Args::new())
                .await
        }
        OpenChannelLookup::Missing | OpenChannelLookup::Ambiguous => {
            let actor = cx.user().id.get();
            if !mark_open_reservation_released(
                ctx,
                user,
                reservation.number,
                reservation.generation,
                actor,
            )
            .await
            .map_err(|error| failed(&error))?
            {
                return cx
                    .reply_private_t("tickets-openings-release-refused", &Args::new())
                    .await;
            }
            tracing::warn!(
                guild = %ctx.guild_id,
                opener = user,
                number = reservation.number,
                actor,
                channel_mismatch,
                "manager released an unconfirmed ticket opening; cleanup record retained"
            );
            cx.reply_private_t(
                "tickets-openings-released",
                &Args::new().with("number", reservation.number),
            )
            .await
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn reserve_open(
    ctx: &EventContext,
    user: u64,
    username: String,
    nickname: Option<String>,
    panel: Option<NamedPanel>,
    settings: Config,
    staff_roles: Vec<u64>,
    form_answers: Vec<(String, String)>,
    form_modal_revision: u64,
    staff_access: Option<teams_survey::StaffAccessSnapshot>,
    message_quote: Option<message_ticket::MessageQuote>,
    global_limit: u32,
    panel_limit: u32,
    staff: bool,
) -> Result<OpenReservationResult, ModuleError> {
    let Some(token) = acquire_open_lock(ctx, user).await? else {
        return Ok(OpenReservationResult::Busy);
    };
    let result = reserve_open_locked(
        ctx,
        user,
        username,
        nickname,
        panel,
        settings,
        staff_roles,
        form_answers,
        form_modal_revision,
        staff_access,
        message_quote,
        global_limit,
        panel_limit,
        staff,
        token,
    )
    .await;
    release_open_lock(ctx, user, token).await?;
    result
}

#[allow(
    clippy::too_many_lines,
    reason = "keeps counting, fencing and sharded reservation persistence in one critical flow"
)]
#[allow(clippy::too_many_arguments)]
async fn reserve_open_locked(
    ctx: &EventContext,
    user: u64,
    username: String,
    nickname: Option<String>,
    panel: Option<NamedPanel>,
    mut settings: Config,
    staff_roles: Vec<u64>,
    form_answers: Vec<(String, String)>,
    form_modal_revision: u64,
    staff_access: Option<teams_survey::StaffAccessSnapshot>,
    message_quote: Option<message_ticket::MessageQuote>,
    global_limit: u32,
    panel_limit: u32,
    staff: bool,
    token: i64,
) -> Result<OpenReservationResult, ModuleError> {
    settings.panels.clear();
    let (_, state) = read_open_state(ctx, user).await?;
    if !owns_open_lock(&state, token, ctx.now()) {
        return Ok(OpenReservationResult::Busy);
    }
    let mut tickets = all_tickets(ctx).await?;
    remove_missing_closing_tickets(ctx, user, &mut tickets).await?;
    let reservations = list_user_reservations(ctx, user).await?;
    let (_, state) = read_open_state(ctx, user).await?;
    if !owns_open_lock(&state, token, ctx.now()) {
        return Ok(OpenReservationResult::Busy);
    }
    let panel_id = panel.as_ref().map(|panel| panel.id.as_str());
    let (open_count, panel_count) = open_counts(user, panel_id, &tickets, &reservations);
    if !staff
        && let Some(panel) = panel.as_ref()
        && panel.customization.open_cooldown_seconds > 0
    {
        if reservations.iter().any(|reservation| {
            reservation.released_at.is_none()
                && reservation
                    .panel
                    .as_ref()
                    .is_some_and(|pending| pending.id == panel.id)
        }) {
            return Ok(OpenReservationResult::Pending);
        }
        let last_opened = ctx
            .store()
            .get::<i64>(&panel_customization::open_cooldown_key(user, &panel.id))
            .await
            .map_err(|error| stored(&error))?;
        let remaining = panel
            .customization
            .cooldown_remaining(last_opened, ctx.now());
        if remaining > 0 {
            return Ok(OpenReservationResult::Cooldown(remaining));
        }
    }
    set_open_score(ctx, user, open_count).await?;
    if !staff {
        if global_limit > 0 && open_count >= u64::from(global_limit) {
            return Ok(
                if reservations
                    .iter()
                    .all(|reservation| reservation.released_at.is_some())
                {
                    OpenReservationResult::GlobalLimit(global_limit)
                } else {
                    OpenReservationResult::Pending
                },
            );
        }
        if panel_limit > 0 && panel_count >= u64::from(panel_limit) {
            return Ok(
                if reservations
                    .iter()
                    .all(|reservation| reservation.released_at.is_some())
                {
                    OpenReservationResult::PanelLimit(panel_limit)
                } else {
                    OpenReservationResult::Pending
                },
            );
        }
    }

    let number = ctx
        .store()
        .score_add(NUMBER, 0_u64, 1)
        .await
        .map_err(|error| stored(&error))?;
    let thread_ticket = settings
        .thread_channel
        .as_ref()
        .and_then(|parent| id_of(Some(parent)))
        .is_some();
    let channel_name = panel.as_ref().map_or_else(
        || {
            if thread_ticket {
                format!("ticket-{number:04}-r{token}")
            } else {
                format!("ticket-{number:04}")
            }
        },
        |panel| {
            panel.customization.channel_name(
                number,
                &username,
                nickname.as_deref(),
                thread_ticket.then_some(token),
            )
        },
    );
    let reservation = OpenReservation {
        opener: user,
        number,
        token,
        created: ctx.now(),
        panel,
        settings,
        staff_roles,
        form_answers,
        form_modal_revision: Some(form_modal_revision),
        staff_access,
        message_quote,
        channel_name,
        staff_opening: Some(staff),
        generation: 1,
        until: ctx.now() + OPEN_LEASE_SECONDS,
        channel: None,
        api_started: false,
        attempts: 0,
        released_at: None,
        released_by: None,
    };
    let ticket_size = serde_json::to_vec(&ticket_from_reservation(&reservation, user))
        .map_err(|error| ModuleError::other(error.to_string()))?
        .len();
    let mut reservation_size_check = reservation.clone();
    reservation_size_check.generation = i64::MAX;
    reservation_size_check.until = i64::MAX;
    reservation_size_check.channel = Some(u64::MAX);
    reservation_size_check.attempts = u32::MAX;
    reservation_size_check.form_modal_revision = Some(u64::MAX);
    let reservation_size = serde_json::to_vec(&reservation_size_check)
        .map_err(|error| ModuleError::other(error.to_string()))?
        .len();
    if ticket_size > MAX_TICKET_BYTES || reservation_size > OPEN_RESERVATION_MAX_BYTES {
        return Ok(OpenReservationResult::TooLarge);
    }
    schedule_open_recovery(ctx, user, number, reservation.until).await?;

    if !staff {
        let mut tickets = all_tickets(ctx).await?;
        remove_missing_closing_tickets(ctx, user, &mut tickets).await?;
        let reservations = list_user_reservations(ctx, user).await?;
        let (_, state) = read_open_state(ctx, user).await?;
        if !owns_open_lock(&state, token, ctx.now()) {
            cancel_open_recovery(ctx, user, number).await;
            return Ok(OpenReservationResult::Busy);
        }
        let (open_count, panel_count) = open_counts(
            user,
            reservation.panel.as_ref().map(|panel| panel.id.as_str()),
            &tickets,
            &reservations,
        );
        if global_limit > 0 && open_count >= u64::from(global_limit) {
            cancel_open_recovery(ctx, user, number).await;
            return Ok(
                if reservations
                    .iter()
                    .all(|pending| pending.released_at.is_some())
                {
                    OpenReservationResult::GlobalLimit(global_limit)
                } else {
                    OpenReservationResult::Pending
                },
            );
        }
        if panel_limit > 0 && panel_count >= u64::from(panel_limit) {
            cancel_open_recovery(ctx, user, number).await;
            return Ok(
                if reservations
                    .iter()
                    .all(|pending| pending.released_at.is_some())
                {
                    OpenReservationResult::PanelLimit(panel_limit)
                } else {
                    OpenReservationResult::Pending
                },
            );
        }
        set_open_score(ctx, user, open_count).await?;
    }

    let (_, state) = read_open_state(ctx, user).await?;
    if !owns_open_lock(&state, token, ctx.now()) {
        cancel_open_recovery(ctx, user, number).await;
        return Ok(OpenReservationResult::Busy);
    }
    match ctx
        .store()
        .replace(
            &open_reservation_key(user, number),
            &reservation,
            Expect::Missing,
            None,
        )
        .await
    {
        Ok(_) => {}
        Err(error) => {
            cancel_open_recovery(ctx, user, number).await;
            return Err(stored(&error));
        }
    }
    let (_, state) = read_open_state(ctx, user).await?;
    if !owns_open_lock(&state, token, ctx.now()) {
        let _ = release_open_reservation(ctx, user, number, reservation.generation).await?;
        return Ok(OpenReservationResult::Pending);
    }
    ctx.store()
        .score_add(OPEN, user, 1)
        .await
        .map_err(|error| stored(&error))?;
    Ok(OpenReservationResult::Reserved(reservation))
}

async fn mark_open_request_started(
    ctx: &EventContext,
    user: u64,
    reservation: &OpenReservation,
) -> Result<bool, ModuleError> {
    let Some(token) = acquire_open_lock(ctx, user).await? else {
        return Ok(false);
    };
    let result = async {
        let (_, state) = read_open_state(ctx, user).await?;
        if !owns_open_lock(&state, token, ctx.now()) {
            return Ok(false);
        }
        let (Some(entry), Some(mut pending)) =
            read_open_reservation(ctx, user, reservation.number).await?
        else {
            return Ok(false);
        };
        if pending.opener != user
            || pending.token != reservation.token
            || pending.generation != reservation.generation
            || pending.until <= ctx.now()
            || pending.api_started
            || pending.released_at.is_some()
        {
            return Ok(false);
        }
        pending.form_answers = reservation.form_answers.clone();
        let ticket_size = serde_json::to_vec(&ticket_from_reservation(&pending, user))
            .map_err(|error| ModuleError::other(error.to_string()))?
            .len();
        let reservation_size = serde_json::to_vec(&pending)
            .map_err(|error| ModuleError::other(error.to_string()))?
            .len();
        if ticket_size > MAX_TICKET_BYTES || reservation_size > OPEN_RESERVATION_MAX_BYTES {
            return Ok(false);
        }
        pending.api_started = true;
        match ctx
            .store()
            .replace(
                &open_reservation_key(user, reservation.number),
                &pending,
                Expect::Revision(entry.revision),
                None,
            )
            .await
        {
            Ok(_) => Ok(true),
            Err(StorageError::Conflict) => Ok(false),
            Err(error) => Err(stored(&error)),
        }
    }
    .await;
    release_open_lock(ctx, user, token).await?;
    result
}

fn display_label(value: &str) -> Option<String> {
    if value.chars().any(char::is_control) {
        return None;
    }
    let label = value.trim();
    (!label.is_empty() && label.chars().count() <= LABEL_MAX_CHARS).then(|| label.to_owned())
}

fn label_key(label: &str) -> String {
    label.trim().to_lowercase()
}

fn label_pages(labels: &[String]) -> Vec<String> {
    let mut pages = Vec::new();
    let mut page = String::new();
    let mut units = 0;
    for label in labels {
        let label = label.replace('@', "@\u{200b}");
        let label_units = label.encode_utf16().count();
        if !page.is_empty() && units + 2 + label_units > LABEL_LIST_UTF16_UNITS {
            pages.push(std::mem::take(&mut page));
            units = 0;
        }
        if !page.is_empty() {
            page.push_str(", ");
            units += 2;
        }
        page.push_str(&label);
        units += label_units;
    }
    if !page.is_empty() {
        pages.push(page);
    }
    pages
}

fn history_number_prefix(number: i64) -> String {
    format!(
        "{HISTORY_PREFIX}{:019}:",
        i64::MAX.saturating_sub(number.max(0))
    )
}

fn history_key(number: i64, channel: ChannelId) -> String {
    format!("{}{}", history_number_prefix(number), channel.get())
}

fn history_ttl_until(expires_at: i64, now: i64) -> Result<Duration, ModuleError> {
    let seconds = expires_at
        .checked_sub(now)
        .filter(|seconds| *seconds > 0)
        .ok_or_else(|| ModuleError::other("ticket history has expired"))?;
    let seconds = u64::try_from(seconds).map_err(|error| ModuleError::other(error.to_string()))?;
    Ok(Duration::from_secs(seconds))
}

fn history_ttl(history: &TicketHistory, now: i64) -> Result<Duration, ModuleError> {
    let expires_at = if history.expires_at > 0 {
        history.expires_at
    } else {
        let ttl = i64::try_from(HISTORY_TTL.as_secs())
            .map_err(|error| ModuleError::other(error.to_string()))?;
        history
            .closed
            .checked_add(ttl)
            .ok_or_else(|| ModuleError::other("ticket history expiry is out of range"))?
    };
    history_ttl_until(expires_at, now)
}

fn history_expires_at(now: i64) -> Result<i64, ModuleError> {
    let ttl = i64::try_from(HISTORY_TTL.as_secs())
        .map_err(|error| ModuleError::other(error.to_string()))?;
    now.checked_add(ttl)
        .ok_or_else(|| ModuleError::other("ticket history expiry is out of range"))
}

fn history_id_key(channel: ChannelId) -> String {
    format!("{HISTORY_ID_PREFIX}{}", channel.get())
}

fn history_content_key(channel: u64, attempt: i64, page: usize) -> String {
    format!("{HISTORY_CONTENT_PREFIX}{channel}:{attempt}:{page:04}")
}

fn history_index_key(channel: u64, attempt: i64, page: usize) -> String {
    format!("{HISTORY_INDEX_PREFIX}{channel}:{attempt}:{page:04}")
}

fn transcript_page_ranges(text: &str) -> Vec<(usize, usize)> {
    let mut pages = Vec::new();
    let mut start = 0;
    while start < text.len() {
        let mut end = start.saturating_add(HISTORY_PAGE_BYTES).min(text.len());
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        while serde_json::to_vec(&text[start..end])
            .map_or(true, |value| value.len() > HISTORY_VALUE_BYTES)
        {
            end = start + (end - start) * 3 / 4;
            while end > start && !text.is_char_boundary(end) {
                end -= 1;
            }
            if end == start {
                end = start + 1;
                while end < text.len() && !text.is_char_boundary(end) {
                    end += 1;
                }
            }
        }
        pages.push((start, end));
        start = end;
    }
    pages
}

fn note_pages_prefix(channel: ChannelId) -> String {
    format!("ticket-notes:{}:", channel.get())
}

fn note_page_key(channel: ChannelId, page: usize) -> String {
    format!("{}{:04}", note_pages_prefix(channel), page)
}

fn note_page_number(key: &str, prefix: &str) -> Option<usize> {
    key.strip_prefix(prefix)?.parse().ok()
}

fn idle_key(channel: ChannelId) -> String {
    format!("idle-{}", channel.get())
}

fn close_request_timeout_key(channel: ChannelId, request_id: i64) -> String {
    format!("close-request-timeout-{}-{request_id}", channel.get())
}

async fn watch_close_request_timeout(
    ctx: &EventContext,
    channel: ChannelId,
    request_id: i64,
    expires_at: i64,
    at: i64,
) -> Result<(), ModuleError> {
    ctx.scheduler()
        .job(
            CLOSE_REQUEST_TIMEOUT,
            &CloseRequestTimeout {
                channel: channel.get(),
                request_id,
                expires_at,
            },
        )
        .map_err(|error| stored(&error))?
        .key(&close_request_timeout_key(channel, request_id))
        .at(at)
        .await
        .map_err(|error| stored(&error))
}

async fn cancel_close_request_timeout(
    ctx: &EventContext,
    channel: ChannelId,
    request: &CloseRequest,
) {
    if request.expires_at.is_none() {
        return;
    }
    if let Err(error) = ctx
        .scheduler()
        .cancel(&close_request_timeout_key(channel, request.id))
        .await
    {
        tracing::warn!(
            ?error,
            channel = %channel,
            request_id = request.id,
            "could not cancel a close request timeout"
        );
    }
}

fn left_close_key(channel: ChannelId) -> String {
    format!("left-close-{}", channel.get())
}

fn failed(error: &impl ToString) -> UserError {
    UserError::from(ModuleError::other(error.to_string()))
}

fn stored(error: &impl ToString) -> ModuleError {
    ModuleError::other(error.to_string())
}

async fn say<I: InteractionRef + 'static>(
    cx: &Cx<I>,
    key: &str,
    args: &Args,
) -> Result<(), UserError> {
    cx.reply_with(InteractionReply {
        content: Some(cx.t(key, args)),
        embeds: Vec::new(),
        ephemeral: true,
        components: Vec::new(),
    })
    .await
}

fn button(ctx: &EventContext, route: &str, style: ButtonStyle, label: &str) -> Component {
    let custom_id = CustomId::new(route).encode(&ctx.module);
    let custom_id = custom_id.strip_suffix(':').unwrap_or(&custom_id).to_owned();
    ui::button(style, ctx.t_in(ctx.locale, label, &Args::new()), custom_id)
}

fn close_request_button(
    ctx: &EventContext,
    route: &str,
    style: ButtonStyle,
    label: &str,
    channel: ChannelId,
    request_id: i64,
) -> Component {
    ui::button(
        style,
        ctx.t_in(ctx.locale, label, &Args::new()),
        CustomId::new(route)
            .data(format!("{}:{request_id}", channel.get()))
            .encode(&ctx.module),
    )
}

fn rating_button_id(
    module: &ModuleId,
    opener: UserId,
    channel: ChannelId,
    number: i64,
    attempt: i64,
    score: u8,
) -> String {
    CustomId::new("rate")
        .only_for(opener)
        .data(format!("{}:{number}:{attempt}:{score}", channel.get()))
        .encode(module)
}

fn parse_rating_request(data: &str) -> Option<RatingRequest> {
    let mut fields = data.split(':');
    let request = RatingRequest {
        channel: fields.next()?.parse().ok()?,
        number: fields.next()?.parse().ok()?,
        attempt: fields.next()?.parse().ok()?,
        score: fields.next()?.parse().ok()?,
    };
    (fields.next().is_none()
        && request.channel > 0
        && request.number > 0
        && request.attempt > 0
        && (1..=5).contains(&request.score))
    .then_some(request)
}

fn rating_components(
    ctx: &EventContext,
    channel: ChannelId,
    ticket: &Ticket,
    attempt: i64,
) -> Vec<Component> {
    vec![ui::row(
        (1_u8..=5)
            .map(|score| {
                ui::button(
                    ButtonStyle::Primary,
                    format!("{score} ★"),
                    rating_button_id(
                        &ctx.module,
                        UserId::new(ticket.opener),
                        channel,
                        ticket.number,
                        attempt,
                        score,
                    ),
                )
            })
            .collect::<Vec<_>>(),
    )]
}

async fn send_rating_prompt(
    ctx: &EventContext,
    channel: ChannelId,
    ticket: &Ticket,
    attempt: i64,
) -> bool {
    // Claim before sending so a lost success response cannot make a recovery retry send a duplicate.
    match claim_rating_prompt(ctx, channel, ticket.number, ticket.opener, attempt).await {
        Ok(true) => {}
        Ok(false) => return false,
        Err(error) => {
            tracing::warn!(?error, channel = %channel, "could not claim the ticket rating prompt");
            return false;
        }
    }
    match ctx
        .api
        .execute(Action::SendDirectMessage {
            guild_id: ctx.guild_id,
            user_id: UserId::new(ticket.opener),
            content: Some(ctx.t_in(
                ctx.locale,
                "tickets-rating-prompt",
                &Args::new().with("number", ticket.number),
            )),
            embeds: Vec::new(),
            components: rating_components(ctx, channel, ticket, attempt),
            files: Vec::new(),
        })
        .await
    {
        Ok(Outcome::Message(_)) => true,
        Ok(_) => {
            tracing::warn!(
                guild = %ctx.guild_id,
                channel = %channel,
                opener = ticket.opener,
                "ticket rating prompt returned no message"
            );
            false
        }
        Err(error) => {
            tracing::warn!(
                ?error,
                guild = %ctx.guild_id,
                channel = %channel,
                opener = ticket.opener,
                "could not confirm the ticket rating DM"
            );
            false
        }
    }
}

#[allow(clippy::too_many_lines)]
async fn send_history_link_dm(ctx: &EventContext, history: &TicketHistory) -> bool {
    if history.status != HistoryStatus::Closed || history.opener_erased {
        return false;
    }
    let channel = ChannelId::new(history.channel_id);
    let key = history_key(history.number, channel);
    let current = match ctx.store().get::<TicketHistory>(&key).await {
        Ok(Some(current))
            if current.status == HistoryStatus::Closed
                && current.number == history.number
                && current.channel_id == history.channel_id
                && current.attempt == history.attempt
                && current.opener == history.opener
                && !current.opener_erased
                && current.privacy_generation == history.privacy_generation =>
        {
            current
        }
        Ok(_) => return false,
        Err(error) => {
            tracing::warn!(
                ?error,
                number = history.number,
                "could not read ticket history for owner link"
            );
            return false;
        }
    };
    match data::history_privacy_generation_current(ctx, &key, current.privacy_generation).await {
        Ok(true) => {}
        Ok(false) => return false,
        Err(error) => {
            tracing::warn!(
                ?error,
                number = current.number,
                "could not validate privacy for ticket history link"
            );
            return false;
        }
    }
    let ttl = match history_ttl(&current, ctx.now()) {
        Ok(ttl) => ttl,
        Err(error) => {
            tracing::warn!(
                ?error,
                number = current.number,
                "could not determine ticket history link retention"
            );
            return false;
        }
    };
    let marker = format!(
        "{HISTORY_LINK_PREFIX}{}:{}:{}",
        current.channel_id, current.number, current.attempt
    );
    match ctx
        .store()
        .replace(&marker, &true, Expect::Missing, Some(ttl))
        .await
    {
        Ok(_) => {}
        Err(StorageError::Conflict) => return false,
        Err(error) => {
            tracing::warn!(
                ?error,
                number = current.number,
                "could not claim ticket history link DM"
            );
            return false;
        }
    }

    let message = match ctx.dashboard_ticket_history_url(current.channel_id) {
        Some(url) => ctx.t_in(
            ctx.locale,
            "tickets-history-dashboard-link",
            &Args::new().with("url", url),
        ),
        None => ctx.t_in(
            ctx.locale,
            "tickets-history-command-fallback",
            &Args::new().with("number", current.number),
        ),
    };
    match ctx
        .api
        .execute(Action::SendDirectMessage {
            guild_id: ctx.guild_id,
            user_id: UserId::new(current.opener),
            content: Some(message),
            embeds: Vec::new(),
            components: Vec::new(),
            files: Vec::new(),
        })
        .await
    {
        Ok(Outcome::Message(_)) => true,
        Ok(_) => {
            tracing::warn!(
                number = current.number,
                "ticket history link DM returned no message"
            );
            false
        }
        Err(error) => {
            tracing::warn!(
                ?error,
                number = current.number,
                "could not send ticket history link DM"
            );
            false
        }
    }
}

async fn claim_rating_prompt(
    ctx: &EventContext,
    channel: ChannelId,
    number: i64,
    opener: u64,
    attempt: i64,
) -> Result<bool, ModuleError> {
    let pointer_key = history_id_key(channel);
    let Some(key) = ctx
        .store()
        .get::<String>(&pointer_key)
        .await
        .map_err(|error| stored(&error))?
    else {
        return Ok(false);
    };
    if !key.starts_with(HISTORY_PREFIX) {
        return Err(ModuleError::other("ticket history pointer is invalid"));
    }
    for _ in 0..5 {
        let Some(entry) = ctx
            .store()
            .entry(&key)
            .await
            .map_err(|error| stored(&error))?
        else {
            return Ok(false);
        };
        let mut history: TicketHistory = serde_json::from_value(entry.value)
            .map_err(|error| ModuleError::other(error.to_string()))?;
        if history.version != 1
            || history.channel_id != channel.get()
            || history.number != number
            || history.opener != opener
            || history.attempt != attempt
            || history.status != HistoryStatus::Closed
            || history.rating.is_some()
            || history.rating_prompt_attempted
        {
            return Ok(false);
        }
        let ttl = history_ttl(&history, ctx.now())?;
        history.rating_prompt_attempted = true;
        match ctx
            .store()
            .replace(&key, &history, Expect::Revision(entry.revision), Some(ttl))
            .await
        {
            Ok(_) => return Ok(true),
            Err(StorageError::Conflict) => {}
            Err(error) => return Err(stored(&error)),
        }
    }
    Err(ModuleError::other(
        "ticket rating prompt changed while it was being claimed",
    ))
}

async fn submit_rating(
    ctx: &EventContext,
    channel: ChannelId,
    number: i64,
    attempt: i64,
    user: UserId,
    score: u8,
) -> Result<RatingSubmission, UserError> {
    if number <= 0 || attempt <= 0 || !(1..=5).contains(&score) {
        return Ok(RatingSubmission::Stale);
    }
    let pointer_key = history_id_key(channel);
    let history_key = match ctx.store().get::<String>(&pointer_key).await {
        Ok(Some(key)) if key.starts_with(HISTORY_PREFIX) => key,
        Ok(Some(_)) => return Err(UserError::message("tickets-failed")),
        Ok(None) => return Ok(RatingSubmission::Stale),
        Err(error) => {
            tracing::warn!(?error, channel = %channel, "could not read the ticket history pointer for a rating");
            return Err(failed(&error));
        }
    };

    for _ in 0..5 {
        let entry = match ctx.store().entry(&history_key).await {
            Ok(Some(entry)) => entry,
            Ok(None) => return Ok(RatingSubmission::Stale),
            Err(error) => {
                tracing::warn!(?error, channel = %channel, "could not read ticket history for a rating");
                return Err(failed(&error));
            }
        };
        let revision = entry.revision;
        let mut history: TicketHistory = match serde_json::from_value(entry.value) {
            Ok(history) => history,
            Err(error) => {
                tracing::warn!(?error, channel = %channel, "could not decode ticket history for a rating");
                return Err(failed(&error));
            }
        };
        if history.version != 1
            || history.channel_id != channel.get()
            || history.number != number
            || history.attempt != attempt
            || !matches!(&history.status, HistoryStatus::Closed)
            || history.opener_erased
            || history.opener != user.get()
        {
            return Ok(RatingSubmission::Stale);
        }
        if history.rating.is_some() {
            return Ok(RatingSubmission::AlreadyRated);
        }
        let now = ctx.now();
        let ttl = match history_ttl(&history, now) {
            Ok(ttl) if !ttl.is_zero() => ttl,
            Ok(_) => return Ok(RatingSubmission::Stale),
            Err(error) => {
                tracing::debug!(?error, channel = %channel, "ticket rating request is expired or invalid");
                return Ok(RatingSubmission::Stale);
            }
        };
        history.rating = Some(TicketRating {
            score,
            created: now,
        });
        match ctx
            .store()
            .replace(
                &history_key,
                &history,
                Expect::Revision(revision),
                Some(ttl),
            )
            .await
        {
            Ok(_) => return Ok(RatingSubmission::Saved),
            Err(StorageError::Conflict) => {}
            Err(error) => {
                tracing::warn!(?error, channel = %channel, "could not save ticket rating");
                return Err(failed(&error));
            }
        }
    }
    Err(UserError::message("tickets-failed"))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RatingSubmission {
    Saved,
    AlreadyRated,
    Stale,
}

async fn post_close_request_message(
    ctx: &EventContext,
    channel: ChannelId,
    opener: u64,
    request: &CloseRequest,
) -> Result<(), closure_core::error::DiscordError> {
    let reason = request
        .reason
        .clone()
        .unwrap_or_else(|| ctx.t_in(ctx.locale, "tickets-close-request-no-reason", &Args::new()));
    let mut content = ctx.t_in(
        ctx.locale,
        "tickets-close-request-message",
        &Args::new()
            .with("staff", format!("<@{}>", request.requested_by))
            .with("reason", reason),
    );
    if let Some(expires_at) = request.expires_at {
        let remaining_seconds = expires_at.saturating_sub(ctx.now());
        if remaining_seconds > 0 {
            let minutes = remaining_seconds.saturating_add(59) / 60;
            content.push('\n');
            content.push_str(&ctx.t_in(
                ctx.locale,
                "tickets-close-request-deadline",
                &Args::new().with("minutes", minutes),
            ));
        }
    }
    ctx.api
        .execute(Action::PostMessage {
            channel_id: channel,
            message_id: None,
            reply_to: None,
            content: Some(content),
            embeds: Vec::new(),
            components: vec![ui::row(vec![
                close_request_button(
                    ctx,
                    "close-request-approve",
                    ButtonStyle::Success,
                    "tickets-close-request-approve-button",
                    channel,
                    request.id,
                ),
                close_request_button(
                    ctx,
                    "close-request-reject",
                    ButtonStyle::Secondary,
                    "tickets-close-request-reject-button",
                    channel,
                    request.id,
                ),
            ])],
            files: Vec::new(),
        })
        .await?;
    if let Err(error) = ctx
        .api
        .execute(Action::SendMessage {
            channel_id: channel,
            content: Some(format!("<@{opener}>")),
            embeds: Vec::new(),
            reply_to: None,
            mentions: MentionPolicy::Users(vec![UserId::new(opener)]),
        })
        .await
    {
        tracing::warn!(?error, channel = %channel, opener, "could not notify the ticket opener");
    }
    Ok(())
}

async fn is_staff_command(cx: &CommandCx, config: &Config) -> bool {
    let actor = cx.actor().await;
    let roles = config.staff();
    actor.roles.iter().any(|role| roles.contains(role))
        || cx.require("tickets.manage").await.is_ok()
}

async fn stats_command(cx: CommandCx, _: StatsArgs) -> Result<(), UserError> {
    if cx.require("tickets.manage").await.is_err() {
        return cx
            .reply_private_t("tickets-stats-manage-only", &Args::new())
            .await;
    }
    let tickets = cx
        .ctx()
        .store()
        .list("ticket:", STATS_LIMIT)
        .await
        .map_err(|error| failed(&error))?;
    if tickets.len() >= STATS_LIMIT {
        return cx
            .reply_private_t("tickets-stats-limited", &Args::new())
            .await;
    }

    let mut open = 0_i64;
    let mut closing = 0_i64;
    let mut assigned = 0_i64;
    let mut unassigned = 0_i64;
    for (_, entry) in tickets {
        let ticket: Ticket = serde_json::from_value(entry.value).map_err(|error| failed(&error))?;
        if ticket.closed {
            continue;
        }
        if ticket.is_open() {
            open += 1;
        } else {
            closing += 1;
        }
        if ticket.claimed_by.is_some() {
            assigned += 1;
        } else {
            unassigned += 1;
        }
    }

    cx.reply_private_t(
        "tickets-stats",
        &Args::new()
            .with("total", open + closing)
            .with("open", open)
            .with("closing", closing)
            .with("assigned", assigned)
            .with("unassigned", unassigned),
    )
    .await
}

async fn is_guild_owner_or_admin(
    ctx: &EventContext,
    user: UserId,
    member_roles: &[RoleId],
) -> Result<bool, UserError> {
    let guild = match ctx
        .api
        .execute(Action::GetGuild {
            guild_id: ctx.guild_id,
        })
        .await
    {
        Ok(Outcome::Guild(guild)) => guild,
        Ok(_) => return Err(UserError::message("tickets-failed")),
        Err(error) => return Err(failed(&error)),
    };
    if guild.owner_id == user {
        return Ok(true);
    }
    let roles = match ctx
        .api
        .execute(Action::ListRoles {
            guild_id: ctx.guild_id,
        })
        .await
    {
        Ok(Outcome::Roles(roles)) => roles,
        Ok(_) => return Err(UserError::message("tickets-failed")),
        Err(error) => return Err(failed(&error)),
    };
    let everyone = RoleId::new(ctx.guild_id.get());
    Ok(roles.iter().any(|role| {
        (role.id == everyone || member_roles.contains(&role.id))
            && role.permissions.contains(Permissions::ADMINISTRATOR)
    }))
}

fn overwrite(target: OverwriteTarget, allow: Permissions, deny: Permissions) -> OverwriteSpec {
    OverwriteSpec {
        target,
        allow,
        deny,
    }
}

fn valid_panel_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 32
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
}

fn panel_component_text(text: &str, max_chars: usize) -> String {
    let normalized = text
        .chars()
        .map(|character| {
            if character.is_control() || character.is_whitespace() {
                ' '
            } else {
                character
            }
        })
        .collect::<String>();
    normalized
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(max_chars)
        .collect()
}

fn panel_label(ctx: &EventContext, panel: &NamedPanel, number: usize) -> String {
    panel
        .name
        .as_deref()
        .map(|name| panel_component_text(name, 80))
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| {
            ctx.t_in(
                ctx.locale,
                "tickets-panel-fallback",
                &Args::new().with("number", number.to_string()),
            )
        })
}

fn panel_description(panel: &NamedPanel) -> Option<String> {
    panel
        .description
        .as_deref()
        .map(str::trim)
        .filter(|description| !description.is_empty())
        .map(|description| panel_component_text(description, 100))
        .filter(|description| !description.is_empty())
}

fn panel_button(
    ctx: &EventContext,
    panel: &NamedPanel,
    label: &str,
    primary_if_unset: bool,
) -> Component {
    let default_style = if primary_if_unset {
        PanelButtonStyle::Primary
    } else {
        PanelButtonStyle::Secondary
    };
    ui::button(
        panel
            .button_style
            .map_or_else(|| default_style.discord(), PanelButtonStyle::discord),
        label.to_owned(),
        CustomId::new("open")
            .data(panel.id.clone())
            .encode(&ctx.module),
    )
}

fn panel_components(
    ctx: &EventContext,
    config: &Config,
    selected_panel: Option<&str>,
) -> Result<Vec<Vec<Component>>, UserError> {
    if let Some(id) = selected_panel {
        if !valid_panel_id(id) {
            return Err(UserError::message("tickets-panel-unavailable"));
        }
        let panel = config.named(id)?;
        let index = config
            .panels
            .iter()
            .position(|candidate| candidate.id == id)
            .unwrap_or(0);
        let label = panel_label(ctx, panel, index + 1);
        return Ok(vec![vec![ui::row(vec![panel_button(
            ctx, panel, &label, true,
        )])]]);
    }

    if config.panels.is_empty() {
        return Ok(vec![vec![ui::row(vec![button(
            ctx,
            "open",
            ButtonStyle::Primary,
            "tickets-open-button",
        )])]]);
    }

    let mut ids = HashSet::new();
    let mut labels = HashSet::new();
    let mut options = Vec::with_capacity(config.panels.len());
    for (index, panel) in config.panels.iter().enumerate() {
        if !valid_panel_id(&panel.id) {
            return Err(UserError::message("tickets-panel-unavailable"));
        }
        if !ids.insert(panel.id.as_str()) {
            return Err(UserError::message("tickets-panel-duplicate"));
        }
        config.named(&panel.id)?;
        let label = panel_label(ctx, panel, index + 1);
        if !labels.insert(label.to_lowercase()) {
            return Err(UserError::message("tickets-panel-label-duplicate"));
        }
        options.push((panel, label, panel_description(panel)));
    }

    match config.panel_display {
        PanelDisplay::Buttons => Ok(options
            .chunks(25)
            .map(|page| {
                page.chunks(5)
                    .map(|group| {
                        ui::row(
                            group
                                .iter()
                                .map(|(panel, label, _)| {
                                    panel_button(ctx, panel, label, options.len() == 1)
                                })
                                .collect::<Vec<_>>(),
                        )
                    })
                    .collect()
            })
            .collect()),
        PanelDisplay::Select => {
            let placeholder =
                ctx.t_in(ctx.locale, "tickets-panel-select-placeholder", &Args::new());
            Ok(options
                .chunks(125)
                .enumerate()
                .map(|(page_index, page)| {
                    page.chunks(25)
                        .enumerate()
                        .map(|(menu_index, group)| {
                            let choices = group.iter().map(|(panel, label, description)| {
                                (label.clone(), panel.id.clone(), description.clone())
                            });
                            let custom_id = CustomId::new("open-select")
                                .data(format!("{page_index}:{menu_index}"))
                                .encode(&ctx.module);
                            ui::row(vec![ui::select_described(
                                custom_id,
                                Some(&placeholder),
                                choices,
                                1,
                                1,
                            )])
                        })
                        .collect()
                })
                .collect())
        }
    }
}

fn strip_panel_page_suffix(content: &str) -> &str {
    let Some((base, suffix)) = content.rsplit_once("\n\n") else {
        return content;
    };
    let english = suffix.starts_with("Panel page ") && suffix.contains(" of ");
    let japanese =
        suffix.starts_with("パネル ") && suffix.contains(" / ") && suffix.ends_with(" ページ");
    if english || japanese { base } else { content }
}

fn page_message_content(ctx: &EventContext, content: &str, page: usize, pages: usize) -> String {
    let content = content.trim_end();
    if pages > 1 {
        format!(
            "{content}\n\n{}",
            ctx.t_in(
                ctx.locale,
                "tickets-panel-page",
                &Args::new()
                    .with("page", page.to_string())
                    .with("pages", pages.to_string())
            )
        )
    } else {
        content.to_owned()
    }
}

fn is_panel_message(message: &twilight_model::channel::Message) -> bool {
    message.components.iter().any(|component| {
        let Component::ActionRow(row) = component else {
            return false;
        };
        row.components.iter().any(|component| match component {
            Component::Button(button) => button
                .custom_id
                .as_deref()
                .is_some_and(|id| id.starts_with("tickets:open:")),
            Component::SelectMenu(menu) => menu.custom_id.starts_with("tickets:open-select:"),
            _ => false,
        })
    })
}

#[allow(
    clippy::too_many_lines,
    reason = "one flow: validate, publish pages, and report partial failures"
)]
async fn panel(cx: CommandCx, args: PanelArgs) -> Result<(), UserError> {
    let PanelArgs {
        panel,
        channel,
        text,
        message,
        page,
    } = args;
    let config: Config = cx.config()?;
    let panel_id = panel.filter(|id| !id.trim().is_empty());
    if let Some(id) = panel_id.as_deref() {
        if !valid_panel_id(id) {
            return Err(UserError::message("tickets-panel-unavailable"));
        }
        if !config.named(id)?.customization.valid() {
            return Err(UserError::message("tickets-panel-settings-invalid"));
        }
    }
    let requested_page = match page {
        Some(page) if page > 0 => Some(
            usize::try_from(page - 1)
                .map_err(|_| UserError::message("tickets-panel-page-invalid"))?,
        ),
        Some(_) => return Err(UserError::message("tickets-panel-page-invalid")),
        None => None,
    };
    if panel_id.is_some() && requested_page.is_some() {
        return Err(UserError::message("tickets-panel-page-conflict"));
    }
    let message_id = message
        .map(|text| {
            text.trim()
                .parse::<u64>()
                .ok()
                .filter(|id| *id > 0)
                .map(MessageId::new)
                .ok_or_else(|| UserError::message("tickets-panel-update-invalid"))
        })
        .transpose()?;
    let ctx = cx.ctx();
    let channel = channel.unwrap_or(cx.invocation().channel_id);
    let pages = panel_components(ctx, &config, panel_id.as_deref())?;
    let panel_embeds = match panel_id.as_deref() {
        Some(id) => config
            .named(id)?
            .customization
            .panel_embed()
            .map_err(|_| UserError::message("tickets-panel-settings-invalid"))?
            .into_iter()
            .collect(),
        None => Vec::new(),
    };
    let page_count = pages.len();
    if requested_page.is_some_and(|page| page >= page_count) {
        return Err(UserError::message("tickets-panel-page-invalid"));
    }
    let updating = message_id.is_some();
    let page_indexes: Vec<usize> = if updating {
        vec![requested_page.unwrap_or(0)]
    } else if let Some(page) = requested_page {
        vec![page]
    } else {
        (0..page_count).collect()
    };
    let existing = if let Some(message_id) = message_id.as_ref() {
        match ctx
            .api
            .execute(Action::GetMessage {
                channel_id: channel,
                message_id: *message_id,
            })
            .await
            .map_err(|_| UserError::message("tickets-panel-update-unavailable"))?
        {
            Outcome::Message(message)
                if ctx.bot().is_some_and(|bot| message.author.id == bot)
                    && message.channel_id == channel
                    && is_panel_message(&message) =>
            {
                Some(message)
            }
            _ => return Err(UserError::message("tickets-panel-update-unavailable")),
        }
    } else {
        None
    };
    let base_content = text
        .filter(|text| !text.trim().is_empty())
        .or_else(|| {
            existing
                .as_ref()
                .map(|message| {
                    strip_panel_page_suffix(&message.content)
                        .trim_end()
                        .to_owned()
                })
                .filter(|content| !content.trim().is_empty())
        })
        .unwrap_or_else(|| cx.t("tickets-panel-text", &Args::new()));
    let total_to_publish = page_indexes.len();
    for (sent, page_index) in page_indexes.iter().enumerate() {
        let content = page_message_content(ctx, &base_content, *page_index + 1, page_count);
        let result = ctx
            .api
            .execute(Action::PostMessage {
                channel_id: channel,
                message_id,
                reply_to: None,
                content: Some(content),
                embeds: panel_embeds.clone(),
                components: pages[*page_index].clone(),
                files: Vec::new(),
            })
            .await;
        if let Err(error) = result {
            if sent == 0 {
                return Err(failed(&error));
            }
            return cx
                .reply_private_t(
                    "tickets-panel-partial-failure",
                    &Args::new()
                        .with("sent", sent.to_string())
                        .with("total", total_to_publish.to_string())
                        .with("page", (*page_index + 1).to_string()),
                )
                .await;
        }
    }

    let audit_key = format!("panel-resend-{}", cx.invocation().interaction_id.get());
    if let Err(error) = audit::record_event(
        ctx,
        cx.user().id.get(),
        audit::AuditAction::PanelResend,
        audit::AuditOutcome::Success,
        &audit_key,
    )
    .await
    {
        tracing::warn!(?error, channel = %channel, "could not record ticket panel audit event");
    }

    if updating {
        if page_count > 1 {
            cx.reply_private_t(
                "tickets-panel-updated-page",
                &Args::new()
                    .with("page", (requested_page.unwrap_or(0) + 1).to_string())
                    .with("pages", page_count.to_string()),
            )
            .await
        } else {
            cx.reply_private_t("tickets-panel-updated", &Args::new())
                .await
        }
    } else if requested_page.is_some() {
        cx.reply_private_t(
            "tickets-panel-page-done",
            &Args::new()
                .with("page", (requested_page.unwrap_or_default() + 1).to_string())
                .with("pages", page_count.to_string()),
        )
        .await
    } else if total_to_publish > 1 {
        cx.reply_private_t(
            "tickets-panel-pages-done",
            &Args::new().with("count", total_to_publish.to_string()),
        )
        .await
    } else {
        cx.reply_private_t("tickets-panel-done", &Args::new()).await
    }
}

fn ticket_setup_error(config: &Config, guild_id: u64) -> Option<&'static str> {
    if !valid_form_configuration(config) {
        return Some("tickets-form-invalid");
    }
    if config
        .panels
        .iter()
        .any(|panel| !panel.customization.valid())
    {
        return Some("tickets-panel-settings-invalid");
    }
    if !integrations::valid_integrations(&config.integrations)
        || !teams_survey::valid_staff_teams(&config.staff_teams)
    {
        return Some("tickets-invalid-config");
    }
    let staff = config.staff();
    if staff.is_empty() {
        Some("tickets-not-set-up")
    } else if staff.contains(&RoleId::new(guild_id)) {
        Some("tickets-everyone-not-staff")
    } else {
        None
    }
}

fn valid_staff_team_configuration(config: &Config) -> bool {
    let panel_refs = config
        .panels
        .iter()
        .map(|panel| panel.staff_teams.clone())
        .collect::<Vec<_>>();
    teams_survey::valid_team_references(&config.staff_teams, &panel_refs)
        && config
            .panels
            .iter()
            .all(|panel| valid_questions(&panel.exit_survey))
}

fn valid_form_configuration(config: &Config) -> bool {
    form_library::valid_forms(&config.forms)
        && config.panels.iter().all(|panel| {
            panel
                .form_id
                .as_deref()
                .is_none_or(|id| form_library::resolve_form(&config.forms, id).is_some())
        })
}

fn panel_ticket_setup_error(
    config: &Config,
    guild_id: u64,
    staff_access: Option<&teams_survey::StaffAccessSnapshot>,
) -> Option<&'static str> {
    let error = ticket_setup_error(config, guild_id);
    if error != Some("tickets-not-set-up") {
        return error;
    }
    staff_access
        .filter(|snapshot| {
            snapshot
                .teams
                .iter()
                .any(|team| !team.roles.is_empty() || !team.users.is_empty())
        })
        .map_or(error, |_| None)
}

async fn is_staff_with_snapshot<I: InteractionRef + 'static>(
    cx: &Cx<I>,
    config: &Config,
    staff_access: Option<&teams_survey::StaffAccessSnapshot>,
) -> bool {
    let actor = cx.actor().await;
    let legacy_roles = config
        .staff()
        .iter()
        .map(|role| role.get())
        .collect::<Vec<_>>();
    teams_survey::may_access(
        staff_access,
        &legacy_roles,
        cx.user().id.get(),
        &actor.roles,
        cx.require("tickets.manage").await.is_ok(),
    )
}

fn team_staff_users(staff_access: Option<&teams_survey::StaffAccessSnapshot>) -> Vec<UserId> {
    let mut users = staff_access
        .into_iter()
        .flat_map(|snapshot| snapshot.teams.iter())
        .flat_map(|team| team.users.iter().copied())
        .collect::<Vec<_>>();
    users.sort_unstable();
    users.dedup();
    users.into_iter().map(UserId::new).collect()
}

async fn is_ticket_staff<I: InteractionRef + 'static>(
    cx: &Cx<I>,
    ticket: &Ticket,
    config: &Config,
) -> bool {
    let actor = cx.actor().await;
    teams_survey::may_access(
        ticket.staff_access.as_ref(),
        &history_staff_roles(ticket, config),
        cx.user().id.get(),
        &actor.roles,
        cx.require("tickets.manage").await.is_ok(),
    )
}

fn valid_questions(questions: &[advanced_forms::FormQuestion]) -> bool {
    advanced_forms::valid_questions(questions)
}

fn valid_hours_panel_references(config: &Config) -> bool {
    config.hours.iter().all(|window| {
        window
            .panel_id()
            .is_none_or(|id| config.panels.iter().filter(|panel| panel.id == id).count() == 1)
    })
}

fn opening_hours(
    config: &Config,
    panel: Option<&NamedPanel>,
    unix_seconds: i64,
) -> Result<bool, &'static str> {
    hours::is_open_for_panel(
        &config.timezone,
        panel.map(|panel| panel.id.as_str()),
        &config.hours,
        unix_seconds,
    )
}

fn ticket_config_revision(config: &Config, panel: Option<&NamedPanel>) -> u64 {
    let mut staff = config.staff();
    staff.sort_unstable();
    staff.dedup();
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    config.category().hash(&mut hasher);
    staff.hash(&mut hasher);
    config.log().hash(&mut hasher);
    config.max_open.hash(&mut hasher);
    config.thread_channel.hash(&mut hasher);
    panel.map_or(0, |panel| panel.max_open).hash(&mut hasher);
    panel.map(|panel| panel.form_id.clone()).hash(&mut hasher);
    config.close_after_days.hash(&mut hasher);
    config.waiting_after_hours.hash(&mut hasher);
    config.first_response_timeout_hours.hash(&mut hasher);
    config.last_message_timeout_hours.hash(&mut hasher);
    config.waiting_category.hash(&mut hasher);
    config.close_request_timeout_hours.hash(&mut hasher);
    config.close_when_opener_leaves.hash(&mut hasher);
    config.questions.hash(&mut hasher);
    config.staff_teams.hash(&mut hasher);
    panel
        .map(|panel| panel.staff_teams.clone())
        .hash(&mut hasher);
    panel
        .map(|panel| panel.exit_survey.clone())
        .hash(&mut hasher);
    config.timezone.hash(&mut hasher);
    config.hours.hash(&mut hasher);
    config.integrations.hash(&mut hasher);
    if let Some(panel) = panel {
        match serde_json::to_vec(&panel.customization) {
            Ok(customization) => customization.hash(&mut hasher),
            Err(error) => error.to_string().hash(&mut hasher),
        }
    }
    hasher.finish()
}

async fn open(cx: ComponentCx) -> Result<(), UserError> {
    let panel_id = cx.data();
    Box::pin(open_panel(
        cx,
        (!panel_id.is_empty()).then_some(panel_id),
        None,
    ))
    .await
}

async fn open_select(cx: ComponentCx) -> Result<(), UserError> {
    let values = cx.values();
    if values.len() != 1 {
        return say(&cx, "tickets-panel-select-invalid", &Args::new()).await;
    }
    let panel_id = values[0].clone();
    Box::pin(open_panel(cx, Some(panel_id), None)).await
}

fn panel_name_needs_nickname(panel: &NamedPanel) -> bool {
    match panel.customization.channel_name_scheme {
        panel_customization::ChannelNameScheme::Nickname => true,
        panel_customization::ChannelNameScheme::Custom => panel
            .customization
            .channel_name_custom
            .as_deref()
            .is_some_and(|template| template.contains("%nickname%")),
        _ => false,
    }
}

#[allow(clippy::too_many_lines, clippy::large_futures)]
async fn open_panel(
    cx: ComponentCx,
    panel_id: Option<String>,
    message_quote: Option<message_ticket::MessageQuote>,
) -> Result<(), UserError> {
    cx.require("tickets.use").await?;
    let original: Config = match cx.config() {
        Ok(config) => config,
        Err(error) => {
            tracing::warn!(?error, "ticket configuration could not be parsed");
            return say(&cx, "tickets-invalid-config", &Args::new()).await;
        }
    };
    if !valid_hours_panel_references(&original) {
        tracing::warn!("ticket opening hours refer to an unknown or duplicate panel");
        return say(&cx, "tickets-hours-invalid-config", &Args::new()).await;
    }
    if !valid_form_configuration(&original) {
        return say(&cx, "tickets-form-invalid", &Args::new()).await;
    }
    if !valid_staff_team_configuration(&original) {
        return say(&cx, "tickets-team-invalid", &Args::new()).await;
    }
    if !teams_survey::valid_thread_staff_team_configuration(&original) {
        return say(&cx, "tickets-team-thread-conflict", &Args::new()).await;
    }
    let panel = panel_id
        .as_deref()
        .filter(|id| !id.is_empty())
        .map(|id| original.named(id).cloned())
        .transpose()?;
    if panel
        .as_ref()
        .is_some_and(|panel| !panel.customization.valid())
    {
        return say(&cx, "tickets-panel-settings-invalid", &Args::new()).await;
    }
    let config = panel
        .as_ref()
        .map_or_else(|| original.clone(), |panel| original.with_panel(panel));
    let ctx = cx.ctx();
    let user = cx.user().id;
    match opening_hours(&config, panel.as_ref(), cx.now()) {
        Ok(true) => {}
        Ok(false) => return say(&cx, "tickets-hours-closed", &Args::new()).await,
        Err(error) => {
            tracing::warn!(?error, guild = %ctx.guild_id, "invalid ticket opening hours");
            return say(&cx, "tickets-hours-invalid-config", &Args::new()).await;
        }
    }
    let mut staff = config.staff();
    staff.sort_unstable();
    staff.dedup();
    let selected_teams = panel
        .as_ref()
        .map_or(&[][..], |panel| panel.staff_teams.as_slice());
    let staff_access = teams_survey::staff_access_snapshot(
        &original.staff_teams,
        selected_teams,
        &staff,
        ctx.guild_id.get(),
    )?;
    let actor_roles = cx.actor().await.roles;
    if panel.as_ref().is_some_and(|panel| {
        let roles = actor_roles
            .iter()
            .map(|role| role.get())
            .collect::<Vec<_>>();
        !panel.customization.allows(&roles, ctx.guild_id.get())
    }) {
        return say(&cx, "tickets-panel-access-denied", &Args::new()).await;
    }
    if let Some(key) = panel_ticket_setup_error(&config, ctx.guild_id.get(), staff_access.as_ref())
    {
        return say(&cx, key, &Args::new()).await;
    }
    if !valid_questions(&config.questions) {
        return say(&cx, "tickets-form-invalid", &Args::new()).await;
    }
    if panel
        .as_ref()
        .is_some_and(|panel| !valid_questions(&panel.exit_survey))
    {
        return say(&cx, "tickets-form-invalid", &Args::new()).await;
    }
    let resolved_questions = advanced_forms::resolve_form_questions(
        ctx,
        &config.questions,
        &config.integrations,
        ctx.guild_id.get(),
    )
    .await?;
    let staff_roles = staff_access.as_ref().map_or_else(
        || staff.iter().map(|role| role.get()).collect(),
        teams_survey::staff_roles,
    );
    let effective_staff = staff_roles
        .iter()
        .copied()
        .map(RoleId::new)
        .collect::<Vec<_>>();
    let staff_users = team_staff_users(staff_access.as_ref());
    if let Some(parent) = config.thread_channel()? {
        thread::ensure_thread_parent_with_access(
            ctx,
            parent,
            &effective_staff,
            cx.user().id,
            &staff_users,
            staff_access.as_ref(),
        )
        .await?;
    }
    if blacklist::is_blocked(
        ctx,
        user,
        &actor_roles,
        panel.as_ref().map(|panel| panel.id.as_str()),
    )
    .await?
    {
        return say(&cx, "tickets-blacklist-blocked", &Args::new()).await;
    }
    let panel_limit = panel.as_ref().map_or(0, |panel| panel.max_open);
    let revision = advanced_forms::form_modal_revision(
        ticket_config_revision(&config, panel.as_ref()),
        &resolved_questions,
    );
    let username = cx.user().name.clone();
    let nickname = if panel.as_ref().is_some_and(panel_name_needs_nickname) {
        match ctx
            .api
            .execute(Action::GetMember {
                guild_id: ctx.guild_id,
                user_id: user,
            })
            .await
        {
            Ok(Outcome::Member(member)) => member.nick,
            Ok(_) | Err(DiscordError::NotFound) => {
                return say(&cx, "tickets-failed", &Args::new()).await;
            }
            Err(error) => {
                tracing::warn!(?error, guild = %ctx.guild_id, "could not resolve the opener nickname for ticket channel naming");
                return say(&cx, "tickets-failed", &Args::new()).await;
            }
        }
    } else {
        None
    };
    let reservation = match reserve_open(
        ctx,
        user.get(),
        username,
        nickname,
        panel.clone(),
        config.clone(),
        staff_roles,
        Vec::new(),
        revision,
        staff_access.clone(),
        message_quote,
        config.max_open,
        panel_limit,
        is_staff_with_snapshot(&cx, &config, staff_access.as_ref()).await,
    )
    .await
    .map_err(|error| failed(&error))?
    {
        OpenReservationResult::Reserved(reservation) => reservation,
        OpenReservationResult::GlobalLimit(max) => {
            return say(
                &cx,
                "tickets-limit",
                &Args::new().with("max", i64::from(max)),
            )
            .await;
        }
        OpenReservationResult::PanelLimit(max) => {
            return say(
                &cx,
                "tickets-panel-limit",
                &Args::new().with("max", i64::from(max)),
            )
            .await;
        }
        OpenReservationResult::Cooldown(seconds) => {
            return say(
                &cx,
                "tickets-panel-cooldown",
                &Args::new().with("seconds", seconds.to_string()),
            )
            .await;
        }
        OpenReservationResult::TooLarge => {
            return say(&cx, "tickets-opening-too-large", &Args::new()).await;
        }
        OpenReservationResult::Pending => {
            return say(&cx, "tickets-opening-pending", &Args::new()).await;
        }
        OpenReservationResult::Busy => {
            return say(&cx, "tickets-opening", &Args::new()).await;
        }
    };
    if resolved_questions.is_empty() {
        if let Err(error) = cx.defer(true).await {
            match release_unstarted_open_reservation(
                ctx,
                user.get(),
                reservation.number,
                reservation.generation,
            )
            .await
            {
                Ok(true) => {}
                Ok(false) => tracing::warn!(
                    guild = %ctx.guild_id,
                    opener = user.get(),
                    number = reservation.number,
                    generation = reservation.generation,
                    "ticket reservation could not be released after the interaction acknowledgement failed"
                ),
                Err(release_error) => tracing::warn!(
                    ?release_error,
                    guild = %ctx.guild_id,
                    opener = user.get(),
                    number = reservation.number,
                    generation = reservation.generation,
                    "ticket reservation could not be released after the interaction acknowledgement failed"
                ),
            }
            return Err(error);
        }
        return create_ticket(cx, reservation, revision, resolved_questions).await;
    }
    let inputs = resolved_questions
        .iter()
        .enumerate()
        .map(|(index, question)| question.component(cx.module(), index));
    let custom_id = CustomId::new(FORM)
        .only_for(user)
        .data(format!("{}:{}", reservation.number, reservation.generation))
        .encode(cx.module());
    if custom_id.len() > MAX_TICKET_CUSTOM_ID_BYTES {
        let _ = release_unstarted_open_reservation(
            ctx,
            user.get(),
            reservation.number,
            reservation.generation,
        )
        .await;
        return say(&cx, "tickets-opening-too-large", &Args::new()).await;
    }
    let modal = ui::modal(custom_id, cx.t("tickets-form-title", &Args::new()), inputs);
    if let Err(error) = cx.show_modal(modal).await {
        if let Err(release_error) = release_unstarted_open_reservation(
            ctx,
            user.get(),
            reservation.number,
            reservation.generation,
        )
        .await
        {
            tracing::warn!(?release_error, guild = %ctx.guild_id, "ticket form reservation could not be released");
        }
        return Err(error);
    }
    Ok(())
}

#[allow(clippy::too_many_lines, clippy::large_futures)]
async fn submit_form(cx: ModalCx) -> Result<(), UserError> {
    cx.require("tickets.use").await?;
    let Some((module, form)) = CustomId::parse(&cx.submission().custom_id) else {
        return Err(UserError::message("tickets-form-invalid"));
    };
    if &module != cx.module() || form.route != FORM || form.owner != Some(cx.user().id) {
        return Err(UserError::message("tickets-form-invalid"));
    }
    let mut data = form.data.split(':');
    let (Some(number), Some(generation), None) = (data.next(), data.next(), data.next()) else {
        return Err(UserError::message("tickets-form-invalid"));
    };
    let (Ok(number), Ok(generation)) = (number.parse::<i64>(), generation.parse::<i64>()) else {
        return Err(UserError::message("tickets-form-invalid"));
    };
    if number <= 0 || generation <= 0 {
        return Err(UserError::message("tickets-form-invalid"));
    }
    let ctx = cx.ctx();
    let user = cx.user().id.get();
    if let Err(error) = cx.defer(true).await {
        match release_unstarted_open_reservation(ctx, user, number, generation).await {
            Ok(true) => {}
            Ok(false) => tracing::warn!(
                guild = %ctx.guild_id,
                opener = user,
                number,
                generation,
                "ticket reservation could not be released after the form acknowledgement failed"
            ),
            Err(release_error) => tracing::warn!(
                ?release_error,
                guild = %ctx.guild_id,
                opener = user,
                number,
                generation,
                "ticket reservation could not be released after the form acknowledgement failed"
            ),
        }
        return Err(error);
    }
    let (Some(_entry), Some(mut reservation)) = read_open_reservation(ctx, user, number)
        .await
        .map_err(|error| failed(&error))?
    else {
        return cx
            .reply_private_t("tickets-opening-pending", &Args::new())
            .await;
    };
    if reservation.opener != user
        || reservation.number != number
        || reservation.generation != generation
        || reservation.until <= cx.now()
        || reservation.api_started
        || reservation.released_at.is_some()
    {
        return cx
            .reply_private_t("tickets-opening-pending", &Args::new())
            .await;
    }
    let reserved_base_revision =
        ticket_config_revision(&reservation.settings, reservation.panel.as_ref());
    let original: Config = match cx.config() {
        Ok(config) => config,
        Err(error) => {
            tracing::warn!(?error, "ticket configuration could not be parsed");
            let _ = release_unstarted_open_reservation(ctx, user, number, generation).await;
            return cx
                .reply_private_t("tickets-invalid-config", &Args::new())
                .await;
        }
    };
    if !valid_hours_panel_references(&original) {
        tracing::warn!("ticket opening hours refer to an unknown or duplicate panel");
        let _ = release_unstarted_open_reservation(ctx, user, number, generation).await;
        return cx
            .reply_private_t("tickets-hours-invalid-config", &Args::new())
            .await;
    }
    if !valid_form_configuration(&original) {
        let _ = release_unstarted_open_reservation(ctx, user, number, generation).await;
        return cx
            .reply_private_t("tickets-form-invalid", &Args::new())
            .await;
    }
    if !valid_staff_team_configuration(&original) {
        let _ = release_unstarted_open_reservation(ctx, user, number, generation).await;
        return cx
            .reply_private_t("tickets-team-invalid", &Args::new())
            .await;
    }
    if !teams_survey::valid_thread_staff_team_configuration(&original) {
        let _ = release_unstarted_open_reservation(ctx, user, number, generation).await;
        return cx
            .reply_private_t("tickets-team-thread-conflict", &Args::new())
            .await;
    }
    let panel = if let Some(saved) = reservation.panel.as_ref() {
        if let Ok(panel) = original.named(&saved.id) {
            Some(panel.clone())
        } else {
            let _ = release_unstarted_open_reservation(ctx, user, number, generation).await;
            return cx
                .reply_private_t("tickets-form-changed", &Args::new())
                .await;
        }
    } else {
        None
    };
    if panel
        .as_ref()
        .is_some_and(|panel| !panel.customization.valid())
    {
        let _ = release_unstarted_open_reservation(ctx, user, number, generation).await;
        return cx
            .reply_private_t("tickets-panel-settings-invalid", &Args::new())
            .await;
    }
    let config = panel
        .as_ref()
        .map_or_else(|| original.clone(), |panel| original.with_panel(panel));
    match opening_hours(&config, panel.as_ref(), cx.now()) {
        Ok(true) => {}
        Ok(false) => {
            let _ = release_unstarted_open_reservation(ctx, user, number, generation).await;
            return say(&cx, "tickets-hours-closed", &Args::new()).await;
        }
        Err(error) => {
            tracing::warn!(?error, guild = %ctx.guild_id, "invalid ticket opening hours");
            let _ = release_unstarted_open_reservation(ctx, user, number, generation).await;
            return say(&cx, "tickets-hours-invalid-config", &Args::new()).await;
        }
    }
    if let Some(key) = panel_ticket_setup_error(
        &config,
        ctx.guild_id.get(),
        reservation.staff_access.as_ref(),
    ) {
        let _ = release_unstarted_open_reservation(ctx, user, number, generation).await;
        return say(&cx, key, &Args::new()).await;
    }
    let actor_roles = cx.actor().await.roles;
    if panel.as_ref().is_some_and(|panel| {
        let roles = actor_roles
            .iter()
            .map(|role| role.get())
            .collect::<Vec<_>>();
        !panel.customization.allows(&roles, ctx.guild_id.get())
    }) {
        let _ = release_unstarted_open_reservation(ctx, user, number, generation).await;
        return cx
            .reply_private_t("tickets-panel-access-denied", &Args::new())
            .await;
    }
    if blacklist::is_blocked(
        ctx,
        UserId::new(user),
        &actor_roles,
        panel.as_ref().map(|panel| panel.id.as_str()),
    )
    .await?
    {
        let _ = release_unstarted_open_reservation(ctx, user, number, generation).await;
        return say(&cx, "tickets-blacklist-blocked", &Args::new()).await;
    }
    if !valid_questions(&config.questions)
        || config.questions.is_empty()
        || reservation.settings.questions.is_empty()
    {
        let _ = release_unstarted_open_reservation(ctx, user, number, generation).await;
        return cx
            .reply_private_t("tickets-form-changed", &Args::new())
            .await;
    }
    let resolved_questions = match advanced_forms::resolve_form_questions_fresh(
        ctx,
        &config.questions,
        &config.integrations,
        ctx.guild_id.get(),
    )
    .await
    {
        Ok(questions) => questions,
        Err(error) => {
            let _ = release_unstarted_open_reservation(ctx, user, number, generation).await;
            return Err(error);
        }
    };
    let current_base_revision = ticket_config_revision(&config, panel.as_ref());
    let current_form_revision =
        advanced_forms::form_modal_revision(current_base_revision, &resolved_questions);
    let expected_revision = reservation
        .form_modal_revision
        .unwrap_or(reserved_base_revision);
    let revision_matches = if reservation.form_modal_revision.is_some() {
        expected_revision == current_form_revision
    } else {
        reserved_base_revision == current_base_revision
    };
    if !revision_matches {
        let _ = release_unstarted_open_reservation(ctx, user, number, generation).await;
        return cx
            .reply_private_t("tickets-form-changed", &Args::new())
            .await;
    }
    let mut answers = Vec::new();
    for (index, question) in resolved_questions.iter().enumerate() {
        let value = cx.value(&format!("question-{index}"));
        match question.answer(value) {
            Ok(Some(answer)) => answers.push(answer),
            Ok(None) => {}
            Err(error) => {
                let _ = release_unstarted_open_reservation(ctx, user, number, generation).await;
                return Err(error);
            }
        }
    }
    if !advanced_forms::valid_form_answers(&resolved_questions, &answers) {
        let _ = release_unstarted_open_reservation(ctx, user, number, generation).await;
        return Err(UserError::message("tickets-form-invalid"));
    }
    reservation.form_answers = answers;
    create_ticket(cx, reservation, expected_revision, resolved_questions).await
}

fn ticket_storage_size(ticket: &Ticket) -> Result<usize, serde_json::Error> {
    serde_json::to_vec(ticket).map(|encoded| encoded.len())
}

pub(crate) async fn refresh_ticket_form_answer_ttl(
    ctx: &EventContext,
    ticket: &Ticket,
) -> Result<(), ModuleError> {
    advanced_forms::refresh_ticket_form_answer_ttl(ctx, ticket)
        .await
        .map_err(|error| stored(&error))
}

async fn delete_ticket_form_answers(
    ctx: &EventContext,
    ticket: &Ticket,
) -> Result<(), ModuleError> {
    advanced_forms::delete_ticket_form_answers(ctx, ticket)
        .await
        .map_err(|error| stored(&error))
}

#[allow(
    clippy::too_many_lines,
    reason = "one flow: reserve a place, make the channel, greet, remember, schedule"
)]
async fn create_ticket<I: InteractionRef + 'static>(
    cx: Cx<I>,
    reservation: OpenReservation,
    expected_revision: u64,
    resolved_questions: Vec<advanced_forms::FormQuestion>,
) -> Result<(), UserError> {
    let ctx = cx.ctx();
    let user = reservation.opener;
    let config = reservation.settings.clone();
    let panel = reservation.panel.clone();
    let answers = reservation.form_answers.clone();
    if !valid_questions(&config.questions)
        || !advanced_forms::valid_questions(&resolved_questions)
        || !advanced_forms::valid_form_answers(&resolved_questions, &answers)
    {
        let _ = release_unstarted_open_reservation(
            ctx,
            user,
            reservation.number,
            reservation.generation,
        )
        .await;
        return Err(UserError::message("tickets-form-invalid"));
    }

    let current_original: Config = match cx.config() {
        Ok(config) => config,
        Err(error) => {
            tracing::warn!(?error, "ticket configuration could not be parsed");
            let _ = release_unstarted_open_reservation(
                ctx,
                user,
                reservation.number,
                reservation.generation,
            )
            .await;
            return say(&cx, "tickets-invalid-config", &Args::new()).await;
        }
    };
    if !valid_hours_panel_references(&current_original) {
        tracing::warn!("ticket opening hours refer to an unknown or duplicate panel");
        let _ = release_unstarted_open_reservation(
            ctx,
            user,
            reservation.number,
            reservation.generation,
        )
        .await;
        return say(&cx, "tickets-hours-invalid-config", &Args::new()).await;
    }
    if !valid_form_configuration(&current_original) {
        let _ = release_unstarted_open_reservation(
            ctx,
            user,
            reservation.number,
            reservation.generation,
        )
        .await;
        return say(&cx, "tickets-form-invalid", &Args::new()).await;
    }
    if !valid_staff_team_configuration(&current_original) {
        let _ = release_unstarted_open_reservation(
            ctx,
            user,
            reservation.number,
            reservation.generation,
        )
        .await;
        return cx
            .reply_private_t("tickets-team-invalid", &Args::new())
            .await;
    }
    if !teams_survey::valid_thread_staff_team_configuration(&current_original) {
        let _ = release_unstarted_open_reservation(
            ctx,
            user,
            reservation.number,
            reservation.generation,
        )
        .await;
        return cx
            .reply_private_t("tickets-team-thread-conflict", &Args::new())
            .await;
    }
    let current_panel = if let Some(saved) = panel.as_ref() {
        if let Ok(panel) = current_original.named(&saved.id) {
            Some(panel.clone())
        } else {
            let _ = release_unstarted_open_reservation(
                ctx,
                user,
                reservation.number,
                reservation.generation,
            )
            .await;
            return cx
                .reply_private_t("tickets-form-changed", &Args::new())
                .await;
        }
    } else {
        None
    };
    if current_panel
        .as_ref()
        .is_some_and(|panel| !panel.customization.valid())
    {
        let _ = release_unstarted_open_reservation(
            ctx,
            user,
            reservation.number,
            reservation.generation,
        )
        .await;
        return cx
            .reply_private_t("tickets-panel-settings-invalid", &Args::new())
            .await;
    }
    let current_config = current_panel.as_ref().map_or_else(
        || current_original.clone(),
        |panel| current_original.with_panel(panel),
    );
    let current_base_revision = ticket_config_revision(&current_config, current_panel.as_ref());
    let current_form_revision = if reservation.form_modal_revision.is_some() {
        advanced_forms::form_modal_revision(current_base_revision, &resolved_questions)
    } else {
        current_base_revision
    };
    if expected_revision != current_form_revision {
        let _ = release_unstarted_open_reservation(
            ctx,
            user,
            reservation.number,
            reservation.generation,
        )
        .await;
        return cx
            .reply_private_t("tickets-form-changed", &Args::new())
            .await;
    }
    match opening_hours(&current_config, current_panel.as_ref(), cx.now()) {
        Ok(true) => {}
        Ok(false) => {
            let _ = release_unstarted_open_reservation(
                ctx,
                user,
                reservation.number,
                reservation.generation,
            )
            .await;
            return say(&cx, "tickets-hours-closed", &Args::new()).await;
        }
        Err(error) => {
            tracing::warn!(?error, guild = %ctx.guild_id, "invalid ticket opening hours");
            let _ = release_unstarted_open_reservation(
                ctx,
                user,
                reservation.number,
                reservation.generation,
            )
            .await;
            return say(&cx, "tickets-hours-invalid-config", &Args::new()).await;
        }
    }
    if let Some(key) = panel_ticket_setup_error(
        &current_config,
        ctx.guild_id.get(),
        reservation.staff_access.as_ref(),
    ) {
        let _ = release_unstarted_open_reservation(
            ctx,
            user,
            reservation.number,
            reservation.generation,
        )
        .await;
        return say(&cx, key, &Args::new()).await;
    }
    let actor_roles = cx.actor().await.roles;
    if current_panel.as_ref().is_some_and(|panel| {
        let roles = actor_roles
            .iter()
            .map(|role| role.get())
            .collect::<Vec<_>>();
        !panel.customization.allows(&roles, ctx.guild_id.get())
    }) {
        let _ = release_unstarted_open_reservation(
            ctx,
            user,
            reservation.number,
            reservation.generation,
        )
        .await;
        return cx
            .reply_private_t("tickets-panel-access-denied", &Args::new())
            .await;
    }
    if blacklist::is_blocked(
        ctx,
        UserId::new(user),
        &actor_roles,
        current_panel.as_ref().map(|panel| panel.id.as_str()),
    )
    .await?
    {
        let _ = release_unstarted_open_reservation(
            ctx,
            user,
            reservation.number,
            reservation.generation,
        )
        .await;
        return say(&cx, "tickets-blacklist-blocked", &Args::new()).await;
    }
    let mut staff: Vec<RoleId> = reservation
        .staff_roles
        .iter()
        .copied()
        .map(RoleId::new)
        .collect();
    staff.sort_unstable();
    staff.dedup();
    let embeds = match advanced_forms::form_answers_embeds(&cx, &answers) {
        Ok(embeds) => embeds,
        Err(error) => {
            let _ = release_unstarted_open_reservation(
                ctx,
                user,
                reservation.number,
                reservation.generation,
            )
            .await;
            return Err(error);
        }
    };
    let ticket = ticket_from_reservation(&reservation, user);
    let ticket_size = ticket_storage_size(&ticket).map_err(|error| failed(&error))?;
    let reservation_size = serde_json::to_vec(&reservation)
        .map_err(|error| failed(&error))?
        .len();
    if ticket_size > MAX_TICKET_BYTES || reservation_size > OPEN_RESERVATION_MAX_BYTES {
        let _ = release_unstarted_open_reservation(
            ctx,
            user,
            reservation.number,
            reservation.generation,
        )
        .await;
        return say(&cx, "tickets-opening-too-large", &Args::new()).await;
    }
    let thread_parent = match current_config.thread_channel() {
        Ok(parent) => parent,
        Err(error) => {
            let _ = release_unstarted_open_reservation(
                ctx,
                user,
                reservation.number,
                reservation.generation,
            )
            .await;
            return Err(error);
        }
    };
    if let Some(parent) = thread_parent {
        let staff_users = team_staff_users(reservation.staff_access.as_ref());
        if let Err(error) = thread::ensure_thread_parent_with_access(
            ctx,
            parent,
            &staff,
            cx.user().id,
            &staff_users,
            reservation.staff_access.as_ref(),
        )
        .await
        {
            let _ = release_unstarted_open_reservation(
                ctx,
                user,
                reservation.number,
                reservation.generation,
            )
            .await;
            return Err(error);
        }
    } else if let Some(parent) = config.category()
        && let Err(error) = ensure_category(ctx, parent).await
    {
        let _ = release_unstarted_open_reservation(
            ctx,
            user,
            reservation.number,
            reservation.generation,
        )
        .await;
        return Err(error);
    }
    match mark_open_request_started(ctx, user, &reservation).await {
        Ok(true) => {}
        Ok(false) => return say(&cx, "tickets-opening-pending", &Args::new()).await,
        Err(error) => {
            tracing::warn!(?error, guild = %ctx.guild_id, "ticket opening reservation could not be advanced");
            return say(&cx, "tickets-opening-pending", &Args::new()).await;
        }
    }

    let created = if let Some(parent) = thread_parent {
        ctx.api
            .execute(Action::CreateThread {
                channel_id: parent,
                name: reservation_channel_name(&reservation),
                source: None,
                kind: ThreadKind::Private,
                auto_archive_minutes: 10080,
            })
            .await
    } else {
        let everyone = RoleId::new(ctx.guild_id.get());
        let mut overwrites = vec![
            overwrite(
                OverwriteTarget::Role(everyone),
                Permissions::empty(),
                Permissions::VIEW_CHANNEL,
            ),
            overwrite(
                OverwriteTarget::Member(cx.user().id),
                TICKET_PERMISSIONS,
                Permissions::empty(),
            ),
        ];
        if let Some(snapshot) = reservation.staff_access.as_ref() {
            let base = Permissions::VIEW_CHANNEL | Permissions::READ_MESSAGE_HISTORY;
            overwrites.extend(teams_survey::role_overwrites(snapshot).into_iter().map(
                |(role, allow, deny)| overwrite(OverwriteTarget::Role(role), base | allow, deny),
            ));
            let bot = ctx.bot();
            overwrites.extend(
                teams_survey::user_overwrites(snapshot)
                    .into_iter()
                    .filter(|(member, _, _)| *member != cx.user().id && Some(*member) != bot)
                    .map(|(member, allow, deny)| {
                        overwrite(OverwriteTarget::Member(member), base | allow, deny)
                    }),
            );
        } else {
            overwrites.extend(staff.iter().map(|role| {
                overwrite(
                    OverwriteTarget::Role(*role),
                    TICKET_PERMISSIONS,
                    Permissions::empty(),
                )
            }));
        }
        if let Some(bot) = ctx.bot() {
            overwrites.push(overwrite(
                OverwriteTarget::Member(bot),
                TICKET_PERMISSIONS | Permissions::MANAGE_CHANNELS,
                Permissions::empty(),
            ));
        }
        ctx.api
            .execute(Action::CreateChannel {
                guild_id: ctx.guild_id,
                spec: NewChannel {
                    name: reservation_channel_name(&reservation),
                    kind: ChannelKind::Text,
                    parent_id: config.category(),
                    topic: Some(format!(
                        "<@{user}> ticket-reservation:{}",
                        reservation.token
                    )),
                    nsfw: false,
                    slowmode_seconds: 0,
                    overwrites,
                },
                reason: Reason::truncated(&cx.t("tickets-reason-open", &Args::new())),
            })
            .await
    };
    let channel = match created {
        Ok(Outcome::Channel(channel)) => Some(channel.id),
        Err(error) => {
            tracing::warn!(?error, guild = %ctx.guild_id, "ticket channel creation was not confirmed");
            match recover_open_channel(ctx, &reservation).await {
                Ok(OpenChannelLookup::Found(channel)) => Some(channel),
                Ok(OpenChannelLookup::Missing) if create_channel_error_is_definitive(&error) => {
                    match release_open_reservation(
                        ctx,
                        user,
                        reservation.number,
                        reservation.generation,
                    )
                    .await
                    {
                        Ok(true) => {
                            let key = if thread_parent.is_some() {
                                "tickets-thread-create-failed"
                            } else {
                                "tickets-failed"
                            };
                            return say(&cx, key, &Args::new()).await;
                        }
                        Ok(false) => {
                            return say(&cx, "tickets-opening-pending", &Args::new()).await;
                        }
                        Err(release_error) => {
                            tracing::warn!(?release_error, guild = %ctx.guild_id, "ticket reservation could not be released");
                            return say(&cx, "tickets-opening-pending", &Args::new()).await;
                        }
                    }
                }
                Ok(OpenChannelLookup::Missing | OpenChannelLookup::Ambiguous) => None,
                Err(recovery_error) => {
                    tracing::warn!(?recovery_error, guild = %ctx.guild_id, "ticket channel lookup will be retried");
                    None
                }
            }
        }
        _ => match recover_open_channel(ctx, &reservation).await {
            Ok(OpenChannelLookup::Found(channel)) => Some(channel),
            Ok(OpenChannelLookup::Missing | OpenChannelLookup::Ambiguous) | Err(_) => None,
        },
    };
    let Some(channel) = channel else {
        return say(&cx, "tickets-opening-pending", &Args::new()).await;
    };
    if thread_parent.is_some() {
        match remember_open_channel(ctx, user, &reservation, channel).await {
            Ok(true) => {}
            Ok(false) => {
                tracing::warn!(guild = %ctx.guild_id, %channel, "ticket thread ID could not be persisted before setup");
            }
            Err(error) => {
                tracing::warn!(?error, guild = %ctx.guild_id, %channel, "ticket thread ID could not be persisted before setup");
            }
        }
    }

    let saved = match save_open_ticket(ctx, user, &reservation, channel).await {
        Ok(saved) => saved,
        Err(error) => {
            tracing::warn!(?error, guild = %ctx.guild_id, "ticket record could not be saved yet");
            return say(&cx, "tickets-opening-pending", &Args::new()).await;
        }
    };
    if saved {
        if let Err(error) = add_team_users_to_thread(ctx, channel, &ticket).await {
            tracing::warn!(?error, %channel, "ticket team members could not be added to the thread");
        }
        let quote_failed =
            if let Err(error) = post_ticket_message_quote(ctx, channel, &ticket).await {
                tracing::warn!(?error, %channel, "ticket source quote could not be posted");
                ticket.message_quote.is_some()
            } else {
                false
            };
        if let Err(error) = automation::start(ctx, channel).await {
            tracing::warn!(?error, %channel, "ticket automation timers could not be started");
        }
        let mut welcome_embeds = embeds.clone();
        match ticket_welcome_embed(ctx, channel, &ticket, &config, &answers).await {
            Ok(Some(embed)) => welcome_embeds.insert(0, embed),
            Ok(None) => {}
            Err(error) => {
                tracing::warn!(?error, %channel, "custom ticket welcome embed could not be built");
            }
        }
        let welcome = ctx
            .api
            .execute(Action::PostMessage {
                channel_id: channel,
                message_id: None,
                reply_to: None,
                content: Some(
                    cx.t(
                        "tickets-welcome",
                        &Args::new()
                            .with("user", format!("<@{user}>"))
                            .with("number", reservation.number),
                    ),
                ),
                embeds: welcome_embeds,
                components: vec![ui::row(vec![
                    button(ctx, "claim", ButtonStyle::Secondary, "tickets-claim-button"),
                    button(ctx, "close", ButtonStyle::Danger, "tickets-close-button"),
                ])],
                files: Vec::new(),
            })
            .await;
        let mut answers_need_retry = false;
        if let Err(error) = welcome {
            tracing::warn!(?error, guild = %ctx.guild_id, "could not greet in a new ticket");
            if !embeds.is_empty()
                && let Err(error) = advanced_forms::send_form_answers(ctx, channel, &embeds).await
            {
                tracing::warn!(?error, guild = %ctx.guild_id, "could not post saved ticket answers");
                answers_need_retry = true;
            }
        }
        if let Err(error) = post_ticket_knowledge_suggestions(ctx, channel, &ticket, &answers).await
        {
            tracing::warn!(?error, %channel, "published ticket knowledge suggestions could not be posted");
        }
        let _ = ctx
            .api
            .execute(Action::SendMessage {
                channel_id: channel,
                content: Some(
                    staff
                        .iter()
                        .map(|role| format!("<@&{role}>"))
                        .collect::<Vec<_>>()
                        .join(" "),
                ),
                embeds: Vec::new(),
                reply_to: None,
                mentions: MentionPolicy::Roles(staff),
            })
            .await;
        if config.close_after_days > 0 {
            let _ = watch(
                ctx,
                channel,
                reservation.created + i64::from(config.close_after_days) * DAY,
            )
            .await;
        }
        if quote_failed {
            return say(
                &cx,
                "tickets-message-quote-failed",
                &Args::new().with("channel", format!("<#{channel}>")),
            )
            .await;
        }
        if answers_need_retry {
            return say(
                &cx,
                "tickets-form-retry",
                &Args::new().with("channel", format!("<#{channel}>")),
            )
            .await;
        }
        return say(
            &cx,
            "tickets-opened",
            &Args::new().with("channel", format!("<#{channel}>")),
        )
        .await;
    }

    match find_ticket_by_number(ctx, reservation.number).await {
        Ok(Some((stored_channel, _ticket))) => {
            if stored_channel != channel {
                cleanup_open_orphan(ctx, &reservation, channel).await;
            }
            return say(
                &cx,
                "tickets-opened",
                &Args::new().with("channel", format!("<#{stored_channel}>")),
            )
            .await;
        }
        Ok(None) => {}
        Err(error) => {
            tracing::warn!(?error, guild = %ctx.guild_id, "ticket recovery could not read stored tickets");
            return say(&cx, "tickets-opening-pending", &Args::new()).await;
        }
    }
    if open_reservation_exists(ctx, user, reservation.number)
        .await
        .unwrap_or(true)
    {
        return say(&cx, "tickets-opening-pending", &Args::new()).await;
    }

    cleanup_open_orphan(ctx, &reservation, channel).await;
    say(&cx, "tickets-opening-pending", &Args::new()).await
}

async fn ticket_answers(cx: CommandCx, (): ()) -> Result<(), UserError> {
    let channel = cx.invocation().channel_id;
    let Some(ticket) = cx
        .store()
        .get::<Ticket>(&ticket_key(channel))
        .await
        .map_err(|error| failed(&error))?
    else {
        return Err(UserError::message("tickets-not-a-ticket"));
    };
    if cx.user().id.get() != ticket.opener {
        let config: Config = cx.config()?;
        let access_config = config.for_ticket(&ticket);
        if !is_ticket_staff(&cx, &ticket, &access_config).await {
            return Err(UserError::message("tickets-form-opener-only"));
        }
    }
    let answers = advanced_forms::load_ticket_form_answers(cx.ctx(), &ticket)
        .await
        .map_err(|error| failed(&error))?;
    if answers.is_empty() {
        return cx
            .reply_private_t("tickets-form-no-answers", &Args::new())
            .await;
    }
    let embeds = advanced_forms::form_answers_embeds(&cx, &answers)?;
    advanced_forms::send_form_answers(cx.ctx(), channel, &embeds).await?;
    cx.reply_private_t("tickets-form-resent", &Args::new())
        .await
}

/// A ticket made under anything but a category would fail at Discord, so a setting that is
/// not one is refused before a place is taken.
async fn ensure_category(ctx: &EventContext, parent: ChannelId) -> Result<(), UserError> {
    match ctx
        .api
        .execute(Action::GetChannel { channel_id: parent })
        .await
    {
        Ok(Outcome::Channel(found)) if found.kind == ChannelType::GuildCategory => Ok(()),
        Ok(_) | Err(closure_core::error::DiscordError::NotFound) => Err(UserError::from(
            ModuleError::Config("the ticket category is not a category channel".into()),
        )),
        Err(error) => Err(failed(&error)),
    }
}

async fn give_back(ctx: &EventContext, user: u64) {
    if let Err(error) = ctx.store().score_add(OPEN, user, -1).await {
        tracing::warn!(?error, guild = %ctx.guild_id, "could not give a ticket place back");
    }
}

/// Looks at the ticket again at `at`, to close it if nobody wrote in it.
async fn watch(ctx: &EventContext, channel: ChannelId, at: i64) -> Result<(), ModuleError> {
    ctx.scheduler()
        .job(
            IDLE,
            &Idle {
                channel: channel.get(),
            },
        )
        .map_err(|e| stored(&e))?
        .key(&idle_key(channel))
        .at(at)
        .await
        .map_err(|e| stored(&e))
}

async fn close_left_later(
    ctx: &EventContext,
    channel: ChannelId,
    at: i64,
) -> Result<(), ModuleError> {
    ctx.scheduler()
        .job(
            LEFT_CLOSE,
            &LeftClose {
                channel: channel.get(),
            },
        )
        .map_err(|e| stored(&e))?
        .key(&left_close_key(channel))
        .at(at)
        .await
        .map_err(|e| stored(&e))
}

/// The time of a Discord id, in Unix seconds.
fn made_at(id: u64) -> i64 {
    i64::try_from(((id >> 22) + 1_420_070_400_000) / 1000).unwrap_or(0)
}

/// A job: the ticket is closed when nobody has written in it for the days of the settings, and
/// looked at again when the time comes otherwise.
#[allow(clippy::large_futures)]
async fn idle_job(cx: JobCx) -> Result<(), ModuleError> {
    let job: Idle = cx.payload()?;
    let ctx = cx.ctx();
    let channel = ChannelId::new(job.channel);
    let Some(ticket) = ctx
        .store()
        .get::<Ticket>(&ticket_key(channel))
        .await
        .map_err(|e| stored(&e))?
    else {
        return Ok(());
    };
    let config: Config = ctx.parse_config()?;
    let config = config.for_ticket(&ticket);
    if ticket.closed || config.close_after_days == 0 {
        return Ok(());
    }
    let last = match ctx
        .api
        .execute(Action::GetChannel {
            channel_id: channel,
        })
        .await
    {
        Ok(Outcome::Channel(found)) => found
            .last_message_id
            .map_or(ticket.created, |id| made_at(id.get())),
        Ok(_) => ticket.created,
        // The channel is gone: nothing is left to close, and the place is given back.
        Err(closure_core::error::DiscordError::NotFound) => {
            if ticket.close_generation > 0 && ticket.closing.is_some() {
                watch(ctx, channel, ctx.now() + CLOSING_WAIT).await?;
            } else {
                forget(ctx, channel, &ticket, false).await?;
            }
            return Ok(());
        }
        // A channel that cannot be read now is not an idle one: the job is tried again.
        Err(error) => return Err(error.into()),
    };
    let due = last + i64::from(config.close_after_days) * DAY;
    if ctx.now() >= due {
        let reason = ctx.t_in(ctx.locale, "tickets-idle-reason", &Args::new());
        if let Closed::BusyUntil(until) = finish(ctx, channel, None, &reason, None).await? {
            watch(ctx, channel, until).await?;
        }
    } else {
        watch(ctx, channel, due).await?;
    }
    Ok(())
}

/// A request that was not answered by its deadline closes the ticket.
#[allow(clippy::large_futures)]
async fn close_request_timeout_job(cx: JobCx) -> Result<(), ModuleError> {
    let job: CloseRequestTimeout = cx.payload()?;
    let ctx = cx.ctx();
    let channel = ChannelId::new(job.channel);
    let Some(ticket) = ctx
        .store()
        .get::<Ticket>(&ticket_key(channel))
        .await
        .map_err(|error| stored(&error))?
    else {
        return Ok(());
    };
    let Some(request) = ticket.close_request.as_ref().filter(|request| {
        request.id == job.request_id && request.expires_at == Some(job.expires_at)
    }) else {
        return Ok(());
    };
    if ctx.now() < job.expires_at {
        return watch_close_request_timeout(
            ctx,
            channel,
            request.id,
            job.expires_at,
            job.expires_at,
        )
        .await;
    }

    let reason = ctx.t_in(
        ctx.locale,
        "tickets-close-request-timeout-reason",
        &Args::new(),
    );
    if let Closed::BusyUntil(until) =
        finish(ctx, channel, None, &reason, Some(job.request_id)).await?
    {
        watch_close_request_timeout(ctx, channel, job.request_id, job.expires_at, until).await?;
    }
    Ok(())
}

/// How the closing of a ticket went.
#[derive(Debug, PartialEq, Eq)]
enum Closed {
    Done,
    /// The ticket closed, but the opener rating DM could not be confirmed.
    RatingDmUnconfirmed,
    /// No such ticket.
    Unknown,
    /// The confirmation no longer matches the pending request.
    RequestChanged,
    /// Somebody else is closing it; nothing was done, and the close may be tried at this time
    /// (Unix seconds).
    BusyUntil(i64),
}

/// What taking a ticket for closing found.
enum Claim {
    Taken(Box<Ticket>),
    Gone,
    Busy(i64),
    RequestChanged,
    AutomationChanged,
}

async fn on_member_left(ctx: EventContext, event: Event) -> Result<Flow, ModuleError> {
    let Event::MemberLeft { guild_id, user } = event else {
        return Ok(Flow::Continue);
    };
    if guild_id != ctx.guild_id {
        return Ok(Flow::Continue);
    }
    let config: Config = ctx.parse_config()?;
    if !config.close_when_opener_leaves {
        return Ok(Flow::Continue);
    }
    let at = ctx.now();
    ctx.scheduler()
        .job(
            LEFT_SCAN,
            &LeftScan {
                opener: user.id.get(),
                at,
            },
        )
        .map_err(|e| stored(&e))?
        .key(&format!("left-scan-{}", user.id.get()))
        .at(at)
        .await
        .map_err(|e| stored(&e))?;
    Ok(Flow::Continue)
}

async fn queue_left_close(
    ctx: &EventContext,
    key: &str,
    opener: u64,
    at: i64,
) -> Result<(), ModuleError> {
    let Some(ticket) = ctx
        .store()
        .get::<Ticket>(key)
        .await
        .map_err(|e| stored(&e))?
    else {
        return Ok(());
    };
    if ticket.closed || ticket.opener != opener || ticket.created > at {
        return Ok(());
    }
    let channel = key
        .strip_prefix("ticket:")
        .and_then(|id| id.parse::<u64>().ok())
        .map(ChannelId::new)
        .ok_or_else(|| ModuleError::other("the ticket key has no valid channel ID"))?;
    close_left_later(ctx, channel, ctx.now()).await
}

async fn left_scan_job(cx: JobCx) -> Result<(), ModuleError> {
    let job: LeftScan = cx.payload()?;
    let ctx = cx.ctx();
    let config: Config = ctx.parse_config()?;
    if !config.close_when_opener_leaves {
        return Ok(());
    }

    let mut prefixes = vec!["ticket:".to_owned()];
    while let Some(prefix) = prefixes.pop() {
        let tickets = ctx
            .store()
            .list(&prefix, TICKET_LIST_LIMIT)
            .await
            .map_err(|e| stored(&e))?;
        if tickets.len() == TICKET_LIST_LIMIT {
            if tickets.iter().any(|(key, _)| key == &prefix) {
                queue_left_close(ctx, &prefix, job.opener, job.at).await?;
            }
            if prefix.len() - "ticket:".len() >= CHANNEL_ID_DIGITS {
                return Err(ModuleError::other(
                    "the ticket listing could not be divided further",
                ));
            }
            prefixes.extend((b'0'..=b'9').map(|digit| format!("{prefix}{}", char::from(digit))));
        } else {
            for (key, _) in tickets {
                queue_left_close(ctx, &key, job.opener, job.at).await?;
            }
        }
    }
    Ok(())
}

#[allow(clippy::large_futures)]
async fn left_close_job(cx: JobCx) -> Result<(), ModuleError> {
    let job: LeftClose = cx.payload()?;
    let ctx = cx.ctx();
    let channel = ChannelId::new(job.channel);
    let Some(ticket) = ctx
        .store()
        .get::<Ticket>(&ticket_key(channel))
        .await
        .map_err(|e| stored(&e))?
    else {
        return Ok(());
    };
    if ticket.closed {
        return Ok(());
    }
    let config: Config = ctx.parse_config()?;
    if !config.close_when_opener_leaves {
        return Ok(());
    }
    match ctx
        .api
        .execute(Action::GetChannel {
            channel_id: channel,
        })
        .await
    {
        Ok(Outcome::Channel(_)) => {}
        Ok(Outcome::None) | Err(DiscordError::NotFound) => {
            if ticket.close_generation > 0 && ticket.closing.is_some() {
                close_left_later(ctx, channel, ctx.now() + CLOSING_WAIT).await?;
            } else {
                forget(ctx, channel, &ticket, false).await?;
            }
            return Ok(());
        }
        Ok(_) => return Err(ModuleError::other("the ticket channel was not returned")),
        Err(error) => return Err(error.into()),
    }
    let reason = ctx.t_in(ctx.locale, "tickets-opener-left-reason", &Args::new());
    if let Closed::BusyUntil(until) = finish(ctx, channel, None, &reason, None).await? {
        close_left_later(ctx, channel, until).await?;
    }
    Ok(())
}

/// How long a close that began is waited for before another may take over.
const CLOSING_WAIT: i64 = 300;

fn closing_key(channel: ChannelId) -> String {
    format!("closing:{}", channel.get())
}

async fn watch_closing(
    ctx: &EventContext,
    closing: &CloseAttempt<'_>,
    at: i64,
) -> Result<(), ModuleError> {
    ctx.scheduler()
        .job(
            CLOSING,
            &ClosingJob {
                channel: closing.channel.get(),
                number: closing.ticket.number,
                attempt: closing.attempt,
            },
        )
        .map_err(|error| stored(&error))?
        .key(&closing_key(closing.channel))
        .at(at)
        .await
        .map_err(|error| stored(&error))
}

async fn closing_job(cx: JobCx) -> Result<(), ModuleError> {
    let job: ClosingJob = cx.payload()?;
    let ctx = cx.ctx();
    let channel = ChannelId::new(job.channel);
    let Some(ticket) = ctx
        .store()
        .get::<Ticket>(&ticket_key(channel))
        .await
        .map_err(|error| stored(&error))?
    else {
        return Ok(());
    };
    if ticket.number != job.number {
        return Ok(());
    }
    let key = history_key(job.number, channel);
    let history: TicketHistory = ctx
        .store()
        .get(&key)
        .await
        .map_err(|error| stored(&error))?
        .ok_or_else(|| ModuleError::other("ticket history is missing"))?;
    if history.channel_id != job.channel || history.number != job.number {
        return Err(ModuleError::other(
            "ticket history does not match its closing job",
        ));
    }
    let config = Config::default();
    let closing = CloseAttempt {
        channel,
        ticket: &ticket,
        config: &config,
        closer: None,
        reason: &history.reason,
        attempt: job.attempt,
    };
    if history.status == HistoryStatus::Closed {
        return complete_history_close(ctx, &closing, &history, true)
            .await
            .map(drop);
    }
    if history.status != HistoryStatus::Closing
        || !history.log_copy_sent
        || ticket.close_generation != job.attempt
        || ticket.closing.is_none()
        || history.attempt != job.attempt
    {
        return Ok(());
    }
    validate_history_archive(ctx, &closing, &history).await?;
    let current: Option<Ticket> = ctx
        .store()
        .get(&ticket_key(channel))
        .await
        .map_err(|error| stored(&error))?;
    let Some(current) = current else {
        return Ok(());
    };
    if current.number != job.number
        || current.close_generation != job.attempt
        || current.closing.is_none()
    {
        return Ok(());
    }
    complete_history_close(ctx, &closing, &history, true)
        .await
        .map(drop)
}

fn history_staff_roles(ticket: &Ticket, config: &Config) -> Vec<u64> {
    let mut roles = ticket.staff_access.as_ref().map_or_else(
        || {
            ticket.staff_roles.as_ref().map_or_else(
                || {
                    config
                        .staff()
                        .into_iter()
                        .map(twilight_model::id::Id::get)
                        .collect()
                },
                Clone::clone,
            )
        },
        teams_survey::staff_roles,
    );
    roles.sort_unstable();
    roles.dedup();
    roles
}

async fn remove_history_generation(
    ctx: &EventContext,
    channel: u64,
    attempt: i64,
    pages: u32,
    index_pages: u32,
) {
    for page in 0..pages {
        if let Err(error) = ctx
            .store()
            .delete(&history_content_key(channel, attempt, page as usize))
            .await
        {
            tracing::warn!(
                ?error,
                channel,
                page,
                "could not remove an incomplete ticket transcript page"
            );
        }
    }
    for page in 0..index_pages {
        if let Err(error) = ctx
            .store()
            .delete(&history_index_key(channel, attempt, page as usize))
            .await
        {
            tracing::warn!(
                ?error,
                channel,
                page,
                "could not remove an incomplete ticket transcript index"
            );
        }
    }
    data::remove_history_span_pages(ctx, channel, attempt, pages).await;
}

fn transcript_info(
    text: &str,
    pages: u32,
    index_pages: u32,
    span_pages: u32,
    page_chars: &[u32],
    messages: usize,
    truncated: bool,
) -> Result<TranscriptInfo, ModuleError> {
    let chars = page_chars.iter().try_fold(0_u64, |total, count| {
        total
            .checked_add(u64::from(*count))
            .ok_or_else(|| ModuleError::other("ticket transcript is too long"))
    })?;
    Ok(TranscriptInfo {
        pages,
        index_pages,
        span_pages,
        chars,
        bytes: u64::try_from(text.len()).map_err(|error| ModuleError::other(error.to_string()))?,
        messages: u32::try_from(messages).map_err(|error| ModuleError::other(error.to_string()))?,
        truncated,
    })
}

fn history_record(
    ctx: &EventContext,
    closing: &CloseAttempt<'_>,
    transcript: TranscriptInfo,
) -> Result<TicketHistory, ModuleError> {
    Ok(TicketHistory {
        version: 1,
        expires_at: history_expires_at(ctx.now())?,
        channel_id: closing.channel.get(),
        number: closing.ticket.number,
        opener: closing.ticket.opener,
        claimed_by: closing.ticket.claimed_by,
        created: closing.ticket.created,
        closed: ctx.now(),
        closer: closing.closer.map(twilight_model::id::Id::get),
        reason: closing.reason.to_owned(),
        panel: closing.ticket.panel.clone(),
        survey_integrations: closing.config.integrations.clone(),
        labels: closing.ticket.labels.clone(),
        rating: None,
        rating_prompt_attempted: false,
        opener_erased: false,
        privacy_generation: 0,
        anonymizing_user: None,
        operation_token: None,
        operation_until: 0,
        log_copy_channel: closing.config.log().map(twilight_model::id::Id::get),
        log_copy_message: None,
        transcript_span_pages: transcript.span_pages,
        staff_access: closing.ticket.staff_access.clone(),
        staff_roles: history_staff_roles(closing.ticket, closing.config),
        status: HistoryStatus::Closing,
        log_copy_sent: closing.config.log().is_none(),
        attempt: closing.attempt,
        transcript_attempt: closing.attempt,
        transcript_pages: transcript.pages,
        transcript_index_pages: transcript.index_pages,
        transcript_char_count: transcript.chars,
        transcript_bytes: transcript.bytes,
        transcript_messages: transcript.messages,
        transcript_truncated: transcript.truncated,
    })
}

async fn write_history_pages(
    ctx: &EventContext,
    channel: ChannelId,
    attempt: i64,
    text: &str,
    pages: &[(usize, usize)],
    page_chars: &[u32],
    expires_at: i64,
) -> Result<u32, ModuleError> {
    let page_count =
        u32::try_from(pages.len()).map_err(|error| ModuleError::other(error.to_string()))?;
    let index_pages = u32::try_from(page_chars.chunks(HISTORY_INDEX_SIZE).len())
        .map_err(|error| ModuleError::other(error.to_string()))?;
    let ttl = history_ttl_until(expires_at, ctx.now())?;
    for (page, (start, end)) in pages.iter().enumerate() {
        let page_text = text[*start..*end].to_owned();
        if let Err(error) = ctx
            .store()
            .set_for(
                &history_content_key(channel.get(), attempt, page),
                &page_text,
                ttl,
            )
            .await
        {
            remove_history_generation(ctx, channel.get(), attempt, page_count, index_pages).await;
            return Err(stored(&error));
        }
    }
    for (page, counts) in page_chars.chunks(HISTORY_INDEX_SIZE).enumerate() {
        let counts = counts.to_vec();
        if let Err(error) = ctx
            .store()
            .set_for(
                &history_index_key(channel.get(), attempt, page),
                &counts,
                ttl,
            )
            .await
        {
            remove_history_generation(ctx, channel.get(), attempt, page_count, index_pages).await;
            return Err(stored(&error));
        }
    }
    Ok(index_pages)
}

#[allow(clippy::too_many_lines)]
async fn save_history(
    ctx: &EventContext,
    closing: &CloseAttempt<'_>,
    transcript: Option<(&str, bool, usize, &[data::TranscriptSpanRange])>,
) -> Result<TicketHistory, ModuleError> {
    let (text, transcript_truncated, transcript_messages, spans) =
        transcript.unwrap_or(("", false, 0, &[]));
    let expires_at = history_expires_at(ctx.now())?;
    let pages = transcript_page_ranges(text);
    let transcript_pages =
        u32::try_from(pages.len()).map_err(|error| ModuleError::other(error.to_string()))?;
    let mut page_chars = Vec::with_capacity(pages.len());
    for (start, end) in &pages {
        page_chars.push(
            u32::try_from(text[*start..*end].chars().count())
                .map_err(|error| ModuleError::other(error.to_string()))?,
        );
    }
    let transcript_index_pages = u32::try_from(page_chars.chunks(HISTORY_INDEX_SIZE).len())
        .map_err(|error| ModuleError::other(error.to_string()))?;
    let transcript_span_pages = transcript_pages;
    let info = transcript_info(
        text,
        transcript_pages,
        transcript_index_pages,
        transcript_span_pages,
        &page_chars,
        transcript_messages,
        transcript_truncated,
    )?;
    let history = history_record(ctx, closing, info)?;
    write_history_pages(
        ctx,
        closing.channel,
        closing.attempt,
        text,
        &pages,
        &page_chars,
        expires_at,
    )
    .await?;
    if let Err(error) = data::write_history_span_pages(
        ctx,
        closing.channel,
        closing.attempt,
        text,
        &pages,
        spans,
        expires_at,
    )
    .await
    {
        remove_history_generation(
            ctx,
            closing.channel.get(),
            closing.attempt,
            transcript_pages,
            transcript_index_pages,
        )
        .await;
        return Err(error);
    }
    let key = history_key(closing.ticket.number, closing.channel);
    let id_ttl = match history_ttl_until(expires_at, ctx.now()) {
        Ok(ttl) => ttl,
        Err(error) => {
            remove_history_generation(
                ctx,
                closing.channel.get(),
                closing.attempt,
                transcript_pages,
                transcript_index_pages,
            )
            .await;
            return Err(error);
        }
    };
    if let Err(error) = ctx
        .store()
        .set_for(&history_id_key(closing.channel), &key, id_ttl)
        .await
    {
        remove_history_generation(
            ctx,
            closing.channel.get(),
            closing.attempt,
            transcript_pages,
            transcript_index_pages,
        )
        .await;
        return Err(stored(&error));
    }
    if let Err(error) = store_history_record(ctx, &key, &history).await {
        remove_history_generation(
            ctx,
            closing.channel.get(),
            closing.attempt,
            transcript_pages,
            transcript_index_pages,
        )
        .await;
        return Err(error);
    }
    Ok(history)
}

async fn store_history_record(
    ctx: &EventContext,
    key: &str,
    history: &TicketHistory,
) -> Result<(), ModuleError> {
    for _ in 0..5 {
        let ttl = history_ttl(history, ctx.now())?;
        let current = ctx
            .store()
            .entry(key)
            .await
            .map_err(|error| stored(&error))?;
        let result = match current {
            Some(entry) => {
                let latest: TicketHistory = serde_json::from_value(entry.value)
                    .map_err(|error| ModuleError::other(error.to_string()))?;
                if latest.status == HistoryStatus::Anonymizing
                    || (latest.status == HistoryStatus::Reopening
                        && latest.operation_until > ctx.now())
                    || latest.attempt > history.attempt
                    || (latest.status == HistoryStatus::Closed && latest.attempt >= history.attempt)
                {
                    return Err(ModuleError::other("ticket history was superseded"));
                }
                ctx.store()
                    .replace(key, history, Expect::Revision(entry.revision), Some(ttl))
                    .await
            }
            None => {
                ctx.store()
                    .replace(key, history, Expect::Missing, Some(ttl))
                    .await
            }
        };
        match result {
            Ok(_) => return Ok(()),
            Err(StorageError::Conflict) => {}
            Err(error) => return Err(stored(&error)),
        }
    }
    Err(ModuleError::other(
        "ticket history changed while it was being saved",
    ))
}

async fn history_by_number(
    ctx: &EventContext,
    number: i64,
) -> Result<Option<TicketHistory>, ModuleError> {
    let Some((_, entry)) = ctx
        .store()
        .list(&history_number_prefix(number), 1)
        .await
        .map_err(|error| stored(&error))?
        .into_iter()
        .next()
    else {
        return Ok(None);
    };
    serde_json::from_value(entry.value)
        .map(Some)
        .map_err(|error| ModuleError::other(error.to_string()))
}

fn history_matches(history: &TicketHistory, query: &str) -> bool {
    [
        history.number.to_string(),
        history.opener.to_string(),
        history
            .claimed_by
            .map(|id| id.to_string())
            .unwrap_or_default(),
        history.closer.map(|id| id.to_string()).unwrap_or_default(),
    ]
    .iter()
    .any(|value| value.contains(query))
        || history.reason.to_lowercase().contains(query)
        || history
            .labels
            .iter()
            .any(|label| label.trim().to_lowercase() == query)
}

fn history_search_session_key(owner: u64, token: &str) -> String {
    format!("{HISTORY_SEARCH_SESSION_PREFIX}{owner}:{token}")
}

fn history_search_token(
    owner: u64,
    query: &str,
    seed: &str,
    after_key: &str,
    current: Option<&HistorySearchResume>,
    now: i64,
) -> String {
    let resume = current.map_or_else(String::new, |resume| {
        format!("{}:{}:{}", resume.key, resume.next_page, resume.carry)
    });
    let mut first = DefaultHasher::new();
    (owner, query, seed, after_key, resume.as_str(), now).hash(&mut first);
    let mut second = DefaultHasher::new();
    (now, resume.as_str(), after_key, seed, query, owner).hash(&mut second);
    format!("{:016x}{:016x}", first.finish(), second.finish())
}

fn history_search_token_is_valid(token: &str) -> bool {
    token.len() == 32 && token.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn history_search_tail(text: &str, query: &str) -> String {
    let keep = query.chars().count().saturating_sub(1);
    let count = text.chars().count();
    text.chars().skip(count.saturating_sub(keep)).collect()
}

fn history_search_excerpt(text: &str, query: &str) -> Option<String> {
    let at = text.find(query)?;
    let before = text[..at].chars().count();
    let matched = query.chars().count();
    let start = before.saturating_sub(24);
    let end = before.saturating_add(matched).saturating_add(24);
    let count = text.chars().count();
    let excerpt = text
        .chars()
        .skip(start)
        .take(end.min(count).saturating_sub(start))
        .collect::<String>()
        .replace(['\n', '\r'], " ");
    Some(format!(
        "{}{}{}",
        if start > 0 { "…" } else { "" },
        excerpt,
        if end < count { "…" } else { "" }
    ))
}

fn history_search_metadata_excerpt(history: &TicketHistory, query: &str) -> String {
    let searchable = format!("{} {}", history.reason, history.labels.join(" ")).to_lowercase();
    history_search_excerpt(&searchable, query).unwrap_or_else(|| {
        history
            .reason
            .replace(['\n', '\r'], " ")
            .chars()
            .take(80)
            .collect()
    })
}

#[allow(clippy::large_enum_variant)]
enum HistoryRecordSearch {
    Inaccessible,
    Match(TicketHistory, String),
    NoMatch,
    Partial(HistorySearchResume),
    Incomplete,
}

async fn history_search_transcript(
    ctx: &EventContext,
    history: &TicketHistory,
    query: &str,
    start_page: u32,
    mut carry: String,
    page_budget: &mut usize,
) -> Result<HistoryTextSearch, ModuleError> {
    if start_page > history.transcript_pages {
        return Ok(HistoryTextSearch::Incomplete);
    }
    let mut page = start_page;
    let mut cached_index: Option<(usize, Vec<u32>)> = None;
    while page < history.transcript_pages {
        if *page_budget == 0 {
            return Ok(HistoryTextSearch::Partial {
                next_page: page,
                carry,
            });
        }
        let content_page =
            usize::try_from(page).map_err(|error| ModuleError::other(error.to_string()))?;
        let index_page = content_page / HISTORY_INDEX_SIZE;
        if index_page >= history.transcript_index_pages as usize {
            return Ok(HistoryTextSearch::Incomplete);
        }
        if cached_index
            .as_ref()
            .is_none_or(|(cached_page, _)| *cached_page != index_page)
        {
            let Some(counts) = ctx
                .store()
                .get::<Vec<u32>>(&history_index_key(
                    history.channel_id,
                    history.transcript_attempt,
                    index_page,
                ))
                .await
                .map_err(|error| stored(&error))?
            else {
                return Ok(HistoryTextSearch::Incomplete);
            };
            if counts.len() > HISTORY_INDEX_SIZE {
                return Ok(HistoryTextSearch::Incomplete);
            }
            cached_index = Some((index_page, counts));
        }
        let Some((_, counts)) = cached_index.as_ref() else {
            return Ok(HistoryTextSearch::Incomplete);
        };
        let Some(expected_chars) = counts.get(content_page % HISTORY_INDEX_SIZE) else {
            return Ok(HistoryTextSearch::Incomplete);
        };
        *page_budget -= 1;
        let Some(content) = ctx
            .store()
            .get::<String>(&history_content_key(
                history.channel_id,
                history.transcript_attempt,
                content_page,
            ))
            .await
            .map_err(|error| stored(&error))?
        else {
            return Ok(HistoryTextSearch::Incomplete);
        };
        let expected_chars = usize::try_from(*expected_chars)
            .map_err(|error| ModuleError::other(error.to_string()))?;
        if content.chars().count() != expected_chars {
            return Ok(HistoryTextSearch::Incomplete);
        }
        let combined = format!("{carry}{}", content.to_lowercase());
        if let Some(excerpt) = history_search_excerpt(&combined, query) {
            return Ok(HistoryTextSearch::Match(excerpt));
        }
        carry = history_search_tail(&combined, query);
        page += 1;
    }
    if history.transcript_truncated {
        Ok(HistoryTextSearch::Incomplete)
    } else {
        Ok(HistoryTextSearch::NoMatch)
    }
}

async fn search_history_record(
    ctx: &EventContext,
    state: &HistorySearchState,
    key: String,
    history: TicketHistory,
    resume: Option<HistorySearchResume>,
    access: &HistorySearchAccess<'_>,
    page_budget: &mut usize,
) -> Result<HistoryRecordSearch, ModuleError> {
    if !may_view_history(access.actor_id, access.roles, access.can_manage, &history) {
        return Ok(HistoryRecordSearch::Inaccessible);
    }
    if history.status != HistoryStatus::Closed {
        return Ok(HistoryRecordSearch::NoMatch);
    }
    if !data::history_privacy_generation_current(ctx, &key, history.privacy_generation).await? {
        return Ok(HistoryRecordSearch::NoMatch);
    }
    if history.version != 1
        || history.number <= 0
        || history.channel_id == 0
        || history_key(history.number, ChannelId::new(history.channel_id)) != key
        || resume.as_ref().is_some_and(|resume| {
            resume.channel_id != history.channel_id
                || resume.number != history.number
                || resume.transcript_attempt != history.transcript_attempt
                || resume.privacy_generation != history.privacy_generation
        })
    {
        return Ok(HistoryRecordSearch::Incomplete);
    }
    let result = if history_matches(&history, &state.query) {
        HistoryRecordSearch::Match(
            history.clone(),
            history_search_metadata_excerpt(&history, &state.query),
        )
    } else {
        let (start_page, carry) = resume.map_or_else(
            || (0, String::new()),
            |resume| (resume.next_page, resume.carry),
        );
        match history_search_transcript(ctx, &history, &state.query, start_page, carry, page_budget)
            .await?
        {
            HistoryTextSearch::Match(excerpt) => {
                HistoryRecordSearch::Match(history.clone(), excerpt)
            }
            HistoryTextSearch::NoMatch => HistoryRecordSearch::NoMatch,
            HistoryTextSearch::Partial { next_page, carry } => {
                HistoryRecordSearch::Partial(HistorySearchResume {
                    key: key.clone(),
                    channel_id: history.channel_id,
                    number: history.number,
                    transcript_attempt: history.transcript_attempt,
                    privacy_generation: history.privacy_generation,
                    next_page,
                    carry,
                })
            }
            HistoryTextSearch::Incomplete => HistoryRecordSearch::Incomplete,
        }
    };
    if !data::history_privacy_generation_current(ctx, &key, history.privacy_generation).await? {
        return Ok(HistoryRecordSearch::NoMatch);
    }
    Ok(result)
}

fn apply_history_search_result(
    state: &mut HistorySearchState,
    key: String,
    result: HistoryRecordSearch,
    hits: &mut Vec<HistorySearchHit>,
) -> bool {
    match result {
        HistoryRecordSearch::Inaccessible | HistoryRecordSearch::NoMatch => {
            state.after_key = key;
        }
        HistoryRecordSearch::Match(history, excerpt) => {
            hits.push(HistorySearchHit { history, excerpt });
            state.after_key = key;
        }
        HistoryRecordSearch::Partial(resume) => {
            state.current = Some(resume);
            return true;
        }
        HistoryRecordSearch::Incomplete => {
            state.incomplete = true;
            state.after_key = key;
        }
    }
    false
}

async fn search_current_history(
    ctx: &EventContext,
    state: &mut HistorySearchState,
    access: &HistorySearchAccess<'_>,
    page_budget: &mut usize,
    hits: &mut Vec<HistorySearchHit>,
) -> Result<(), ModuleError> {
    let Some(resume) = state.current.take() else {
        return Ok(());
    };
    let key = resume.key.clone();
    let Some(history) = ctx
        .store()
        .get::<TicketHistory>(&key)
        .await
        .map_err(|error| stored(&error))?
    else {
        state.incomplete = true;
        state.after_key = key;
        return Ok(());
    };
    let result = search_history_record(
        ctx,
        state,
        key.clone(),
        history,
        Some(resume),
        access,
        page_budget,
    )
    .await?;
    apply_history_search_result(state, key, result, hits);
    Ok(())
}

async fn filter_current_history_hits(
    ctx: &EventContext,
    hits: &mut Vec<HistorySearchHit>,
) -> Result<(), ModuleError> {
    let mut current = Vec::with_capacity(hits.len());
    for hit in std::mem::take(hits) {
        let key = history_key(hit.history.number, ChannelId::new(hit.history.channel_id));
        if data::history_privacy_generation_current(ctx, &key, hit.history.privacy_generation)
            .await?
        {
            current.push(hit);
        }
    }
    *hits = current;
    Ok(())
}

async fn search_history_batch(
    ctx: &EventContext,
    state: &mut HistorySearchState,
    access: &HistorySearchAccess<'_>,
) -> Result<Vec<HistorySearchHit>, ModuleError> {
    let mut hits = Vec::new();
    let mut records = 0;
    let mut page_budget = HISTORY_SEARCH_PAGE_BUDGET;
    if state.current.is_some() {
        records += 1;
        search_current_history(ctx, state, access, &mut page_budget, &mut hits).await?;
        if state.current.is_some()
            || records >= HISTORY_SEARCH_BATCH
            || hits.len() >= HISTORY_RESULTS
            || page_budget == 0
        {
            state.complete = false;
            return Ok(hits);
        }
    }
    let limit = HISTORY_SEARCH_BATCH.saturating_sub(records);
    if limit == 0 {
        state.complete = false;
        return Ok(hits);
    }
    let entries = ctx
        .store()
        .list_after(HISTORY_PREFIX, &state.after_key, limit)
        .await
        .map_err(|error| stored(&error))?;
    if entries.is_empty() {
        state.complete = true;
        return Ok(hits);
    }
    let entries_len = entries.len();
    let mut processed = 0;
    for (key, entry) in entries {
        if records >= HISTORY_SEARCH_BATCH || hits.len() >= HISTORY_RESULTS || page_budget == 0 {
            break;
        }
        records += 1;
        processed += 1;
        let Ok(history) = serde_json::from_value(entry.value) else {
            state.incomplete = true;
            state.after_key = key;
            continue;
        };
        let result = search_history_record(
            ctx,
            state,
            key.clone(),
            history,
            None,
            access,
            &mut page_budget,
        )
        .await?;
        if apply_history_search_result(state, key, result, &mut hits) {
            break;
        }
    }
    state.complete = state.current.is_none() && processed == entries_len && entries_len < limit;
    Ok(hits)
}

fn history_search_state_valid(
    state: &HistorySearchState,
    owner: u64,
    token: &str,
    now: i64,
) -> bool {
    let ttl = state.expires_at.checked_sub(now);
    let max_ttl = i64::try_from(HISTORY_SEARCH_TTL.as_secs()).unwrap_or(i64::MAX);
    state.owner == owner
        && state.token == token
        && history_search_token_is_valid(token)
        && !state.consumed
        && !state.complete
        && !state.query.is_empty()
        && state.query.chars().count() <= HISTORY_SEARCH_NORMALIZED_CHARS
        && state.query == state.query.trim().to_lowercase()
        && (state.after_key == HISTORY_PREFIX
            || (state.after_key.starts_with(HISTORY_PREFIX) && state.after_key.len() <= 128))
        && ttl.is_some_and(|seconds| seconds > 0 && seconds <= max_ttl)
        && state.current.as_ref().is_none_or(|resume| {
            resume.key.starts_with(HISTORY_PREFIX)
                && resume.key.len() <= 128
                && resume.key > state.after_key
                && resume.carry.chars().count() < state.query.chars().count()
        })
}

fn history_search_initial_state(
    owner: u64,
    query: String,
    now: i64,
    interaction_id: u64,
) -> Result<HistorySearchState, ModuleError> {
    let ttl = i64::try_from(HISTORY_SEARCH_TTL.as_secs())
        .map_err(|error| ModuleError::other(error.to_string()))?;
    let expires_at = now
        .checked_add(ttl)
        .ok_or_else(|| ModuleError::other("ticket search expiry is out of range"))?;
    let seed = format!("start:{now}:{interaction_id}");
    let token = history_search_token(owner, &query, &seed, HISTORY_PREFIX, None, now);
    Ok(HistorySearchState {
        owner,
        token,
        query,
        after_key: HISTORY_PREFIX.to_owned(),
        current: None,
        expires_at,
        incomplete: false,
        complete: false,
        consumed: false,
    })
}

fn history_search_next_token(state: &HistorySearchState, old_token: &str) -> String {
    history_search_token(
        state.owner,
        &state.query,
        old_token,
        &state.after_key,
        state.current.as_ref(),
        0,
    )
}

fn history_search_button(
    ctx: &EventContext,
    state: &HistorySearchState,
    label: String,
) -> Result<Component, ModuleError> {
    let now = ctx.now();
    let lifetime = history_ttl_until(state.expires_at, now)?;
    let custom_id = CustomId::new("history-search-next")
        .only_for(UserId::new(state.owner))
        .data(state.token.clone())
        .expires_in(now, lifetime)
        .encode(&ctx.module);
    Ok(ui::button(ButtonStyle::Primary, label, custom_id))
}

fn history_search_retry_button(
    ctx: &EventContext,
    owner: u64,
    token: &str,
    label: String,
) -> Component {
    let custom_id = CustomId::new("history-search-next")
        .only_for(UserId::new(owner))
        .data(token.to_owned())
        .expires_in(ctx.now(), HISTORY_SEARCH_TTL)
        .encode(&ctx.module);
    ui::button(ButtonStyle::Primary, label, custom_id)
}

async fn retry_history_search_storage(
    cx: &ComponentCx,
    ctx: &EventContext,
    owner: u64,
    token: &str,
) -> Result<(), UserError> {
    let button = history_search_retry_button(
        ctx,
        owner,
        token,
        cx.t("tickets-history-search-next", &Args::new()),
    );
    cx.update(history_search_update(
        cx.t("tickets-history-search-retry", &Args::new()),
        vec![ui::row(vec![button])],
    ))
    .await
}

fn history_search_components(
    ctx: &EventContext,
    state: &HistorySearchState,
    label: String,
) -> Result<Vec<Component>, ModuleError> {
    if state.complete {
        return Ok(Vec::new());
    }
    Ok(vec![ui::row(vec![history_search_button(
        ctx, state, label,
    )?])])
}

fn history_search_reply(content: String, components: Vec<Component>) -> InteractionReply {
    InteractionReply {
        content: Some(content),
        components,
        ephemeral: true,
        ..InteractionReply::default()
    }
}

fn history_search_update(content: String, components: Vec<Component>) -> InteractionReply {
    InteractionReply {
        content: Some(content),
        components,
        ..InteractionReply::default()
    }
}

fn history_search_display(text: &str, limit: usize) -> String {
    let normalized = text.replace(['\n', '\r'], " ");
    let mut chars = normalized.chars();
    let display = chars.by_ref().take(limit).collect::<String>();
    if chars.next().is_some() {
        format!("{display}…")
    } else {
        display
    }
}

fn history_search_reply_text<F>(
    state: &HistorySearchState,
    hits: &[HistorySearchHit],
    mut translate: F,
) -> String
where
    F: FnMut(&str, &Args) -> String,
{
    let mut lines = Vec::new();
    if hits.is_empty() {
        lines.push(translate("tickets-history-search-none-batch", &Args::new()));
    } else {
        lines.extend(hits.iter().map(|hit| {
            let opener = if hit.history.opener_erased {
                translate("tickets-history-erased-opener", &Args::new())
            } else {
                format!("<@{}>", hit.history.opener)
            };
            translate(
                "tickets-history-search-result",
                &Args::new()
                    .with("number", hit.history.number)
                    .with("opener", opener)
                    .with(
                        "closer",
                        hit.history
                            .closer
                            .map_or_else(|| "-".to_owned(), |id| format!("<@{id}>")),
                    )
                    .with("closed", format!("<t:{}:R>", hit.history.closed))
                    .with("reason", history_search_display(&hit.history.reason, 20))
                    .with("excerpt", history_search_display(&hit.excerpt, 48)),
            )
        }));
    }
    if state.incomplete {
        lines.push(translate("tickets-history-search-incomplete", &Args::new()));
    }
    let progress_key = if state.complete {
        "tickets-history-search-complete"
    } else {
        "tickets-history-search-more"
    };
    lines.push(translate(progress_key, &Args::new()));
    lines.join("\n")
}

fn may_view_history(
    actor_id: u64,
    roles: &[RoleId],
    can_manage: bool,
    history: &TicketHistory,
) -> bool {
    teams_survey::may_access(
        history.staff_access.as_ref(),
        &history.staff_roles,
        actor_id,
        roles,
        can_manage,
    )
}

async fn history_page(
    ctx: &EventContext,
    history: &TicketHistory,
    page: u64,
) -> Result<String, ModuleError> {
    let start = (page - 1) * HISTORY_DISPLAY_CHARS as u64;
    let end = start
        .saturating_add(HISTORY_DISPLAY_CHARS as u64)
        .min(history.transcript_char_count);
    let mut chars_before = 0_u64;
    let mut text = String::new();
    for index_page in 0..history.transcript_index_pages {
        let counts: Vec<u32> = ctx
            .store()
            .get(&history_index_key(
                history.channel_id,
                history.transcript_attempt,
                index_page as usize,
            ))
            .await
            .map_err(|error| stored(&error))?
            .ok_or_else(|| ModuleError::other("ticket transcript index is missing"))?;
        if counts.len() > HISTORY_INDEX_SIZE {
            return Err(ModuleError::other("ticket transcript index is invalid"));
        }
        for (offset, count) in counts.iter().enumerate() {
            let content_page = index_page as usize * HISTORY_INDEX_SIZE + offset;
            if content_page >= history.transcript_pages as usize {
                return Err(ModuleError::other("ticket transcript index is invalid"));
            }
            let page_end = chars_before + u64::from(*count);
            if page_end > start && chars_before < end {
                let content: String = ctx
                    .store()
                    .get(&history_content_key(
                        history.channel_id,
                        history.transcript_attempt,
                        content_page,
                    ))
                    .await
                    .map_err(|error| stored(&error))?
                    .ok_or_else(|| ModuleError::other("ticket transcript page is missing"))?;
                let expected_chars = usize::try_from(*count)
                    .map_err(|error| ModuleError::other(error.to_string()))?;
                if content.chars().count() != expected_chars {
                    return Err(ModuleError::other("ticket transcript page is invalid"));
                }
                let from = usize::try_from(start.saturating_sub(chars_before))
                    .map_err(|error| ModuleError::other(error.to_string()))?;
                let to = usize::try_from(end.min(page_end) - chars_before)
                    .map_err(|error| ModuleError::other(error.to_string()))?;
                text.extend(content.chars().skip(from).take(to - from));
            }
            chars_before = page_end;
            if chars_before >= end {
                break;
            }
        }
        if chars_before >= end {
            break;
        }
    }
    let expected =
        usize::try_from(end - start).map_err(|error| ModuleError::other(error.to_string()))?;
    if text.chars().count() != expected {
        return Err(ModuleError::other("ticket transcript is incomplete"));
    }
    Ok(text)
}

async fn mark_history_closed(
    ctx: &EventContext,
    history: &TicketHistory,
) -> Result<(), ModuleError> {
    let key = history_key(history.number, ChannelId::new(history.channel_id));
    for _ in 0..5 {
        let entry = ctx
            .store()
            .entry(&key)
            .await
            .map_err(|error| stored(&error))?
            .ok_or_else(|| ModuleError::other("ticket history is missing"))?;
        let mut latest: TicketHistory = serde_json::from_value(entry.value)
            .map_err(|error| ModuleError::other(error.to_string()))?;
        if latest.status == HistoryStatus::Anonymizing
            || (latest.status == HistoryStatus::Reopening && latest.operation_until > ctx.now())
        {
            return Err(ModuleError::other(
                "ticket history has an active privacy operation",
            ));
        }
        if latest.status == HistoryStatus::Closed {
            return Ok(());
        }
        latest.status = HistoryStatus::Closed;
        latest.operation_token = None;
        latest.operation_until = 0;
        latest.anonymizing_user = None;
        latest.closed = ctx.now();
        let ttl = history_ttl(&latest, ctx.now())?;
        match ctx
            .store()
            .replace(&key, &latest, Expect::Revision(entry.revision), Some(ttl))
            .await
        {
            Ok(_) => return Ok(()),
            Err(StorageError::Conflict) => {}
            Err(error) => return Err(stored(&error)),
        }
    }
    Err(ModuleError::other(
        "ticket history changed while it was closing",
    ))
}

async fn discard_history_generation(ctx: &EventContext, history: &TicketHistory) {
    remove_history_generation(
        ctx,
        history.channel_id,
        history.transcript_attempt,
        history.transcript_pages,
        history.transcript_index_pages,
    )
    .await;
    if let Err(error) = teams_survey::delete_exit_survey(ctx, history).await {
        tracing::warn!(
            ?error,
            channel = history.channel_id,
            "could not remove ticket exit survey with its transcript generation"
        );
    }
}

async fn validate_history_archive(
    ctx: &EventContext,
    closing: &CloseAttempt<'_>,
    history: &TicketHistory,
) -> Result<(), ModuleError> {
    let page_count = usize::try_from(history.transcript_pages)
        .map_err(|error| ModuleError::other(error.to_string()))?;
    let expected_index_pages =
        page_count.saturating_add(HISTORY_INDEX_SIZE - 1) / HISTORY_INDEX_SIZE;
    let index_page_count = usize::try_from(history.transcript_index_pages)
        .map_err(|error| ModuleError::other(error.to_string()))?;
    if history.version != 1
        || history.channel_id != closing.channel.get()
        || history.number != closing.ticket.number
        || history.attempt != closing.attempt
        || history.status != HistoryStatus::Closing
        || page_count == 0
        || history.transcript_char_count == 0
        || index_page_count != expected_index_pages
    {
        return Err(ModuleError::other("ticket history metadata is incomplete"));
    }
    let mut chars = 0_u64;
    let mut bytes = 0_u64;
    for index_page in 0..history.transcript_index_pages {
        let index_page =
            usize::try_from(index_page).map_err(|error| ModuleError::other(error.to_string()))?;
        let page_start = index_page
            .checked_mul(HISTORY_INDEX_SIZE)
            .ok_or_else(|| ModuleError::other("ticket history index is too large"))?;
        let expected_entries = page_count
            .saturating_sub(page_start)
            .min(HISTORY_INDEX_SIZE);
        let counts: Vec<u32> = ctx
            .store()
            .get(&history_index_key(
                history.channel_id,
                history.transcript_attempt,
                index_page,
            ))
            .await
            .map_err(|error| stored(&error))?
            .ok_or_else(|| ModuleError::other("ticket transcript index is missing"))?;
        if counts.len() != expected_entries {
            return Err(ModuleError::other("ticket transcript index is invalid"));
        }
        for (offset, count) in counts.iter().enumerate() {
            let page = page_start + offset;
            let content: String = ctx
                .store()
                .get(&history_content_key(
                    history.channel_id,
                    history.transcript_attempt,
                    page,
                ))
                .await
                .map_err(|error| stored(&error))?
                .ok_or_else(|| ModuleError::other("ticket transcript page is missing"))?;
            let expected_chars =
                usize::try_from(*count).map_err(|error| ModuleError::other(error.to_string()))?;
            let serialized = serde_json::to_vec(&content)
                .map_err(|error| ModuleError::other(error.to_string()))?;
            if content.chars().count() != expected_chars || serialized.len() > HISTORY_VALUE_BYTES {
                return Err(ModuleError::other("ticket transcript page is invalid"));
            }
            chars = chars
                .checked_add(u64::from(*count))
                .ok_or_else(|| ModuleError::other("ticket transcript is too long"))?;
            bytes = bytes
                .checked_add(
                    u64::try_from(content.len())
                        .map_err(|error| ModuleError::other(error.to_string()))?,
                )
                .ok_or_else(|| ModuleError::other("ticket transcript is too long"))?;
        }
    }
    if chars != history.transcript_char_count || bytes != history.transcript_bytes {
        return Err(ModuleError::other(
            "ticket transcript does not match its metadata",
        ));
    }
    data::validate_history_span_pages(
        ctx,
        history.channel_id,
        history.transcript_attempt,
        history.transcript_pages,
        history.transcript_span_pages,
    )
    .await?;
    Ok(())
}

async fn renew_close_lease(
    ctx: &EventContext,
    closing: &CloseAttempt<'_>,
) -> Result<bool, ModuleError> {
    let key = ticket_key(closing.channel);
    for _ in 0..5 {
        let Some(entry) = ctx
            .store()
            .entry(&key)
            .await
            .map_err(|error| stored(&error))?
        else {
            return Ok(false);
        };
        let mut ticket: Ticket = serde_json::from_value(entry.value)
            .map_err(|error| ModuleError::other(error.to_string()))?;
        if ticket.number != closing.ticket.number
            || ticket.close_generation != closing.attempt
            || ticket.closing.is_none()
        {
            return Ok(false);
        }
        ticket.closing = Some(ctx.now());
        refresh_ticket_form_answer_ttl(ctx, &ticket).await?;
        match ctx
            .store()
            .replace(
                &key,
                &ticket,
                Expect::Revision(entry.revision),
                Some(ticket_ttl(&ticket, ctx.now())),
            )
            .await
        {
            Ok(_) => return Ok(true),
            Err(StorageError::Conflict) => {}
            Err(error) => return Err(stored(&error)),
        }
    }
    Err(ModuleError::other(
        "ticket changed while its close lease was renewed",
    ))
}

#[allow(clippy::too_many_lines)]
async fn complete_history_close(
    ctx: &EventContext,
    closing: &CloseAttempt<'_>,
    history: &TicketHistory,
    recovery_job: bool,
) -> Result<Closed, ModuleError> {
    if history.status == HistoryStatus::Closed {
        forget(ctx, closing.channel, closing.ticket, true).await?;
        knowledge::cancel_ticket_suggestions(ctx, closing.channel, closing.ticket.number).await?;
        let _ = send_history_link_dm(ctx, history).await;
        let rating_enabled = history
            .panel
            .as_ref()
            .is_none_or(|panel| panel.customization.rating_enabled);
        if !rating_enabled || history.rating.is_some() {
            let _ = teams_survey::send_exit_survey_prompt(ctx, history).await;
        }
        if !rating_enabled || history.rating.is_some() || history.rating_prompt_attempted {
            return Ok(Closed::Done);
        }
        return Ok(
            if send_rating_prompt(ctx, closing.channel, closing.ticket, closing.attempt).await {
                Closed::Done
            } else {
                Closed::RatingDmUnconfirmed
            },
        );
    }
    if history.status != HistoryStatus::Closing
        || history.channel_id != closing.channel.get()
        || history.number != closing.ticket.number
        || history.attempt != closing.attempt
        || !history.log_copy_sent
    {
        return Err(ModuleError::other("ticket history is not ready to close"));
    }
    let lease = match renew_close_lease(ctx, closing).await {
        Ok(lease) => lease,
        Err(error) => {
            reopen(ctx, closing).await;
            return Err(error);
        }
    };
    if !lease {
        return Ok(Closed::BusyUntil(ctx.now() + CLOSING_WAIT));
    }
    if !recovery_job && let Err(error) = watch_closing(ctx, closing, ctx.now() + CLOSING_WAIT).await
    {
        reopen(ctx, closing).await;
        return Err(error);
    }
    let not_found = if closing.ticket.thread {
        match ctx
            .api
            .execute(Action::EditThread {
                thread_id: closing.channel,
                patch: ThreadPatch {
                    archived: Some(true),
                    locked: Some(true),
                    ..ThreadPatch::default()
                },
                reason: Reason::truncated(closing.reason),
            })
            .await
        {
            Ok(_) => false,
            Err(DiscordError::NotFound) => true,
            Err(error) => return Err(stored(&error)),
        }
    } else {
        match ctx
            .api
            .execute(Action::DeleteChannel {
                channel_id: closing.channel,
                reason: Reason::truncated(closing.reason),
            })
            .await
        {
            Ok(_) => false,
            Err(DiscordError::NotFound) => true,
            Err(error) => return Err(stored(&error)),
        }
    };
    if not_found && !recovery_job {
        validate_history_archive(ctx, closing, history).await?;
    }
    if !renew_close_lease(ctx, closing).await? {
        return Ok(Closed::BusyUntil(ctx.now() + CLOSING_WAIT));
    }
    mark_history_closed(ctx, history).await?;
    forget(
        ctx,
        closing.channel,
        closing.ticket,
        closing.ticket.thread && !not_found,
    )
    .await?;
    knowledge::cancel_ticket_suggestions(ctx, closing.channel, closing.ticket.number).await?;
    let _ = send_history_link_dm(ctx, history).await;
    let rating_enabled = history
        .panel
        .as_ref()
        .is_none_or(|panel| panel.customization.rating_enabled);
    if !rating_enabled || history.rating.is_some() {
        let _ = teams_survey::send_exit_survey_prompt(ctx, history).await;
    }
    if !rating_enabled || history.rating.is_some() || history.rating_prompt_attempted {
        return Ok(Closed::Done);
    }
    Ok(
        if send_rating_prompt(ctx, closing.channel, closing.ticket, closing.attempt).await {
            Closed::Done
        } else {
            Closed::RatingDmUnconfirmed
        },
    )
}

#[allow(clippy::too_many_lines)]
async fn prepare_history(
    ctx: &EventContext,
    closing: &CloseAttempt<'_>,
    previous: Option<TicketHistory>,
) -> Result<TicketHistory, ModuleError> {
    let key = history_key(closing.ticket.number, closing.channel);
    if let Some(mut history) = previous
        .as_ref()
        .filter(|history| history.status == HistoryStatus::Closing && history.log_copy_sent)
        .cloned()
    {
        history.attempt = closing.attempt;
        store_history_record(ctx, &key, &history).await?;
        validate_history_archive(ctx, closing, &history).await?;
        return Ok(history);
    }
    let mut archive = match data::capture_transcript(ctx, closing.channel, closing.ticket).await {
        Ok(archive) => archive,
        Err(DiscordError::NotFound) => {
            let Some(mut history) =
                previous.filter(|history| history.status == HistoryStatus::Closing)
            else {
                reopen(ctx, closing).await;
                return Err(stored(&DiscordError::NotFound));
            };
            history.attempt = closing.attempt;
            validate_history_archive(ctx, closing, &history).await?;
            history.log_copy_sent = true;
            store_history_record(ctx, &key, &history).await?;
            return Ok(history);
        }
        Err(error) => {
            reopen(ctx, closing).await;
            return Err(stored(&error));
        }
    };
    let log_copy_text = archive.text.clone();
    let answers = match advanced_forms::load_ticket_form_answers(ctx, closing.ticket).await {
        Ok(answers) => answers,
        Err(error) => {
            reopen(ctx, closing).await;
            return Err(stored(&error));
        }
    };
    data::append_form_answers(ctx, &mut archive, &answers, closing.ticket.opener);
    let transcript_messages = archive.messages.len();
    let mut history = match save_history(
        ctx,
        closing,
        Some((
            &archive.text,
            archive.truncated,
            transcript_messages,
            &archive.spans,
        )),
    )
    .await
    {
        Ok(history) => history,
        Err(error) => {
            reopen(ctx, closing).await;
            return Err(error);
        }
    };
    if let Some(log) = closing.config.log() {
        let note = ctx.t_in(
            ctx.locale,
            "tickets-closed-log",
            &Args::new()
                .with("number", closing.ticket.number)
                .with("opener", format!("<@{}>", closing.ticket.opener))
                .with(
                    "closer",
                    closing
                        .closer
                        .map_or_else(|| "-".to_owned(), |user| format!("<@{user}>")),
                )
                .with("reason", closing.reason.to_owned()),
        );
        let posted = ctx
            .api
            .execute(Action::PostMessage {
                channel_id: log,
                message_id: None,
                reply_to: None,
                content: Some(note),
                embeds: Vec::new(),
                components: Vec::new(),
                files: vec![FileUpload {
                    filename: format!("ticket-{:04}.txt", closing.ticket.number),
                    data: log_copy_text.into_bytes(),
                }],
            })
            .await;
        history.log_copy_message = match posted {
            Ok(Outcome::Message(message)) => Some(message.id.get()),
            Ok(_) => None,
            Err(error) => {
                reopen(ctx, closing).await;
                return Err(stored(&error));
            }
        };
        history.log_copy_sent = true;
        if let Err(error) = store_history_record(ctx, &key, &history).await {
            reopen(ctx, closing).await;
            return Err(error);
        }
    }
    if let Some(old) = previous
        .as_ref()
        .filter(|old| old.transcript_attempt != history.transcript_attempt)
    {
        discard_history_generation(ctx, old).await;
    }
    Ok(history)
}

async fn update_exit_survey_archive(
    ctx: &EventContext,
    history: &TicketHistory,
) -> Result<(), ModuleError> {
    if history.status != HistoryStatus::Closed || history.opener_erased {
        return Ok(());
    }
    let (Some(channel), Some(message)) = (history.log_copy_channel, history.log_copy_message)
    else {
        return Ok(());
    };
    if teams_survey::exit_survey_answers_for_export(ctx, history, UserId::new(history.opener))
        .await
        .map_err(|error| stored(&error))?
        .is_none_or(|answers| answers.is_empty())
    {
        return Ok(());
    }
    let key = history_key(history.number, ChannelId::new(history.channel_id));
    if !data::history_privacy_generation_current(ctx, &key, history.privacy_generation).await? {
        return Ok(());
    }

    let channel = ChannelId::new(channel);
    let message_id = MessageId::new(message);
    // Preserve the transcript body and embeds while changing only its components.
    let existing = match ctx
        .api
        .execute(Action::GetMessage {
            channel_id: channel,
            message_id,
        })
        .await
    {
        Ok(Outcome::Message(message))
            if message.channel_id == channel
                && ctx.bot().is_some_and(|bot| message.author.id == bot) =>
        {
            message
        }
        Ok(_) | Err(DiscordError::NotFound) => {
            return Err(ModuleError::other("ticket archive message is missing"));
        }
        Err(error) => return Err(stored(&error)),
    };
    let components = teams_survey::survey_view_components(ctx, history);
    if components.is_empty() {
        return Err(ModuleError::other(
            "ticket exit survey button could not be encoded",
        ));
    }
    match ctx
        .api
        .execute(Action::PostMessage {
            channel_id: channel,
            message_id: Some(message_id),
            reply_to: None,
            content: Some(existing.content),
            embeds: existing.embeds,
            components,
            files: Vec::new(),
        })
        .await
    {
        Ok(Outcome::Message(_)) => Ok(()),
        Ok(_) => Err(ModuleError::other(
            "ticket exit survey archive update returned no message",
        )),
        Err(error) => Err(stored(&error)),
    }
}

async fn begin_closing(
    ctx: &EventContext,
    key: &str,
    request_id: Option<i64>,
    automation_guard: Option<(i64, u64)>,
) -> Result<Claim, ModuleError> {
    for _ in 0..5 {
        let Some(entry) = ctx
            .store()
            .entry(key)
            .await
            .map_err(|error| stored(&error))?
        else {
            return Ok(Claim::Gone);
        };
        let mut ticket: Ticket = serde_json::from_value(entry.value)
            .map_err(|error| ModuleError::other(error.to_string()))?;
        if let Some((close_generation, automation_generation)) = automation_guard
            && (ticket.close_generation != close_generation
                || ticket.automation.generation != automation_generation)
        {
            return Ok(Claim::AutomationChanged);
        }
        if ticket.closed {
            return Ok(Claim::Gone);
        }
        if ticket.pending_operation.is_some() {
            return Ok(Claim::Busy(ctx.now() + CLOSING_WAIT));
        }
        if request_id.is_some_and(|request_id| {
            ticket
                .close_request
                .as_ref()
                .is_none_or(|request| request.id != request_id)
        }) {
            return Ok(Claim::RequestChanged);
        }
        if let Some(since) = ticket
            .closing
            .filter(|since| ctx.now() - *since < CLOSING_WAIT)
        {
            return Ok(Claim::Busy(since + CLOSING_WAIT));
        }
        ticket.close_generation = ticket
            .close_generation
            .checked_add(1)
            .ok_or_else(|| ModuleError::other("ticket close generation is exhausted"))?;
        ticket.closing = Some(ctx.now());
        refresh_ticket_form_answer_ttl(ctx, &ticket).await?;
        match ctx
            .store()
            .replace(
                key,
                &ticket,
                Expect::Revision(entry.revision),
                Some(ticket_ttl(&ticket, ctx.now())),
            )
            .await
        {
            Ok(_) => return Ok(Claim::Taken(Box::new(ticket))),
            Err(StorageError::Conflict) => {}
            Err(error) => return Err(stored(&error)),
        }
    }
    Err(ModuleError::other(
        "the ticket changed while it was being closed",
    ))
}

async fn forget(
    ctx: &EventContext,
    channel: ChannelId,
    ticket: &Ticket,
    keep_thread: bool,
) -> Result<(), ModuleError> {
    if keep_thread && ticket.thread {
        if !ticket.closed {
            let mut closed = ticket.clone();
            closed.closing = None;
            closed.close_request = None;
            closed.closed = true;
            refresh_ticket_form_answer_ttl(ctx, &closed).await?;
            ctx.store()
                .set_for(
                    &ticket_key(channel),
                    &closed,
                    ticket_ttl(&closed, ctx.now()),
                )
                .await
                .map_err(|error| stored(&error))?;
            give_back(ctx, ticket.opener).await;
        }
    } else {
        if ctx
            .store()
            .delete(&ticket_key(channel))
            .await
            .map_err(|error| stored(&error))?
        {
            give_back(ctx, ticket.opener).await;
        }
        delete_ticket_form_answers(ctx, ticket).await?;
    }
    let _ = ctx.scheduler().cancel(&idle_key(channel)).await;
    let _ = ctx.scheduler().cancel(&left_close_key(channel)).await;
    let _ = ctx.scheduler().cancel(&closing_key(channel)).await;
    if let Some(request) = &ticket.close_request {
        cancel_close_request_timeout(ctx, channel, request).await;
    }
    Ok(())
}

#[allow(clippy::large_futures)]
async fn finish(
    ctx: &EventContext,
    channel: ChannelId,
    closer: Option<UserId>,
    reason: &str,
    request_id: Option<i64>,
) -> Result<Closed, ModuleError> {
    finish_with_guard(ctx, channel, closer, reason, request_id, None).await
}

#[allow(clippy::large_futures)]
pub(crate) async fn finish_if_automation_generation(
    ctx: &EventContext,
    channel: ChannelId,
    expected_close_generation: i64,
    expected_automation_generation: u64,
    reason: &str,
) -> Result<Closed, ModuleError> {
    finish_with_guard(
        ctx,
        channel,
        None,
        reason,
        None,
        Some((expected_close_generation, expected_automation_generation)),
    )
    .await
}

#[allow(clippy::large_futures)]
async fn finish_with_guard(
    ctx: &EventContext,
    channel: ChannelId,
    closer: Option<UserId>,
    reason: &str,
    request_id: Option<i64>,
    automation_guard: Option<(i64, u64)>,
) -> Result<Closed, ModuleError> {
    if let Some(until) = operations::resume_pending(ctx, channel).await? {
        return Ok(Closed::BusyUntil(until));
    }
    let ticket =
        match begin_closing(ctx, &ticket_key(channel), request_id, automation_guard).await? {
            Claim::Taken(ticket) => *ticket,
            Claim::Gone | Claim::AutomationChanged => return Ok(Closed::Unknown),
            Claim::Busy(until) => return Ok(Closed::BusyUntil(until)),
            Claim::RequestChanged => return Ok(Closed::RequestChanged),
        };
    let fallback = Config::default();
    let raw_config: Config = match ctx.parse_config() {
        Ok(config) => config,
        Err(error) => {
            let closing = CloseAttempt {
                channel,
                ticket: &ticket,
                config: &fallback,
                closer,
                reason,
                attempt: ticket.close_generation,
            };
            reopen(ctx, &closing).await;
            return Err(error);
        }
    };
    let config = raw_config.for_ticket(&ticket);
    let closing = CloseAttempt {
        channel,
        ticket: &ticket,
        config: &config,
        closer,
        reason,
        attempt: ticket.close_generation,
    };
    let key = history_key(ticket.number, channel);
    let previous = match ctx.store().get::<TicketHistory>(&key).await {
        Ok(history) => history,
        Err(error) => {
            reopen(ctx, &closing).await;
            return Err(stored(&error));
        }
    };
    if let Some(history) = previous.as_ref().filter(|history| {
        history.status == HistoryStatus::Closed && history.attempt == closing.attempt
    }) {
        return complete_history_close(ctx, &closing, history, false).await;
    }
    let history = match prepare_history(ctx, &closing, previous).await {
        Ok(history) => history,
        Err(error) => {
            reopen(ctx, &closing).await;
            return Err(error);
        }
    };
    complete_history_close(ctx, &closing, &history, false).await
}

async fn reopen(ctx: &EventContext, closing: &CloseAttempt<'_>) {
    if closing.ticket.thread
        && let Err(error) = thread::unarchive(ctx, closing.channel, "Ticket closure retry").await
    {
        tracing::warn!(?error, channel = %closing.channel, "could not reopen the ticket thread");
        return;
    }
    let key = ticket_key(closing.channel);
    for _ in 0..5 {
        let entry = match ctx.store().entry(&key).await {
            Ok(Some(entry)) => entry,
            Ok(None) => return,
            Err(error) => {
                tracing::warn!(?error, channel = %closing.channel, "could not read ticket to reopen it");
                return;
            }
        };
        let mut ticket: Ticket = match serde_json::from_value(entry.value) {
            Ok(ticket) => ticket,
            Err(error) => {
                tracing::warn!(?error, channel = %closing.channel, "could not decode ticket to reopen it");
                return;
            }
        };
        if ticket.close_generation != closing.attempt || ticket.closing.is_none() {
            return;
        }
        ticket.closing = None;
        if let Err(error) = refresh_ticket_form_answer_ttl(ctx, &ticket).await {
            tracing::warn!(?error, channel = %closing.channel, "could not refresh ticket form answer TTL while reopening");
            return;
        }
        match ctx
            .store()
            .replace(
                &key,
                &ticket,
                Expect::Revision(entry.revision),
                Some(ticket_ttl(&ticket, ctx.now())),
            )
            .await
        {
            Ok(_) => {
                let _ = ctx.scheduler().cancel(&closing_key(closing.channel)).await;
                return;
            }
            Err(StorageError::Conflict) => {}
            Err(error) => {
                tracing::warn!(?error, channel = %closing.channel, "could not reopen ticket");
                return;
            }
        }
    }
    tracing::warn!(channel = %closing.channel, "ticket changed repeatedly while reopening");
}

/// Whether `user` may close the ticket: whoever opened it, or the staff.
fn may_close(ticket: &Ticket, user: UserId, staff: bool) -> bool {
    staff || ticket.opener == user.get()
}

#[allow(clippy::large_futures)]
async fn close_button(cx: ComponentCx) -> Result<(), UserError> {
    let channel = cx.click().channel_id;
    let ctx = cx.ctx();
    let Some(ticket) = ctx
        .store()
        .get::<Ticket>(&ticket_key(channel))
        .await
        .map_err(|e| failed(&e))?
    else {
        return say(&cx, "tickets-not-a-ticket", &Args::new()).await;
    };
    let config: Config = cx.config()?;
    let config = config.for_ticket(&ticket);
    if !may_close(
        &ticket,
        cx.user().id,
        is_ticket_staff(&cx, &ticket, &config).await,
    ) {
        return say(&cx, "tickets-not-yours", &Args::new()).await;
    }
    say(&cx, "tickets-closing", &Args::new()).await?;
    let reason = cx.t(
        "tickets-closed-by",
        &Args::new().with("user", cx.user().name.clone()),
    );
    let closed = finish(ctx, channel, Some(cx.user().id), &reason, None)
        .await
        .map_err(|error| failed(&error))?;
    if closed == Closed::Done || closed == Closed::RatingDmUnconfirmed {
        let audit_key = format!("close-{}", cx.click().interaction_id.get());
        if let Err(error) = audit::record_event(
            ctx,
            cx.user().id.get(),
            audit::AuditAction::TicketClose,
            audit::AuditOutcome::Success,
            &audit_key,
        )
        .await
        {
            tracing::warn!(?error, channel = %channel, "could not record ticket close audit event");
        }
    }
    if closed == Closed::RatingDmUnconfirmed
        && let Err(error) = cx
            .reply_private_t(
                "tickets-rating-dm-fallback",
                &Args::new().with("number", ticket.number),
            )
            .await
    {
        tracing::warn!(?error, channel = %channel, "could not tell the ticket closer about the rating fallback");
    }
    Ok(())
}

async fn rate_button(cx: ComponentCx) -> Result<(), UserError> {
    let Some(custom_id) = cx.custom_id() else {
        return say(&cx, "tickets-rating-stale", &Args::new()).await;
    };
    if custom_id.route != "rate" || custom_id.owner != Some(cx.user().id) {
        return say(&cx, "tickets-rating-stale", &Args::new()).await;
    }
    let Some(request) = parse_rating_request(&custom_id.data) else {
        return say(&cx, "tickets-rating-stale", &Args::new()).await;
    };
    let ctx = cx.ctx();
    let result = submit_rating(
        ctx,
        ChannelId::new(request.channel),
        request.number,
        request.attempt,
        cx.user().id,
        request.score,
    )
    .await?;
    if matches!(result, RatingSubmission::Saved) {
        if let Some(history) = history_by_number(ctx, request.number)
            .await
            .map_err(|error| failed(&error))?
        {
            let prompt_sent = teams_survey::send_exit_survey_prompt(ctx, &history).await;
            if !prompt_sent
                && teams_survey::maybe_open_exit_survey(
                    &cx,
                    ChannelId::new(request.channel),
                    request.number,
                    request.attempt,
                )
                .await?
            {
                return Ok(());
            }
        }
    } else if matches!(result, RatingSubmission::AlreadyRated)
        && teams_survey::maybe_open_exit_survey(
            &cx,
            ChannelId::new(request.channel),
            request.number,
            request.attempt,
        )
        .await?
    {
        return Ok(());
    }
    let message = match result {
        RatingSubmission::Saved => "tickets-rating-saved",
        RatingSubmission::AlreadyRated => "tickets-rating-already",
        RatingSubmission::Stale => "tickets-rating-stale",
    };
    say(&cx, message, &Args::new().with("number", request.number)).await
}

async fn rate_command(cx: CommandCx, args: RateArgs) -> Result<(), UserError> {
    let Some(score) = u8::try_from(args.score)
        .ok()
        .filter(|score| (1..=5).contains(score))
    else {
        return cx
            .reply_private_t("tickets-rating-invalid", &Args::new())
            .await;
    };
    if args.ticket <= 0 {
        return cx
            .reply_private_t("tickets-rating-stale", &Args::new())
            .await;
    }
    let ctx = cx.ctx();
    let history = history_by_number(ctx, args.ticket)
        .await
        .map_err(|error| failed(&error))?;
    let Some(history) = history else {
        return cx
            .reply_private_t("tickets-rating-stale", &Args::new())
            .await;
    };
    let result = submit_rating(
        ctx,
        ChannelId::new(history.channel_id),
        history.number,
        history.attempt,
        cx.user().id,
        score,
    )
    .await?;
    if matches!(
        result,
        RatingSubmission::Saved | RatingSubmission::AlreadyRated
    ) && teams_survey::survey_open_component(ctx, &history).is_some()
    {
        let prompt_sent = matches!(result, RatingSubmission::Saved)
            && teams_survey::send_exit_survey_prompt(ctx, &history).await;
        if !prompt_sent {
            return teams_survey::survey_command(cx, history.number).await;
        }
    }
    let message = match result {
        RatingSubmission::Saved => "tickets-rating-saved",
        RatingSubmission::AlreadyRated => "tickets-rating-already",
        RatingSubmission::Stale => "tickets-rating-stale",
    };
    cx.reply_private_t(message, &Args::new().with("number", history.number))
        .await
}

async fn survey_command(cx: CommandCx, args: SurveyArgs) -> Result<(), UserError> {
    teams_survey::survey_command(cx, args.number).await
}

async fn knowledge_send(cx: CommandCx, args: KnowledgeSendArgs) -> Result<(), UserError> {
    cx.defer(true).await?;
    let channel = cx.invocation().channel_id;
    let ctx = cx.ctx();
    staff_ticket(&cx, ctx, channel).await?;
    let can_manage = cx.require("tickets.manage").await.is_ok();
    knowledge::send_article(ctx, channel, &args.id, can_manage)
        .await
        .map_err(|error| failed(&error))?;
    cx.reply_private_t("tickets-knowledge-send-complete", &Args::new())
        .await
}

fn close_request_id(cx: &ComponentCx, channel: ChannelId) -> Option<i64> {
    let data = cx.custom_id()?.data;
    let (button_channel, request_id) = data.split_once(':')?;
    if button_channel.parse::<u64>().ok()? != channel.get() {
        return None;
    }
    request_id.parse::<i64>().ok().filter(|id| *id > 0)
}

async fn close_request_followup(cx: &ComponentCx, channel: ChannelId, key: &str) {
    if let Err(error) = cx.reply_private_t(key, &Args::new()).await {
        tracing::warn!(?error, channel = %channel, "could not send a close request follow-up");
    }
}

async fn approve_request(
    cx: &ComponentCx,
    channel: ChannelId,
    request_id: i64,
    number: i64,
    request: CloseRequest,
) -> Result<(), UserError> {
    cx.reply_private_t("tickets-close-request-processing", &Args::new())
        .await?;
    let reason = request
        .reason
        .unwrap_or_else(|| cx.t("tickets-close-request-approved-reason", &Args::new()));
    let closed = match Box::pin(finish(
        cx.ctx(),
        channel,
        Some(cx.user().id),
        &reason,
        Some(request_id),
    ))
    .await
    {
        Ok(closed) => closed,
        Err(error) => {
            tracing::warn!(?error, channel = %channel, "could not close an approved ticket");
            close_request_followup(cx, channel, "tickets-failed").await;
            return Ok(());
        }
    };
    let message = match closed {
        Closed::Done => "tickets-close-request-approved",
        Closed::RatingDmUnconfirmed => {
            if let Err(error) = cx
                .reply_private_t(
                    "tickets-rating-dm-fallback",
                    &Args::new().with("number", number),
                )
                .await
            {
                tracing::warn!(?error, channel = %channel, "could not tell the ticket opener about the rating fallback");
            }
            return Ok(());
        }
        Closed::BusyUntil(_) => "tickets-closing",
        Closed::Unknown | Closed::RequestChanged => "tickets-close-request-stale",
    };
    close_request_followup(cx, channel, message).await;
    Ok(())
}

async fn expire_close_request(
    cx: &ComponentCx,
    channel: ChannelId,
    request_id: i64,
    number: i64,
) -> Result<(), UserError> {
    cx.reply_private_t("tickets-close-request-processing", &Args::new())
        .await?;
    let reason = cx.t("tickets-close-request-timeout-reason", &Args::new());
    let closed = match Box::pin(finish(cx.ctx(), channel, None, &reason, Some(request_id))).await {
        Ok(closed) => closed,
        Err(error) => {
            tracing::warn!(?error, channel = %channel, "could not close an expired request");
            close_request_followup(cx, channel, "tickets-failed").await;
            return Ok(());
        }
    };
    let message = match closed {
        Closed::Done => "tickets-close-request-expired",
        Closed::RatingDmUnconfirmed => {
            if let Err(error) = cx
                .reply_private_t(
                    "tickets-rating-dm-fallback",
                    &Args::new().with("number", number),
                )
                .await
            {
                tracing::warn!(?error, channel = %channel, "could not tell the ticket opener about the rating fallback");
            }
            return Ok(());
        }
        Closed::BusyUntil(_) => "tickets-closing",
        Closed::Unknown | Closed::RequestChanged => "tickets-close-request-stale",
    };
    close_request_followup(cx, channel, message).await;
    Ok(())
}

#[allow(clippy::too_many_lines, clippy::large_futures)]
async fn close_request_decision(cx: ComponentCx, approve: bool) -> Result<(), UserError> {
    let channel = cx.click().channel_id;
    let Some(request_id) = close_request_id(&cx, channel) else {
        return say(&cx, "tickets-close-request-stale", &Args::new()).await;
    };
    cx.defer(true).await?;
    let ctx = cx.ctx();
    let key = ticket_key(channel);
    let entry = match ctx.store().entry(&key).await {
        Ok(Some(entry)) => entry,
        Ok(None) => return say(&cx, "tickets-close-request-stale", &Args::new()).await,
        Err(error) => {
            tracing::warn!(?error, channel = %channel, "could not read a close request");
            return say(&cx, "tickets-failed", &Args::new()).await;
        }
    };
    let mut revision = entry.revision;
    let mut ticket: Ticket = match serde_json::from_value(entry.value) {
        Ok(ticket) => ticket,
        Err(error) => {
            tracing::warn!(?error, channel = %channel, "could not read a close request");
            return say(&cx, "tickets-failed", &Args::new()).await;
        }
    };
    if cx.user().id.get() != ticket.opener {
        return say(&cx, "tickets-close-request-opener-only", &Args::new()).await;
    }
    let Some(request) = ticket
        .close_request
        .as_ref()
        .filter(|request| request.id == request_id)
        .cloned()
    else {
        return say(&cx, "tickets-close-request-stale", &Args::new()).await;
    };
    if request
        .expires_at
        .is_some_and(|deadline| cx.now() >= deadline)
    {
        return expire_close_request(&cx, channel, request_id, ticket.number).await;
    }
    if approve {
        return approve_request(&cx, channel, request_id, ticket.number, request).await;
    }
    if !ticket.is_open() {
        return say(&cx, "tickets-closing", &Args::new()).await;
    }
    for _ in 0..5 {
        ticket.close_request = None;
        refresh_ticket_form_answer_ttl(ctx, &ticket)
            .await
            .map_err(|error| failed(&error))?;
        match ctx
            .store()
            .replace(
                &key,
                &ticket,
                Expect::Revision(revision),
                Some(ticket_ttl(&ticket, ctx.now())),
            )
            .await
        {
            Ok(_) => {
                cancel_close_request_timeout(ctx, channel, &request).await;
                return say(&cx, "tickets-close-request-rejected", &Args::new()).await;
            }
            Err(StorageError::Conflict) => {
                let next = match ctx.store().entry(&key).await {
                    Ok(Some(next)) => next,
                    Ok(None) => return say(&cx, "tickets-close-request-stale", &Args::new()).await,
                    Err(error) => {
                        tracing::warn!(?error, channel = %channel, "could not reread a close request");
                        return say(&cx, "tickets-failed", &Args::new()).await;
                    }
                };
                let next_revision = next.revision;
                ticket = match serde_json::from_value(next.value) {
                    Ok(ticket) => ticket,
                    Err(error) => {
                        tracing::warn!(?error, channel = %channel, "could not reread a close request");
                        return say(&cx, "tickets-failed", &Args::new()).await;
                    }
                };
                if cx.user().id.get() != ticket.opener {
                    return say(&cx, "tickets-close-request-opener-only", &Args::new()).await;
                }
                if !ticket.is_open() {
                    return say(&cx, "tickets-closing", &Args::new()).await;
                }
                if ticket
                    .close_request
                    .as_ref()
                    .is_none_or(|request| request.id != request_id)
                {
                    return say(&cx, "tickets-close-request-stale", &Args::new()).await;
                }
                revision = next_revision;
            }
            Err(error) => {
                tracing::warn!(?error, channel = %channel, "could not reject a close request");
                return say(&cx, "tickets-failed", &Args::new()).await;
            }
        }
    }
    say(&cx, "tickets-failed", &Args::new()).await
}

#[allow(clippy::large_futures)]
async fn approve_close_request(cx: ComponentCx) -> Result<(), UserError> {
    close_request_decision(cx, true).await
}

#[allow(clippy::large_futures)]
async fn reject_close_request(cx: ComponentCx) -> Result<(), UserError> {
    close_request_decision(cx, false).await
}

#[allow(clippy::large_futures)]
async fn close_command(cx: CommandCx, args: Close) -> Result<(), UserError> {
    let channel = cx.invocation().channel_id;
    let ctx = cx.ctx();
    let Some(ticket) = ctx
        .store()
        .get::<Ticket>(&ticket_key(channel))
        .await
        .map_err(|e| failed(&e))?
    else {
        return Err(UserError::message("tickets-not-a-ticket"));
    };
    let config: Config = cx.config()?;
    let config = config.for_ticket(&ticket);
    if !may_close(
        &ticket,
        cx.user().id,
        is_ticket_staff(&cx, &ticket, &config).await,
    ) {
        return Err(UserError::message("tickets-not-yours"));
    }
    let requested_reason = args.reason.filter(|text| !text.trim().is_empty());
    if ticket.panel.as_ref().is_some_and(|panel| {
        !panel
            .customization
            .close_reason_allowed(requested_reason.as_deref())
    }) {
        return cx
            .reply_private_t("tickets-close-reason-invalid", &Args::new())
            .await;
    }
    cx.reply_private_t("tickets-closing", &Args::new()).await?;
    let reason = requested_reason.unwrap_or_else(|| {
        cx.t(
            "tickets-closed-by",
            &Args::new().with("user", cx.user().name.clone()),
        )
    });
    let closed = finish(ctx, channel, Some(cx.user().id), &reason, None)
        .await
        .map_err(|error| failed(&error))?;
    if closed == Closed::Done || closed == Closed::RatingDmUnconfirmed {
        let audit_key = format!("close-{}", cx.invocation().interaction_id.get());
        if let Err(error) = audit::record_event(
            ctx,
            cx.user().id.get(),
            audit::AuditAction::TicketClose,
            audit::AuditOutcome::Success,
            &audit_key,
        )
        .await
        {
            tracing::warn!(?error, channel = %channel, "could not record ticket close audit event");
        }
    }
    if closed == Closed::RatingDmUnconfirmed
        && let Err(error) = cx
            .reply_private_t(
                "tickets-rating-dm-fallback",
                &Args::new().with("number", ticket.number),
            )
            .await
    {
        tracing::warn!(?error, channel = %channel, "could not tell the ticket closer about the rating fallback");
    }
    Ok(())
}

async fn resend_close_request(
    cx: &CommandCx,
    channel: ChannelId,
    opener: u64,
    request: &CloseRequest,
    reason: Option<&str>,
) -> Result<(), UserError> {
    if reason.is_some_and(|reason| request.reason.as_deref() != Some(reason)) {
        return cx
            .reply_private_t("tickets-close-request-pending", &Args::new())
            .await;
    }
    match post_close_request_message(cx.ctx(), channel, opener, request).await {
        Ok(()) => {
            if let Some(deadline) = request.expires_at
                && let Err(error) =
                    watch_close_request_timeout(cx.ctx(), channel, request.id, deadline, deadline)
                        .await
            {
                tracing::warn!(
                    ?error,
                    channel = %channel,
                    request_id = request.id,
                    "could not reschedule a close request timeout"
                );
                return cx
                    .reply_private_t(
                        "tickets-close-request-timeout-schedule-failed",
                        &Args::new(),
                    )
                    .await;
            }
            cx.reply_private_t("tickets-close-request-sent", &Args::new())
                .await
        }
        Err(error) => {
            tracing::warn!(?error, channel = %channel, request_id = request.id, "could not resend a close request");
            cx.reply_private_t("tickets-failed", &Args::new()).await
        }
    }
}

async fn close_request(cx: CommandCx, args: CloseRequestArgs) -> Result<(), UserError> {
    let config: Config = cx.config()?;
    cx.defer(true).await?;
    let ctx = cx.ctx();
    let channel = cx.invocation().channel_id;
    let key = ticket_key(channel);
    let entry = match ctx.store().entry(&key).await {
        Ok(Some(entry)) => entry,
        Ok(None) => {
            return cx
                .reply_private_t("tickets-not-a-ticket", &Args::new())
                .await;
        }
        Err(error) => {
            tracing::warn!(?error, channel = %channel, "could not read a ticket for a close request");
            return cx.reply_private_t("tickets-failed", &Args::new()).await;
        }
    };
    let revision = entry.revision;
    let ticket: Ticket = match serde_json::from_value(entry.value) {
        Ok(ticket) => ticket,
        Err(error) => {
            tracing::warn!(?error, channel = %channel, "could not read a ticket for a close request");
            return cx.reply_private_t("tickets-failed", &Args::new()).await;
        }
    };
    let config = config.for_ticket(&ticket);
    if !is_ticket_staff(&cx, &ticket, &config).await {
        return cx.reply_private_t("tickets-staff-only", &Args::new()).await;
    }
    if !ticket.is_open() {
        return cx
            .reply_private_t("tickets-not-a-ticket", &Args::new())
            .await;
    }
    let timeout_hours = config.close_request_timeout_hours;
    let actor_id = cx.user().id.get();
    let audit_key = format!("close-request-{}", cx.invocation().interaction_id.get());
    let reason = args.reason.filter(|text| !text.trim().is_empty());
    if ticket
        .panel
        .as_ref()
        .is_some_and(|panel| !panel.customization.close_reason_allowed(reason.as_deref()))
    {
        return cx
            .reply_private_t("tickets-close-reason-invalid", &Args::new())
            .await;
    }
    let result = if let Some(request) = ticket.close_request.clone() {
        resend_close_request(&cx, channel, ticket.opener, &request, reason.as_deref()).await
    } else {
        create_close_request(cx.clone(), ticket, revision, reason, timeout_hours).await
    };
    if result.is_ok()
        && let Err(error) = audit::record_event(
            ctx,
            actor_id,
            audit::AuditAction::TicketCloseRequest,
            audit::AuditOutcome::Success,
            &audit_key,
        )
        .await
    {
        tracing::warn!(?error, channel = %channel, "could not record ticket close request audit event");
    }
    result
}

#[allow(clippy::too_many_lines)]
async fn create_close_request(
    cx: CommandCx,
    mut ticket: Ticket,
    mut revision: i64,
    reason: Option<String>,
    timeout_hours: u32,
) -> Result<(), UserError> {
    let ctx = cx.ctx();
    let channel = cx.invocation().channel_id;
    let key = ticket_key(channel);
    let expires_at = match close_request_expiry(cx.now(), timeout_hours) {
        Some(CloseRequestExpiry::Disabled) => None,
        Some(CloseRequestExpiry::At(deadline)) => Some(deadline),
        None => {
            return cx
                .reply_private_t("tickets-invalid-config", &Args::new())
                .await;
        }
    };
    let request_id = match ctx.store().score_add(CLOSE_REQUESTS, 0_u64, 1).await {
        Ok(id) if id > 0 => id,
        Ok(_) => return cx.reply_private_t("tickets-failed", &Args::new()).await,
        Err(error) => {
            tracing::warn!(?error, channel = %channel, "could not allocate a close request id");
            return cx.reply_private_t("tickets-failed", &Args::new()).await;
        }
    };
    let request = CloseRequest {
        id: request_id,
        requested_by: cx.user().id.get(),
        reason: reason.clone(),
        expires_at,
    };
    if let Some(deadline) = request.expires_at
        && let Err(error) =
            watch_close_request_timeout(ctx, channel, request.id, deadline, deadline).await
    {
        tracing::warn!(
            ?error,
            channel = %channel,
            request_id = request.id,
            "could not schedule a close request timeout"
        );
        cancel_close_request_timeout(ctx, channel, &request).await;
        return cx
            .reply_private_t(
                "tickets-close-request-timeout-schedule-failed",
                &Args::new(),
            )
            .await;
    }

    let mut saved = false;
    for _ in 0..5 {
        ticket.close_request = Some(request.clone());
        refresh_ticket_form_answer_ttl(ctx, &ticket)
            .await
            .map_err(|error| failed(&error))?;
        match ctx
            .store()
            .replace(
                &key,
                &ticket,
                Expect::Revision(revision),
                Some(ticket_ttl(&ticket, ctx.now())),
            )
            .await
        {
            Ok(_) => {
                saved = true;
                break;
            }
            Err(StorageError::Conflict) => {
                let next = match ctx.store().entry(&key).await {
                    Ok(Some(next)) => next,
                    Ok(None) => {
                        cancel_close_request_timeout(ctx, channel, &request).await;
                        return cx
                            .reply_private_t("tickets-not-a-ticket", &Args::new())
                            .await;
                    }
                    Err(error) => {
                        tracing::warn!(?error, channel = %channel, "could not reread a ticket for a close request");
                        cancel_close_request_timeout(ctx, channel, &request).await;
                        return cx.reply_private_t("tickets-failed", &Args::new()).await;
                    }
                };
                let next_revision = next.revision;
                ticket = match serde_json::from_value(next.value) {
                    Ok(ticket) => ticket,
                    Err(error) => {
                        tracing::warn!(?error, channel = %channel, "could not reread a ticket for a close request");
                        cancel_close_request_timeout(ctx, channel, &request).await;
                        return cx.reply_private_t("tickets-failed", &Args::new()).await;
                    }
                };
                if !ticket.is_open() {
                    cancel_close_request_timeout(ctx, channel, &request).await;
                    return cx
                        .reply_private_t("tickets-not-a-ticket", &Args::new())
                        .await;
                }
                if let Some(existing) = ticket.close_request.clone() {
                    cancel_close_request_timeout(ctx, channel, &request).await;
                    return resend_close_request(
                        &cx,
                        channel,
                        ticket.opener,
                        &existing,
                        reason.as_deref(),
                    )
                    .await;
                }
                revision = next_revision;
            }
            Err(error) => {
                tracing::warn!(?error, channel = %channel, "could not save a close request");
                cancel_close_request_timeout(ctx, channel, &request).await;
                return cx.reply_private_t("tickets-failed", &Args::new()).await;
            }
        }
    }
    if !saved {
        cancel_close_request_timeout(ctx, channel, &request).await;
        return cx.reply_private_t("tickets-failed", &Args::new()).await;
    }
    match post_close_request_message(ctx, channel, ticket.opener, &request).await {
        Ok(()) => {
            cx.reply_private_t("tickets-close-request-sent", &Args::new())
                .await
        }
        Err(error) => {
            tracing::warn!(?error, channel = %channel, request_id = request.id, "could not send a close request");
            cx.reply_private_t("tickets-failed", &Args::new()).await
        }
    }
}

async fn claim(cx: ComponentCx) -> Result<(), UserError> {
    let channel = cx.click().channel_id;
    let ctx = cx.ctx();
    let key = ticket_key(channel);
    let config: Config = cx.config()?;
    // Written against the revision read: two who press at once do not both take it, and a close
    // that began in between wins.
    for _ in 0..5 {
        let Some(entry) = ctx.store().entry(&key).await.map_err(|e| failed(&e))? else {
            return say(&cx, "tickets-not-a-ticket", &Args::new()).await;
        };
        let mut ticket: Ticket =
            serde_json::from_value(entry.value).map_err(|error| failed(&error))?;
        if !ticket.is_open() {
            return say(&cx, "tickets-not-a-ticket", &Args::new()).await;
        }
        let access_config = config.for_ticket(&ticket);
        if !is_ticket_staff(&cx, &ticket, &access_config).await {
            return say(&cx, "tickets-staff-only", &Args::new()).await;
        }
        if let Some(holder) = ticket.claimed_by {
            return say(
                &cx,
                "tickets-claimed-already",
                &Args::new().with("user", format!("<@{holder}>")),
            )
            .await;
        }
        ticket.claimed_by = Some(cx.user().id.get());
        refresh_ticket_form_answer_ttl(ctx, &ticket)
            .await
            .map_err(|error| failed(&error))?;
        match ctx
            .store()
            .replace(
                &key,
                &ticket,
                Expect::Revision(entry.revision),
                Some(ticket_ttl(&ticket, ctx.now())),
            )
            .await
        {
            Ok(_) => {
                let audit_key = format!("ticket-claim-{}", cx.click().interaction_id.get());
                if let Err(error) = audit::record_event(
                    ctx,
                    cx.user().id.get(),
                    audit::AuditAction::TicketClaim,
                    audit::AuditOutcome::Success,
                    &audit_key,
                )
                .await
                {
                    tracing::warn!(?error, channel = %channel, "could not record ticket claim audit event");
                }
                ctx.api
                    .execute(Action::SendMessage {
                        channel_id: channel,
                        content: Some(cx.t(
                            "tickets-claimed",
                            &Args::new().with("user", format!("<@{}>", cx.user().id)),
                        )),
                        embeds: Vec::new(),
                        reply_to: None,
                        mentions: MentionPolicy::None,
                    })
                    .await
                    .map_err(|e| failed(&e))?;
                return say(&cx, "tickets-claim-done", &Args::new()).await;
            }
            Err(StorageError::Conflict) => {}
            Err(error) => return Err(failed(&error)),
        }
    }
    say(&cx, "tickets-failed", &Args::new()).await
}

#[allow(clippy::too_many_lines)]
async fn set_claim(cx: CommandCx, claimed_by: Option<UserId>) -> Result<(), UserError> {
    let ctx = cx.ctx();
    let channel = cx.invocation().channel_id;
    let config: Config = cx.config()?;
    let key = ticket_key(channel);
    let initial_ticket = ctx
        .store()
        .get::<Ticket>(&key)
        .await
        .map_err(|error| failed(&error))?
        .ok_or_else(|| UserError::message("tickets-not-a-ticket"))?;
    let access_config = config.for_ticket(&initial_ticket);
    if !is_ticket_staff(&cx, &initial_ticket, &access_config).await {
        return Err(UserError::message("tickets-staff-only"));
    }
    if !initial_ticket.is_open() {
        return Err(UserError::message("tickets-not-a-ticket"));
    }
    let staff_roles = history_staff_roles(&initial_ticket, &access_config);
    if let Some(user) = claimed_by {
        let member = match ctx
            .api
            .execute(Action::GetMember {
                guild_id: ctx.guild_id,
                user_id: user,
            })
            .await
        {
            Ok(Outcome::Member(member)) => member,
            Ok(_) | Err(DiscordError::NotFound) => {
                return Err(UserError::message("tickets-transfer-not-staff"));
            }
            Err(error) => return Err(failed(&error)),
        };
        let is_ticket_staff = teams_survey::may_access(
            initial_ticket.staff_access.as_ref(),
            &staff_roles,
            user.get(),
            &member.roles,
            false,
        );
        if !is_ticket_staff && !is_guild_owner_or_admin(ctx, user, &member.roles).await? {
            return Err(UserError::message("tickets-transfer-not-staff"));
        }
    }
    // CAS lets a close that begins during the update win.
    for _ in 0..5 {
        let Some(entry) = ctx.store().entry(&key).await.map_err(|e| failed(&e))? else {
            return Err(UserError::message("tickets-not-a-ticket"));
        };
        let mut ticket: Ticket =
            serde_json::from_value(entry.value).map_err(|error| failed(&error))?;
        if !ticket.is_open() {
            return Err(UserError::message("tickets-not-a-ticket"));
        }
        if ticket.claimed_by.is_none() {
            return Err(UserError::message("tickets-not-assigned"));
        }
        ticket.claimed_by = claimed_by.map(twilight_model::id::Id::get);
        refresh_ticket_form_answer_ttl(ctx, &ticket)
            .await
            .map_err(|error| failed(&error))?;
        let (message, args) = match claimed_by {
            Some(user) => (
                "tickets-transferred",
                Args::new().with("user", format!("<@{user}>")),
            ),
            None => ("tickets-unclaimed", Args::new()),
        };
        match ctx
            .store()
            .replace(
                &key,
                &ticket,
                Expect::Revision(entry.revision),
                Some(ticket_ttl(&ticket, ctx.now())),
            )
            .await
        {
            Ok(_) => {
                let audit_action = if claimed_by.is_some() {
                    audit::AuditAction::TicketTransfer
                } else {
                    audit::AuditAction::TicketUnclaim
                };
                let audit_key =
                    format!("ticket-assignment-{}", cx.invocation().interaction_id.get());
                if let Err(error) = audit::record_event(
                    ctx,
                    cx.user().id.get(),
                    audit_action,
                    audit::AuditOutcome::Success,
                    &audit_key,
                )
                .await
                {
                    tracing::warn!(?error, channel = %channel, "could not record ticket assignment audit event");
                }
                let content = cx.t(message, &args);
                if let Err(error) = ctx
                    .api
                    .execute(Action::SendMessage {
                        channel_id: channel,
                        content: Some(content),
                        embeds: Vec::new(),
                        reply_to: None,
                        mentions: MentionPolicy::None,
                    })
                    .await
                {
                    tracing::warn!(?error, channel = %channel, "could not announce a ticket assignment change");
                }
                return cx.reply_private_t(message, &args).await;
            }
            Err(StorageError::Conflict) => {}
            Err(error) => return Err(failed(&error)),
        }
    }
    Err(UserError::message("tickets-failed"))
}

#[allow(clippy::large_futures)]
async fn change_member(cx: CommandCx, user: UserId, add: bool) -> Result<(), UserError> {
    let ctx = cx.ctx();
    let actor_id = cx.user().id.get();
    let audit_key = format!("ticket-member-{}", cx.invocation().interaction_id.get());
    let result = operations::change_member(cx.clone(), user, add).await;
    if result.is_ok() {
        let action = if add {
            audit::AuditAction::TicketMemberAdd
        } else {
            audit::AuditAction::TicketMemberRemove
        };
        if let Err(error) = audit::record_event(
            ctx,
            actor_id,
            action,
            audit::AuditOutcome::Success,
            &audit_key,
        )
        .await
        {
            tracing::warn!(?error, "could not record ticket member audit event");
        }
    }
    result
}

#[allow(clippy::large_futures)]
async fn add(cx: CommandCx, args: Member) -> Result<(), UserError> {
    change_member(cx, args.user, true).await
}

#[allow(clippy::large_futures)]
async fn remove(cx: CommandCx, args: Member) -> Result<(), UserError> {
    change_member(cx, args.user, false).await
}

#[allow(clippy::too_many_lines)]
async fn change_label(cx: CommandCx, value: String, add: bool) -> Result<(), UserError> {
    let Some(label) = display_label(&value) else {
        return cx
            .reply_private_t("tickets-label-invalid", &Args::new())
            .await;
    };
    let normalized = label_key(&label);
    let ctx = cx.ctx();
    let key = ticket_key(cx.invocation().channel_id);
    let config: Config = cx.config()?;
    for _ in 0..5 {
        let Some(entry) = ctx
            .store()
            .entry(&key)
            .await
            .map_err(|error| failed(&error))?
        else {
            return Err(UserError::message("tickets-not-a-ticket"));
        };
        let mut ticket: Ticket =
            serde_json::from_value(entry.value).map_err(|error| failed(&error))?;
        if !is_ticket_staff(&cx, &ticket, &config.for_ticket(&ticket)).await {
            return Err(UserError::message("tickets-staff-only"));
        }
        if !ticket.is_open() {
            return Err(UserError::message("tickets-not-a-ticket"));
        }

        if add {
            if ticket
                .labels
                .iter()
                .any(|existing| label_key(existing) == normalized)
            {
                return cx
                    .reply_private_t("tickets-label-exists", &Args::new())
                    .await;
            }
            transcript_management::register_label(ctx, &label)
                .await
                .map_err(|error| failed(&error))?;
            ticket.labels.push(label.clone());
            ticket.labels.sort_by_cached_key(|value| label_key(value));
        } else {
            let count = ticket.labels.len();
            ticket
                .labels
                .retain(|existing| label_key(existing) != normalized);
            if ticket.labels.len() == count {
                return cx
                    .reply_private_t("tickets-label-missing", &Args::new())
                    .await;
            }
        }

        refresh_ticket_form_answer_ttl(ctx, &ticket)
            .await
            .map_err(|error| failed(&error))?;
        match ctx
            .store()
            .replace(
                &key,
                &ticket,
                Expect::Revision(entry.revision),
                Some(ticket_ttl(&ticket, ctx.now())),
            )
            .await
        {
            Ok(_) => {
                let audit_action = if add {
                    audit::AuditAction::TicketLabelAssign
                } else {
                    audit::AuditAction::TicketLabelDelete
                };
                let audit_key = format!("ticket-label-{}", cx.invocation().interaction_id.get());
                if let Err(error) = audit::record_event(
                    ctx,
                    cx.user().id.get(),
                    audit_action,
                    audit::AuditOutcome::Success,
                    &audit_key,
                )
                .await
                {
                    tracing::warn!(?error, channel = %cx.invocation().channel_id, "could not record ticket label audit event");
                }
                return cx
                    .reply_private_t(
                        if add {
                            "tickets-label-added"
                        } else {
                            "tickets-label-removed"
                        },
                        &Args::new(),
                    )
                    .await;
            }
            Err(StorageError::Conflict) => {}
            Err(StorageError::TooLarge { .. }) => {
                return cx
                    .reply_private_t("tickets-label-storage-limit", &Args::new())
                    .await;
            }
            Err(error) => return Err(failed(&error)),
        }
    }
    Err(UserError::message("tickets-failed"))
}

async fn add_label(cx: CommandCx, args: LabelArgs) -> Result<(), UserError> {
    change_label(cx, args.label, true).await
}

async fn remove_label(cx: CommandCx, args: LabelArgs) -> Result<(), UserError> {
    change_label(cx, args.label, false).await
}

async fn list_labels(cx: CommandCx, args: LabelListArgs) -> Result<(), UserError> {
    let channel = cx.invocation().channel_id;
    let ctx = cx.ctx();
    let ticket = staff_ticket(&cx, ctx, channel).await?;
    if ticket.labels.is_empty() {
        return cx
            .reply_private_t("tickets-label-empty", &Args::new())
            .await;
    }
    let page_index = usize::try_from(args.page.unwrap_or(1))
        .ok()
        .and_then(|page| page.checked_sub(1));
    let Some(page_index) = page_index else {
        return Err(UserError::message("tickets-label-page-not-found"));
    };
    let pages = label_pages(&ticket.labels);
    let Some(labels) = pages.get(page_index) else {
        return Err(UserError::message("tickets-label-page-not-found"));
    };
    cx.reply_private_t(
        "tickets-label-list",
        &Args::new()
            .with("labels", labels.clone())
            .with("page", (page_index + 1).to_string())
            .with("pages", pages.len().to_string()),
    )
    .await
}

async fn unclaim(cx: CommandCx, _: Unclaim) -> Result<(), UserError> {
    set_claim(cx, None).await
}

async fn transfer(cx: CommandCx, args: Member) -> Result<(), UserError> {
    cx.defer(true).await?;
    match set_claim(cx.clone(), Some(args.user)).await {
        Err(UserError::Module(error)) => {
            tracing::warn!(
                ?error,
                channel = %cx.invocation().channel_id,
                user = %args.user,
                "could not transfer a ticket"
            );
            match error {
                closure_core::error::ModuleError::Config(_) => {
                    cx.reply_private_t("tickets-invalid-config", &Args::new())
                        .await
                }
                _ => cx.reply_private_t("tickets-failed", &Args::new()).await,
            }
        }
        result => result,
    }
}

async fn rename(cx: CommandCx, args: Rename) -> Result<(), UserError> {
    let ctx = cx.ctx();
    let channel = cx.invocation().channel_id;
    let Some(ticket) = ctx
        .store()
        .get::<Ticket>(&ticket_key(channel))
        .await
        .map_err(|error| failed(&error))?
    else {
        return Err(UserError::message("tickets-not-a-ticket"));
    };
    let config: Config = cx.config()?;
    let access_config = config.for_ticket(&ticket);
    if !is_ticket_staff(&cx, &ticket, &access_config).await {
        return Err(UserError::message("tickets-staff-only"));
    }
    if !ticket.is_open() {
        return Err(UserError::message("tickets-not-a-ticket"));
    }
    let reason = Reason::truncated(&cx.t("tickets-reason-rename", &Args::new()));
    if ticket.thread {
        thread::rename(ctx, channel, &args.name, reason).await?;
    } else {
        let action = Action::EditChannel {
            channel_id: channel,
            patch: ChannelPatch {
                name: Some(args.name.clone()),
                ..ChannelPatch::default()
            },
            reason,
        };
        action
            .validate()
            .map_err(|_| UserError::message("tickets-invalid-name"))?;
        ctx.api
            .execute(action)
            .await
            .map_err(|error| failed(&error))?;
    }
    let audit_key = format!("ticket-rename-{}", cx.invocation().interaction_id.get());
    if let Err(error) = audit::record_event(
        ctx,
        cx.user().id.get(),
        audit::AuditAction::TicketRename,
        audit::AuditOutcome::Success,
        &audit_key,
    )
    .await
    {
        tracing::warn!(?error, channel = %channel, "could not record ticket rename audit event");
    }
    cx.reply_private_t("tickets-renamed", &Args::new().with("name", args.name))
        .await
}

#[derive(Debug)]
enum NoteSave {
    Saved,
    LimitReached,
}

#[derive(Debug)]
struct NoteAccess {
    panel: Option<NamedPanel>,
    ticket_number: i64,
}

async fn staff_ticket(
    cx: &CommandCx,
    ctx: &EventContext,
    channel: ChannelId,
) -> Result<Ticket, UserError> {
    let ticket = ctx
        .store()
        .get::<Ticket>(&ticket_key(channel))
        .await
        .map_err(|error| failed(&error))?
        .ok_or_else(|| UserError::message("tickets-not-a-ticket"))?;
    let config: Config = cx.config()?;
    let access_config = config.for_ticket(&ticket);
    if !is_ticket_staff(cx, &ticket, &access_config).await {
        return Err(UserError::message("tickets-staff-only"));
    }
    if !ticket.is_open() {
        return Err(UserError::message("tickets-not-a-ticket"));
    }
    Ok(ticket)
}

async fn save_note(
    ctx: &EventContext,
    channel: ChannelId,
    ticket: &Ticket,
    note: StaffNote,
) -> Result<NoteSave, UserError> {
    let prefix = note_pages_prefix(channel);
    for _ in 0..5 {
        let pages = match ctx.store().list(&prefix, NOTE_PAGE_LIMIT).await {
            Ok(pages) => pages,
            Err(error) => {
                tracing::warn!(?error, channel = %channel, "could not read ticket notes");
                return Err(UserError::message("tickets-failed"));
            }
        };
        let (key, page, expect) = if let Some((last_key, last_entry)) = pages.last() {
            let Some(last_number) = note_page_number(last_key, &prefix)
                .filter(|number| (1..=NOTE_PAGE_LIMIT).contains(number))
            else {
                return Err(UserError::message("tickets-failed"));
            };
            let mut page: StaffNotePage = match serde_json::from_value(last_entry.value.clone()) {
                Ok(page) => page,
                Err(error) => {
                    tracing::warn!(?error, channel = %channel, "could not read a ticket note page");
                    return Err(UserError::message("tickets-failed"));
                }
            };
            if page.notes.is_empty() || page.notes.len() > NOTES_PER_PAGE {
                return Err(UserError::message("tickets-failed"));
            }
            if page.notes.len() < NOTES_PER_PAGE {
                page.notes.push(note.clone());
                (
                    last_key.clone(),
                    page,
                    Expect::Revision(last_entry.revision),
                )
            } else {
                let next_page = last_number + 1;
                if next_page > NOTE_PAGE_LIMIT {
                    return Ok(NoteSave::LimitReached);
                }
                (
                    note_page_key(channel, next_page),
                    StaffNotePage {
                        ticket_number: ticket.number,
                        panel: ticket.panel.clone(),
                        staff_access: ticket.staff_access.clone(),
                        notes: vec![note.clone()],
                    },
                    Expect::Missing,
                )
            }
        } else {
            (
                note_page_key(channel, 1),
                StaffNotePage {
                    ticket_number: ticket.number,
                    panel: ticket.panel.clone(),
                    staff_access: ticket.staff_access.clone(),
                    notes: vec![note.clone()],
                },
                Expect::Missing,
            )
        };

        match ctx
            .store()
            .replace(&key, &page, expect, Some(NOTE_RETENTION))
            .await
        {
            Ok(_) => return Ok(NoteSave::Saved),
            Err(StorageError::Conflict) => {}
            Err(error) => {
                tracing::warn!(?error, channel = %channel, "could not save a ticket note");
                return Err(UserError::message("tickets-failed"));
            }
        }
    }
    Err(UserError::message("tickets-failed"))
}

async fn note_access(
    cx: &CommandCx,
    ctx: &EventContext,
    channel: ChannelId,
    config: &Config,
) -> Result<NoteAccess, UserError> {
    let live_ticket = ctx
        .store()
        .get::<Ticket>(&ticket_key(channel))
        .await
        .map_err(|error| {
            tracing::warn!(?error, channel = %channel, "could not read a ticket");
            UserError::message("tickets-failed")
        })?;
    let first_page = ctx
        .store()
        .list(&note_pages_prefix(channel), 1)
        .await
        .map_err(|error| {
            tracing::warn!(?error, channel = %channel, "could not read ticket note access");
            UserError::message("tickets-failed")
        })?;
    let Some((_, entry)) = first_page.first() else {
        return Err(UserError::message("tickets-notes-empty"));
    };
    let page: StaffNotePage = serde_json::from_value(entry.value.clone()).map_err(|error| {
        tracing::warn!(?error, channel = %channel, "could not read a ticket note page");
        UserError::message("tickets-failed")
    })?;
    let access_config = live_ticket.as_ref().map_or_else(
        || {
            page.panel
                .as_ref()
                .map_or_else(|| config.clone(), |panel| config.with_panel(panel))
        },
        |ticket| config.for_ticket(ticket),
    );
    let may_access = if let Some(ticket) = live_ticket.as_ref() {
        is_ticket_staff(cx, ticket, &access_config).await
    } else {
        is_staff_with_snapshot(cx, &access_config, page.staff_access.as_ref()).await
    };
    if !may_access {
        return Err(UserError::message("tickets-staff-only"));
    }
    if live_ticket.as_ref().is_some_and(|ticket| {
        ticket.panel.as_ref() != page.panel.as_ref()
            || ticket.staff_access != page.staff_access
            || ticket.number != page.ticket_number
    }) {
        return Err(UserError::message("tickets-failed"));
    }
    Ok(NoteAccess {
        panel: page.panel,
        ticket_number: page.ticket_number,
    })
}

async fn read_note_pages(
    ctx: &EventContext,
    channel: ChannelId,
    access: &NoteAccess,
) -> Result<Vec<StaffNotePage>, UserError> {
    let pages = ctx
        .store()
        .list(&note_pages_prefix(channel), NOTE_PAGE_LIMIT)
        .await
        .map_err(|error| {
            tracing::warn!(?error, channel = %channel, "could not list ticket notes");
            UserError::message("tickets-failed")
        })?;
    let mut note_pages = Vec::with_capacity(pages.len());
    for (_, entry) in &pages {
        let page: StaffNotePage = serde_json::from_value(entry.value.clone()).map_err(|error| {
            tracing::warn!(?error, channel = %channel, "could not read a ticket note page");
            UserError::message("tickets-failed")
        })?;
        if page.panel != access.panel
            || page.ticket_number != access.ticket_number
            || page.notes.is_empty()
            || page.notes.len() > NOTES_PER_PAGE
            || page
                .notes
                .iter()
                .any(|note| note.text.chars().count() > NOTE_MAX_CHARS || note.text.contains('\0'))
        {
            return Err(UserError::message("tickets-failed"));
        }
        note_pages.push(page);
    }
    if note_pages.is_empty() {
        return Err(UserError::message("tickets-notes-empty"));
    }
    Ok(note_pages)
}

async fn add_note(cx: CommandCx, args: NoteArgs) -> Result<(), UserError> {
    let text = args.text.trim();
    if text.is_empty() || text.chars().count() > NOTE_MAX_CHARS || text.contains('\0') {
        return cx
            .reply_private_t("tickets-note-invalid", &Args::new())
            .await;
    }

    let ctx = cx.ctx();
    let channel = cx.invocation().channel_id;
    let ticket = staff_ticket(&cx, ctx, channel).await?;
    // An accepted note stays in its staff-only archive if closure races the write.
    cx.defer(true).await?;
    let note = StaffNote {
        author: cx.user().id.get(),
        created: cx.now(),
        text: text.to_owned(),
    };
    match save_note(ctx, channel, &ticket, note).await? {
        NoteSave::Saved => {
            let audit_key = format!("staff-note-{}", cx.invocation().interaction_id.get());
            if let Err(error) = audit::record_event(
                ctx,
                cx.user().id.get(),
                audit::AuditAction::StaffNoteAdd,
                audit::AuditOutcome::Success,
                &audit_key,
            )
            .await
            {
                tracing::warn!(?error, channel = %channel, "could not record ticket note audit event");
            }
            cx.reply_private_t("tickets-note-saved", &Args::new()).await
        }
        NoteSave::LimitReached => {
            cx.reply_private_t("tickets-notes-limit", &Args::new())
                .await
        }
    }
}

async fn list_notes(cx: CommandCx, args: NotesArgs) -> Result<(), UserError> {
    let Some(channel) = id_of(Some(&args.ticket)).map(ChannelId::new) else {
        return cx
            .reply_private_t("tickets-notes-empty", &Args::new())
            .await;
    };
    let config: Config = cx.config()?;
    cx.defer(true).await?;
    let ctx = cx.ctx();
    let access = note_access(&cx, ctx, channel, &config).await?;
    let page_index = usize::try_from(args.page.unwrap_or(1))
        .ok()
        .and_then(|page| page.checked_sub(1));
    let Some(page_index) = page_index else {
        return Err(UserError::message("tickets-notes-page-not-found"));
    };
    let pages = read_note_pages(ctx, channel, &access).await?;
    let Some(page) = pages.get(page_index) else {
        return Err(UserError::message("tickets-notes-page-not-found"));
    };

    let mut content = cx.t(
        "tickets-notes-page",
        &Args::new()
            .with("number", page.ticket_number.to_string())
            .with("page", (page_index + 1).to_string())
            .with("pages", pages.len().to_string()),
    );
    for note in &page.notes {
        content.push('\n');
        content.push_str(
            &cx.t(
                "tickets-note-entry",
                &Args::new()
                    .with("created", note.created.to_string())
                    .with("author", note.author.to_string()),
            ),
        );
        content.push('\n');
        content.push_str(&note.text);
    }
    cx.reply_private(content).await
}

async fn search_history(cx: CommandCx, args: SearchHistory) -> Result<(), UserError> {
    let query = args.query.trim().to_lowercase();
    if query.is_empty() {
        return cx
            .reply_private_t("tickets-history-search-empty", &Args::new())
            .await;
    }
    let ctx = cx.ctx();
    let now = ctx.now();
    let mut state = history_search_initial_state(
        cx.user().id.get(),
        query,
        now,
        cx.invocation().interaction_id.get(),
    )
    .map_err(|error| failed(&error))?;
    let roles = cx.actor().await.roles;
    let access = HistorySearchAccess {
        actor_id: cx.user().id.get(),
        roles: &roles,
        can_manage: cx.require("tickets.manage").await.is_ok(),
    };
    let mut hits = search_history_batch(ctx, &mut state, &access)
        .await
        .map_err(|error| failed(&error))?;
    filter_current_history_hits(ctx, &mut hits)
        .await
        .map_err(|error| failed(&error))?;
    let content = history_search_reply_text(&state, &hits, |key, args| cx.t(key, args));
    let components = if state.complete {
        Vec::new()
    } else {
        let key = history_search_session_key(state.owner, &state.token);
        let ttl = history_ttl_until(state.expires_at, ctx.now()).map_err(|error| failed(&error))?;
        ctx.store()
            .set_for(&key, &state, ttl)
            .await
            .map_err(|error| failed(&error))?;
        history_search_components(
            ctx,
            &state,
            cx.t("tickets-history-search-next", &Args::new()),
        )
        .map_err(|error| failed(&error))?
    };
    cx.reply_with(history_search_reply(content, components))
        .await
}

async fn consume_history_search_state(
    ctx: &EventContext,
    key: &str,
    state: &HistorySearchState,
    revision: i64,
    ttl: Duration,
) -> Result<(), StorageError> {
    let mut consumed = state.clone();
    consumed.consumed = true;
    ctx.store()
        .replace(
            key,
            &consumed,
            Expect::Revision(revision),
            Some(ttl.min(Duration::from_secs(60))),
        )
        .await
        .map(drop)
}

async fn retry_history_search_message(
    cx: &ComponentCx,
    ctx: &EventContext,
    state: &HistorySearchState,
) -> Result<(), UserError> {
    let components = history_search_components(
        ctx,
        state,
        cx.t("tickets-history-search-next", &Args::new()),
    )
    .map_err(|error| failed(&error))?;
    cx.update(history_search_update(
        cx.t("tickets-history-search-retry", &Args::new()),
        components,
    ))
    .await
}

async fn stale_history_search_message(cx: &ComponentCx) -> Result<(), UserError> {
    cx.update(history_search_update(
        cx.t("tickets-history-search-expired", &Args::new()),
        Vec::new(),
    ))
    .await
}

async fn search_history_next(cx: ComponentCx) -> Result<(), UserError> {
    let token = cx.data().clone();
    let owner = cx.user().id.get();
    cx.defer_update().await?;
    if !history_search_token_is_valid(&token) {
        return stale_history_search_message(&cx).await;
    }
    let ctx = cx.ctx();
    let key = history_search_session_key(owner, &token);
    let entry = match ctx.store().entry(&key).await {
        Ok(Some(entry)) => entry,
        Ok(None) => return stale_history_search_message(&cx).await,
        Err(error) => {
            tracing::warn!(
                ?error,
                owner,
                "could not read a ticket history search session"
            );
            return retry_history_search_storage(&cx, ctx, owner, &token).await;
        }
    };
    let state: HistorySearchState = match serde_json::from_value(entry.value.clone()) {
        Ok(state) => state,
        Err(error) => {
            tracing::warn!(
                ?error,
                owner,
                "could not decode a ticket history search session"
            );
            return stale_history_search_message(&cx).await;
        }
    };
    if !history_search_state_valid(&state, owner, &token, ctx.now()) {
        return stale_history_search_message(&cx).await;
    }
    let original = state.clone();
    let roles = cx.actor().await.roles;
    let access = HistorySearchAccess {
        actor_id: cx.user().id.get(),
        roles: &roles,
        can_manage: cx.require("tickets.manage").await.is_ok(),
    };
    let mut next = state;
    let hits = match search_history_batch(ctx, &mut next, &access).await {
        Ok(hits) => hits,
        Err(error) => {
            tracing::warn!(?error, owner, "could not continue a ticket history search");
            return retry_history_search_message(&cx, ctx, &original).await;
        }
    };
    let mut hits = hits;
    if let Err(error) = filter_current_history_hits(ctx, &mut hits).await {
        tracing::warn!(
            ?error,
            owner,
            "could not validate ticket history search results"
        );
        return retry_history_search_message(&cx, ctx, &original).await;
    }
    let old_token = original.token.clone();
    if !next.complete {
        next.token = history_search_next_token(&next, &old_token);
        let next_key = history_search_session_key(owner, &next.token);
        let Ok(ttl) = history_ttl_until(next.expires_at, ctx.now()) else {
            return stale_history_search_message(&cx).await;
        };
        if let Err(error) = ctx.store().set_for(&next_key, &next, ttl).await {
            tracing::warn!(
                ?error,
                owner,
                "could not save a ticket history search cursor"
            );
            return retry_history_search_message(&cx, ctx, &original).await;
        }
    }
    let content = history_search_reply_text(&next, &hits, |key, args| cx.t(key, args));
    let Ok(components) = history_search_components(
        ctx,
        &next,
        cx.t("tickets-history-search-next", &Args::new()),
    ) else {
        return stale_history_search_message(&cx).await;
    };
    cx.update(history_search_update(content, components))
        .await?;
    let ttl = history_ttl_until(original.expires_at, ctx.now()).map_err(|error| failed(&error))?;
    match consume_history_search_state(ctx, &key, &original, entry.revision, ttl).await {
        Ok(()) | Err(StorageError::Conflict) => {}
        Err(error) => {
            tracing::warn!(
                ?error,
                owner,
                "could not retire a ticket history search cursor"
            );
        }
    }
    Ok(())
}

#[allow(clippy::too_many_lines)]
async fn view_history(cx: CommandCx, args: ViewHistory) -> Result<(), UserError> {
    if args.number <= 0 {
        return cx
            .reply_private_t("tickets-history-not-found", &Args::new())
            .await;
    }
    let ctx = cx.ctx();
    let history = match history_by_number(ctx, args.number).await {
        Ok(Some(history))
            if history.version == 1
                && history.number == args.number
                && history.status == HistoryStatus::Closed =>
        {
            history
        }
        Ok(_) => {
            return cx
                .reply_private_t("tickets-history-not-found", &Args::new())
                .await;
        }
        Err(error) => {
            tracing::warn!(
                ?error,
                number = args.number,
                "could not read ticket history"
            );
            return cx
                .reply_private_t("tickets-history-unavailable", &Args::new())
                .await;
        }
    };
    let history_key = history_key(history.number, ChannelId::new(history.channel_id));
    let privacy_current = match data::history_privacy_generation_current(
        ctx,
        &history_key,
        history.privacy_generation,
    )
    .await
    {
        Ok(current) => current,
        Err(error) => {
            tracing::warn!(
                ?error,
                number = history.number,
                "could not validate ticket history privacy state"
            );
            return cx
                .reply_private_t("tickets-history-unavailable", &Args::new())
                .await;
        }
    };
    if !privacy_current {
        return cx
            .reply_private_t("tickets-history-not-found", &Args::new())
            .await;
    }
    let actor_id = cx.user().id.get();
    let opener_may_view = history.opener == actor_id && !history.opener_erased;
    let roles = cx.actor().await.roles;
    let can_manage = cx.require("tickets.manage").await.is_ok();
    if !opener_may_view && !may_view_history(actor_id, &roles, can_manage, &history) {
        return cx
            .reply_private_t("tickets-history-not-found", &Args::new())
            .await;
    }
    if history.transcript_pages == 0 || history.transcript_char_count == 0 {
        return cx
            .reply_private_t(
                "tickets-history-no-transcript",
                &Args::new().with("number", history.number),
            )
            .await;
    }
    let requested_page = args.page.unwrap_or(1);
    let total_pages = history
        .transcript_char_count
        .saturating_add(HISTORY_DISPLAY_CHARS as u64 - 1)
        / HISTORY_DISPLAY_CHARS as u64;
    if requested_page <= 0 || requested_page.cast_unsigned() > total_pages {
        return cx
            .reply_private_t(
                "tickets-history-page-invalid",
                &Args::new().with("pages", total_pages),
            )
            .await;
    }
    let page = requested_page.cast_unsigned();
    let text = match history_page(ctx, &history, page).await {
        Ok(text) => text,
        Err(error) => {
            tracing::warn!(
                ?error,
                channel_id = history.channel_id,
                page,
                "could not read a ticket transcript page"
            );
            return cx
                .reply_private_t("tickets-history-unavailable", &Args::new())
                .await;
        }
    };
    let privacy_current = match data::history_privacy_generation_current(
        ctx,
        &history_key,
        history.privacy_generation,
    )
    .await
    {
        Ok(current) => current,
        Err(error) => {
            tracing::warn!(
                ?error,
                number = history.number,
                "could not revalidate ticket history privacy state"
            );
            return cx
                .reply_private_t("tickets-history-unavailable", &Args::new())
                .await;
        }
    };
    if !privacy_current {
        return cx
            .reply_private_t("tickets-history-not-found", &Args::new())
            .await;
    }
    let notice = if history.transcript_truncated {
        cx.t(
            "tickets-history-incomplete",
            &Args::new().with("count", u64::from(history.transcript_messages)),
        )
    } else {
        String::new()
    };
    let content = if notice.is_empty() {
        text
    } else {
        format!("{notice}\n{text}")
    };
    cx.reply_private_t(
        "tickets-history-page",
        &Args::new()
            .with("number", history.number)
            .with("page", page)
            .with("pages", total_pages)
            .with("content", content),
    )
    .await
}

fn reply_prefix(ticket: &Ticket) -> String {
    let panel = ticket.panel.as_ref().map_or_else(
        || "default".to_owned(),
        |panel| format!("panel:{}", panel.id),
    );
    format!("ticket-reply:{panel}:")
}

fn reply_key(ticket: &Ticket, id: &str) -> String {
    format!("{}{id}", reply_prefix(ticket))
}

fn valid_reply_id(id: &str) -> bool {
    !id.is_empty()
        && id.chars().count() <= REPLY_ID_MAX_CHARS
        && id
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || matches!(character, '_' | '-'))
}

fn valid_reply_content(title: &str, body: &str) -> bool {
    !title.trim().is_empty()
        && title.chars().count() <= REPLY_TITLE_MAX_CHARS
        && !title.contains('\0')
        && !body.trim().is_empty()
        && body.chars().count() <= REPLY_BODY_MAX_CHARS
        && !body.contains('\0')
}

fn valid_saved_reply(reply: &SavedReply) -> bool {
    valid_reply_id(&reply.id)
        && if reply.deleted {
            reply.title.is_empty() && reply.body.is_empty()
        } else {
            valid_reply_content(&reply.title, &reply.body)
        }
}

fn split_reply(body: &str) -> Vec<&str> {
    let mut chunks = Vec::new();
    let mut start = 0;
    let mut utf16_len = 0;
    for (index, character) in body.char_indices() {
        let character_len = character.len_utf16();
        if utf16_len + character_len > REPLY_CHUNK_MAX_UTF16 {
            chunks.push(&body[start..index]);
            start = index;
            utf16_len = 0;
        }
        utf16_len += character_len;
    }
    if start < body.len() {
        chunks.push(&body[start..]);
    }
    chunks
}

fn decode_saved_reply(
    ticket: &Ticket,
    key: &str,
    entry: Entry,
) -> Result<(SavedReply, i64), UserError> {
    let reply: SavedReply = serde_json::from_value(entry.value).map_err(|error| {
        tracing::warn!(?error, key = %key, "could not read a saved ticket reply");
        UserError::message("tickets-failed")
    })?;
    if key != reply_key(ticket, &reply.id) || !valid_saved_reply(&reply) {
        return Err(UserError::message("tickets-failed"));
    }
    Ok((reply, entry.revision))
}

async fn saved_reply(
    ctx: &EventContext,
    ticket: &Ticket,
    id: &str,
) -> Result<Option<(SavedReply, i64)>, UserError> {
    let key = reply_key(ticket, id);
    ctx.store()
        .entry(&key)
        .await
        .map_err(|error| failed(&error))?
        .map(|entry| decode_saved_reply(ticket, &key, entry))
        .transpose()
}

fn reply_send_attempt_key(channel: ChannelId, owner: u64, attempt: i64) -> String {
    format!("ticket-reply-send:{}:{owner}:{attempt}", channel.get())
}

fn reply_send_button(ctx: &EventContext, route: &str, label: &str, attempt: i64) -> Component {
    ui::button(
        ButtonStyle::Primary,
        ctx.t_in(ctx.locale, label, &Args::new()),
        CustomId::new(route)
            .data(attempt.to_string())
            .encode(&ctx.module),
    )
}

fn valid_reply_send_attempt(
    attempt: &ReplySendAttempt,
    ticket: &Ticket,
    channel: ChannelId,
    owner: u64,
) -> bool {
    attempt.channel == channel.get()
        && attempt.owner == owner
        && attempt.ticket_number == ticket.number
        && attempt.panel == ticket.panel
        && valid_reply_id(&attempt.reply_id)
        && !attempt.chunks.is_empty()
        && attempt.chunks.iter().all(|chunk| {
            !chunk.is_empty()
                && !chunk.contains('\0')
                && chunk.encode_utf16().count() <= REPLY_CHUNK_MAX_UTF16
        })
        && attempt.confirmed <= attempt.chunks.len()
        && match attempt.in_flight {
            Some(index) => index == attempt.confirmed && index < attempt.chunks.len(),
            None => true,
        }
}

async fn reply_send_attempt(
    ctx: &EventContext,
    ticket: &Ticket,
    channel: ChannelId,
    owner: u64,
    attempt_id: i64,
) -> Result<Option<(ReplySendAttempt, i64)>, UserError> {
    let key = reply_send_attempt_key(channel, owner, attempt_id);
    ctx.store()
        .entry(&key)
        .await
        .map_err(|error| failed(&error))?
        .map(|entry| {
            let attempt: ReplySendAttempt =
                serde_json::from_value(entry.value).map_err(|error| {
                    tracing::warn!(?error, key = %key, "could not read a saved reply send");
                    UserError::message("tickets-failed")
                })?;
            if !valid_reply_send_attempt(&attempt, ticket, channel, owner) {
                return Err(UserError::message("tickets-failed"));
            }
            Ok((attempt, entry.revision))
        })
        .transpose()
}

async fn staff_component_ticket(
    cx: &ComponentCx,
    ctx: &EventContext,
    channel: ChannelId,
) -> Result<Ticket, &'static str> {
    let ticket = ctx
        .store()
        .get::<Ticket>(&ticket_key(channel))
        .await
        .map_err(|error| {
            tracing::warn!(?error, channel = %channel, "could not read a ticket for a saved reply");
            "tickets-failed"
        })?
        .ok_or("tickets-not-a-ticket")?;
    let config: Config = cx.config().map_err(|_| "tickets-failed")?;
    let access_config = config.for_ticket(&ticket);
    if !is_ticket_staff(cx, &ticket, &access_config).await {
        return Err("tickets-staff-only");
    }
    if !ticket.is_open() {
        return Err("tickets-not-a-ticket");
    }
    Ok(ticket)
}

async fn reply_send_uncertain(
    cx: &ComponentCx,
    ctx: &EventContext,
    attempt_id: i64,
    attempt: &ReplySendAttempt,
) -> Result<(), UserError> {
    let current = attempt.in_flight.unwrap_or(attempt.confirmed) + 1;
    cx.reply_with(InteractionReply {
        content: Some(
            cx.t(
                "tickets-reply-send-uncertain",
                &Args::new()
                    .with("id", attempt.reply_id.clone())
                    .with("confirmed", attempt.confirmed.to_string())
                    .with("current", current.to_string())
                    .with("parts", attempt.chunks.len().to_string()),
            ),
        ),
        embeds: Vec::new(),
        ephemeral: true,
        components: vec![ui::row(vec![reply_send_button(
            ctx,
            "reply-retry-uncertain",
            "tickets-reply-send-uncertain-button",
            attempt_id,
        )])],
    })
    .await
}

async fn resume_reply_send(
    cx: &ComponentCx,
    ctx: &EventContext,
    channel: ChannelId,
    attempt_id: i64,
    mut attempt: ReplySendAttempt,
    mut revision: i64,
) -> Result<(), UserError> {
    let key = reply_send_attempt_key(channel, attempt.owner, attempt_id);
    let total = attempt.chunks.len();
    if attempt.in_flight.is_some() {
        return reply_send_uncertain(cx, ctx, attempt_id, &attempt).await;
    }

    for index in attempt.confirmed..total {
        let chunk = attempt.chunks[index].clone();
        attempt.in_flight = Some(index);
        revision = match ctx
            .store()
            .replace(
                &key,
                &attempt,
                Expect::Revision(revision),
                Some(REPLY_SEND_RETENTION),
            )
            .await
        {
            Ok(revision) => revision,
            Err(StorageError::Conflict) => {
                return cx
                    .reply_private_t("tickets-reply-send-busy", &Args::new())
                    .await;
            }
            Err(error) => {
                tracing::warn!(?error, channel = %channel, attempt_id, "could not save reply send progress");
                return cx
                    .reply_private_t("tickets-reply-send-state-error", &Args::new())
                    .await;
            }
        };
        if let Err(error) = ctx
            .api
            .execute(Action::SendMessage {
                channel_id: channel,
                content: Some(chunk),
                embeds: Vec::new(),
                reply_to: None,
                mentions: MentionPolicy::None,
            })
            .await
        {
            tracing::warn!(
                ?error,
                channel = %channel,
                attempt_id,
                chunk = index + 1,
                "could not send a saved reply part"
            );
            return reply_send_uncertain(cx, ctx, attempt_id, &attempt).await;
        }

        attempt.confirmed = index + 1;
        attempt.in_flight = None;
        revision = match ctx
            .store()
            .replace(
                &key,
                &attempt,
                Expect::Revision(revision),
                Some(REPLY_SEND_RETENTION),
            )
            .await
        {
            Ok(revision) => revision,
            Err(error) => {
                tracing::warn!(
                    ?error,
                    channel = %channel,
                    attempt_id,
                    chunk = index + 1,
                    "could not confirm a sent saved reply part"
                );
                attempt.confirmed = index;
                attempt.in_flight = Some(index);
                return reply_send_uncertain(cx, ctx, attempt_id, &attempt).await;
            }
        };
    }

    cx.reply_private_t(
        "tickets-reply-sent",
        &Args::new()
            .with("id", attempt.reply_id)
            .with("parts", total.to_string()),
    )
    .await
}

async fn select_reply(cx: ComponentCx) -> Result<(), UserError> {
    let channel = cx.click().channel_id;
    let ctx = cx.ctx();
    let ticket = match staff_component_ticket(&cx, ctx, channel).await {
        Ok(ticket) => ticket,
        Err(message) => return cx.reply_private_t(message, &Args::new()).await,
    };
    if cx.values().len() != 1 || !valid_reply_id(&cx.values()[0]) {
        return cx
            .reply_private_t("tickets-reply-not-found", &Args::new())
            .await;
    }
    let id = cx.values()[0].clone();
    cx.defer(true).await?;
    let Some((reply, _)) = saved_reply(ctx, &ticket, &id).await? else {
        return cx
            .reply_private_t("tickets-reply-not-found", &Args::new())
            .await;
    };
    if reply.deleted {
        return cx
            .reply_private_t("tickets-reply-not-found", &Args::new())
            .await;
    }

    let owner = cx.user().id.get();
    let attempt_id = match ctx.store().counter_add(REPLY_SEND_COUNTER, owner, 1).await {
        Ok(attempt_id) => attempt_id,
        Err(error) => {
            tracing::warn!(?error, channel = %channel, owner, "could not create a reply send attempt");
            return cx
                .reply_private_t("tickets-reply-send-state-error", &Args::new())
                .await;
        }
    };
    let attempt = ReplySendAttempt {
        channel: channel.get(),
        owner,
        ticket_number: ticket.number,
        panel: ticket.panel.clone(),
        reply_id: reply.id,
        chunks: split_reply(&reply.body)
            .into_iter()
            .map(str::to_owned)
            .collect(),
        confirmed: 0,
        in_flight: None,
    };
    let key = reply_send_attempt_key(channel, owner, attempt_id);
    let revision = match ctx
        .store()
        .replace(&key, &attempt, Expect::Missing, Some(REPLY_SEND_RETENTION))
        .await
    {
        Ok(revision) => revision,
        Err(StorageError::TooLarge { .. }) => {
            return cx
                .reply_private_t("tickets-reply-too-large", &Args::new())
                .await;
        }
        Err(error) => {
            tracing::warn!(?error, channel = %channel, owner, "could not save a reply send attempt");
            return cx
                .reply_private_t("tickets-reply-send-state-error", &Args::new())
                .await;
        }
    };

    cx.reply_with(InteractionReply {
        content: Some(
            cx.t(
                "tickets-reply-sending",
                &Args::new()
                    .with("id", attempt.reply_id.clone())
                    .with("parts", attempt.chunks.len().to_string()),
            ),
        ),
        embeds: Vec::new(),
        ephemeral: true,
        components: vec![ui::row(vec![reply_send_button(
            ctx,
            "reply-retry",
            "tickets-reply-send-retry-button",
            attempt_id,
        )])],
    })
    .await?;
    resume_reply_send(&cx, ctx, channel, attempt_id, attempt, revision).await
}

async fn retry_reply(cx: ComponentCx) -> Result<(), UserError> {
    let channel = cx.click().channel_id;
    let ctx = cx.ctx();
    let ticket = match staff_component_ticket(&cx, ctx, channel).await {
        Ok(ticket) => ticket,
        Err(message) => return cx.reply_private_t(message, &Args::new()).await,
    };
    let Some(attempt_id) = cx
        .data()
        .parse::<i64>()
        .ok()
        .filter(|attempt_id| *attempt_id > 0)
    else {
        return cx
            .reply_private_t("tickets-reply-send-not-found", &Args::new())
            .await;
    };
    cx.defer(true).await?;
    let Some((attempt, revision)) =
        reply_send_attempt(ctx, &ticket, channel, cx.user().id.get(), attempt_id).await?
    else {
        return cx
            .reply_private_t("tickets-reply-send-not-found", &Args::new())
            .await;
    };
    if attempt.confirmed == attempt.chunks.len() {
        return cx
            .reply_private_t(
                "tickets-reply-send-already-complete",
                &Args::new().with("id", attempt.reply_id),
            )
            .await;
    }
    if attempt.in_flight.is_some() {
        return reply_send_uncertain(&cx, ctx, attempt_id, &attempt).await;
    }
    resume_reply_send(&cx, ctx, channel, attempt_id, attempt, revision).await
}

async fn retry_uncertain_reply(cx: ComponentCx) -> Result<(), UserError> {
    let channel = cx.click().channel_id;
    let ctx = cx.ctx();
    let ticket = match staff_component_ticket(&cx, ctx, channel).await {
        Ok(ticket) => ticket,
        Err(message) => return cx.reply_private_t(message, &Args::new()).await,
    };
    let Some(attempt_id) = cx
        .data()
        .parse::<i64>()
        .ok()
        .filter(|attempt_id| *attempt_id > 0)
    else {
        return cx
            .reply_private_t("tickets-reply-send-not-found", &Args::new())
            .await;
    };
    cx.defer(true).await?;
    let Some((mut attempt, revision)) =
        reply_send_attempt(ctx, &ticket, channel, cx.user().id.get(), attempt_id).await?
    else {
        return cx
            .reply_private_t("tickets-reply-send-not-found", &Args::new())
            .await;
    };
    if attempt.confirmed == attempt.chunks.len() {
        return cx
            .reply_private_t(
                "tickets-reply-send-already-complete",
                &Args::new().with("id", attempt.reply_id),
            )
            .await;
    }
    if attempt.in_flight.is_some() {
        attempt.in_flight = None;
        let key = reply_send_attempt_key(channel, attempt.owner, attempt_id);
        let revision = match ctx
            .store()
            .replace(
                &key,
                &attempt,
                Expect::Revision(revision),
                Some(REPLY_SEND_RETENTION),
            )
            .await
        {
            Ok(revision) => revision,
            Err(StorageError::Conflict) => {
                return cx
                    .reply_private_t("tickets-reply-send-busy", &Args::new())
                    .await;
            }
            Err(error) => {
                tracing::warn!(?error, channel = %channel, attempt_id, "could not confirm retry of an uncertain reply part");
                return cx
                    .reply_private_t("tickets-reply-send-state-error", &Args::new())
                    .await;
            }
        };
        return resume_reply_send(&cx, ctx, channel, attempt_id, attempt, revision).await;
    }
    resume_reply_send(&cx, ctx, channel, attempt_id, attempt, revision).await
}

async fn add_reply(cx: CommandCx, args: ReplyAddArgs) -> Result<(), UserError> {
    let ctx = cx.ctx();
    let ticket = staff_ticket(&cx, ctx, cx.invocation().channel_id).await?;
    cx.defer(true).await?;
    let title = args.title.trim();
    if !valid_reply_id(&args.id) || !valid_reply_content(title, &args.body) {
        return cx
            .reply_private_t("tickets-reply-invalid", &Args::new())
            .await;
    }

    let existing = saved_reply(ctx, &ticket, &args.id).await?;
    let expect = match existing.as_ref() {
        Some((reply, _)) if !reply.deleted => {
            return cx
                .reply_private_t("tickets-reply-exists", &Args::new())
                .await;
        }
        Some((_, revision)) => Expect::Revision(*revision),
        None => Expect::Missing,
    };
    let reply = SavedReply {
        id: args.id.clone(),
        title: title.to_owned(),
        body: args.body,
        deleted: false,
    };
    match ctx
        .store()
        .replace(&reply_key(&ticket, &reply.id), &reply, expect, None)
        .await
    {
        Ok(_) => {
            cx.reply_private_t("tickets-reply-saved", &Args::new())
                .await
        }
        Err(StorageError::TooLarge { .. }) => {
            cx.reply_private_t("tickets-reply-too-large", &Args::new())
                .await
        }
        Err(StorageError::Conflict) => {
            cx.reply_private_t("tickets-reply-conflict", &Args::new())
                .await
        }
        Err(error) => Err(failed(&error)),
    }
}

async fn edit_reply(cx: CommandCx, args: ReplyEditArgs) -> Result<(), UserError> {
    let ctx = cx.ctx();
    let ticket = staff_ticket(&cx, ctx, cx.invocation().channel_id).await?;
    cx.defer(true).await?;
    let title = args.title.trim();
    if !valid_reply_id(&args.id) || !valid_reply_content(title, &args.body) {
        return cx
            .reply_private_t("tickets-reply-invalid", &Args::new())
            .await;
    }

    let Some((existing, revision)) = saved_reply(ctx, &ticket, &args.id).await? else {
        return cx
            .reply_private_t("tickets-reply-not-found", &Args::new())
            .await;
    };
    if existing.deleted {
        return cx
            .reply_private_t("tickets-reply-not-found", &Args::new())
            .await;
    }
    let reply = SavedReply {
        id: args.id.clone(),
        title: title.to_owned(),
        body: args.body,
        deleted: false,
    };
    match ctx
        .store()
        .replace(
            &reply_key(&ticket, &reply.id),
            &reply,
            Expect::Revision(revision),
            None,
        )
        .await
    {
        Ok(_) => {
            cx.reply_private_t("tickets-reply-updated", &Args::new())
                .await
        }
        Err(StorageError::TooLarge { .. }) => {
            cx.reply_private_t("tickets-reply-too-large", &Args::new())
                .await
        }
        Err(StorageError::Conflict) => {
            cx.reply_private_t("tickets-reply-conflict", &Args::new())
                .await
        }
        Err(error) => Err(failed(&error)),
    }
}

async fn delete_reply(cx: CommandCx, args: ReplyDeleteArgs) -> Result<(), UserError> {
    let ctx = cx.ctx();
    let ticket = staff_ticket(&cx, ctx, cx.invocation().channel_id).await?;
    cx.defer(true).await?;
    if !valid_reply_id(&args.id) {
        return cx
            .reply_private_t("tickets-reply-not-found", &Args::new())
            .await;
    }

    let Some((existing, revision)) = saved_reply(ctx, &ticket, &args.id).await? else {
        return cx
            .reply_private_t("tickets-reply-not-found", &Args::new())
            .await;
    };
    if existing.deleted {
        return cx
            .reply_private_t("tickets-reply-not-found", &Args::new())
            .await;
    }
    let reply = SavedReply {
        id: args.id.clone(),
        title: String::new(),
        body: String::new(),
        deleted: true,
    };
    match ctx
        .store()
        .replace(
            &reply_key(&ticket, &reply.id),
            &reply,
            Expect::Revision(revision),
            None,
        )
        .await
    {
        Ok(_) => {
            cx.reply_private_t("tickets-reply-deleted", &Args::new())
                .await
        }
        Err(StorageError::Conflict) => {
            cx.reply_private_t("tickets-reply-conflict", &Args::new())
                .await
        }
        Err(error) => Err(failed(&error)),
    }
}

#[allow(clippy::too_many_lines)]
async fn search_replies(cx: CommandCx, args: ReplySearchArgs) -> Result<(), UserError> {
    let ctx = cx.ctx();
    let ticket = staff_ticket(&cx, ctx, cx.invocation().channel_id).await?;
    cx.defer(true).await?;
    let query = args
        .query
        .as_deref()
        .unwrap_or_default()
        .trim()
        .to_lowercase();
    let Some(page_index) = usize::try_from(args.page.unwrap_or(1))
        .ok()
        .and_then(|page| page.checked_sub(1))
    else {
        return cx
            .reply_private_t("tickets-replies-page-not-found", &Args::new())
            .await;
    };
    let Some(start) = page_index.checked_mul(REPLY_PAGE_SIZE) else {
        return cx
            .reply_private_t("tickets-replies-page-not-found", &Args::new())
            .await;
    };
    let Some(end) = start.checked_add(REPLY_PAGE_SIZE) else {
        return cx
            .reply_private_t("tickets-replies-page-not-found", &Args::new())
            .await;
    };

    let prefix = reply_prefix(&ticket);
    let mut cursor: Option<String> = None;
    let mut matching_count = 0;
    let mut matches = Vec::with_capacity(REPLY_PAGE_SIZE);
    let mut has_more = false;
    loop {
        let entries = match cursor.as_deref() {
            Some(after) => {
                ctx.store()
                    .list_after(&prefix, after, REPLY_SCAN_BATCH_SIZE)
                    .await
            }
            None => ctx.store().list(&prefix, REPLY_SCAN_BATCH_SIZE).await,
        }
        .map_err(|error| failed(&error))?;
        if entries.is_empty() {
            break;
        }
        let full_batch = entries.len() == REPLY_SCAN_BATCH_SIZE;
        let next_cursor = entries.last().map(|(key, _)| key.clone());
        for (key, entry) in entries {
            let (reply, _) = decode_saved_reply(&ticket, &key, entry)?;
            if reply.deleted
                || (!query.is_empty()
                    && !reply.id.to_lowercase().contains(&query)
                    && !reply.title.to_lowercase().contains(&query))
            {
                continue;
            }
            if matching_count < start {
                matching_count += 1;
                continue;
            }
            if matching_count < end {
                matches.push((reply.title, reply.id));
                matching_count += 1;
                continue;
            }
            has_more = true;
            break;
        }
        if has_more || !full_batch {
            break;
        }
        cursor = next_cursor;
    }

    if matches.is_empty() {
        let key = if matching_count == 0 {
            "tickets-replies-empty"
        } else {
            "tickets-replies-page-not-found"
        };
        return cx.reply_private_t(key, &Args::new()).await;
    }

    let result_key = if has_more {
        "tickets-replies-results-more"
    } else {
        "tickets-replies-results-last"
    };
    let placeholder = cx.t("tickets-replies-select", &Args::new());
    let menu = ui::select_described(
        CustomId::new("reply-select").encode(&ctx.module),
        Some(&placeholder),
        matches
            .iter()
            .map(|(title, id)| (title.clone(), id.clone(), Some(id.clone()))),
        1,
        1,
    );
    cx.reply_with(InteractionReply {
        content: Some(cx.t(
            result_key,
            &Args::new().with("page", (page_index + 1).to_string()),
        )),
        embeds: Vec::new(),
        ephemeral: true,
        components: vec![ui::row(vec![menu])],
    })
    .await
}

#[allow(clippy::too_many_lines)]
fn schema() -> serde_json::Value {
    let forms_schema = form_library::schema();
    let team_permissions_schema = json!({
        "type": "object",
        "additionalProperties": false,
        "description": "config-tickets-staff-team-permissions",
        "properties": {
            "add_reactions": { "type": "boolean", "default": true, "description": "config-tickets-team-permission-add_reactions" },
            "send_messages": { "type": "boolean", "default": true, "description": "config-tickets-team-permission-send_messages" },
            "send_tts_messages": { "type": "boolean", "default": true, "description": "config-tickets-team-permission-send_tts_messages" },
            "embed_links": { "type": "boolean", "default": true, "description": "config-tickets-team-permission-embed_links" },
            "attach_files": { "type": "boolean", "default": true, "description": "config-tickets-team-permission-attach_files" },
            "mention_everyone": { "type": "boolean", "default": false, "description": "config-tickets-team-permission-mention_everyone" },
            "use_external_emojis": { "type": "boolean", "default": true, "description": "config-tickets-team-permission-use_external_emojis" },
            "use_application_commands": { "type": "boolean", "default": true, "description": "config-tickets-team-permission-use_application_commands" },
            "use_external_stickers": { "type": "boolean", "default": true, "description": "config-tickets-team-permission-use_external_stickers" },
            "send_voice_messages": { "type": "boolean", "default": true, "description": "config-tickets-team-permission-send_voice_messages" }
        }
    });
    let staff_team_schema = json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["id", "name"],
        "properties": {
            "id": { "type": "string", "minLength": 1, "maxLength": 32, "pattern": "^[a-zA-Z0-9_-]+$", "description": "config-tickets-staff-team-id" },
            "name": { "type": "string", "minLength": 1, "maxLength": 80, "pattern": ".*\\S.*", "description": "config-tickets-staff-team-name" },
            "roles": { "type": "array", "items": { "type": "string", "pattern": "^[0-9]{1,20}$" }, "description": "config-tickets-staff-team-roles", "x-ui": { "widget": "roles" } },
            "users": { "type": "array", "items": { "type": "string", "pattern": "^[0-9]{1,20}$" }, "description": "config-tickets-staff-team-users", "x-ui": { "widget": "users" } },
            "permissions": team_permissions_schema
        }
    });
    let mut exit_survey_schema = advanced_forms::form_questions_schema();
    exit_survey_schema["description"] = json!("config-tickets-panel-exit_survey");
    let mut named_panel_schema = json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["id"],
        "properties": {
            "id": { "type": "string", "pattern": "^[a-zA-Z0-9_-]{1,32}$", "description": "config-tickets-panel-id" },
            "name": { "anyOf": [{ "type": "string", "maxLength": 80 }, { "type": "null" }], "description": "config-tickets-panel-name", "x-ui": { "level": "basic" } },
            "description": { "type": ["string", "null"], "maxLength": 100, "description": "config-tickets-panel-description" },
            "button_style": { "anyOf": [{ "type": "string", "enum": ["primary", "secondary", "success", "danger"] }, { "type": "null" }], "description": "config-tickets-panel-button_style" },
            "category": { "anyOf": [{ "type": "string", "pattern": "^[0-9]{1,20}$" }, { "type": "null" }], "description": "config-tickets-category", "x-ui": { "widget": "channel", "channel_kinds": ["category"] } },
            "thread_channel": { "anyOf": [{ "type": "string", "pattern": "^[0-9]{1,20}$" }, { "type": "null" }], "description": "config-tickets-thread_channel", "x-ui": { "widget": "channel", "channel_kinds": ["text"] } },
            "staff_roles": { "type": "array", "maxItems": 20, "items": { "type": "string", "pattern": "^[0-9]{1,20}$" }, "description": "config-tickets-panel-staff", "x-ui": { "level": "basic", "widget": "roles" } },
            "staff_teams": { "type": "array", "description": "config-tickets-panel-staff_teams", "x-ui": { "level": "basic", "widget": "ticket-staff-teams" }, "items": { "type": "string", "pattern": "^(default|[a-zA-Z0-9_-]{1,32})$" } },
            "log_channel": { "anyOf": [{ "type": "string", "pattern": "^[0-9]{1,20}$" }, { "type": "null" }], "description": "config-tickets-log_channel", "x-ui": { "widget": "channel" } },
            "timezone": { "anyOf": [{ "type": "string", "minLength": 1, "maxLength": 64 }, { "type": "null" }], "description": "config-tickets-panel-timezone", "x-ui": { "level": "basic" } },
            "close_after_days": { "type": "integer", "minimum": 0, "maximum": 365, "description": "config-tickets-close_after_days" },
            "waiting_after_hours": { "type": "integer", "minimum": 0, "maximum": u32::MAX, "default": 0, "description": "config-tickets-panel-waiting_after_hours", "x-ui": { "level": "basic" } },
            "first_response_timeout_hours": { "type": "integer", "minimum": 0, "maximum": u32::MAX, "default": 0, "description": "config-tickets-panel-first_response_timeout_hours", "x-ui": { "level": "basic" } },
            "last_message_timeout_hours": { "type": "integer", "minimum": 0, "maximum": u32::MAX, "default": 0, "description": "config-tickets-panel-last_message_timeout_hours", "x-ui": { "level": "basic" } },
            "waiting_category": { "anyOf": [{ "type": "string", "pattern": "^[0-9]{1,20}$" }, { "type": "null" }], "description": "config-tickets-panel-waiting_category", "x-ui": { "widget": "channel", "channel_kinds": ["category"] } },
            "max_open": { "type": "integer", "minimum": 0, "maximum": u32::MAX, "default": 0, "description": "config-tickets-panel-max_open", "x-ui": { "level": "basic" } },
            "form_id": { "anyOf": [{ "type": "string", "pattern": "^[a-z0-9_-]{1,32}$" }, { "type": "null" }], "description": "tickets-form-id", "x-ui": { "widget": "ticket-form" } },
            "questions": advanced_forms::form_questions_schema(),
            "exit_survey": exit_survey_schema
        }
    });
    let panel_customization_schema = panel_customization::PanelCustomization::schema_properties();
    if let (Some(properties), Some(custom_properties)) = (
        named_panel_schema["properties"].as_object_mut(),
        panel_customization_schema.as_object(),
    ) {
        properties.extend(custom_properties.clone());
    }
    let mut config_schema = json!({
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "category": {
                "type": ["string", "null"],
                "pattern": "^[0-9]{1,20}$",
                "description": "config-tickets-category",
                "x-ui": { "level": "basic", "widget": "channel", "channel_kinds": ["category"] }
            },
            "thread_channel": {
                "type": ["string", "null"],
                "pattern": "^[0-9]{1,20}$",
                "description": "config-tickets-thread_channel",
                "x-ui": { "level": "basic", "widget": "channel", "channel_kinds": ["text"] }
            },
            "staff_roles": {
                "type": "array",
                "maxItems": 20,
                "items": { "type": "string", "pattern": "^[0-9]{1,20}$" },
                "description": "config-tickets-staff_roles",
                "x-ui": { "level": "basic", "widget": "roles" }
            },
            "staff_teams": {
                "type": "array",
                "description": "config-tickets-staff_teams",
                "x-ui": { "level": "basic" },
                "items": staff_team_schema
            },
            "log_channel": {
                "type": ["string", "null"],
                "pattern": "^[0-9]{1,20}$",
                "description": "config-tickets-log_channel",
                "x-ui": { "level": "basic", "widget": "channel" }
            },
            "max_open": {
                "type": "integer",
                "minimum": 0,
                "maximum": u32::MAX,
                "description": "config-tickets-max_open",
                "x-ui": { "level": "basic" }
            },
            "close_after_days": {
                "type": "integer",
                "minimum": 0,
                "maximum": 365,
                "description": "config-tickets-close_after_days",
                "x-ui": { "level": "basic" }
            },
            "waiting_after_hours": {
                "type": "integer",
                "minimum": 0,
                "maximum": u32::MAX,
                "description": "config-tickets-waiting_after_hours",
                "x-ui": { "level": "basic" }
            },
            "first_response_timeout_hours": {
                "type": "integer",
                "minimum": 0,
                "maximum": u32::MAX,
                "description": "config-tickets-first_response_timeout_hours",
                "x-ui": { "level": "basic" }
            },
            "last_message_timeout_hours": {
                "type": "integer",
                "minimum": 0,
                "maximum": u32::MAX,
                "description": "config-tickets-last_message_timeout_hours",
                "x-ui": { "level": "basic" }
            },
            "waiting_category": {
                "type": ["string", "null"],
                "pattern": "^[0-9]{1,20}$",
                "description": "config-tickets-waiting_category",
                "x-ui": { "level": "basic", "widget": "channel", "channel_kinds": ["category"] }
            },
            "close_request_timeout_hours": {
                "type": "integer",
                "minimum": 0,
                "maximum": u32::MAX,
                "description": "config-tickets-close_request_timeout_hours",
                "x-ui": { "level": "basic" }
            },
            "questions": advanced_forms::form_questions_schema(),
            "integrations": integrations::config_schema(),
            "timezone": {
                "type": "string",
                "minLength": 1,
                "maxLength": 64,
                "description": "config-tickets-timezone",
                "x-ui": { "level": "basic" }
            },
            "hours": {
                "type": "array",
                "description": "config-tickets-hours",
                "x-ui": { "level": "basic" },
                "items": {
                    "type": "object",
                    "additionalProperties": false,
                    "required": ["panel", "weekday", "start_minute", "end_minute"],
                    "properties": {
                        "panel": {
                            "anyOf": [
                                { "type": "string", "pattern": "^[a-zA-Z0-9_-]{1,32}$" },
                                { "type": "null" }
                            ],
                            "description": "config-tickets-hours-panel",
                            "x-ui": { "widget": "ticket-panel" }
                        },
                        "weekday": {
                            "type": "string",
                            "enum": ["monday", "tuesday", "wednesday", "thursday", "friday", "saturday", "sunday"],
                            "description": "config-tickets-hours-weekday"
                        },
                        "start_minute": {
                            "type": "integer",
                            "minimum": 0,
                            "maximum": 1439,
                            "description": "config-tickets-hours-start_minute",
                            "x-ui": { "widget": "time-minute", "max": 1439 }
                        },
                        "end_minute": {
                            "type": "integer",
                            "minimum": 0,
                            "maximum": 1440,
                            "description": "config-tickets-hours-end_minute",
                            "x-ui": { "widget": "time-minute", "max": 1440, "allow_24_00": true }
                        }
                    }
                }
            },
            "close_when_opener_leaves": {
                "type": "boolean",
                "description": "config-tickets-close_when_opener_leaves",
                "x-ui": { "level": "basic" }
            },
            "panel_display": {
                "type": "string",
                "enum": ["buttons", "select"],
                "default": "buttons",
                "description": "config-tickets-panel-display",
                "x-ui": { "level": "basic" }
            },
            "panels": {
                "type": "array",
                "description": "config-tickets-panels",
                "x-ui": { "level": "basic" },
                "items": named_panel_schema
            }
        }
    });
    if let Some(properties) = config_schema
        .get_mut("properties")
        .and_then(|value| value.as_object_mut())
    {
        properties.insert("forms".to_owned(), forms_schema);
    }
    config_schema
}

/// # Errors
/// Returns an error when the SDK rejects a configured module route or handler.
#[allow(clippy::too_many_lines)]
pub fn builder() -> Result<ModuleBuilder, CoreError> {
    let schema = schema();
    Ok(
        ModuleBuilder::new("tickets", "Tickets", env!("CARGO_PKG_VERSION"))?
            .capability(closure_sdk::Capability::Storage)
            .capability(closure_sdk::Capability::Schedule)
            .capability(closure_sdk::Capability::SendMessages)
            .capability(closure_sdk::Capability::SendDirectMessages)
            .capability(closure_sdk::Capability::ReadMessages)
            .capability(closure_sdk::Capability::ReadGuild)
            .capability(closure_sdk::Capability::ManageChannels)
            .capability(closure_sdk::Capability::ManageThreads)
            .capability(closure_sdk::Capability::RespondToInteractions)
            .intents(Intents::GUILD_MEMBERS | Intents::GUILD_MESSAGES)
            .permission(PermissionSpec::new(
                "tickets.manage",
                Level::MODERATOR,
                "permission-tickets-manage",
            )?)
            .permission(PermissionSpec::new(
                "tickets.use",
                Level::GENERAL,
                "permission-tickets-use",
            )?)
            .config(
                schema,
                json!({
                    "category": null,
                    "thread_channel": null,
                    "staff_roles": [],
                    "staff_teams": [],
                    "log_channel": null,
                    "timezone": "UTC",
                    "hours": [],
                    "max_open": 1,
                    "close_after_days": 0,
                    "waiting_after_hours": 0,
                    "first_response_timeout_hours": 0,
                    "last_message_timeout_hours": 0,
                    "waiting_category": null,
                    "close_request_timeout_hours": 0,
                    "questions": [],
                    "forms": [],
                    "integrations": [],
                    "staff_teams": [],
                    "close_when_opener_leaves": false,
                    "panel_display": "buttons",
                    "panels": []
                }),
            )
            .group(
                "ticket",
                [
                    Command::<PanelArgs>::new("panel")
                        .requires("tickets.manage")
                        .handler(panel),
                    Command::<SearchHistory>::new("search").handler(search_history),
                    Command::<RateArgs>::new("rate").handler(rate_command),
                    Command::<SurveyArgs>::new("survey")
                        .requires("tickets.use")
                        .handler(survey_command),
                    Command::<ViewHistory>::new("history").handler(view_history),
                    Command::<Close>::new("close")
                        .requires("tickets.use")
                        .handler(close_command),
                    Command::<()>::new("reopen")
                        .requires("tickets.manage")
                        .handler(thread::reopen_ticket),
                    Command::<()>::new("answers")
                        .requires("tickets.use")
                        .handler(ticket_answers),
                    Command::<CloseRequestArgs>::new("close-request").handler(close_request),
                    Command::<Member>::new("add").handler(add),
                    Command::<Member>::new("remove").handler(remove),
                    Command::<Unclaim>::new("unclaim").handler(unclaim),
                    Command::<Member>::new("transfer").handler(transfer),
                    Command::<Rename>::new("rename").handler(rename),
                    Command::<StatsArgs>::new("stats").handler(stats_command),
                    Command::<tags::TagCommandArgs>::new("tag")
                        .requires("tickets.use")
                        .handler(tags::tag_command),
                    Command::<NoteArgs>::new("note").handler(add_note),
                    Command::<NotesArgs>::new("notes").handler(list_notes),
                ],
            )
            .groups(
                "ticket",
                [
                    SubcommandGroup::new("settings")
                        .subcommand(
                            Command::<operations::SetPanelArgs>::new("set-panel")
                                .handler(operations::set_panel),
                        )
                        .subcommand(
                            Command::<operations::RoleArgs>::new("add-role")
                                .handler(operations::add_role),
                        )
                        .subcommand(
                            Command::<operations::RoleArgs>::new("remove-role")
                                .handler(operations::remove_role),
                        )
                        .subcommand(
                            Command::<operations::WaitingArgs>::new("waiting")
                                .handler(operations::waiting),
                        ),
                    SubcommandGroup::new("data")
                        .subcommand(
                            Command::<data::ExportMyData>::new("export")
                                .handler(data::export_my_data),
                        )
                        .subcommand(
                            Command::<data::EraseMyData>::new("erase").handler(data::erase_my_data),
                        ),
                    SubcommandGroup::new("blacklist")
                        .subcommand(
                            Command::<blacklist::UserArgs>::new("add-user")
                                .handler(blacklist::add_user),
                        )
                        .subcommand(
                            Command::<blacklist::UserArgs>::new("remove-user")
                                .handler(blacklist::remove_user),
                        )
                        .subcommand(
                            Command::<blacklist::RoleArgs>::new("add-role")
                                .handler(blacklist::add_role),
                        )
                        .subcommand(
                            Command::<blacklist::RoleArgs>::new("remove-role")
                                .handler(blacklist::remove_role),
                        )
                        .subcommand(
                            Command::<blacklist::ScopeArgs>::new("list").handler(blacklist::list),
                        ),
                    SubcommandGroup::new("knowledge")
                        .subcommand(
                            Command::<knowledge::SearchArgs>::new("search")
                                .handler(knowledge::search),
                        )
                        .subcommand(
                            Command::<knowledge::ViewArgs>::new("view").handler(knowledge::view),
                        )
                        .subcommand(
                            Command::<KnowledgeSendArgs>::new("send").handler(knowledge_send),
                        )
                        .subcommand(
                            Command::<knowledge::AddArgs>::new("add")
                                .requires("tickets.manage")
                                .handler(knowledge::add),
                        )
                        .subcommand(
                            Command::<knowledge::EditArgs>::new("edit")
                                .requires("tickets.manage")
                                .handler(knowledge::edit),
                        )
                        .subcommand(
                            Command::<knowledge::DeleteArgs>::new("delete")
                                .requires("tickets.manage")
                                .handler(knowledge::delete),
                        ),
                    SubcommandGroup::new("label")
                        .subcommand(Command::<LabelArgs>::new("add").handler(add_label))
                        .subcommand(Command::<LabelArgs>::new("remove").handler(remove_label))
                        .subcommand(Command::<LabelListArgs>::new("list").handler(list_labels)),
                    SubcommandGroup::new("reply")
                        .subcommand(
                            Command::<ReplySearchArgs>::new("search").handler(search_replies),
                        )
                        .subcommand(Command::<ReplyAddArgs>::new("add").handler(add_reply))
                        .subcommand(Command::<ReplyEditArgs>::new("edit").handler(edit_reply))
                        .subcommand(
                            Command::<ReplyDeleteArgs>::new("delete").handler(delete_reply),
                        ),
                    SubcommandGroup::new("maintenance")
                        .subcommand(
                            Command::<OpeningsArgs>::new("openings")
                                .requires("tickets.manage")
                                .handler(openings_command),
                        )
                        .subcommand(
                            Command::<OpenReservationArgs>::new("recover-open")
                                .requires("tickets.manage")
                                .handler(recover_open_command),
                        )
                        .subcommand(
                            Command::<OpenReservationArgs>::new("release-open")
                                .requires("tickets.manage")
                                .handler(release_open_command),
                        ),
                ],
            )
            .component("open", open)
            .component("open-select", open_select)
            .modal(FORM, submit_form)
            .component("close", close_button)
            .component("rate", rate_button)
            .component("close-request-approve", approve_close_request)
            .component("close-request-reject", reject_close_request)
            .component("claim", claim)
            .component("history-search-next", search_history_next)
            .job(knowledge::KNOWLEDGE_SEARCH_JOB, knowledge::search_job)
            .job(
                knowledge::KNOWLEDGE_SUGGESTION_JOB,
                knowledge::suggestion_job,
            )
            .component("reply-select", select_reply)
            .component("reply-retry", retry_reply)
            .component("reply-retry-uncertain", retry_uncertain_reply)
            .component(message_ticket::PANEL_SELECT, message_ticket::select_panel)
            .component("survey-open", teams_survey::open_exit_survey_button)
            .component("survey-view", teams_survey::view_exit_survey)
            .modal("survey-submit", teams_survey::submit_exit_survey)
            .message_command(
                message_ticket::COMMAND_NAME,
                message_ticket::message_command,
            )
            .dashboard_state(dashboard::state)
            .dashboard_public_state(dashboard::public_state)
            .dashboard_action("blacklist.add", dashboard::action)
            .dashboard_action("blacklist.remove", dashboard::action)
            .dashboard_action("ticket.close", dashboard::action)
            .dashboard_action("ticket.request_close", dashboard::action)
            .dashboard_action("ticket.reply", dashboard::action)
            .dashboard_action("ticket.add_label", dashboard::action)
            .dashboard_action("ticket.remove_label", dashboard::action)
            .dashboard_action("knowledge.create", dashboard::action)
            .dashboard_action("knowledge.update", dashboard::action)
            .dashboard_action("knowledge.delete", dashboard::action)
            .dashboard_action("integration.secret.set", dashboard::action)
            .dashboard_action("integration.secret.remove", dashboard::action)
            .dashboard_action("tag.create", dashboard::action)
            .dashboard_action("tag.update", dashboard::action)
            .dashboard_action("tag.clone", dashboard::action)
            .dashboard_action("tag.delete", dashboard::action)
            .dashboard_action("gallery.import", dashboard::action)
            .dashboard_action("gallery.export", dashboard::action)
            .dashboard_action("audit.search", dashboard::action)
            .dashboard_action("ticket.history.update", dashboard::action)
            .dashboard_action("ticket.label.create", dashboard::action)
            .dashboard_action("ticket.label.delete", dashboard::action)
            .on(Subscription::new(EventKind::MemberLeft), on_member_left)
            .on(
                Subscription::new(EventKind::MessageCreated),
                automation::on_message,
            )
            .job(IDLE, idle_job)
            .job(automation::AUTOMATION, automation::job)
            .job("ticket-operation-recovery", operations::retry_pending_job)
            .job(
                transcript_management::LABEL_BACKFILL_JOB,
                transcript_management::backfill_job,
            )
            .job(LEFT_SCAN, left_scan_job)
            .job(LEFT_CLOSE, left_close_job)
            .job(CLOSING, closing_job)
            .job(OPEN_RECOVERY, open_recovery_job)
            .job(CLOSE_REQUEST_TIMEOUT, close_request_timeout_job)
            .dynamic_command_aliases(tags::dynamic_command_aliases)
            .dynamic_command_alias(tags::dynamic_command_alias)
            .catalog(Locale::En, EN)
            .catalog(Locale::Ja, JA),
    )
}

/// # Errors
/// The module declaration is refused by the SDK.
pub fn module() -> Result<SdkModule, CoreError> {
    builder()?.build()
}

mod advanced_forms;
mod analytics;
mod audit;
mod automation;
mod blacklist;
mod dashboard;
mod data;

#[cfg(test)]
pub(crate) use data::transcript_is_incomplete;
mod form_library;
mod hours;
mod integrations;
mod knowledge;
mod message_ticket;
mod operations;
mod panel_customization;
mod tags;
mod teams_survey;
mod thread;
mod transcript_management;

#[cfg(test)]
mod tests;
