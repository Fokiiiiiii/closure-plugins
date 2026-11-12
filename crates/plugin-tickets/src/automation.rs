//! Per-ticket waiting and automatic-close timers.

use super::*;
use closure_core::action::{Action, ChannelPatch, Outcome, Reason};
use closure_core::error::DiscordError;
use closure_core::permission::{Actor, PermissionKey};
use closure_core::storage::Expect;
use closure_sdk::JobCx;
use serde::{Deserialize, Serialize};
use twilight_model::channel::ChannelType;
use twilight_model::guild::Permissions;

const HOUR: i64 = 3_600;
pub(super) const AUTOMATION: &str = "ticket-automation";

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub(super) struct State {
    #[serde(default)]
    pub(super) generation: u64,
    #[serde(default)]
    pub(super) latest_message_id: u64,
    #[serde(default)]
    pub(super) first_response_generation: u64,
    #[serde(default)]
    pub(super) first_response_replied: bool,
    #[serde(default)]
    pub(super) started_close_generation: Option<i64>,
    #[serde(default)]
    pub(super) first_response_since: Option<i64>,
    #[serde(default)]
    pub(super) last_resolved_staff_check_id: u64,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum Kind {
    FirstResponse,
    LastMessage,
    Waiting,
    Restore,
    StaffCheck,
}

impl Kind {
    const fn key(self) -> &'static str {
        match self {
            Self::FirstResponse => "first-response",
            Self::LastMessage => "last-message",
            Self::Waiting => "waiting",
            Self::Restore => "restore",
            Self::StaffCheck => "staff-check",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct PendingStaffCheck {
    message_id: u64,
    author: UserId,
    roles: Vec<RoleId>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Timer {
    channel: u64,
    close_generation: i64,
    generation: u64,
    first_response_generation: u64,
    since: i64,
    kind: Kind,
    #[serde(default)]
    staff_check: Option<PendingStaffCheck>,
}

fn timer_key(channel: ChannelId, kind: Kind) -> String {
    format!("ticket-automation-{}-{}", channel.get(), kind.key())
}

fn pending_staff_prefix(channel: ChannelId, close_generation: i64) -> String {
    format!(
        "ticket-automation-pending-{}-{close_generation}-",
        channel.get()
    )
}

fn pending_staff_key(channel: ChannelId, close_generation: i64, message_id: u64) -> String {
    format!(
        "{}{message_id:020}",
        pending_staff_prefix(channel, close_generation)
    )
}

fn staff_check_key(channel: ChannelId, close_generation: i64, message_id: u64) -> String {
    pending_staff_key(channel, close_generation, message_id)
}

async fn delete_pending_staff_check(
    ctx: &EventContext,
    channel: ChannelId,
    close_generation: i64,
    message_id: u64,
) -> Result<(), ModuleError> {
    ctx.store()
        .delete(&pending_staff_key(channel, close_generation, message_id))
        .await
        .map(drop)
        .map_err(|error| stored(&error))
}

fn due(since: i64, hours: u32) -> i64 {
    since + i64::from(hours) * HOUR
}

async fn schedule(
    ctx: &EventContext,
    channel: ChannelId,
    ticket: &Ticket,
    kind: Kind,
    since: i64,
    at: i64,
) -> Result<(), ModuleError> {
    let timer = Timer {
        channel: channel.get(),
        close_generation: ticket.close_generation,
        generation: ticket.automation.generation,
        first_response_generation: ticket.automation.first_response_generation,
        since,
        kind,
        staff_check: None,
    };
    ctx.scheduler()
        .job(AUTOMATION, &timer)
        .map_err(|error| stored(&error))?
        .key(&timer_key(channel, kind))
        .at(at.max(ctx.now()))
        .await
        .map_err(|error| stored(&error))
}

async fn schedule_staff_check(
    ctx: &EventContext,
    channel: ChannelId,
    ticket: &Ticket,
    check: &PendingStaffCheck,
    at: i64,
) -> Result<(), ModuleError> {
    let timer = Timer {
        channel: channel.get(),
        close_generation: ticket.close_generation,
        generation: ticket.automation.generation,
        first_response_generation: ticket.automation.first_response_generation,
        since: made_at(check.message_id),
        kind: Kind::StaffCheck,
        staff_check: Some(check.clone()),
    };
    ctx.scheduler()
        .job(AUTOMATION, &timer)
        .map_err(|error| stored(&error))?
        .key(&staff_check_key(
            channel,
            ticket.close_generation,
            check.message_id,
        ))
        .at(at.max(ctx.now()))
        .await
        .map_err(|error| stored(&error))
}

async fn schedule_pending_staff_checks(
    ctx: &EventContext,
    channel: ChannelId,
    ticket: &Ticket,
) -> Result<bool, ModuleError> {
    let prefix = pending_staff_prefix(channel, ticket.close_generation);
    let mut after: Option<String> = None;
    let mut found = false;
    loop {
        let page = match after.as_deref() {
            Some(after) => ctx.store().list_after(&prefix, after, 1_000).await,
            None => ctx.store().list(&prefix, 1_000).await,
        }
        .map_err(|error| stored(&error))?;
        if page.is_empty() {
            break;
        }
        let page_len = page.len();
        let mut last_key = None;
        for (key, entry) in page {
            last_key = Some(key.clone());
            let check: PendingStaffCheck = serde_json::from_value(entry.value)
                .map_err(|error| ModuleError::other(error.to_string()))?;
            if check.message_id <= ticket.automation.last_resolved_staff_check_id {
                ctx.store()
                    .delete(&key)
                    .await
                    .map_err(|error| stored(&error))?;
                continue;
            }
            found = true;
            schedule_staff_check(ctx, channel, ticket, &check, ctx.now()).await?;
        }
        if page_len < 1_000 {
            break;
        }
        after = last_key;
    }
    Ok(found)
}

/// Start timers for a newly opened ticket, or reset them after a ticket is reopened.
pub(super) async fn start(ctx: &EventContext, channel: ChannelId) -> Result<(), ModuleError> {
    let key = ticket_key(channel);
    let mut current = None;
    for _ in 0..5 {
        let Some(entry) = ctx
            .store()
            .entry(&key)
            .await
            .map_err(|error| stored(&error))?
        else {
            return Ok(());
        };
        let mut ticket: Ticket = serde_json::from_value(entry.value)
            .map_err(|error| ModuleError::other(error.to_string()))?;
        if !ticket.is_open() {
            return Ok(());
        }
        if ticket.automation.started_close_generation != Some(ticket.close_generation)
            || ticket.automation.first_response_since.is_none()
        {
            ticket.automation.generation =
                ticket.automation.generation.checked_add(1).ok_or_else(|| {
                    ModuleError::other("ticket automation generation is exhausted")
                })?;
            ticket.automation.latest_message_id = 0;
            ticket.automation.first_response_replied = false;
            ticket.automation.started_close_generation = Some(ticket.close_generation);
            ticket.automation.first_response_since = Some(ctx.now());
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
                Ok(_) => {
                    current = Some(ticket);
                    break;
                }
                Err(StorageError::Conflict) => continue,
                Err(error) => return Err(stored(&error)),
            }
        }
        current = Some(ticket);
        break;
    }
    let ticket = current
        .ok_or_else(|| ModuleError::other("ticket changed repeatedly while starting automation"))?;
    let config: Config = ctx.parse_config()?;
    let config = config.for_ticket(&ticket);
    let since = ticket
        .automation
        .first_response_since
        .unwrap_or(ticket.created);

    if config.first_response_timeout_hours > 0 && !ticket.automation.first_response_replied {
        schedule(
            ctx,
            channel,
            &ticket,
            Kind::FirstResponse,
            since,
            due(since, config.first_response_timeout_hours),
        )
        .await?;
    }

    if config.last_message_timeout_hours > 0 {
        let activity_since = latest_activity(&ticket, since);
        schedule(
            ctx,
            channel,
            &ticket,
            Kind::LastMessage,
            activity_since,
            due(activity_since, config.last_message_timeout_hours),
        )
        .await?;
    }

    if !ticket.thread
        && ticket.waiting_for_reply
        && config.waiting_after_hours > 0
        && waiting_category(&config).is_some()
    {
        let activity_since = latest_activity(&ticket, since);
        schedule(
            ctx,
            channel,
            &ticket,
            Kind::Waiting,
            activity_since,
            due(activity_since, config.waiting_after_hours),
        )
        .await?;
    }
    schedule_pending_staff_checks(ctx, channel, &ticket).await?;
    Ok(())
}

fn latest_activity(ticket: &Ticket, fallback: i64) -> i64 {
    if ticket.automation.latest_message_id > 0 {
        made_at(ticket.automation.latest_message_id)
    } else {
        fallback
    }
}

fn waiting_category(config: &Config) -> Option<ChannelId> {
    id_of(config.waiting_category.as_ref()).map(ChannelId::new)
}

async fn roles_for_user(
    ctx: &EventContext,
    user: UserId,
    roles: &[RoleId],
) -> Result<Vec<RoleId>, ModuleError> {
    if !roles.is_empty() {
        return Ok(roles.to_vec());
    }
    match ctx
        .api
        .execute(Action::GetMember {
            guild_id: ctx.guild_id,
            user_id: user,
        })
        .await
    {
        Ok(Outcome::Member(member)) => Ok(member.roles),
        Ok(Outcome::None) | Err(DiscordError::NotFound) => Ok(Vec::new()),
        Ok(_) => Err(ModuleError::other(
            "unexpected result getting ticket message author",
        )),
        Err(error) => Err(error.into()),
    }
}

async fn may_manage_ticket(
    ctx: &EventContext,
    user: UserId,
    roles: &[RoleId],
) -> Result<bool, ModuleError> {
    let key = PermissionKey::new("tickets.manage")
        .map_err(|error| ModuleError::other(error.to_string()))?;
    let mut actor = Actor {
        user,
        roles: roles.to_vec(),
        administrator: false,
        owner: false,
    };
    if ctx.permissions().check(&actor, &key).allowed {
        return Ok(true);
    }
    actor.owner = match ctx
        .api
        .guild_owner()
        .await
        .map_err(|error| ModuleError::from(error))?
    {
        Some(owner) => owner == user,
        None => {
            ctx.api
                .get_guild(ctx.guild_id)
                .await
                .map_err(|error| ModuleError::from(error))?
                .owner_id
                == user
        }
    };
    if actor.owner {
        return Ok(ctx.permissions().check(&actor, &key).allowed);
    }
    let guild_roles = match ctx
        .api
        .execute(Action::ListRoles {
            guild_id: ctx.guild_id,
        })
        .await
    {
        Ok(Outcome::Roles(roles)) => roles,
        Ok(_) => return Err(ModuleError::other("unexpected result listing guild roles")),
        Err(error) => return Err(error.into()),
    };
    actor.administrator = guild_roles.iter().any(|role| {
        roles.contains(&role.id) && role.permissions.contains(Permissions::ADMINISTRATOR)
    });
    Ok(ctx.permissions().check(&actor, &key).allowed)
}

/// Record human ticket messages and refresh timers. Message content is not needed.
pub(super) async fn on_message(ctx: EventContext, event: Event) -> Result<Flow, ModuleError> {
    let Event::MessageCreated(message) = event else {
        return Ok(Flow::Continue);
    };
    if message.author.bot || message.from_webhook || message.guild_id != ctx.guild_id {
        return Ok(Flow::Continue);
    }

    let channel = message.channel_id;
    let key = ticket_key(channel);
    let message_id = message.id.get();
    let message_at = made_at(message_id);

    for _ in 0..5 {
        let Some(entry) = ctx
            .store()
            .entry(&key)
            .await
            .map_err(|error| stored(&error))?
        else {
            return Ok(Flow::Continue);
        };
        let mut ticket: Ticket = serde_json::from_value(entry.value)
            .map_err(|error| ModuleError::other(error.to_string()))?;
        if !ticket.is_open() {
            return Ok(Flow::Continue);
        }
        if ticket.automation.started_close_generation != Some(ticket.close_generation) {
            if message_id <= ticket.automation.latest_message_id {
                return Ok(Flow::Continue);
            }
            let first_cycle = ticket.automation.started_close_generation.is_none();
            ticket.automation.generation =
                ticket.automation.generation.checked_add(1).ok_or_else(|| {
                    ModuleError::other("ticket automation generation is exhausted")
                })?;
            ticket.automation.latest_message_id = 0;
            ticket.automation.first_response_replied = false;
            ticket.automation.started_close_generation = Some(ticket.close_generation);
            ticket.automation.first_response_since = Some(if first_cycle {
                ticket.created
            } else {
                ctx.now()
            });
        }
        if message_id < ticket.automation.latest_message_id {
            return Ok(Flow::Continue);
        }
        let config: Config = ctx.parse_config()?;
        let config = config.for_ticket(&ticket);
        let opener = message.author.id.get() == ticket.opener;
        if message_id == ticket.automation.latest_message_id {
            schedule_after_message(&ctx, channel, &ticket, &config, message_at).await?;
            return Ok(Flow::Continue);
        }

        let staff_roles = config.staff();
        let staff = !opener
            && message
                .author_roles
                .iter()
                .any(|role| staff_roles.contains(role));
        let permission_pending = !opener && !staff;

        ticket.automation.generation = ticket
            .automation
            .generation
            .checked_add(1)
            .ok_or_else(|| ModuleError::other("ticket automation generation is exhausted"))?;
        ticket.automation.latest_message_id = message_id;
        if staff && !ticket.automation.first_response_replied {
            ticket.automation.first_response_generation = ticket
                .automation
                .first_response_generation
                .checked_add(1)
                .ok_or_else(|| {
                    ModuleError::other("ticket first response generation is exhausted")
                })?;
            ticket.automation.first_response_replied = true;
        }
        if opener {
            ticket.waiting_for_reply = false;
        } else if staff {
            ticket.waiting_for_reply = true;
        }
        if permission_pending {
            let check = PendingStaffCheck {
                message_id,
                author: message.author.id,
                roles,
            };
            ctx.store()
                .set_for(
                    &pending_staff_key(channel, ticket.close_generation, message_id),
                    &check,
                    ticket_ttl(&ticket, ctx.now()),
                )
                .await
                .map_err(|error| stored(&error))?;
            schedule_staff_check(&ctx, channel, &ticket, &check, ctx.now()).await?;
        }
        refresh_ticket_form_answer_ttl(&ctx, &ticket).await?;
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
                schedule_after_message(&ctx, channel, &ticket, &config, message_at).await?;
                return Ok(Flow::Continue);
            }
            Err(StorageError::Conflict) => continue,
            Err(error) => return Err(stored(&error)),
        }
    }
    Err(ModuleError::other(
        "ticket changed repeatedly while recording a message",
    ))
}

async fn schedule_after_message(
    ctx: &EventContext,
    channel: ChannelId,
    ticket: &Ticket,
    config: &Config,
    message_at: i64,
) -> Result<(), ModuleError> {
    if config.first_response_timeout_hours > 0 && !ticket.automation.first_response_replied {
        let since = ticket
            .automation
            .first_response_since
            .unwrap_or(ticket.created);
        schedule(
            ctx,
            channel,
            ticket,
            Kind::FirstResponse,
            since,
            due(since, config.first_response_timeout_hours),
        )
        .await?;
    }

    if config.last_message_timeout_hours > 0 {
        schedule(
            ctx,
            channel,
            ticket,
            Kind::LastMessage,
            message_at,
            due(message_at, config.last_message_timeout_hours),
        )
        .await?;
    }

    if !ticket.thread && ticket.waiting_for_reply {
        if config.waiting_after_hours > 0 && waiting_category(config).is_some() {
            schedule(
                ctx,
                channel,
                ticket,
                Kind::Waiting,
                message_at,
                due(message_at, config.waiting_after_hours),
            )
            .await?;
        }
    } else if !ticket.thread {
        schedule(ctx, channel, ticket, Kind::Restore, message_at, ctx.now()).await?;
    }
    let pending = schedule_pending_staff_checks(ctx, channel, ticket).await?;
    if pending && (ticket.thread || ticket.waiting_for_reply) {
        schedule(
            ctx,
            channel,
            ticket,
            Kind::Restore,
            message_at,
            ctx.now() + CLOSING_WAIT,
        )
        .await?;
    }
    Ok(())
}

/// Run a persisted timer. Its ticket generations fence obsolete jobs after newer messages or closes.
pub(super) async fn job(cx: JobCx) -> Result<(), ModuleError> {
    let timer: Timer = cx.payload()?;
    let ctx = cx.ctx();
    let channel = ChannelId::new(timer.channel);
    let Some(ticket) = ctx
        .store()
        .get::<Ticket>(&ticket_key(channel))
        .await
        .map_err(|error| stored(&error))?
    else {
        if let Some(check) = &timer.staff_check {
            delete_pending_staff_check(&ctx, channel, timer.close_generation, check.message_id)
                .await?;
        }
        return Ok(());
    };
    if !ticket.is_open() || ticket.close_generation != timer.close_generation {
        if let Some(check) = &timer.staff_check {
            delete_pending_staff_check(&ctx, channel, timer.close_generation, check.message_id)
                .await?;
        }
        return Ok(());
    }

    let config: Config = ctx.parse_config()?;
    let config = config.for_ticket(&ticket);
    match timer.kind {
        Kind::FirstResponse => {
            if ticket.automation.first_response_replied
                || ticket.automation.first_response_generation != timer.first_response_generation
                || config.first_response_timeout_hours == 0
            {
                return Ok(());
            }
            let since = ticket
                .automation
                .first_response_since
                .unwrap_or(timer.since);
            if schedule_pending_staff_checks(&ctx, channel, &ticket).await? {
                return schedule(
                    &ctx,
                    channel,
                    &ticket,
                    Kind::FirstResponse,
                    since,
                    ctx.now() + CLOSING_WAIT,
                )
                .await;
            }
            if ticket.automation.generation != timer.generation {
                return schedule(
                    &ctx,
                    channel,
                    &ticket,
                    Kind::FirstResponse,
                    since,
                    due(since, config.first_response_timeout_hours),
                )
                .await;
            }
            let at = due(since, config.first_response_timeout_hours);
            if ctx.now() < at {
                return schedule(&ctx, channel, &ticket, Kind::FirstResponse, since, at).await;
            }
            auto_close(
                &ctx,
                channel,
                &ticket,
                Kind::FirstResponse,
                timer.generation,
                since,
                "tickets-first-response-timeout-reason",
            )
            .await
        }
        Kind::LastMessage => {
            if config.last_message_timeout_hours == 0 {
                return Ok(());
            }
            if schedule_pending_staff_checks(&ctx, channel, &ticket).await? {
                return schedule(
                    &ctx,
                    channel,
                    &ticket,
                    Kind::LastMessage,
                    timer.since,
                    ctx.now() + CLOSING_WAIT,
                )
                .await;
            }
            if ticket.automation.generation != timer.generation {
                let since = latest_activity(&ticket, timer.since);
                return schedule(
                    &ctx,
                    channel,
                    &ticket,
                    Kind::LastMessage,
                    since,
                    due(since, config.last_message_timeout_hours),
                )
                .await;
            }
            let at = due(timer.since, config.last_message_timeout_hours);
            if ctx.now() < at {
                return schedule(&ctx, channel, &ticket, Kind::LastMessage, timer.since, at).await;
            }
            auto_close(
                &ctx,
                channel,
                &ticket,
                Kind::LastMessage,
                timer.generation,
                timer.since,
                "tickets-last-message-timeout-reason",
            )
            .await
        }
        Kind::Waiting => {
            if ticket.thread
                || !ticket.waiting_for_reply
                || config.waiting_after_hours == 0
                || waiting_category(&config).is_none()
            {
                return Ok(());
            }
            if ticket.pending_operation.is_some() {
                return schedule(
                    &ctx,
                    channel,
                    &ticket,
                    Kind::Waiting,
                    timer.since,
                    ctx.now() + CLOSING_WAIT,
                )
                .await;
            }
            if ticket.automation.generation != timer.generation {
                let since = latest_activity(&ticket, timer.since);
                return schedule(
                    &ctx,
                    channel,
                    &ticket,
                    Kind::Waiting,
                    since,
                    due(since, config.waiting_after_hours),
                )
                .await;
            }
            let at = due(timer.since, config.waiting_after_hours);
            if ctx.now() < at {
                return schedule(&ctx, channel, &ticket, Kind::Waiting, timer.since, at).await;
            }
            move_category(&ctx, channel, &ticket, true).await?;
            reconcile_category(&ctx, channel, timer.generation).await
        }
        Kind::Restore => {
            let has_pending = schedule_pending_staff_checks(&ctx, channel, &ticket).await?;
            if has_pending {
                return schedule(
                    &ctx,
                    channel,
                    &ticket,
                    Kind::Restore,
                    timer.since,
                    ctx.now() + CLOSING_WAIT,
                )
                .await;
            }
            if ticket.thread
                || ticket.waiting_for_reply
                || ticket.automation.generation != timer.generation
            {
                return Ok(());
            }
            if waiting_category(&config).is_none() {
                return Ok(());
            }
            if ticket.pending_operation.is_some() {
                return schedule(
                    &ctx,
                    channel,
                    &ticket,
                    Kind::Restore,
                    timer.since,
                    ctx.now() + CLOSING_WAIT,
                )
                .await;
            }
            move_category(&ctx, channel, &ticket, false).await?;
            reconcile_category(&ctx, channel, timer.generation).await
        }
        Kind::StaffCheck => resolve_staff_check(&ctx, channel, &ticket, &timer).await,
    }
}

async fn resolve_staff_check(
    ctx: &EventContext,
    channel: ChannelId,
    current: &Ticket,
    timer: &Timer,
) -> Result<(), ModuleError> {
    let Some(payload) = timer.staff_check.as_ref() else {
        return Ok(());
    };
    let key = pending_staff_key(channel, timer.close_generation, payload.message_id);
    let Some(check) = ctx
        .store()
        .get::<PendingStaffCheck>(&key)
        .await
        .map_err(|error| stored(&error))?
    else {
        return Ok(());
    };

    let prefix = pending_staff_prefix(channel, timer.close_generation);
    let first = ctx
        .store()
        .list(&prefix, 1)
        .await
        .map_err(|error| stored(&error))?;
    if let Some((first_key, first_entry)) = first.into_iter().next() {
        if first_key != key {
            let next: PendingStaffCheck = serde_json::from_value(first_entry.value)
                .map_err(|error| ModuleError::other(error.to_string()))?;
            schedule_staff_check(ctx, channel, current, &next, ctx.now()).await?;
            return schedule_staff_check(
                ctx,
                channel,
                current,
                &check,
                ctx.now() + STAFF_CHECK_ORDER_WAIT,
            )
            .await;
        }
    }

    let ticket_key = ticket_key(channel);
    for _ in 0..5 {
        let Some(entry) = ctx
            .store()
            .entry(&ticket_key)
            .await
            .map_err(|error| stored(&error))?
        else {
            delete_pending_staff_check(ctx, channel, timer.close_generation, check.message_id)
                .await?;
            return Ok(());
        };
        let mut ticket: Ticket = serde_json::from_value(entry.value)
            .map_err(|error| ModuleError::other(error.to_string()))?;
        if !ticket.is_open() || ticket.close_generation != timer.close_generation {
            delete_pending_staff_check(ctx, channel, timer.close_generation, check.message_id)
                .await?;
            return Ok(());
        }
        if check.message_id <= ticket.automation.last_resolved_staff_check_id {
            let config: Config = ctx.parse_config()?;
            let config = config.for_ticket(&ticket);
            let since = latest_activity(&ticket, ticket.created);
            schedule_after_message(ctx, channel, &ticket, &config, since).await?;
            delete_pending_staff_check(ctx, channel, timer.close_generation, check.message_id)
                .await?;
            return Ok(());
        }
        let config: Config = ctx.parse_config()?;
        let config = config.for_ticket(&ticket);
        let roles = roles_for_user(ctx, check.author, &check.roles).await?;
        let opener = check.author.get() == ticket.opener;
        let staff_roles = config.staff();
        let staff = if opener {
            false
        } else if roles.iter().any(|role| staff_roles.contains(role)) {
            true
        } else {
            may_manage_ticket(ctx, check.author, &roles).await?
        };

        ticket.automation.last_resolved_staff_check_id = check.message_id;
        ticket.automation.generation = ticket
            .automation
            .generation
            .checked_add(1)
            .ok_or_else(|| ModuleError::other("ticket automation generation is exhausted"))?;
        if check.message_id > ticket.automation.latest_message_id {
            ticket.automation.latest_message_id = check.message_id;
        }
        if staff && !ticket.automation.first_response_replied {
            ticket.automation.first_response_generation = ticket
                .automation
                .first_response_generation
                .checked_add(1)
                .ok_or_else(|| {
                    ModuleError::other("ticket first response generation is exhausted")
                })?;
            ticket.automation.first_response_replied = true;
        }
        if staff && ticket.automation.latest_message_id == check.message_id {
            ticket.waiting_for_reply = true;
        }
        refresh_ticket_form_answer_ttl(ctx, &ticket).await?;
        match ctx
            .store()
            .replace(
                &ticket_key,
                &ticket,
                Expect::Revision(entry.revision),
                Some(ticket_ttl(&ticket, ctx.now())),
            )
            .await
        {
            Ok(_) => {
                let since = latest_activity(&ticket, ticket.created);
                schedule_after_message(ctx, channel, &ticket, &config, since).await?;
                delete_pending_staff_check(ctx, channel, timer.close_generation, check.message_id)
                    .await?;
                return Ok(());
            }
            Err(StorageError::Conflict) => continue,
            Err(error) => return Err(stored(&error)),
        }
    }
    Err(ModuleError::other(
        "ticket changed repeatedly while resolving a staff permission",
    ))
}

async fn auto_close(
    ctx: &EventContext,
    channel: ChannelId,
    ticket: &Ticket,
    kind: Kind,
    expected_generation: u64,
    since: i64,
    reason_key: &str,
) -> Result<(), ModuleError> {
    if ticket.automation.generation != expected_generation {
        return Ok(());
    }
    let reason = ctx.t_in(ctx.locale, reason_key, &Args::new());
    if let Closed::BusyUntil(until) = finish_if_automation_generation(
        ctx,
        channel,
        ticket.close_generation,
        expected_generation,
        &reason,
    )
    .await?
    {
        schedule(ctx, channel, ticket, kind, since, until).await?;
    }
    Ok(())
}

async fn move_category(
    ctx: &EventContext,
    channel: ChannelId,
    ticket: &Ticket,
    waiting: bool,
) -> Result<(), ModuleError> {
    // Discord threads cannot be moved between category channels.
    if ticket.thread {
        return Ok(());
    }
    if ticket.pending_operation.is_some() {
        return Ok(());
    }
    let config: Config = ctx.parse_config()?;
    let config = config.for_ticket(ticket);
    let Some(waiting_category) = waiting_category(&config) else {
        return Ok(());
    };
    let original_category = config.category();
    let (from, to) = if waiting {
        (original_category, Some(waiting_category))
    } else {
        (Some(waiting_category), original_category)
    };
    if from == to {
        return Ok(());
    }
    if let Some(target) = to {
        match ctx
            .api
            .execute(Action::GetChannel { channel_id: target })
            .await
        {
            Ok(Outcome::Channel(found)) if found.kind == ChannelType::GuildCategory => {}
            Ok(_) | Err(DiscordError::NotFound) => {
                tracing::warn!(channel = %channel, target = %target, "ticket automation target is not a category");
                return Ok(());
            }
            Err(error) => return Err(error.into()),
        }
    }
    let found = match ctx
        .api
        .execute(Action::GetChannel {
            channel_id: channel,
        })
        .await
    {
        Ok(Outcome::Channel(found)) => found,
        Ok(_) | Err(DiscordError::NotFound) => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    if found.kind != ChannelType::GuildText || found.parent_id != from {
        return Ok(());
    }
    ctx.api
        .execute(Action::EditChannel {
            channel_id: channel,
            patch: ChannelPatch {
                parent_id: Some(to),
                ..ChannelPatch::default()
            },
            reason: Reason::truncated(&ctx.t_in(ctx.locale, "tickets-reason-panel", &Args::new())),
        })
        .await
        .map(|_| ())
        .map_err(|error| ModuleError::from(error))
}

async fn reconcile_category(
    ctx: &EventContext,
    channel: ChannelId,
    mut expected_generation: u64,
) -> Result<(), ModuleError> {
    for _ in 0..3 {
        let Some(ticket) = ctx
            .store()
            .get::<Ticket>(&ticket_key(channel))
            .await
            .map_err(|error| stored(&error))?
        else {
            return Ok(());
        };
        if !ticket.is_open() || ticket.automation.generation == expected_generation {
            return Ok(());
        }
        let config: Config = ctx.parse_config()?;
        let config = config.for_ticket(&ticket);
        if ticket.thread || waiting_category(&config).is_none() {
            return Ok(());
        }
        if ticket.pending_operation.is_some() {
            if ticket.waiting_for_reply && config.waiting_after_hours == 0 {
                return Ok(());
            }
            let kind = if ticket.waiting_for_reply {
                Kind::Waiting
            } else {
                Kind::Restore
            };
            let since = latest_activity(&ticket, ctx.now());
            schedule(ctx, channel, &ticket, kind, since, ctx.now() + CLOSING_WAIT).await?;
            return Ok(());
        }
        if ticket.waiting_for_reply {
            if config.waiting_after_hours == 0 {
                return Ok(());
            }
            let since = latest_activity(&ticket, ctx.now());
            let at = due(since, config.waiting_after_hours);
            if ctx.now() < at {
                schedule(ctx, channel, &ticket, Kind::Waiting, since, at).await?;
                return Ok(());
            }
            move_category(ctx, channel, &ticket, true).await?;
        } else {
            move_category(ctx, channel, &ticket, false).await?;
        }
        expected_generation = ticket.automation.generation;
    }
    Ok(())
}
