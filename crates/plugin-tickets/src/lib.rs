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
    Action, ChannelKind, FileUpload, MentionPolicy, NewChannel, Outcome, OverwriteSpec,
    OverwriteTarget, Reason,
};
use closure_core::error::ModuleError;
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

#[derive(Debug, Deserialize)]
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
}

impl Default for Config {
    fn default() -> Self {
        Self {
            category: None,
            staff_roles: Vec::new(),
            log_channel: None,
            max_open: 1,
            close_after_days: 0,
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
}

#[derive(Serialize, Deserialize)]
struct Idle {
    channel: u64,
}

#[derive(CommandArgs)]
struct Panel {
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
struct Member {
    /// The member
    user: UserId,
}

fn ticket_key(channel: ChannelId) -> String {
    format!("ticket:{}", channel.get())
}

fn idle_key(channel: ChannelId) -> String {
    format!("idle-{}", channel.get())
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

fn overwrite(target: OverwriteTarget, allow: Permissions, deny: Permissions) -> OverwriteSpec {
    OverwriteSpec {
        target,
        allow,
        deny,
    }
}

async fn panel(cx: CommandCx, args: Panel) -> Result<(), UserError> {
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
            components: vec![ui::row(vec![button(
                ctx,
                "open",
                ButtonStyle::Primary,
                "tickets-open-button",
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
    let config: Config = cx.config()?;
    let staff = config.staff();
    if staff.is_empty() {
        return say(&cx, "tickets-not-set-up", &Args::new()).await;
    }
    let ctx = cx.ctx();
    let user = cx.user().id;
    // The place is taken first: two presses at once cannot go over the limit.
    let open = cx
        .store()
        .score_add(OPEN, user.get(), 1)
        .await
        .map_err(|e| failed(&e))?;
    if open > i64::from(config.max_open) {
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
        finish(ctx, channel, None, &reason).await?;
    } else {
        watch(ctx, channel, due).await?;
    }
    Ok(())
}

/// How the closing of a ticket went.
#[derive(Debug, PartialEq, Eq)]
enum Closed {
    Done,
    /// No such ticket, or somebody else is closing it.
    Unknown,
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
/// claim that was read before it cannot be written after it. `false` when somebody else is closing
/// it, or it is gone.
async fn begin_closing(ctx: &EventContext, key: &str) -> Result<Option<Ticket>, ModuleError> {
    for _ in 0..5 {
        let Some(entry) = ctx.store().entry(key).await.map_err(|e| stored(&e))? else {
            return Ok(None);
        };
        let mut ticket: Ticket = serde_json::from_value(entry.value)
            .map_err(|error| ModuleError::other(error.to_string()))?;
        if ticket
            .closing
            .is_some_and(|since| ctx.now() - since < CLOSING_WAIT)
        {
            return Ok(None);
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
            Ok(_) => return Ok(Some(ticket)),
            Err(StorageError::Conflict) => {}
            Err(error) => return Err(stored(&error)),
        }
    }
    Ok(None)
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
) -> Result<Closed, ModuleError> {
    let key = ticket_key(channel);
    let Some(ticket) = begin_closing(ctx, &key).await? else {
        return Ok(Closed::Unknown);
    };
    let config: Config = ctx.parse_config()?;
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
    if !may_close(&ticket, cx.user().id, is_staff(&cx, &config).await) {
        return say(&cx, "tickets-not-yours", &Args::new()).await;
    }
    say(&cx, "tickets-closing", &Args::new()).await?;
    let reason = cx.t(
        "tickets-closed-by",
        &Args::new().with("user", cx.user().name.clone()),
    );
    finish(ctx, channel, Some(cx.user().id), &reason)
        .await
        .map(drop)
        .map_err(|e| failed(&e))
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
    finish(ctx, channel, Some(cx.user().id), &reason)
        .await
        .map(drop)
        .map_err(|e| failed(&e))
}

async fn claim(cx: ComponentCx) -> Result<(), UserError> {
    let channel = cx.click().channel_id;
    let ctx = cx.ctx();
    let key = ticket_key(channel);
    let config: Config = cx.config()?;
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

/// Lets a member into the ticket, or takes them out of it.
async fn change_member(cx: CommandCx, user: UserId, add: bool) -> Result<(), UserError> {
    let channel = cx.invocation().channel_id;
    let ctx = cx.ctx();
    if ctx
        .store()
        .get::<Ticket>(&ticket_key(channel))
        .await
        .map_err(|e| failed(&e))?
        .is_none()
    {
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

/// # Errors
/// The module declaration is refused by the SDK.
pub fn builder() -> Result<ModuleBuilder, CoreError> {
    let schema = json!({
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "category": {
                "type": ["string", "null"],
                "pattern": "^[0-9]{1,20}$",
                "description": "config-tickets-category",
                "x-ui": { "level": "basic", "widget": "channel" }
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
                "minimum": 1,
                "maximum": 20,
                "description": "config-tickets-max_open",
                "x-ui": { "level": "basic" }
            },
            "close_after_days": {
                "type": "integer",
                "minimum": 0,
                "maximum": 365,
                "description": "config-tickets-close_after_days",
                "x-ui": { "level": "basic" }
            }
        }
    });
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
                    "close_after_days": 0
                }),
            )
            .group(
                "ticket",
                [
                    Command::<Panel>::new("panel")
                        .requires("tickets.manage")
                        .handler(panel),
                    Command::<Close>::new("close")
                        .requires("tickets.use")
                        .handler(close_command),
                    Command::<Member>::new("add")
                        .requires("tickets.manage")
                        .handler(add),
                    Command::<Member>::new("remove")
                        .requires("tickets.manage")
                        .handler(remove),
                ],
            )
            .component("open", open)
            .component("close", close_button)
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
