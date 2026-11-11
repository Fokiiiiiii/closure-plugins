//! Ticket panel, participant-role, and reply-waiting operations.
//! Ticket state changes are CAS-written and use ticket_ttl so close/history fences survive.

use std::time::Duration;

use closure_core::action::{
    Action, ChannelPatch, MentionPolicy, Outcome, OverwriteSpec, OverwriteTarget, Reason,
};
use closure_core::error::{DiscordError, ModuleError};
use closure_core::event::{ChannelId, RoleId, UserId};
use closure_core::module::EventContext;
use closure_core::storage::{Expect, StorageError};
use closure_sdk::{Args, CommandArgs, CommandCx, UserError};
use serde::{Deserialize, Serialize};
use twilight_model::channel::permission_overwrite::{
    PermissionOverwrite, PermissionOverwriteType,
};
use twilight_model::channel::ChannelType;
use twilight_model::guild::Permissions;

use super::{
    Config, NamedPanel, Ticket, ensure_category, failed, id_of, is_staff_command, stored,
    ticket_key, ticket_setup_error, ticket_ttl, thread, TICKET_PERMISSIONS,
};

const OPERATION_IDS: &str = "ticket-operation-ids";
const OPERATION_LEASE_SECONDS: i64 = 300;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct RoleAccess {
    pub(super) role: u64,
    pub(super) allow: Permissions,
    pub(super) deny: Permissions,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(super) enum PanelPhase {
    Forward,
    Rollback,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(super) enum PendingOperation {
    Panel {
        token: i64,
        lease_until: i64,
        phase: PanelPhase,
        panel: Option<NamedPanel>,
        settings: Config,
        staff_roles: Vec<u64>,
        old_staff_roles: Vec<u64>,
        old_parent: Option<u64>,
        role_baselines: Option<Vec<RoleAccess>>,
    },
    ParticipantRole {
        token: i64,
        lease_until: i64,
        role: u64,
        add: bool,
        staff_roles: Vec<u64>,
        baseline: Option<RoleAccess>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ApplyResult {
    Done,
    RolledBack,
}

#[derive(CommandArgs)]
pub(super) struct SetPanelArgs {
    /// Panel ID; omit for the current default settings
    #[arg(max_length = 32)]
    panel: Option<String>,
}

#[derive(CommandArgs)]
pub(super) struct RoleArgs {
    /// Role to add or remove from this ticket
    role: RoleId,
}

#[derive(CommandArgs)]
pub(super) struct WaitingArgs {
    /// Whether staff are waiting for a reply
    waiting: bool,
}

fn operation_token(operation: &PendingOperation) -> i64 {
    match operation {
        PendingOperation::Panel { token, .. }
        | PendingOperation::ParticipantRole { token, .. } => *token,
    }
}

fn operation_lease(operation: &PendingOperation) -> i64 {
    match operation {
        PendingOperation::Panel { lease_until, .. }
        | PendingOperation::ParticipantRole { lease_until, .. } => *lease_until,
    }
}

fn set_lease(operation: &mut PendingOperation, token: i64, until: i64) {
    match operation {
        PendingOperation::Panel {
            token: saved_token,
            lease_until,
            ..
        }
        | PendingOperation::ParticipantRole {
            token: saved_token,
            lease_until,
            ..
        } => {
            *saved_token = token;
            *lease_until = until;
        }
    }
}

fn empty_access(role: RoleId) -> RoleAccess {
    RoleAccess {
        role: role.get(),
        allow: Permissions::empty(),
        deny: Permissions::empty(),
    }
}

fn role_access(overwrites: &[PermissionOverwrite], role: RoleId) -> Option<RoleAccess> {
    overwrites
        .iter()
        .find(|overwrite| {
            overwrite.kind == PermissionOverwriteType::Role
                && overwrite.id.get() == role.get()
        })
        .map(|overwrite| RoleAccess {
            role: role.get(),
            allow: overwrite.allow,
            deny: overwrite.deny,
        })
}

fn saved_role_access(ticket: &Ticket, role: RoleId) -> RoleAccess {
    ticket
        .panel_role_baselines
        .iter()
        .chain(&ticket.participant_roles)
        .find(|access| access.role == role.get())
        .copied()
        .unwrap_or_else(|| empty_access(role))
}

fn panel_roles(config: &Config) -> Vec<RoleId> {
    let mut roles = config.staff();
    roles.sort_unstable();
    roles.dedup();
    roles
}

fn adjust_access(current: RoleAccess, baseline: RoleAccess, grant: bool) -> RoleAccess {
    if grant {
        RoleAccess {
            allow: current.allow | TICKET_PERMISSIONS,
            deny: current.deny & !TICKET_PERMISSIONS,
            ..current
        }
    } else {
        RoleAccess {
            allow: (current.allow & !TICKET_PERMISSIONS)
                | (baseline.allow & TICKET_PERMISSIONS),
            deny: (current.deny & !TICKET_PERMISSIONS)
                | (baseline.deny & TICKET_PERMISSIONS),
            ..current
        }
    }
}

async fn current_channel(
    ctx: &EventContext,
    channel: ChannelId,
) -> Result<twilight_model::channel::Channel, UserError> {
    match ctx.api.execute(Action::GetChannel { channel_id: channel }).await {
        Ok(Outcome::Channel(found)) => Ok(*found),
        Ok(_) | Err(DiscordError::NotFound) => Err(UserError::message("tickets-not-a-ticket")),
        Err(error) => Err(failed(&error)),
    }
}

async fn current_text_parent(
    ctx: &EventContext,
    channel: ChannelId,
) -> Result<Option<u64>, UserError> {
    let found = current_channel(ctx, channel).await?;
    if found.kind != ChannelType::GuildText {
        return Err(UserError::message("tickets-not-a-ticket"));
    }
    Ok(found.parent_id.map(|parent| parent.get()))
}

async fn validate_panel(
    ctx: &EventContext,
    ticket: &Ticket,
    settings: &Config,
) -> Result<Vec<RoleId>, UserError> {
    if let Some(key) = ticket_setup_error(settings, ctx.guild_id.get()) {
        return Err(UserError::message(key));
    }
    let roles = panel_roles(settings);
    let Outcome::Roles(known) = ctx
        .api
        .execute(Action::ListRoles {
            guild_id: ctx.guild_id,
        })
        .await
        .map_err(|error| failed(&error))?
    else {
        return Err(UserError::message("tickets-failed"));
    };
    if roles.iter().any(|role| !known.iter().any(|known| known.id == *role)) {
        return Err(UserError::message("tickets-panel-role-unavailable"));
    }

    let target_thread = settings.thread_channel()?;
    if ticket.thread {
        let parent = ticket
            .thread_parent
            .map(ChannelId::new)
            .ok_or_else(|| UserError::message("tickets-thread-parent-unavailable"))?;
        if target_thread != Some(parent) {
            return Err(UserError::message("tickets-thread-parent-change"));
        }
        let mut old_roles: Vec<_> = ticket.staff_roles.as_ref().map_or_else(
            || {
                ticket.settings.as_ref().map_or_else(
                    || panel_roles(settings),
                    |saved| panel_roles(saved),
                )
            },
            |saved| saved.iter().copied().map(RoleId::new).collect(),
        );
        old_roles.sort_unstable();
        old_roles.dedup();
        if old_roles != roles {
            return Err(UserError::message("tickets-thread-staff-change"));
        }
        thread::ensure_thread_parent(
            ctx,
            parent,
            &roles,
            UserId::new(ticket.opener),
        )
        .await?;
    } else {
        if target_thread.is_some() {
            return Err(UserError::message("tickets-panel-kind-change"));
        }
        if settings.category.is_some() && settings.category().is_none() {
            return Err(UserError::message("tickets-invalid-config"));
        }
        if let Some(category) = settings.category() {
            ensure_category(ctx, category).await?;
        }
    }
    if settings.log_channel.is_some() && settings.log().is_none() {
        return Err(UserError::message("tickets-invalid-config"));
    }
    if let Some(log) = settings.log() {
        match ctx.api.execute(Action::GetChannel { channel_id: log }).await {
            Ok(Outcome::Channel(found))
                if matches!(found.kind, ChannelType::GuildText | ChannelType::GuildAnnouncement) =>
            {
            }
            Ok(_) | Err(DiscordError::NotFound) => {
                return Err(UserError::message("tickets-log-unavailable"));
            }
            Err(error) => return Err(failed(&error)),
        }
    }
    Ok(roles)
}

async fn ensure_role(ctx: &EventContext, role: RoleId) -> Result<(), UserError> {
    if role == RoleId::new(ctx.guild_id.get()) {
        return Err(UserError::message("tickets-everyone-not-participant"));
    }
    let Outcome::Roles(roles) = ctx
        .api
        .execute(Action::ListRoles {
            guild_id: ctx.guild_id,
        })
        .await
        .map_err(|error| failed(&error))?
    else {
        return Err(UserError::message("tickets-failed"));
    };
    if !roles.iter().any(|known| known.id == role) {
        return Err(UserError::message("tickets-role-unavailable"));
    }
    Ok(())
}

async fn stage(
    ctx: &EventContext,
    channel: ChannelId,
    mut operation: PendingOperation,
) -> Result<i64, UserError> {
    let key = ticket_key(channel);
    for _ in 0..5 {
        let Some(entry) = ctx.store().entry(&key).await.map_err(|error| failed(&error))? else {
            return Err(UserError::message("tickets-not-a-ticket"));
        };
        let mut ticket: Ticket =
            serde_json::from_value(entry.value).map_err(|error| failed(&error))?;
        if ticket.closed || ticket.closing.is_some() {
            return Err(UserError::message("tickets-not-a-ticket"));
        }
        if ticket.pending_operation.is_some() {
            return Err(UserError::message("tickets-operation-pending"));
        }
        let token = ctx
            .store()
            .score_add(OPERATION_IDS, 0_u64, 1)
            .await
            .map_err(|error| failed(&error))?;
        set_lease(&mut operation, token, ctx.now() + OPERATION_LEASE_SECONDS);
        ticket.pending_operation = Some(operation.clone());
        super::refresh_ticket_form_answer_ttl(ctx, &ticket)
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
            Ok(_) => return Ok(token),
            Err(StorageError::Conflict) => {}
            Err(error) => return Err(failed(&error)),
        }
    }
    Err(UserError::message("tickets-failed"))
}

async fn renew_operation(
    ctx: &EventContext,
    channel: ChannelId,
    token: i64,
) -> Result<Ticket, ModuleError> {
    let key = ticket_key(channel);
    for _ in 0..5 {
        let Some(entry) = ctx.store().entry(&key).await.map_err(|error| stored(&error))? else {
            return Err(ModuleError::other("the ticket no longer exists"));
        };
        let mut ticket: Ticket = serde_json::from_value(entry.value)
            .map_err(|error| ModuleError::other(error.to_string()))?;
        let Some(operation) = ticket.pending_operation.as_mut() else {
            return Err(ModuleError::other("the ticket operation is no longer active"));
        };
        if operation_token(operation) != token || ticket.closed || ticket.closing.is_some() {
            return Err(ModuleError::other("the ticket operation is no longer active"));
        }
        set_lease(operation, token, ctx.now() + OPERATION_LEASE_SECONDS);
        super::refresh_ticket_form_answer_ttl(ctx, &ticket).await?;
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
            Ok(_) => return Ok(ticket),
            Err(StorageError::Conflict) => {}
            Err(error) => return Err(stored(&error)),
        }
    }
    Err(ModuleError::other("the ticket changed during its operation"))
}

async fn release_operation(ctx: &EventContext, channel: ChannelId, token: i64) {
    let key = ticket_key(channel);
    for _ in 0..5 {
        let Ok(Some(entry)) = ctx.store().entry(&key).await else {
            return;
        };
        let Ok(mut ticket) = serde_json::from_value::<Ticket>(entry.value) else {
            return;
        };
        let Some(operation) = ticket.pending_operation.as_mut() else {
            return;
        };
        if operation_token(operation) != token {
            return;
        }
        set_lease(operation, token, ctx.now());
        if let Err(error) = super::refresh_ticket_form_answer_ttl(ctx, &ticket).await {
            tracing::warn!(?error, %channel, "could not refresh ticket answer TTL while releasing an operation");
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
            Ok(_) => return,
            Err(StorageError::Conflict) => {}
            Err(error) => {
                tracing::warn!(?error, %channel, "could not release a ticket operation");
                return;
            }
        }
    }
}

pub(super) async fn resume_pending(
    ctx: &EventContext,
    channel: ChannelId,
) -> Result<Option<i64>, ModuleError> {
    let key = ticket_key(channel);
    let mut claimed = None;
    for _ in 0..5 {
        let Some(entry) = ctx.store().entry(&key).await.map_err(|error| stored(&error))? else {
            return Ok(None);
        };
        let mut ticket: Ticket = serde_json::from_value(entry.value)
            .map_err(|error| ModuleError::other(error.to_string()))?;
        let Some(operation) = ticket.pending_operation.as_mut() else {
            return Ok(None);
        };
        if ticket.closed || ticket.closing.is_some() {
            return Err(ModuleError::other("a closing ticket has a pending operation"));
        }
        if operation_lease(operation) > ctx.now() {
            return Ok(Some(operation_lease(operation)));
        }
        let token = ctx
            .store()
            .score_add(OPERATION_IDS, 0_u64, 1)
            .await
            .map_err(|error| stored(&error))?;
        set_lease(operation, token, ctx.now() + OPERATION_LEASE_SECONDS);
        super::refresh_ticket_form_answer_ttl(ctx, &ticket).await?;
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
                claimed = Some(token);
                break;
            }
            Err(StorageError::Conflict) => {}
            Err(error) => return Err(stored(&error)),
        }
    }
    let Some(token) = claimed else {
        return Err(ModuleError::other("the ticket kept changing during its operation"));
    };
    run_owned(ctx, channel, token).await?;
    Ok(None)
}

async fn run_owned(
    ctx: &EventContext,
    channel: ChannelId,
    token: i64,
) -> Result<ApplyResult, ModuleError> {
    match apply_pending(ctx, channel, token).await {
        Ok(result) => Ok(result),
        Err(error) => {
            release_operation(ctx, channel, token).await;
            Err(error)
        }
    }
}

async fn operation_channel(
    ctx: &EventContext,
    channel: ChannelId,
    token: i64,
) -> Result<twilight_model::channel::Channel, ModuleError> {
    renew_operation(ctx, channel, token).await?;
    match ctx.api.execute(Action::GetChannel { channel_id: channel }).await {
        Ok(Outcome::Channel(found)) => Ok(*found),
        Ok(_) => Err(ModuleError::other("the ticket channel is unavailable")),
        Err(error) => Err(stored(&error)),
    }
}

async fn set_role_access(
    ctx: &EventContext,
    channel: ChannelId,
    role: RoleId,
    baseline: RoleAccess,
    grant: bool,
    token: i64,
    reason: &Reason,
) -> Result<(), ModuleError> {
    let found = operation_channel(ctx, channel, token).await?;
    if found.kind != ChannelType::GuildText {
        return Err(ModuleError::other("role overwrites apply only to ticket text channels"));
    }
    let overwrites = found
        .permission_overwrites
        .as_deref()
        .ok_or_else(|| ModuleError::other("the ticket channel has no permission overwrites"))?;
    let current = role_access(overwrites, role).unwrap_or_else(|| empty_access(role));
    let next = adjust_access(current, baseline, grant);
    if current == next {
        return Ok(());
    }
    renew_operation(ctx, channel, token).await?;
    let action = if next.allow.is_empty() && next.deny.is_empty() {
        Action::DeletePermissionOverwrite {
            channel_id: channel,
            target: OverwriteTarget::Role(role),
            reason: reason.clone(),
        }
    } else {
        Action::SetPermissionOverwrite {
            channel_id: channel,
            overwrite: OverwriteSpec {
                target: OverwriteTarget::Role(role),
                allow: next.allow,
                deny: next.deny,
            },
            reason: reason.clone(),
        }
    };
    ctx.api
        .execute(action)
        .await
        .map(|_| ())
        .map_err(|error| stored(&error))
}

async fn save_panel_baselines(
    ctx: &EventContext,
    channel: ChannelId,
    token: i64,
    baselines: &[RoleAccess],
) -> Result<(), ModuleError> {
    let key = ticket_key(channel);
    for _ in 0..5 {
        let Some(entry) = ctx.store().entry(&key).await.map_err(|error| stored(&error))? else {
            return Err(ModuleError::other("the ticket no longer exists"));
        };
        let mut ticket: Ticket = serde_json::from_value(entry.value)
            .map_err(|error| ModuleError::other(error.to_string()))?;
        let Some(PendingOperation::Panel {
            token: current,
            lease_until,
            role_baselines,
            ..
        }) = ticket.pending_operation.as_mut()
        else {
            return Err(ModuleError::other("the panel change is no longer active"));
        };
        if *current != token {
            return Err(ModuleError::other("the panel change is no longer active"));
        }
        *role_baselines = Some(baselines.to_vec());
        *lease_until = ctx.now() + OPERATION_LEASE_SECONDS;
        super::refresh_ticket_form_answer_ttl(ctx, &ticket).await?;
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
            Ok(_) => return Ok(()),
            Err(StorageError::Conflict) => {}
            Err(error) => return Err(stored(&error)),
        }
    }
    Err(ModuleError::other("the ticket changed while saving panel permissions"))
}

async fn save_role_baseline(
    ctx: &EventContext,
    channel: ChannelId,
    token: i64,
    baseline: RoleAccess,
) -> Result<(), ModuleError> {
    let key = ticket_key(channel);
    for _ in 0..5 {
        let Some(entry) = ctx.store().entry(&key).await.map_err(|error| stored(&error))? else {
            return Err(ModuleError::other("the ticket no longer exists"));
        };
        let mut ticket: Ticket = serde_json::from_value(entry.value)
            .map_err(|error| ModuleError::other(error.to_string()))?;
        let Some(PendingOperation::ParticipantRole {
            token: current,
            lease_until,
            baseline: saved,
            ..
        }) = ticket.pending_operation.as_mut()
        else {
            return Err(ModuleError::other("the role change is no longer active"));
        };
        if *current != token {
            return Err(ModuleError::other("the role change is no longer active"));
        }
        *saved = Some(baseline);
        *lease_until = ctx.now() + OPERATION_LEASE_SECONDS;
        super::refresh_ticket_form_answer_ttl(ctx, &ticket).await?;
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
            Ok(_) => return Ok(()),
            Err(StorageError::Conflict) => {}
            Err(error) => return Err(stored(&error)),
        }
    }
    Err(ModuleError::other("the ticket changed while saving role permissions"))
}

async fn move_text_ticket(
    ctx: &EventContext,
    channel: ChannelId,
    parent: Option<ChannelId>,
    token: i64,
    reason: &Reason,
) -> Result<(), ModuleError> {
    let found = operation_channel(ctx, channel, token).await?;
    if found.kind != ChannelType::GuildText {
        return Err(ModuleError::other("the ticket channel is not a text channel"));
    }
    if found.parent_id == parent {
        return Ok(());
    }
    renew_operation(ctx, channel, token).await?;
    ctx.api
        .execute(Action::EditChannel {
            channel_id: channel,
            patch: ChannelPatch {
                parent_id: Some(parent),
                ..ChannelPatch::default()
            },
            reason: reason.clone(),
        })
        .await
        .map(|_| ())
        .map_err(|error| stored(&error))
}

async fn finish_panel(
    ctx: &EventContext,
    channel: ChannelId,
    token: i64,
    panel: Option<NamedPanel>,
    settings: &Config,
    staff_roles: &[u64],
    baselines: &[RoleAccess],
) -> Result<(), ModuleError> {
    let key = ticket_key(channel);
    for _ in 0..5 {
        let Some(entry) = ctx.store().entry(&key).await.map_err(|error| stored(&error))? else {
            return Err(ModuleError::other("the ticket no longer exists"));
        };
        let mut ticket: Ticket = serde_json::from_value(entry.value)
            .map_err(|error| ModuleError::other(error.to_string()))?;
        if ticket.pending_operation.as_ref().is_none_or(|operation| operation_token(operation) != token) {
            return Err(ModuleError::other("the panel change is no longer active"));
        }
        ticket.panel = panel.clone();
        ticket.settings = Some(settings.clone());
        ticket.staff_roles = Some(staff_roles.to_vec());
        ticket.panel_role_baselines = baselines.to_vec();
        ticket.pending_operation = None;
        super::refresh_ticket_form_answer_ttl(ctx, &ticket).await?;
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
                if settings.close_after_days > 0 {
                    if let Err(error) = super::watch(
                        ctx,
                        channel,
                        ctx.now() + i64::from(settings.close_after_days) * super::DAY,
                    )
                    .await
                    {
                        tracing::warn!(?error, %channel, "could not reschedule ticket idle close");
                    }
                }
                return Ok(());
            }
            Err(StorageError::Conflict) => {}
            Err(error) => return Err(stored(&error)),
        }
    }
    Err(ModuleError::other("the ticket changed while saving its panel"))
}

async fn commit_role(
    ctx: &EventContext,
    channel: ChannelId,
    token: i64,
    role: RoleId,
    add: bool,
    baseline: RoleAccess,
) -> Result<(), ModuleError> {
    let key = ticket_key(channel);
    for _ in 0..5 {
        let Some(entry) = ctx.store().entry(&key).await.map_err(|error| stored(&error))? else {
            return Err(ModuleError::other("the ticket no longer exists"));
        };
        let mut ticket: Ticket = serde_json::from_value(entry.value)
            .map_err(|error| ModuleError::other(error.to_string()))?;
        if ticket.pending_operation.as_ref().is_none_or(|operation| operation_token(operation) != token) {
            return Err(ModuleError::other("the role change is no longer active"));
        }
        ticket.participant_roles.retain(|access| access.role != role.get());
        if add {
            ticket.participant_roles.push(baseline);
            ticket.participant_roles.sort_unstable_by_key(|access| access.role);
        }
        ticket.pending_operation = None;
        super::refresh_ticket_form_answer_ttl(ctx, &ticket).await?;
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
            Ok(_) => return Ok(()),
            Err(StorageError::Conflict) => {}
            Err(error) => return Err(stored(&error)),
        }
    }
    Err(ModuleError::other("the ticket changed while saving its participant role"))
}

async fn apply_panel_forward(
    ctx: &EventContext,
    channel: ChannelId,
    token: i64,
) -> Result<(), ModuleError> {
    let ticket = renew_operation(ctx, channel, token).await?;
    let (panel, settings, staff_roles, old_staff_roles, saved_baselines) =
        match ticket.pending_operation.as_ref() {
            Some(PendingOperation::Panel {
                phase: PanelPhase::Forward,
                panel,
                settings,
                staff_roles,
                old_staff_roles,
                role_baselines,
                ..
            }) => (
                panel.clone(),
                settings.clone(),
                staff_roles.clone(),
                old_staff_roles.clone(),
                role_baselines.clone(),
            ),
            _ => return Err(ModuleError::other("the panel change is no longer active")),
        };
    let target_roles: Vec<_> = staff_roles.iter().copied().map(RoleId::new).collect();
    let reason = Reason::truncated(&ctx.t_in(ctx.locale, "tickets-reason-panel", &Args::new()));

    if ticket.thread {
        let parent = ticket
            .thread_parent
            .map(ChannelId::new)
            .ok_or_else(|| ModuleError::other("ticket thread parent is missing"))?;
        thread::ensure_thread_parent(ctx, parent, &target_roles, UserId::new(ticket.opener))
            .await
            .map_err(|_| ModuleError::other("ticket thread staff access changed"))?;
        return finish_panel(ctx, channel, token, panel, &settings, &staff_roles, &[]).await;
    }

    let old_roles: Vec<_> = old_staff_roles.iter().copied().map(RoleId::new).collect();
    let baselines = if let Some(baselines) = saved_baselines {
        baselines
    } else {
        let found = operation_channel(ctx, channel, token).await?;
        let overwrites = found
            .permission_overwrites
            .as_deref()
            .ok_or_else(|| ModuleError::other("the ticket channel has no permission overwrites"))?;
        let baselines: Vec<_> = target_roles
            .iter()
            .map(|role| {
                if old_roles.contains(role)
                    || ticket.participant_roles.iter().any(|access| access.role == role.get())
                {
                    saved_role_access(&ticket, *role)
                } else {
                    role_access(overwrites, *role).unwrap_or_else(|| empty_access(*role))
                }
            })
            .collect();
        save_panel_baselines(ctx, channel, token, &baselines).await?;
        baselines
    };

    for role in target_roles.iter().filter(|role| !old_roles.contains(role)) {
        let baseline = baselines
            .iter()
            .find(|access| access.role == role.get())
            .copied()
            .unwrap_or_else(|| empty_access(*role));
        set_role_access(ctx, channel, *role, baseline, true, token, &reason).await?;
    }
    move_text_ticket(ctx, channel, settings.category(), token, &reason).await?;

    for role in old_roles.iter().filter(|role| !target_roles.contains(role)) {
        if ticket.participant_roles.iter().any(|access| access.role == role.get()) {
            continue;
        }
        let baseline = ticket
            .panel_role_baselines
            .iter()
            .find(|access| access.role == role.get())
            .copied()
            .unwrap_or_else(|| empty_access(*role));
        set_role_access(ctx, channel, *role, baseline, false, token, &reason).await?;
    }
    finish_panel(ctx, channel, token, panel, &settings, &staff_roles, &baselines).await
}


async fn mark_panel_rollback(
    ctx: &EventContext,
    channel: ChannelId,
    token: i64,
) -> Result<(), ModuleError> {
    let key = ticket_key(channel);
    for _ in 0..5 {
        let Some(entry) = ctx.store().entry(&key).await.map_err(|error| stored(&error))? else {
            return Err(ModuleError::other("the ticket no longer exists"));
        };
        let mut ticket: Ticket = serde_json::from_value(entry.value)
            .map_err(|error| ModuleError::other(error.to_string()))?;
        let Some(PendingOperation::Panel {
            token: current,
            lease_until,
            phase,
            ..
        }) = ticket.pending_operation.as_mut()
        else {
            return Err(ModuleError::other("the panel change is no longer active"));
        };
        if *current != token {
            return Err(ModuleError::other("the panel change is no longer active"));
        }
        *phase = PanelPhase::Rollback;
        *lease_until = ctx.now() + OPERATION_LEASE_SECONDS;
        super::refresh_ticket_form_answer_ttl(ctx, &ticket).await?;
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
            Ok(_) => return Ok(()),
            Err(StorageError::Conflict) => {}
            Err(error) => return Err(stored(&error)),
        }
    }
    Err(ModuleError::other("the ticket changed while preparing its rollback"))
}

async fn clear_panel_operation(
    ctx: &EventContext,
    channel: ChannelId,
    token: i64,
) -> Result<(), ModuleError> {
    let key = ticket_key(channel);
    for _ in 0..5 {
        let Some(entry) = ctx.store().entry(&key).await.map_err(|error| stored(&error))? else {
            return Err(ModuleError::other("the ticket no longer exists"));
        };
        let mut ticket: Ticket = serde_json::from_value(entry.value)
            .map_err(|error| ModuleError::other(error.to_string()))?;
        if ticket.pending_operation.as_ref().is_none_or(|operation| operation_token(operation) != token) {
            return Err(ModuleError::other("the panel operation is no longer active"));
        }
        ticket.pending_operation = None;
        super::refresh_ticket_form_answer_ttl(ctx, &ticket).await?;
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
            Ok(_) => return Ok(()),
            Err(StorageError::Conflict) => {}
            Err(error) => return Err(stored(&error)),
        }
    }
    Err(ModuleError::other("the ticket changed while clearing its panel operation"))
}

async fn apply_panel_rollback(
    ctx: &EventContext,
    channel: ChannelId,
    token: i64,
) -> Result<(), ModuleError> {
    let ticket = renew_operation(ctx, channel, token).await?;
    let (settings, old_staff_roles, old_parent, saved_baselines) =
        match ticket.pending_operation.as_ref() {
            Some(PendingOperation::Panel {
                settings,
                old_staff_roles,
                old_parent,
                role_baselines,
                phase: PanelPhase::Rollback,
                ..
            }) => (
                settings.clone(),
                old_staff_roles.clone(),
                *old_parent,
                role_baselines.clone(),
            ),
            _ => return Err(ModuleError::other("the panel rollback is no longer active")),
        };
    if ticket.thread || saved_baselines.is_none() {
        return clear_panel_operation(ctx, channel, token).await;
    }
    let baselines = saved_baselines.unwrap_or_default();
    let reason = Reason::truncated(&ctx.t_in(ctx.locale, "tickets-reason-panel", &Args::new()));
    let old_roles: Vec<_> = old_staff_roles.iter().copied().map(RoleId::new).collect();
    for role in &old_roles {
        let baseline = ticket
            .panel_role_baselines
            .iter()
            .find(|access| access.role == role.get())
            .copied()
            .unwrap_or_else(|| empty_access(*role));
        set_role_access(ctx, channel, *role, baseline, true, token, &reason).await?;
    }
    move_text_ticket(
        ctx,
        channel,
        old_parent.map(ChannelId::new),
        token,
        &reason,
    )
    .await?;
    let target_roles = panel_roles(&settings);
    for role in target_roles.iter().filter(|role| !old_roles.contains(role)) {
        if ticket.participant_roles.iter().any(|access| access.role == role.get()) {
            continue;
        }
        let baseline = baselines
            .iter()
            .find(|access| access.role == role.get())
            .copied()
            .unwrap_or_else(|| empty_access(*role));
        set_role_access(ctx, channel, *role, baseline, false, token, &reason).await?;
    }
    clear_panel_operation(ctx, channel, token).await
}

async fn apply_participant_role(
    ctx: &EventContext,
    channel: ChannelId,
    token: i64,
) -> Result<(), ModuleError> {
    let ticket = renew_operation(ctx, channel, token).await?;
    let (role, add, staff_roles, saved_baseline) = match ticket.pending_operation.as_ref() {
        Some(PendingOperation::ParticipantRole {
            role,
            add,
            staff_roles,
            baseline,
            ..
        }) => (
            RoleId::new(*role),
            *add,
            staff_roles.clone(),
            *baseline,
        ),
        _ => return Err(ModuleError::other("the role change is no longer active")),
    };
    if ticket.thread {
        return Err(ModuleError::other("role participants are not supported in private ticket threads"));
    }
    let baseline = if let Some(baseline) = saved_baseline {
        baseline
    } else if staff_roles.contains(&role.get()) {
        saved_role_access(&ticket, role)
    } else {
        let found = operation_channel(ctx, channel, token).await?;
        let overwrites = found
            .permission_overwrites
            .as_deref()
            .ok_or_else(|| ModuleError::other("the ticket channel has no permission overwrites"))?;
        role_access(overwrites, role).unwrap_or_else(|| empty_access(role))
    };
    if saved_baseline.is_none() {
        save_role_baseline(ctx, channel, token, baseline).await?;
    }
    if !staff_roles.contains(&role.get()) {
        let reason = Reason::truncated(&ctx.t_in(ctx.locale, "tickets-reason-role", &Args::new()));
        set_role_access(ctx, channel, role, baseline, add, token, &reason).await?;
    }
    commit_role(ctx, channel, token, role, add, baseline).await
}

async fn apply_pending(
    ctx: &EventContext,
    channel: ChannelId,
    token: i64,
) -> Result<ApplyResult, ModuleError> {
    let ticket = renew_operation(ctx, channel, token).await?;
    match ticket.pending_operation {
        Some(PendingOperation::Panel {
            phase: PanelPhase::Rollback,
            ..
        }) => {
            apply_panel_rollback(ctx, channel, token).await?;
            Ok(ApplyResult::RolledBack)
        }
        Some(PendingOperation::Panel { .. }) => match apply_panel_forward(ctx, channel, token).await {
            Ok(()) => Ok(ApplyResult::Done),
            Err(error) => {
                tracing::warn!(?error, %channel, "panel change failed; restoring the previous panel");
                mark_panel_rollback(ctx, channel, token).await?;
                apply_panel_rollback(ctx, channel, token).await?;
                Ok(ApplyResult::RolledBack)
            }
        },
        Some(PendingOperation::ParticipantRole { .. }) => {
            apply_participant_role(ctx, channel, token).await?;
            Ok(ApplyResult::Done)
        }
        None => Err(ModuleError::other("the ticket operation is no longer active")),
    }
}

pub(super) async fn set_panel(cx: CommandCx, args: SetPanelArgs) -> Result<(), UserError> {
    let ctx = cx.ctx();
    let channel = cx.invocation().channel_id;
    let config: Config = cx.config()?;
    let mut ticket = ctx
        .store()
        .get::<Ticket>(&ticket_key(channel))
        .await
        .map_err(|error| failed(&error))?
        .ok_or_else(|| UserError::message("tickets-not-a-ticket"))?;
    if ticket.closed || ticket.closing.is_some() {
        return Err(UserError::message("tickets-not-a-ticket"));
    }
    if !is_staff_command(&cx, &config.for_ticket(&ticket)).await {
        return Err(UserError::message("tickets-staff-only"));
    }
    cx.defer(true).await?;
    if resume_pending(ctx, channel)
        .await
        .map_err(|error| failed(&error))?
        .is_some()
    {
        return cx
            .reply_private_t("tickets-operation-pending", &Args::new())
            .await;
    }
    ticket = ctx
        .store()
        .get::<Ticket>(&ticket_key(channel))
        .await
        .map_err(|error| failed(&error))?
        .ok_or_else(|| UserError::message("tickets-not-a-ticket"))?;
    if ticket.closed || ticket.closing.is_some() {
        return Err(UserError::message("tickets-not-a-ticket"));
    }
    let current = config.for_ticket(&ticket);
    if !is_staff_command(&cx, &current).await {
        return Err(UserError::message("tickets-staff-only"));
    }
    let (panel, settings) = if let Some(id) = args.panel {
        let panel = config.named(&id)?.clone();
        let settings = config.with_panel(&panel);
        (Some(panel), settings)
    } else {
        (None, config.clone())
    };
    let staff = validate_panel(ctx, &ticket, &settings).await?;
    let old_staff_roles = ticket.staff_roles.clone().unwrap_or_else(|| {
        current.staff().iter().map(|role| role.get()).collect()
    });
    let old_parent = if ticket.thread {
        ticket.thread_parent
    } else {
        current_text_parent(ctx, channel).await?
    };
    let token = stage(
        ctx,
        channel,
        PendingOperation::Panel {
            token: 0,
            lease_until: 0,
            phase: PanelPhase::Forward,
            panel: panel.clone(),
            settings: settings.clone(),
            staff_roles: staff.iter().map(|role| role.get()).collect(),
            old_staff_roles,
            old_parent,
            role_baselines: None,
        },
    )
    .await?;
    match run_owned(ctx, channel, token).await {
        Ok(ApplyResult::Done) => {
            let args = panel
                .as_ref()
                .map_or_else(Args::new, |panel| Args::new().with("panel", panel.id.clone()));
            cx.reply_private_t(
                if panel.is_some() {
                    "tickets-panel-changed"
                } else {
                    "tickets-panel-default"
                },
                &args,
            )
            .await
        }
        Ok(ApplyResult::RolledBack) => {
            cx.reply_private_t("tickets-panel-restored", &Args::new()).await
        }
        Err(error) => {
            tracing::warn!(?error, %channel, "panel operation remains pending");
            cx.reply_private_t("tickets-operation-pending", &Args::new())
                .await
        }
    }
}

async fn change_role(cx: CommandCx, args: RoleArgs, add: bool) -> Result<(), UserError> {
    let ctx = cx.ctx();
    let channel = cx.invocation().channel_id;
    let config: Config = cx.config()?;
    let mut ticket = ctx
        .store()
        .get::<Ticket>(&ticket_key(channel))
        .await
        .map_err(|error| failed(&error))?
        .ok_or_else(|| UserError::message("tickets-not-a-ticket"))?;
    if ticket.closed || ticket.closing.is_some() {
        return Err(UserError::message("tickets-not-a-ticket"));
    }
    if !is_staff_command(&cx, &config.for_ticket(&ticket)).await {
        return Err(UserError::message("tickets-staff-only"));
    }
    if ticket.thread {
        return Err(UserError::message("tickets-thread-role-unavailable"));
    }
    cx.defer(true).await?;
    if resume_pending(ctx, channel)
        .await
        .map_err(|error| failed(&error))?
        .is_some()
    {
        return cx
            .reply_private_t("tickets-operation-pending", &Args::new())
            .await;
    }
    ticket = ctx
        .store()
        .get::<Ticket>(&ticket_key(channel))
        .await
        .map_err(|error| failed(&error))?
        .ok_or_else(|| UserError::message("tickets-not-a-ticket"))?;
    let current = config.for_ticket(&ticket);
    if !is_staff_command(&cx, &current).await {
        return Err(UserError::message("tickets-staff-only"));
    }
    if ticket.closed || ticket.closing.is_some() {
        return Err(UserError::message("tickets-not-a-ticket"));
    }
    ensure_role(ctx, args.role).await?;
    let existing = ticket
        .participant_roles
        .iter()
        .find(|access| access.role == args.role.get())
        .copied();
    if add && existing.is_some() {
        return cx
            .reply_private_t("tickets-role-already-added", &Args::new())
            .await;
    }
    if !add && existing.is_none() {
        return cx
            .reply_private_t("tickets-role-not-added", &Args::new())
            .await;
    }
    let staff_roles: Vec<u64> = ticket.staff_roles.clone().unwrap_or_else(|| {
        current.staff().iter().map(|role| role.get()).collect()
    });
    let baseline = if add && staff_roles.contains(&args.role.get()) {
        Some(saved_role_access(&ticket, args.role))
    } else {
        existing
    };
    let token = stage(
        ctx,
        channel,
        PendingOperation::ParticipantRole {
            token: 0,
            lease_until: 0,
            role: args.role.get(),
            add,
            staff_roles,
            baseline,
        },
    )
    .await?;
    match run_owned(ctx, channel, token).await {
        Ok(ApplyResult::Done) => {
            cx.reply_private_t(
                if add {
                    "tickets-role-added"
                } else {
                    "tickets-role-removed"
                },
                &Args::new().with("role", format!("<@&{}>", args.role)),
            )
            .await
        }
        Ok(ApplyResult::RolledBack) => {
            cx.reply_private_t("tickets-operation-pending", &Args::new())
                .await
        }
        Err(error) => {
            tracing::warn!(?error, %channel, %args.role, "participant role operation remains pending");
            cx.reply_private_t("tickets-operation-pending", &Args::new())
                .await
        }
    }
}

pub(super) async fn add_role(cx: CommandCx, args: RoleArgs) -> Result<(), UserError> {
    change_role(cx, args, true).await
}

pub(super) async fn remove_role(cx: CommandCx, args: RoleArgs) -> Result<(), UserError> {
    change_role(cx, args, false).await
}

pub(super) async fn waiting(cx: CommandCx, args: WaitingArgs) -> Result<(), UserError> {
    let ctx = cx.ctx();
    let channel = cx.invocation().channel_id;
    let config: Config = cx.config()?;
    cx.defer(true).await?;
    if resume_pending(ctx, channel)
        .await
        .map_err(|error| failed(&error))?
        .is_some()
    {
        return cx
            .reply_private_t("tickets-operation-pending", &Args::new())
            .await;
    }
    for _ in 0..5 {
        let Some(entry) = ctx
            .store()
            .entry(&ticket_key(channel))
            .await
            .map_err(|error| failed(&error))?
        else {
            return Err(UserError::message("tickets-not-a-ticket"));
        };
        let mut ticket: Ticket =
            serde_json::from_value(entry.value).map_err(|error| failed(&error))?;
        if ticket.closed || ticket.closing.is_some() {
            return Err(UserError::message("tickets-not-a-ticket"));
        }
        if ticket.pending_operation.is_some() {
            return cx
                .reply_private_t("tickets-operation-pending", &Args::new())
                .await;
        }
        if !is_staff_command(&cx, &config.for_ticket(&ticket)).await {
            return Err(UserError::message("tickets-staff-only"));
        }
        let changed = ticket.waiting_for_reply != args.waiting;
        ticket.waiting_for_reply = args.waiting;
        super::refresh_ticket_form_answer_ttl(ctx, &ticket)
            .await
            .map_err(|error| failed(&error))?;
        match ctx
            .store()
            .replace(
                &ticket_key(channel),
                &ticket,
                Expect::Revision(entry.revision),
                Some(ticket_ttl(&ticket, ctx.now())),
            )
            .await
        {
            Ok(_) => {
                let message = if args.waiting {
                    "tickets-waiting-set"
                } else {
                    "tickets-waiting-cleared"
                };
                if changed {
                    if let Err(error) = ctx
                        .api
                        .execute(Action::SendMessage {
                            channel_id: channel,
                            content: Some(cx.t(message, &Args::new())),
                            embeds: Vec::new(),
                            reply_to: None,
                            mentions: MentionPolicy::None,
                        })
                        .await
                    {
                        tracing::warn!(?error, %channel, "could not announce ticket reply status");
                    }
                }
                return cx.reply_private_t(message, &Args::new()).await;
            }
            Err(StorageError::Conflict) => {}
            Err(error) => return Err(failed(&error)),
        }
    }
    Err(UserError::message("tickets-failed"))
}
