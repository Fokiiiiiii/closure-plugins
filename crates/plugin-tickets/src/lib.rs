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
const NOTES_PER_PAGE: usize = 8;
const NOTE_MAX_CHARS: usize = 300;
const NOTE_PAGE_LIMIT: usize = 1_000;
const NOTE_RETENTION: Duration = Duration::from_hours(24 * 365);
const STATS_LIMIT: usize = 1_000;
/// How many messages a transcript holds at most: 100 for each of 50 pages.
const TRANSCRIPT_PAGES: usize = 50;
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
    /// The channel the transcripts go to. Without one no transcript is kept.
    log_channel: Option<String>,
    max_open: u32,
    /// A ticket nobody writes in for this many days is closed; 0 never closes one.
    close_after_days: u32,
    panels: Vec<NamedPanel>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
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
    notes: Vec<StaffNote>,
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

fn ticket_key(channel: ChannelId) -> String {
    format!("ticket:{}", channel.get())
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
        if ticket.closing.is_some() {
            closing += 1;
        } else {
            open += 1;
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

/// The messages of the channel, the oldest first, as text.
async fn transcript(
    ctx: &EventContext,
    channel: ChannelId,
    ticket: &Ticket,
) -> Result<String, ModuleError> {
    let mut messages = Vec::new();
    let mut cursor = None;
    let mut cut = false;
    for page_number in 0..TRANSCRIPT_PAGES {
        let page = ctx
            .api
            .get_messages(channel, cursor, 100)
            .await
            .map_err(|e| stored(&e))?;
        let Some(oldest) = page.last().map(|message| message.id) else {
            break;
        };
        let full = page.len() == 100;
        messages.extend(page);
        if !full {
            break;
        }
        if page_number + 1 == TRANSCRIPT_PAGES {
            // The limit was reached with more behind it: the file says so.
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
    Ok(text)
}

/// How long a close that began is waited for before another may take over.
const CLOSING_WAIT: i64 = 300;

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

/// Saves the transcript, then deletes the channel; a failed deletion is tried again later.
/// A transcript that cannot be sent keeps the ticket open.
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
    let file = match config.log() {
        Some(log) => match transcript(ctx, channel, &ticket).await {
            Ok(text) => Some((log, text)),
            Err(error) => {
                reopen(ctx, &key).await;
                return Err(error);
            }
        },
        None => None,
    };
    if let Some((log, text)) = file {
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
            // The transcript is not lost: the ticket stays open.
            reopen(ctx, &key).await;
            return Err(stored(&error));
        }
    }
    match ctx
        .api
        .execute(Action::DeleteChannel {
            channel_id: channel,
            reason: Reason::truncated(reason),
        })
        .await
    {
        Ok(_) | Err(closure_core::error::DiscordError::NotFound) => {}
        // The entry is kept (marked as closing): the close is tried again.
        Err(error) => return Err(stored(&error)),
    }
    forget(ctx, channel, &ticket).await?;
    Ok(Closed::Done)
}

/// A close that could not finish: the ticket is open again.
async fn reopen(ctx: &EventContext, key: &str) {
    if let Ok(Some(mut ticket)) = ctx.store().get::<Ticket>(key).await {
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
    if !is_staff_command(cx, &config.for_ticket(&ticket)).await {
        return Err(UserError::message("tickets-staff-only"));
    }
    if ticket.closing.is_some() {
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
    if !is_staff_command(cx, &access_config).await {
        return Err(UserError::message("tickets-staff-only"));
    }
    if live_ticket.as_ref().is_some_and(|ticket| {
        ticket.panel.as_ref() != page.panel.as_ref() || ticket.number != page.ticket_number
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
        NoteSave::Saved => cx.reply_private_t("tickets-note-saved", &Args::new()).await,
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
                    Command::<Close>::new("close")
                        .requires("tickets.use")
                        .handler(close_command),
                    Command::<CloseRequestArgs>::new("close-request").handler(close_request),
                    Command::<Member>::new("add").handler(add),
                    Command::<Member>::new("remove").handler(remove),
                    Command::<Unclaim>::new("unclaim").handler(unclaim),
                    Command::<Member>::new("transfer").handler(transfer),
                    Command::<Rename>::new("rename").handler(rename),
                    Command::<StatsArgs>::new("stats").handler(stats_command),
                    Command::<NoteArgs>::new("note").handler(add_note),
                    Command::<NotesArgs>::new("notes").handler(list_notes),
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
