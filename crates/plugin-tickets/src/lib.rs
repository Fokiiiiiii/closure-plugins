//! Tickets: a button opens a private channel for the member and the staff, and closing it saves
//! a transcript.
//!
//! `/ticket panel` posts the button; a press opens `ticket-0001` for the member and the staff.
//! Closing writes the channel's messages to the log channel as text, then deletes the channel.

#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

use std::fmt::Write as _;
use std::time::Duration;

use closure_core::action::MessageCursor;
use closure_core::action::{
    Action, ChannelKind, ChannelPatch, FileUpload, MentionPolicy, NewChannel, Outcome,
    OverwriteSpec, OverwriteTarget, Reason,
};
use closure_core::error::{DiscordError, ModuleError};
use closure_core::event::{ChannelId, RoleId, UserId};
use closure_core::module::EventContext;
use closure_core::storage::{Expect, StorageError};
use closure_sdk::ui::{self, CustomId};
use closure_sdk::{
    Args, Command, CommandArgs, CommandCx, ComponentCx, CoreError, InteractionReply, JobCx, Level,
    Locale, ModuleBuilder, PermissionSpec, SdkModule, UserError,
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
/// How many tickets a member has open, and the last number given.
const OPEN: &str = "open";
const NUMBER: &str = "number";
const CLOSE_REQUESTS: &str = "close-requests";
/// How many messages a transcript holds at most: 100 for each of 50 pages.
const TRANSCRIPT_PAGES: usize = 50;
const HISTORY_PREFIX: &str = "ticket-history:";
const HISTORY_CONTENT_PREFIX: &str = "ticket-history-content:";
const HISTORY_INDEX_PREFIX: &str = "ticket-history-index:";
const HISTORY_ID_PREFIX: &str = "ticket-history-id:";
const HISTORY_LIMIT: usize = 1000;
const HISTORY_RESULTS: usize = 8;
const HISTORY_PAGE_BYTES: usize = 24 * 1024;
const HISTORY_VALUE_BYTES: usize = 30 * 1024;
const HISTORY_INDEX_SIZE: usize = 1000;
const HISTORY_DISPLAY_CHARS: usize = 700;
/// A ticket nobody writes in is looked at again after at most this long.
const DAY: i64 = 86_400;
/// What the member and the staff may do in a ticket.
const TICKET_PERMISSIONS: Permissions = Permissions::VIEW_CHANNEL
    .union(Permissions::SEND_MESSAGES)
    .union(Permissions::READ_MESSAGE_HISTORY)
    .union(Permissions::ATTACH_FILES)
    .union(Permissions::EMBED_LINKS);

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
struct Config {
    /// The category the tickets are made in; without one they are made at the top.
    category: Option<String>,
    /// The roles that see every ticket. Tickets are off without one.
    staff_roles: Vec<String>,
    /// A channel to receive a copy of each transcript.
    log_channel: Option<String>,
    max_open: u32,
    /// A ticket nobody writes in for this many days is closed; 0 never closes one.
    close_after_days: u32,
    panels: Vec<NamedPanel>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
struct NamedPanel {
    id: String,
    category: Option<String>,
    staff_roles: Vec<String>,
    log_channel: Option<String>,
    close_after_days: u32,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            category: None,
            staff_roles: Vec::new(),
            log_channel: None,
            max_open: 1,
            close_after_days: 0,
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

    fn with_panel(&self, panel: &NamedPanel) -> Self {
        Self {
            category: panel.category.clone(),
            staff_roles: panel.staff_roles.clone(),
            log_channel: panel.log_channel.clone(),
            max_open: self.max_open,
            close_after_days: panel.close_after_days,
            panels: Vec::new(),
        }
    }

    fn for_ticket(&self, ticket: &Ticket) -> Self {
        ticket
            .panel
            .as_ref()
            .map_or_else(|| self.clone(), |panel| self.with_panel(panel))
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
#[derive(Debug, Serialize, Deserialize)]
struct Ticket {
    number: i64,
    opener: u64,
    claimed_by: Option<u64>,
    created: i64,
    /// When somebody began to close it (unix seconds): a claim made now is refused, and a second
    /// close waits for the first, unless that one stopped long ago.
    #[serde(default)]
    closing: Option<i64>,
    #[serde(default)]
    close_request: Option<CloseRequest>,
    #[serde(default)]
    panel: Option<NamedPanel>,
    #[serde(default)]
    staff_roles: Vec<u64>,
    #[serde(default)]
    form_answers: Vec<(String, String)>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum HistoryStatus {
    Closing,
    Closed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct TicketHistory {
    version: u8,
    channel_id: u64,
    number: i64,
    opener: u64,
    claimed_by: Option<u64>,
    created: i64,
    closed: i64,
    closer: Option<u64>,
    reason: String,
    panel: Option<NamedPanel>,
    staff_roles: Vec<u64>,
    status: HistoryStatus,
    attempt: i64,
    transcript_attempt: i64,
    transcript_pages: u32,
    transcript_index_pages: u32,
    transcript_char_count: u64,
    transcript_bytes: u64,
    transcript_messages: u32,
    transcript_truncated: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CloseRequest {
    id: i64,
    requested_by: u64,
    reason: Option<String>,
}

#[derive(Serialize, Deserialize)]
struct Idle {
    channel: u64,
}

#[derive(CommandArgs)]
struct PanelArgs {
    /// The panel ID; omit for the default panel
    #[arg(max_length = 32)]
    panel: Option<String>,
    /// Where the message with the button goes; this channel when left out
    channel: Option<ChannelId>,
    /// What the message says
    #[arg(max_length = 1000)]
    text: Option<String>,
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
struct SearchHistory {
    /// Ticket number, member ID, or close reason
    #[arg(max_length = 100)]
    query: String,
}

#[derive(CommandArgs)]
struct ViewHistory {
    /// Ticket number
    number: i64,
    /// Transcript page
    page: Option<i64>,
}

fn ticket_key(channel: ChannelId) -> String {
    format!("ticket:{}", channel.get())
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

fn idle_key(channel: ChannelId) -> String {
    format!("idle-{}", channel.get())
}

async fn ticket_config(
    ctx: &EventContext,
    config: &Config,
    channel: ChannelId,
) -> Result<Config, UserError> {
    let ticket = ctx
        .store()
        .get::<Ticket>(&ticket_key(channel))
        .await
        .map_err(|error| failed(&error))?
        .ok_or_else(|| UserError::message("tickets-not-a-ticket"))?;
    Ok(config.for_ticket(&ticket))
}

fn failed(error: &impl ToString) -> UserError {
    UserError::from(ModuleError::other(error.to_string()))
}

fn stored(error: &impl ToString) -> ModuleError {
    ModuleError::other(error.to_string())
}

async fn say(cx: &ComponentCx, key: &str, args: &Args) -> Result<(), UserError> {
    cx.reply_with(InteractionReply {
        content: Some(cx.t(key, args)),
        embeds: Vec::new(),
        ephemeral: true,
        components: Vec::new(),
    })
    .await
}

fn button(ctx: &EventContext, route: &str, style: ButtonStyle, label: &str) -> Component {
    ui::button(
        style,
        ctx.t_in(ctx.locale, label, &Args::new()),
        CustomId::new(route).encode(&ctx.module),
    )
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

async fn post_close_request_message(
    cx: &CommandCx,
    ctx: &EventContext,
    channel: ChannelId,
    opener: u64,
    request: &CloseRequest,
) -> Result<(), closure_core::error::DiscordError> {
    let reason = request
        .reason
        .clone()
        .unwrap_or_else(|| cx.t("tickets-close-request-no-reason", &Args::new()));
    let content = cx.t(
        "tickets-close-request-message",
        &Args::new()
            .with("staff", format!("<@{}>", request.requested_by))
            .with("reason", reason),
    );
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

/// Whether the member may act as staff: a role of the settings, or the permission to manage.
async fn is_staff(cx: &ComponentCx, config: &Config) -> bool {
    let actor = cx.actor().await;
    let roles = config.staff();
    actor.roles.iter().any(|role| roles.contains(role))
        || cx.require("tickets.manage").await.is_ok()
}

async fn is_staff_command(cx: &CommandCx, config: &Config) -> bool {
    let actor = cx.actor().await;
    let roles = config.staff();
    actor.roles.iter().any(|role| roles.contains(role))
        || cx.require("tickets.manage").await.is_ok()
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

async fn panel(cx: CommandCx, args: PanelArgs) -> Result<(), UserError> {
    let config: Config = cx.config()?;
    let panel = args.panel.unwrap_or_default();
    if !panel.is_empty() {
        config.named(&panel)?;
    }
    let ctx = cx.ctx();
    let channel = args.channel.unwrap_or(cx.invocation().channel_id);
    let text = args
        .text
        .filter(|text| !text.trim().is_empty())
        .unwrap_or_else(|| cx.t("tickets-panel-text", &Args::new()));
    ctx.api
        .execute(Action::PostMessage {
            channel_id: channel,
            message_id: None,
            reply_to: None,
            content: Some(text),
            embeds: Vec::new(),
            components: vec![ui::row(vec![ui::button(
                ButtonStyle::Primary,
                ctx.t_in(ctx.locale, "tickets-open-button", &Args::new()),
                CustomId::new("open").data(panel).encode(&ctx.module),
            )])],
            files: Vec::new(),
        })
        .await
        .map_err(|e| failed(&e))?;
    cx.reply_private_t("tickets-panel-done", &Args::new()).await
}

/// A member pressed the button: a private channel for them and the staff.
#[allow(
    clippy::too_many_lines,
    reason = "one flow: reserve a place, make the channel, greet, remember, schedule"
)]
async fn open(cx: ComponentCx) -> Result<(), UserError> {
    // The button is not a way around what `/ticket` asks: the same permission.
    cx.require("tickets.use").await?;
    let original: Config = cx.config()?;
    let panel_id = cx.data();
    let panel = if panel_id.is_empty() {
        None
    } else {
        Some(original.named(&panel_id)?.clone())
    };
    let config = panel
        .as_ref()
        .map_or_else(|| original.clone(), |panel| original.with_panel(panel));
    let ctx = cx.ctx();
    let mut staff = config.staff();
    staff.sort_unstable();
    staff.dedup();
    if staff.is_empty() {
        return say(&cx, "tickets-not-set-up", &Args::new()).await;
    }
    if staff.contains(&RoleId::new(ctx.guild_id.get())) {
        return say(&cx, "tickets-everyone-not-staff", &Args::new()).await;
    }
    let user = cx.user().id;
    if let Some(parent) = config.category() {
        ensure_category(ctx, parent).await?;
    }
    // The place is taken first: two presses at once cannot go over the limit.
    let open = cx
        .store()
        .score_add(OPEN, user.get(), 1)
        .await
        .map_err(|e| failed(&e))?;
    if config.max_open > 0 && open > i64::from(config.max_open) && !is_staff(&cx, &config).await {
        give_back(ctx, user.get()).await;
        return say(
            &cx,
            "tickets-limit",
            &Args::new().with("max", i64::from(config.max_open)),
        )
        .await;
    }
    let number = cx
        .store()
        .score_add(NUMBER, 0_u64, 1)
        .await
        .map_err(|e| failed(&e))?;
    let everyone = RoleId::new(ctx.guild_id.get());
    let mut overwrites = vec![
        overwrite(
            OverwriteTarget::Role(everyone),
            Permissions::empty(),
            Permissions::VIEW_CHANNEL,
        ),
        overwrite(
            OverwriteTarget::Member(user),
            TICKET_PERMISSIONS,
            Permissions::empty(),
        ),
    ];
    overwrites.extend(staff.iter().map(|role| {
        overwrite(
            OverwriteTarget::Role(*role),
            TICKET_PERMISSIONS,
            Permissions::empty(),
        )
    }));
    if let Some(bot) = ctx.bot() {
        overwrites.push(overwrite(
            OverwriteTarget::Member(bot),
            TICKET_PERMISSIONS | Permissions::MANAGE_CHANNELS,
            Permissions::empty(),
        ));
    }
    let created = ctx
        .api
        .execute(Action::CreateChannel {
            guild_id: ctx.guild_id,
            spec: NewChannel {
                name: format!("ticket-{number:04}"),
                kind: ChannelKind::Text,
                parent_id: config.category(),
                topic: Some(format!("<@{user}>")),
                nsfw: false,
                slowmode_seconds: 0,
                overwrites,
            },
            reason: Reason::truncated(&cx.t("tickets-reason-open", &Args::new())),
        })
        .await;
    let channel = match created {
        Ok(Outcome::Channel(channel)) => channel.id,
        other => {
            give_back(ctx, user.get()).await;
            return match other {
                Err(error) => Err(failed(&error)),
                _ => Err(UserError::message("tickets-failed")),
            };
        }
    };
    let ticket = Ticket {
        number,
        opener: user.get(),
        claimed_by: None,
        created: cx.now(),
        closing: None,
        close_request: None,
        panel,
        staff_roles: staff.iter().map(|role| role.get()).collect(),
        form_answers: Vec::new(),
    };
    ctx.store()
        .set_for(
            &ticket_key(channel),
            &ticket,
            Duration::from_hours(24 * 365),
        )
        .await
        .map_err(|e| failed(&e))?;
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
                        .with("number", number),
                ),
            ),
            embeds: Vec::new(),
            components: vec![ui::row(vec![
                button(ctx, "claim", ButtonStyle::Secondary, "tickets-claim-button"),
                button(ctx, "close", ButtonStyle::Danger, "tickets-close-button"),
            ])],
            files: Vec::new(),
        })
        .await;
    if let Err(error) = welcome {
        tracing::warn!(?error, guild = %ctx.guild_id, "could not greet in a new ticket");
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
            mentions: MentionPolicy::Roles(staff.clone()),
        })
        .await;
    if config.close_after_days > 0 {
        let _ = watch(
            ctx,
            channel,
            cx.now() + i64::from(config.close_after_days) * DAY,
        )
        .await;
    }
    say(
        &cx,
        "tickets-opened",
        &Args::new().with("channel", format!("<#{channel}>")),
    )
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

/// The time of a Discord id, in Unix seconds.
fn made_at(id: u64) -> i64 {
    i64::try_from(((id >> 22) + 1_420_070_400_000) / 1000).unwrap_or(0)
}

/// A job: the ticket is closed when nobody has written in it for the days of the settings, and
/// looked at again when the time comes otherwise.
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
    if config.close_after_days == 0 {
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
            forget(ctx, channel, &ticket).await?;
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

/// How the closing of a ticket went.
#[derive(Debug, PartialEq, Eq)]
enum Closed {
    Done,
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
    Taken(Ticket),
    Gone,
    Busy(i64),
    RequestChanged,
}

async fn transcript(
    ctx: &EventContext,
    channel: ChannelId,
    ticket: &Ticket,
) -> Result<(String, bool, usize), DiscordError> {
    let mut messages = Vec::new();
    let mut cursor = None;
    let mut cut = false;
    for page_number in 0..TRANSCRIPT_PAGES {
        let page = ctx.api.get_messages(channel, cursor, 100).await?;
        let Some(oldest) = page.last().map(|message| message.id) else {
            break;
        };
        let full = page.len() == 100;
        messages.extend(page);
        if !full {
            break;
        }
        if page_number + 1 == TRANSCRIPT_PAGES {
            cut = true;
        }
        cursor = Some(MessageCursor::Before(oldest));
    }
    messages.reverse();
    let mut text = ctx.t_in(
        ctx.locale,
        "tickets-transcript-head",
        &Args::new()
            .with("number", ticket.number)
            .with("opener", ticket.opener.to_string()),
    );
    text.push('\n');
    if cut {
        text.push_str(&ctx.t_in(
            ctx.locale,
            "tickets-transcript-cut",
            &Args::new().with("count", messages.len() as u64),
        ));
        text.push('\n');
    }
    for message in &messages {
        let attached = message
            .attachments
            .iter()
            .map(|file| file.url.as_str())
            .collect::<Vec<_>>()
            .join(" ");
        let _ = writeln!(
            text,
            "[{}] {} ({}): {} {attached}",
            made_at(message.id.get()),
            message.author.name,
            message.author.id,
            message.content.replace('\n', " ")
        );
    }
    Ok((text, cut, messages.len()))
}

fn history_transcript(
    ctx: &EventContext,
    transcript: &str,
    form_answers: &[(String, String)],
) -> Option<String> {
    if form_answers.is_empty() {
        return None;
    }
    let mut text = transcript.to_owned();
    let _ = writeln!(
        text,
        "\n{}",
        ctx.t_in(ctx.locale, "tickets-transcript-answers-title", &Args::new(),)
    );
    for (question, answer) in form_answers {
        let _ = writeln!(
            text,
            "- {}: {}",
            question.replace('\n', " ").replace('\r', " "),
            answer.replace('\n', " ").replace('\r', " "),
        );
    }
    Some(text)
}

/// How long a close that began is waited for before another may take over.
const CLOSING_WAIT: i64 = 300;

fn history_staff_roles(ticket: &Ticket, config: &Config) -> Vec<u64> {
    let mut roles = if ticket.staff_roles.is_empty() {
        config.staff().into_iter().map(|role| role.get()).collect()
    } else {
        ticket.staff_roles.clone()
    };
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
}

async fn save_history(
    ctx: &EventContext,
    channel: ChannelId,
    ticket: &Ticket,
    config: &Config,
    closer: Option<UserId>,
    reason: &str,
    attempt: i64,
    transcript: Option<(&str, bool, usize)>,
) -> Result<TicketHistory, ModuleError> {
    let (text, transcript_truncated, transcript_messages) = transcript.unwrap_or(("", false, 0));
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
    let transcript_char_count = page_chars.iter().try_fold(0_u64, |total, chars| {
        total
            .checked_add(u64::from(*chars))
            .ok_or_else(|| ModuleError::other("ticket transcript is too long"))
    })?;
    let transcript_messages = u32::try_from(transcript_messages)
        .map_err(|error| ModuleError::other(error.to_string()))?;
    let history = TicketHistory {
        version: 1,
        channel_id: channel.get(),
        number: ticket.number,
        opener: ticket.opener,
        claimed_by: ticket.claimed_by,
        created: ticket.created,
        closed: ctx.now(),
        closer: closer.map(|user| user.get()),
        reason: reason.to_owned(),
        panel: ticket.panel.clone(),
        staff_roles: history_staff_roles(ticket, config),
        status: HistoryStatus::Closing,
        attempt,
        transcript_attempt: attempt,
        transcript_pages,
        transcript_index_pages,
        transcript_char_count,
        transcript_bytes: text.len() as u64,
        transcript_messages,
        transcript_truncated,
    };
    for (page, (start, end)) in pages.iter().enumerate() {
        let page_text = text[*start..*end].to_owned();
        if let Err(error) = ctx
            .store()
            .set_for(
                &history_content_key(channel.get(), attempt, page),
                &page_text,
                Duration::from_hours(24 * 365),
            )
            .await
        {
            remove_history_generation(
                ctx,
                channel.get(),
                attempt,
                transcript_pages,
                transcript_index_pages,
            )
            .await;
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
                Duration::from_hours(24 * 365),
            )
            .await
        {
            remove_history_generation(
                ctx,
                channel.get(),
                attempt,
                transcript_pages,
                transcript_index_pages,
            )
            .await;
            return Err(stored(&error));
        }
    }
    let key = history_key(ticket.number, channel);
    if let Err(error) = ctx
        .store()
        .set_for(
            &history_id_key(channel),
            &key,
            Duration::from_hours(24 * 365),
        )
        .await
    {
        remove_history_generation(
            ctx,
            channel.get(),
            attempt,
            transcript_pages,
            transcript_index_pages,
        )
        .await;
        return Err(stored(&error));
    }
    if let Err(error) = store_history_record(ctx, &key, &history).await {
        remove_history_generation(
            ctx,
            channel.get(),
            attempt,
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
        let current = ctx
            .store()
            .entry(key)
            .await
            .map_err(|error| stored(&error))?;
        let result = match current {
            Some(entry) => {
                let latest: TicketHistory = serde_json::from_value(entry.value)
                    .map_err(|error| ModuleError::other(error.to_string()))?;
                if latest.status == HistoryStatus::Closed || latest.attempt > history.attempt {
                    return Err(ModuleError::other("ticket history was superseded"));
                }
                ctx.store()
                    .replace(
                        key,
                        history,
                        Expect::Revision(entry.revision),
                        Some(Duration::from_hours(24 * 365)),
                    )
                    .await
            }
            None => {
                ctx.store()
                    .replace(
                        key,
                        history,
                        Expect::Missing,
                        Some(Duration::from_hours(24 * 365)),
                    )
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
}

fn may_view_history(roles: &[RoleId], can_manage: bool, history: &TicketHistory) -> bool {
    can_manage
        || roles
            .iter()
            .any(|role| history.staff_roles.contains(&role.get()))
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
                if content.chars().count() != *count as usize {
                    return Err(ModuleError::other("ticket transcript page is invalid"));
                }
                let from = start.saturating_sub(chars_before) as usize;
                let to = (end.min(page_end) - chars_before) as usize;
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
    if text.chars().count() != (end - start) as usize {
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
        if latest.status == HistoryStatus::Closed {
            return Ok(());
        }
        latest.status = HistoryStatus::Closed;
        latest.closed = ctx.now();
        match ctx
            .store()
            .replace(
                &key,
                &latest,
                Expect::Revision(entry.revision),
                Some(Duration::from_hours(24 * 365)),
            )
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
}

/// Takes the ticket for closing, against the revision read: whoever writes first closes it, and a
/// claim that was read before it cannot be written after it.
async fn begin_closing(
    ctx: &EventContext,
    key: &str,
    request_id: Option<i64>,
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
        ticket.closing = Some(ctx.now());
        match ctx
            .store()
            .replace(
                key,
                &ticket,
                Expect::Revision(entry.revision),
                Some(Duration::from_hours(24 * 365)),
            )
            .await
        {
            Ok(_) => return Ok(Claim::Taken(ticket)),
            Err(StorageError::Conflict) => {}
            Err(error) => return Err(stored(&error)),
        }
    }
    Err(ModuleError::other(
        "the ticket changed while it was being closed",
    ))
}

/// Stops tracking a ticket whose channel is gone: its entry, its idle job and its place.
async fn forget(
    ctx: &EventContext,
    channel: ChannelId,
    ticket: &Ticket,
) -> Result<(), ModuleError> {
    if ctx
        .store()
        .delete(&ticket_key(channel))
        .await
        .map_err(|e| stored(&e))?
    {
        give_back(ctx, ticket.opener).await;
    }
    let _ = ctx.scheduler().cancel(&idle_key(channel)).await;
    Ok(())
}

async fn finish(
    ctx: &EventContext,
    channel: ChannelId,
    closer: Option<UserId>,
    reason: &str,
    request_id: Option<i64>,
) -> Result<Closed, ModuleError> {
    let key = ticket_key(channel);
    let ticket = match begin_closing(ctx, &key, request_id).await? {
        Claim::Taken(ticket) => ticket,
        Claim::Gone => return Ok(Closed::Unknown),
        Claim::Busy(until) => return Ok(Closed::BusyUntil(until)),
        Claim::RequestChanged => return Ok(Closed::RequestChanged),
    };
    let config: Config = ctx.parse_config()?;
    let config = config.for_ticket(&ticket);
    let archive_key = history_key(ticket.number, channel);
    let mut previous = ctx
        .store()
        .get::<TicketHistory>(&archive_key)
        .await
        .map_err(|error| stored(&error))?;
    if previous
        .as_ref()
        .is_some_and(|history| history.status == HistoryStatus::Closed)
    {
        match ctx
            .api
            .execute(Action::DeleteChannel {
                channel_id: channel,
                reason: Reason::truncated(reason),
            })
            .await
        {
            Ok(_) | Err(DiscordError::NotFound) => {}
            Err(error) => return Err(stored(&error)),
        }
        forget(ctx, channel, &ticket).await?;
        return Ok(Closed::Done);
    }
    let attempt = ticket.closing.unwrap_or_else(|| ctx.now());
    let history = match transcript(ctx, channel, &ticket).await {
        Ok((text, transcript_truncated, transcript_messages)) => {
            let history_text = history_transcript(ctx, &text, &ticket.form_answers);
            let history_text = history_text.as_deref().unwrap_or(&text);
            let history = match save_history(
                ctx,
                channel,
                &ticket,
                &config,
                closer,
                reason,
                attempt,
                Some((&history_text, transcript_truncated, transcript_messages)),
            )
            .await
            {
                Ok(history) => history,
                Err(error) => {
                    reopen(ctx, &key, attempt).await;
                    return Err(error);
                }
            };
            let current: Option<Ticket> = ctx
                .store()
                .get(&key)
                .await
                .map_err(|error| stored(&error))?;
            let Some(current) = current else {
                return Ok(Closed::Unknown);
            };
            if current.closing != Some(attempt) {
                return Ok(Closed::BusyUntil(
                    current
                        .closing
                        .map_or_else(|| ctx.now(), |since| since + CLOSING_WAIT),
                ));
            }
            if let Some(old) = previous
                .as_ref()
                .filter(|old| old.transcript_attempt != history.transcript_attempt)
            {
                discard_history_generation(ctx, old).await;
            }
            if let Some(log) = config.log() {
                let note = ctx.t_in(
                    ctx.locale,
                    "tickets-closed-log",
                    &Args::new()
                        .with("number", ticket.number)
                        .with("opener", format!("<@{}>", ticket.opener))
                        .with(
                            "closer",
                            closer.map_or_else(|| "-".to_owned(), |user| format!("<@{user}>")),
                        )
                        .with("reason", reason.to_owned()),
                );
                let sent = ctx
                    .api
                    .execute(Action::PostMessage {
                        channel_id: log,
                        message_id: None,
                        reply_to: None,
                        content: Some(note),
                        embeds: Vec::new(),
                        components: Vec::new(),
                        files: vec![FileUpload {
                            filename: format!("ticket-{:04}.txt", ticket.number),
                            data: text.into_bytes(),
                        }],
                    })
                    .await;
                if let Err(error) = sent {
                    reopen(ctx, &key, attempt).await;
                    return Err(stored(&error));
                }
            }
            history
        }
        Err(DiscordError::NotFound) => {
            let Some(mut history) = previous
                .take()
                .filter(|history| history.status == HistoryStatus::Closing)
            else {
                reopen(ctx, &key, attempt).await;
                return Err(stored(&DiscordError::NotFound));
            };
            history.attempt = attempt;
            history.closed = ctx.now();
            store_history_record(ctx, &archive_key, &history).await?;
            history
        }
        Err(error) => {
            reopen(ctx, &key, attempt).await;
            return Err(stored(&error));
        }
    };
    let current: Option<Ticket> = ctx
        .store()
        .get(&key)
        .await
        .map_err(|error| stored(&error))?;
    let Some(current) = current else {
        return Ok(Closed::Unknown);
    };
    if current.closing != Some(attempt) {
        return Ok(Closed::BusyUntil(
            current
                .closing
                .map_or_else(|| ctx.now(), |since| since + CLOSING_WAIT),
        ));
    }
    match ctx
        .api
        .execute(Action::DeleteChannel {
            channel_id: channel,
            reason: Reason::truncated(reason),
        })
        .await
    {
        Ok(_) | Err(DiscordError::NotFound) => {}
        Err(error) => return Err(stored(&error)),
    }
    mark_history_closed(ctx, &history).await?;
    forget(ctx, channel, &ticket).await?;
    Ok(Closed::Done)
}

/// A close that could not finish: the ticket is open again.
async fn reopen(ctx: &EventContext, key: &str, attempt: i64) {
    if let Ok(Some(mut ticket)) = ctx.store().get::<Ticket>(key).await {
        if ticket.closing != Some(attempt) {
            return;
        }
        ticket.closing = None;
        let _ = ctx
            .store()
            .set_for(key, &ticket, Duration::from_hours(24 * 365))
            .await;
    }
}

/// Whether `user` may close the ticket: whoever opened it, or the staff.
fn may_close(ticket: &Ticket, user: UserId, staff: bool) -> bool {
    staff || ticket.opener == user.get()
}

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
    if !may_close(&ticket, cx.user().id, is_staff(&cx, &config).await) {
        return say(&cx, "tickets-not-yours", &Args::new()).await;
    }
    say(&cx, "tickets-closing", &Args::new()).await?;
    let reason = cx.t(
        "tickets-closed-by",
        &Args::new().with("user", cx.user().name.clone()),
    );
    finish(ctx, channel, Some(cx.user().id), &reason, None)
        .await
        .map(drop)
        .map_err(|e| failed(&e))
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
    request: CloseRequest,
) -> Result<(), UserError> {
    cx.reply_private_t("tickets-close-request-processing", &Args::new())
        .await?;
    let reason = request
        .reason
        .unwrap_or_else(|| cx.t("tickets-close-request-approved-reason", &Args::new()));
    let closed = match finish(
        cx.ctx(),
        channel,
        Some(cx.user().id),
        &reason,
        Some(request_id),
    )
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
        Closed::BusyUntil(_) => "tickets-closing",
        Closed::Unknown | Closed::RequestChanged => "tickets-close-request-stale",
    };
    close_request_followup(cx, channel, message).await;
    Ok(())
}

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
    if approve {
        return approve_request(&cx, channel, request_id, request).await;
    }
    if ticket.closing.is_some() {
        return say(&cx, "tickets-closing", &Args::new()).await;
    }
    for _ in 0..5 {
        ticket.close_request = None;
        match ctx
            .store()
            .replace(
                &key,
                &ticket,
                Expect::Revision(revision),
                Some(Duration::from_hours(24 * 365)),
            )
            .await
        {
            Ok(_) => {
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
                if ticket.closing.is_some() {
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

async fn approve_close_request(cx: ComponentCx) -> Result<(), UserError> {
    close_request_decision(cx, true).await
}

async fn reject_close_request(cx: ComponentCx) -> Result<(), UserError> {
    close_request_decision(cx, false).await
}

/// shortcut: searches metadata in the newest 1,000 tickets only; add transcript indexing for message-text search.

async fn search_history(cx: CommandCx, args: SearchHistory) -> Result<(), UserError> {
    let query = args.query.trim().to_lowercase();
    if query.is_empty() {
        return cx
            .reply_private_t("tickets-history-search-empty", &Args::new())
            .await;
    }
    let entries = cx
        .ctx()
        .store()
        .list(HISTORY_PREFIX, HISTORY_LIMIT)
        .await
        .map_err(|error| failed(&error))?;
    let roles = cx.actor().await.roles;
    let can_manage = cx.require("tickets.manage").await.is_ok();
    let mut matches = Vec::new();
    for (_, entry) in entries {
        let history: TicketHistory =
            serde_json::from_value(entry.value).map_err(|error| failed(&error))?;
        if history.version == 1
            && history.status == HistoryStatus::Closed
            && history_matches(&history, &query)
            && may_view_history(&roles, can_manage, &history)
        {
            matches.push(history);
        }
    }
    let shown = matches.len().min(HISTORY_RESULTS);
    if matches.is_empty() {
        return cx
            .reply_private_t(
                "tickets-history-search-none",
                &Args::new().with("limit", HISTORY_LIMIT as u64),
            )
            .await;
    }
    let results = matches
        .iter()
        .take(HISTORY_RESULTS)
        .map(|history| {
            let reason = history.reason.replace('\n', " ").replace('\r', " ");
            let reason = if reason.chars().count() > 32 {
                format!("{}…", reason.chars().take(32).collect::<String>())
            } else {
                reason
            };
            cx.t(
                "tickets-history-search-result",
                &Args::new()
                    .with("number", history.number)
                    .with("opener", format!("<@{}>", history.opener))
                    .with(
                        "closer",
                        history
                            .closer
                            .map_or_else(|| "-".to_owned(), |id| format!("<@{id}>")),
                    )
                    .with("closed", format!("<t:{}:R>", history.closed))
                    .with("reason", reason),
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    cx.reply_private_t(
        "tickets-history-search-results",
        &Args::new()
            .with("limit", HISTORY_LIMIT as u64)
            .with("count", matches.len() as u64)
            .with("shown", shown as u64)
            .with("results", results),
    )
    .await
}

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
    let roles = cx.actor().await.roles;
    let can_manage = cx.require("tickets.manage").await.is_ok();
    if !may_view_history(&roles, can_manage, &history) {
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
    if requested_page <= 0 || requested_page as u64 > total_pages {
        return cx
            .reply_private_t(
                "tickets-history-page-invalid",
                &Args::new().with("pages", total_pages),
            )
            .await;
    }
    let page = requested_page as u64;
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
    if !may_close(&ticket, cx.user().id, is_staff_command(&cx, &config).await) {
        return Err(UserError::message("tickets-not-yours"));
    }
    cx.reply_private_t("tickets-closing", &Args::new()).await?;
    let reason = args
        .reason
        .filter(|text| !text.trim().is_empty())
        .unwrap_or_else(|| {
            cx.t(
                "tickets-closed-by",
                &Args::new().with("user", cx.user().name.clone()),
            )
        });
    finish(ctx, channel, Some(cx.user().id), &reason, None)
        .await
        .map(drop)
        .map_err(|e| failed(&e))
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
    match post_close_request_message(cx, cx.ctx(), channel, opener, request).await {
        Ok(()) => {
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
    if !is_staff_command(&cx, &config).await {
        return cx.reply_private_t("tickets-staff-only", &Args::new()).await;
    }
    if ticket.closing.is_some() {
        return cx
            .reply_private_t("tickets-not-a-ticket", &Args::new())
            .await;
    }
    let reason = args.reason.filter(|text| !text.trim().is_empty());
    if let Some(request) = ticket.close_request.clone() {
        return resend_close_request(&cx, channel, ticket.opener, &request, reason.as_deref())
            .await;
    }
    create_close_request(cx, ticket, revision, reason).await
}

async fn create_close_request(
    cx: CommandCx,
    mut ticket: Ticket,
    mut revision: i64,
    reason: Option<String>,
) -> Result<(), UserError> {
    let ctx = cx.ctx();
    let channel = cx.invocation().channel_id;
    let key = ticket_key(channel);
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
    };
    let mut saved = false;
    for _ in 0..5 {
        ticket.close_request = Some(request.clone());
        match ctx
            .store()
            .replace(
                &key,
                &ticket,
                Expect::Revision(revision),
                Some(Duration::from_hours(24 * 365)),
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
                        return cx
                            .reply_private_t("tickets-not-a-ticket", &Args::new())
                            .await;
                    }
                    Err(error) => {
                        tracing::warn!(?error, channel = %channel, "could not reread a ticket for a close request");
                        return cx.reply_private_t("tickets-failed", &Args::new()).await;
                    }
                };
                let next_revision = next.revision;
                ticket = match serde_json::from_value(next.value) {
                    Ok(ticket) => ticket,
                    Err(error) => {
                        tracing::warn!(?error, channel = %channel, "could not reread a ticket for a close request");
                        return cx.reply_private_t("tickets-failed", &Args::new()).await;
                    }
                };
                if ticket.closing.is_some() {
                    return cx
                        .reply_private_t("tickets-not-a-ticket", &Args::new())
                        .await;
                }
                if let Some(existing) = ticket.close_request.clone() {
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
                return cx.reply_private_t("tickets-failed", &Args::new()).await;
            }
        }
    }
    if !saved {
        return cx.reply_private_t("tickets-failed", &Args::new()).await;
    }
    match post_close_request_message(&cx, ctx, channel, ticket.opener, &request).await {
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
    let config = ticket_config(ctx, &config, channel).await?;
    if !is_staff(&cx, &config).await {
        return say(&cx, "tickets-staff-only", &Args::new()).await;
    }
    // Written against the revision read: two who press at once do not both take it, and a close
    // that began in between wins.
    for _ in 0..5 {
        let Some(entry) = ctx.store().entry(&key).await.map_err(|e| failed(&e))? else {
            return say(&cx, "tickets-not-a-ticket", &Args::new()).await;
        };
        let mut ticket: Ticket =
            serde_json::from_value(entry.value).map_err(|error| failed(&error))?;
        if ticket.closing.is_some() {
            return say(&cx, "tickets-not-a-ticket", &Args::new()).await;
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
        match ctx
            .store()
            .replace(
                &key,
                &ticket,
                Expect::Revision(entry.revision),
                Some(Duration::from_hours(24 * 365)),
            )
            .await
        {
            Ok(_) => {
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

async fn set_claim(cx: CommandCx, claimed_by: Option<UserId>) -> Result<(), UserError> {
    let ctx = cx.ctx();
    let channel = cx.invocation().channel_id;
    let config: Config = cx.config()?;
    let config = ticket_config(ctx, &config, channel).await?;
    if !is_staff_command(&cx, &config).await {
        return Err(UserError::message("tickets-staff-only"));
    }
    let staff = config.staff();
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
        if !member.roles.iter().any(|role| staff.contains(role))
            && !is_guild_owner_or_admin(ctx, user, &member.roles).await?
        {
            return Err(UserError::message("tickets-transfer-not-staff"));
        }
    }
    let key = ticket_key(channel);
    // CAS lets a close that begins during the update win.
    for _ in 0..5 {
        let Some(entry) = ctx.store().entry(&key).await.map_err(|e| failed(&e))? else {
            return Err(UserError::message("tickets-not-a-ticket"));
        };
        let mut ticket: Ticket =
            serde_json::from_value(entry.value).map_err(|error| failed(&error))?;
        if ticket.closing.is_some() {
            return Err(UserError::message("tickets-not-a-ticket"));
        }
        if ticket.claimed_by.is_none() {
            return Err(UserError::message("tickets-not-assigned"));
        }
        ticket.claimed_by = claimed_by.map(twilight_model::id::Id::get);
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
                Some(Duration::from_hours(24 * 365)),
            )
            .await
        {
            Ok(_) => {
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

/// Lets a member into the ticket, or takes them out of it.
async fn change_member(cx: CommandCx, user: UserId, add: bool) -> Result<(), UserError> {
    let channel = cx.invocation().channel_id;
    let ctx = cx.ctx();
    let ticket = ctx
        .store()
        .get::<Ticket>(&ticket_key(channel))
        .await
        .map_err(|e| failed(&e))?
        .ok_or_else(|| UserError::message("tickets-not-a-ticket"))?;
    let config: Config = cx.config()?;
    let config = config.for_ticket(&ticket);
    if !is_staff_command(&cx, &config).await {
        return Err(UserError::message("tickets-staff-only"));
    }
    if ticket.closing.is_some() {
        return Err(UserError::message("tickets-not-a-ticket"));
    }
    let reason = Reason::truncated(&cx.t("tickets-reason-member", &Args::new()));
    let action = if add {
        Action::SetPermissionOverwrite {
            channel_id: channel,
            overwrite: overwrite(
                OverwriteTarget::Member(user),
                TICKET_PERMISSIONS,
                Permissions::empty(),
            ),
            reason,
        }
    } else {
        Action::DeletePermissionOverwrite {
            channel_id: channel,
            target: OverwriteTarget::Member(user),
            reason,
        }
    };
    ctx.api.execute(action).await.map_err(|e| failed(&e))?;
    cx.reply_private_t(
        if add {
            "tickets-added"
        } else {
            "tickets-removed"
        },
        &Args::new().with("user", format!("<@{user}>")),
    )
    .await
}

async fn add(cx: CommandCx, args: Member) -> Result<(), UserError> {
    change_member(cx, args.user, true).await
}

async fn remove(cx: CommandCx, args: Member) -> Result<(), UserError> {
    change_member(cx, args.user, false).await
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
    let config: Config = cx.config()?;
    let config = ticket_config(ctx, &config, channel).await?;
    if !is_staff_command(&cx, &config).await {
        return Err(UserError::message("tickets-staff-only"));
    }
    let Some(ticket) = ctx
        .store()
        .get::<Ticket>(&ticket_key(channel))
        .await
        .map_err(|error| failed(&error))?
    else {
        return Err(UserError::message("tickets-not-a-ticket"));
    };
    if ticket.closing.is_some() {
        return Err(UserError::message("tickets-not-a-ticket"));
    }
    let action = Action::EditChannel {
        channel_id: channel,
        patch: ChannelPatch {
            name: Some(args.name.clone()),
            ..ChannelPatch::default()
        },
        reason: Reason::truncated(&cx.t("tickets-reason-rename", &Args::new())),
    };
    action
        .validate()
        .map_err(|_| UserError::message("tickets-invalid-name"))?;
    ctx.api
        .execute(action)
        .await
        .map_err(|error| failed(&error))?;
    cx.reply_private_t("tickets-renamed", &Args::new().with("name", args.name))
        .await
}

fn schema() -> serde_json::Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "category": {
                "type": ["string", "null"],
                "pattern": "^[0-9]{1,20}$",
                "description": "config-tickets-category",
                "x-ui": { "level": "basic", "widget": "channel", "channel_kinds": ["category"] }
            },
            "staff_roles": {
                "type": "array",
                "maxItems": 20,
                "items": { "type": "string", "pattern": "^[0-9]{1,20}$" },
                "description": "config-tickets-staff_roles",
                "x-ui": { "level": "basic" }
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
            "panels": {
                "type": "array",
                "description": "config-tickets-panels",
                "x-ui": { "level": "basic" },
                "items": {
                    "type": "object",
                    "additionalProperties": false,
                    "required": ["id"],
                    "properties": {
                        "id": { "type": "string", "pattern": "^[a-zA-Z0-9_-]{1,32}$", "description": "config-tickets-panel-id" },
                        "category": { "anyOf": [{ "type": "string", "pattern": "^[0-9]{1,20}$" }, { "type": "null" }], "description": "config-tickets-category", "x-ui": { "widget": "channel", "channel_kinds": ["category"] } },
                        "staff_roles": { "type": "array", "maxItems": 20, "items": { "type": "string", "pattern": "^[0-9]{1,20}$" }, "description": "config-tickets-panel-staff" },
                        "log_channel": { "anyOf": [{ "type": "string", "pattern": "^[0-9]{1,20}$" }, { "type": "null" }], "description": "config-tickets-log_channel", "x-ui": { "widget": "channel" } },
                        "close_after_days": { "type": "integer", "minimum": 0, "maximum": 365, "description": "config-tickets-close_after_days" }
                    }
                }
            }
        }
    })
}

/// # Errors
/// The module declaration is refused by the SDK.
pub fn builder() -> Result<ModuleBuilder, CoreError> {
    let schema = schema();
    Ok(
        ModuleBuilder::new("tickets", "Tickets", env!("CARGO_PKG_VERSION"))?
            .capability(closure_sdk::Capability::Storage)
            .capability(closure_sdk::Capability::Schedule)
            .capability(closure_sdk::Capability::SendMessages)
            .capability(closure_sdk::Capability::ReadMessages)
            .capability(closure_sdk::Capability::ReadGuild)
            .capability(closure_sdk::Capability::ManageChannels)
            .capability(closure_sdk::Capability::RespondToInteractions)
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
                    "staff_roles": [],
                    "log_channel": null,
                    "max_open": 1,
                    "close_after_days": 0,
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
                    Command::<ViewHistory>::new("history").handler(view_history),
                    Command::<Close>::new("close")
                        .requires("tickets.use")
                        .handler(close_command),
                    Command::<CloseRequestArgs>::new("close-request").handler(close_request),
                    Command::<Member>::new("add").handler(add),
                    Command::<Member>::new("remove").handler(remove),
                    Command::<Unclaim>::new("unclaim").handler(unclaim),
                    Command::<Member>::new("transfer").handler(transfer),
                    Command::<Rename>::new("rename").handler(rename),
                ],
            )
            .component("open", open)
            .component("close", close_button)
            .component("close-request-approve", approve_close_request)
            .component("close-request-reject", reject_close_request)
            .component("claim", claim)
            .job(IDLE, idle_job)
            .catalog(Locale::En, EN)
            .catalog(Locale::Ja, JA),
    )
}

/// # Errors
/// The module declaration is refused by the SDK.
pub fn module() -> Result<SdkModule, CoreError> {
    builder()?.build()
}

#[cfg(test)]
mod tests;
