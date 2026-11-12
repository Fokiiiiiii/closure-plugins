//! Ticket panel, participant-role, and reply-waiting operations.
//! Ticket state changes are CAS-written and use `ticket_ttl` so close/history fences survive.

use closure_core::action::{
    Action, ChannelPatch, MentionPolicy, Outcome, OverwriteSpec, OverwriteTarget, Reason,
};
use closure_core::error::{DiscordError, ModuleError};
use closure_core::event::{ChannelId, RoleId, UserId};
use closure_core::module::EventContext;
use closure_core::storage::{Expect, StorageError};
use closure_sdk::{Args, CommandArgs, CommandCx, JobCx, UserError};
use serde::{Deserialize, Serialize};
use twilight_model::channel::ChannelType;
use twilight_model::channel::permission_overwrite::{PermissionOverwrite, PermissionOverwriteType};
use twilight_model::guild::Permissions;

use super::{
    Config, NamedPanel, TICKET_PERMISSIONS, Ticket, ensure_category, failed, is_ticket_staff,
    stored, thread, ticket_key, ticket_setup_error, ticket_ttl,
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
pub(super) struct UserAccess {
    pub(super) user: u64,
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
        panel: Option<Box<NamedPanel>>,
        settings: Box<Config>,
        staff_roles: Vec<u64>,
        old_staff_roles: Vec<u64>,
        old_parent: Option<u64>,
        #[serde(default)]
        target_staff_access: Option<super::teams_survey::StaffAccessSnapshot>,
        #[serde(default)]
        old_staff_access: Option<super::teams_survey::StaffAccessSnapshot>,
        #[serde(default)]
        role_baselines: Option<Vec<RoleAccess>>,
        #[serde(default)]
        user_baselines: Option<Vec<UserAccess>>,
    },
    ParticipantRole {
        token: i64,
        lease_until: i64,
        role: u64,
        add: bool,
        staff_roles: Vec<u64>,
        baseline: Option<RoleAccess>,
    },
    ParticipantUser {
        token: i64,
        lease_until: i64,
        user: u64,
        add: bool,
        #[serde(default)]
        baseline: Option<UserAccess>,
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
        | PendingOperation::ParticipantRole { token, .. }
        | PendingOperation::ParticipantUser { token, .. } => *token,
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct OperationRecovery {
    channel: u64,
    token: i64,
}

const OPERATION_RECOVERY_JOB: &str = "ticket-operation-recovery";
const OPERATION_RETRY_SECONDS: i64 = 60;

fn operation_recovery_key(channel: ChannelId, token: i64) -> String {
    format!("ticket-operation-recovery-{}-{token}", channel.get())
}

async fn schedule_operation_recovery(
    ctx: &EventContext,
    channel: ChannelId,
    token: i64,
    at: i64,
) -> Result<(), ModuleError> {
    let payload = OperationRecovery {
        channel: channel.get(),
        token,
    };
    ctx.scheduler()
        .job(OPERATION_RECOVERY_JOB, &payload)
        .map_err(|error| stored(&error))?
        .key(&operation_recovery_key(channel, token))
        .at(at.max(ctx.now()))
        .await
        .map_err(|error| stored(&error))
}

fn operation_lease(operation: &PendingOperation) -> i64 {
    match operation {
        PendingOperation::Panel { lease_until, .. }
        | PendingOperation::ParticipantRole { lease_until, .. }
        | PendingOperation::ParticipantUser { lease_until, .. } => *lease_until,
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
        }
        | PendingOperation::ParticipantUser {
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
            overwrite.kind == PermissionOverwriteType::Role && overwrite.id.get() == role.get()
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

fn user_access(overwrites: &[PermissionOverwrite], user: UserId) -> Option<UserAccess> {
    overwrites
        .iter()
        .find(|overwrite| {
            overwrite.kind == PermissionOverwriteType::Member && overwrite.id.get() == user.get()
        })
        .map(|overwrite| UserAccess {
            user: user.get(),
            allow: overwrite.allow,
            deny: overwrite.deny,
        })
}

fn empty_user_access(user: UserId) -> UserAccess {
    UserAccess {
        user: user.get(),
        allow: Permissions::empty(),
        deny: Permissions::empty(),
    }
}

fn saved_user_access(ticket: &Ticket, user: UserId) -> UserAccess {
    ticket
        .panel_user_baselines
        .iter()
        .find(|access| access.user == user.get())
        .copied()
        .unwrap_or_else(|| empty_user_access(user))
}

fn adjust_user_access(current: UserAccess, baseline: UserAccess, grant: bool) -> UserAccess {
    if grant {
        UserAccess {
            allow: current.allow | TICKET_PERMISSIONS,
            deny: current.deny & !TICKET_PERMISSIONS,
            ..current
        }
    } else {
        UserAccess {
            allow: (current.allow & !TICKET_PERMISSIONS) | (baseline.allow & TICKET_PERMISSIONS),
            deny: (current.deny & !TICKET_PERMISSIONS) | (baseline.deny & TICKET_PERMISSIONS),
            ..current
        }
    }
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
            allow: (current.allow & !TICKET_PERMISSIONS) | (baseline.allow & TICKET_PERMISSIONS),
            deny: (current.deny & !TICKET_PERMISSIONS) | (baseline.deny & TICKET_PERMISSIONS),
            ..current
        }
    }
}

async fn current_channel(
    ctx: &EventContext,
    channel: ChannelId,
) -> Result<twilight_model::channel::Channel, UserError> {
    match ctx
        .api
        .execute(Action::GetChannel {
            channel_id: channel,
        })
        .await
    {
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
    Ok(found.parent_id.map(twilight_model::id::Id::get))
}

#[allow(clippy::too_many_lines)]
async fn validate_panel(
    ctx: &EventContext,
    ticket: &Ticket,
    settings: &Config,
    panel: Option<&NamedPanel>,
) -> Result<(Vec<u64>, Option<super::teams_survey::StaffAccessSnapshot>), UserError> {
    if let Some(key) = ticket_setup_error(settings, ctx.guild_id.get()) {
        return Err(UserError::message(key));
    }
    let mut base_roles = panel_roles(settings);
    base_roles.sort_unstable();
    base_roles.dedup();
    let selected_teams = panel.map_or(&[][..], |panel| panel.staff_teams.as_slice());
    let staff_access = super::teams_survey::staff_access_snapshot(
        &settings.staff_teams,
        selected_teams,
        &base_roles,
        ctx.guild_id.get(),
    )?;
    let mut staff_roles = staff_access.as_ref().map_or_else(
        || base_roles.iter().map(twilight_model::id::Id::get).collect(),
        super::teams_survey::staff_roles,
    );
    staff_roles.sort_unstable();
    staff_roles.dedup();
    let roles: Vec<_> = staff_roles.iter().copied().map(RoleId::new).collect();
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
    if roles
        .iter()
        .any(|role| !known.iter().any(|known| known.id == *role))
    {
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
        let mut old_roles = ticket.staff_roles.clone().unwrap_or_else(|| {
            ticket
                .settings
                .as_ref()
                .map_or_else(|| panel_roles(settings), panel_roles)
                .into_iter()
                .map(|role| role.get())
                .collect()
        });
        old_roles.sort_unstable();
        old_roles.dedup();
        if old_roles != staff_roles || ticket.staff_access != staff_access {
            return Err(UserError::message("tickets-thread-staff-change"));
        }
        thread::ensure_thread_parent_with_access(
            ctx,
            parent,
            &roles,
            UserId::new(ticket.opener),
            &super::team_staff_users(staff_access.as_ref()),
            staff_access.as_ref(),
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
        match ctx
            .api
            .execute(Action::GetChannel { channel_id: log })
            .await
        {
            Ok(Outcome::Channel(found))
                if matches!(
                    found.kind,
                    ChannelType::GuildText | ChannelType::GuildAnnouncement
                ) => {}
            Ok(_) | Err(DiscordError::NotFound) => {
                return Err(UserError::message("tickets-log-unavailable"));
            }
            Err(error) => return Err(failed(&error)),
        }
    }
    Ok((staff_roles, staff_access))
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
            Ok(_) => {
                if let Err(error) = schedule_operation_recovery(
                    ctx,
                    channel,
                    token,
                    ctx.now() + OPERATION_LEASE_SECONDS,
                )
                .await
                {
                    tracing::warn!(?error, %channel, "could not schedule ticket operation recovery");
                }
                return Ok(token);
            }
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
        let Some(entry) = ctx
            .store()
            .entry(&key)
            .await
            .map_err(|error| stored(&error))?
        else {
            return Err(ModuleError::other("the ticket no longer exists"));
        };
        let mut ticket: Ticket = serde_json::from_value(entry.value)
            .map_err(|error| ModuleError::other(error.to_string()))?;
        let Some(operation) = ticket.pending_operation.as_mut() else {
            return Err(ModuleError::other(
                "the ticket operation is no longer active",
            ));
        };
        if operation_token(operation) != token || ticket.closed || ticket.closing.is_some() {
            return Err(ModuleError::other(
                "the ticket operation is no longer active",
            ));
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
    Err(ModuleError::other(
        "the ticket changed during its operation",
    ))
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
        let Some(entry) = ctx
            .store()
            .entry(&key)
            .await
            .map_err(|error| stored(&error))?
        else {
            return Ok(None);
        };
        let mut ticket: Ticket = serde_json::from_value(entry.value)
            .map_err(|error| ModuleError::other(error.to_string()))?;
        let Some(operation) = ticket.pending_operation.as_mut() else {
            return Ok(None);
        };
        if ticket.closed || ticket.closing.is_some() {
            return Err(ModuleError::other(
                "a closing ticket has a pending operation",
            ));
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
        return Err(ModuleError::other(
            "the ticket kept changing during its operation",
        ));
    };
    run_owned(ctx, channel, token).await?;
    Ok(None)
}

pub(super) async fn retry_pending_job(cx: JobCx) -> Result<(), ModuleError> {
    let recovery: OperationRecovery = cx.payload()?;
    let ctx = cx.ctx();
    let channel = ChannelId::new(recovery.channel);
    let Some(ticket) = ctx
        .store()
        .get::<Ticket>(&ticket_key(channel))
        .await
        .map_err(|error| stored(&error))?
    else {
        return Ok(());
    };
    if !ticket.is_open() {
        return Ok(());
    }
    let Some(operation) = ticket.pending_operation.as_ref() else {
        return Ok(());
    };
    if operation_token(operation) != recovery.token {
        return Ok(());
    }
    if operation_lease(operation) > ctx.now() {
        return schedule_operation_recovery(
            ctx,
            channel,
            recovery.token,
            operation_lease(operation),
        )
        .await;
    }

    match Box::pin(resume_pending(ctx, channel)).await {
        Ok(Some(until)) => {
            if let Some(ticket) = ctx
                .store()
                .get::<Ticket>(&ticket_key(channel))
                .await
                .map_err(|error| stored(&error))?
                && let Some(operation) = ticket.pending_operation.as_ref()
            {
                return schedule_operation_recovery(
                    ctx,
                    channel,
                    operation_token(operation),
                    until.max(operation_lease(operation)),
                )
                .await;
            }
        }
        Ok(None) => {}
        Err(error) => {
            tracing::warn!(?error, %channel, "ticket operation recovery attempt failed");
            if let Some(ticket) = ctx
                .store()
                .get::<Ticket>(&ticket_key(channel))
                .await
                .map_err(|store_error| stored(&store_error))?
                && ticket.is_open()
                && let Some(operation) = ticket.pending_operation.as_ref()
            {
                schedule_operation_recovery(
                    ctx,
                    channel,
                    operation_token(operation),
                    (ctx.now() + OPERATION_RETRY_SECONDS).max(operation_lease(operation)),
                )
                .await?;
            }
        }
    }
    Ok(())
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
            if let Err(schedule_error) = schedule_operation_recovery(
                ctx,
                channel,
                token,
                ctx.now() + OPERATION_RETRY_SECONDS,
            )
            .await
            {
                tracing::warn!(?schedule_error, %channel, "could not retry ticket operation automatically");
            }
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
    match ctx
        .api
        .execute(Action::GetChannel {
            channel_id: channel,
        })
        .await
    {
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
        return Err(ModuleError::other(
            "role overwrites apply only to ticket text channels",
        ));
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

async fn set_user_access(
    ctx: &EventContext,
    channel: ChannelId,
    user: UserId,
    baseline: UserAccess,
    grant: bool,
    token: i64,
    reason: &Reason,
) -> Result<(), ModuleError> {
    let found = operation_channel(ctx, channel, token).await?;
    if found.kind != ChannelType::GuildText {
        return Err(ModuleError::other(
            "member overwrites apply only to ticket text channels",
        ));
    }
    let overwrites = found
        .permission_overwrites
        .as_deref()
        .ok_or_else(|| ModuleError::other("the ticket channel has no permission overwrites"))?;
    let current = user_access(overwrites, user).unwrap_or_else(|| empty_user_access(user));
    let next = adjust_user_access(current, baseline, grant);
    if current == next {
        return Ok(());
    }
    renew_operation(ctx, channel, token).await?;
    let action = if next.allow.is_empty() && next.deny.is_empty() {
        Action::DeletePermissionOverwrite {
            channel_id: channel,
            target: OverwriteTarget::Member(user),
            reason: reason.clone(),
        }
    } else {
        Action::SetPermissionOverwrite {
            channel_id: channel,
            overwrite: OverwriteSpec {
                target: OverwriteTarget::Member(user),
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

fn apply_team_policy(
    current_allow: Permissions,
    current_deny: Permissions,
    team_allow: Permissions,
    team_deny: Permissions,
) -> (Permissions, Permissions) {
    let base = Permissions::VIEW_CHANNEL | Permissions::READ_MESSAGE_HISTORY;
    let managed = base | team_allow | team_deny;
    (
        (current_allow & !managed) | base | team_allow,
        (current_deny & !managed) | team_deny,
    )
}

fn restore_team_policy(
    current_allow: Permissions,
    current_deny: Permissions,
    baseline_allow: Permissions,
    baseline_deny: Permissions,
    team_allow: Permissions,
    team_deny: Permissions,
) -> (Permissions, Permissions) {
    let base = Permissions::VIEW_CHANNEL | Permissions::READ_MESSAGE_HISTORY;
    let managed = base | team_allow | team_deny;
    (
        (current_allow & !managed) | (baseline_allow & managed),
        (current_deny & !managed) | (baseline_deny & managed),
    )
}

async fn set_role_overwrite(
    ctx: &EventContext,
    channel: ChannelId,
    role: RoleId,
    next: RoleAccess,
    token: i64,
    reason: &Reason,
) -> Result<(), ModuleError> {
    let found = operation_channel(ctx, channel, token).await?;
    if found.kind != ChannelType::GuildText {
        return Err(ModuleError::other(
            "role overwrites apply only to ticket text channels",
        ));
    }
    let overwrites = found
        .permission_overwrites
        .as_deref()
        .ok_or_else(|| ModuleError::other("the ticket channel has no permission overwrites"))?;
    let current = role_access(overwrites, role).unwrap_or_else(|| empty_access(role));
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

async fn set_user_overwrite(
    ctx: &EventContext,
    channel: ChannelId,
    user: UserId,
    next: UserAccess,
    token: i64,
    reason: &Reason,
) -> Result<(), ModuleError> {
    let found = operation_channel(ctx, channel, token).await?;
    if found.kind != ChannelType::GuildText {
        return Err(ModuleError::other(
            "member overwrites apply only to ticket text channels",
        ));
    }
    let overwrites = found
        .permission_overwrites
        .as_deref()
        .ok_or_else(|| ModuleError::other("the ticket channel has no permission overwrites"))?;
    let current = user_access(overwrites, user).unwrap_or_else(|| empty_user_access(user));
    if current == next {
        return Ok(());
    }
    renew_operation(ctx, channel, token).await?;
    let action = if next.allow.is_empty() && next.deny.is_empty() {
        Action::DeletePermissionOverwrite {
            channel_id: channel,
            target: OverwriteTarget::Member(user),
            reason: reason.clone(),
        }
    } else {
        Action::SetPermissionOverwrite {
            channel_id: channel,
            overwrite: OverwriteSpec {
                target: OverwriteTarget::Member(user),
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

async fn apply_team_role_policy(
    ctx: &EventContext,
    channel: ChannelId,
    role: RoleId,
    allow: Permissions,
    deny: Permissions,
    token: i64,
    reason: &Reason,
) -> Result<(), ModuleError> {
    let found = operation_channel(ctx, channel, token).await?;
    let overwrites = found
        .permission_overwrites
        .as_deref()
        .ok_or_else(|| ModuleError::other("the ticket channel has no permission overwrites"))?;
    let current = role_access(overwrites, role).unwrap_or_else(|| empty_access(role));
    let (allow, deny) = apply_team_policy(current.allow, current.deny, allow, deny);
    set_role_overwrite(
        ctx,
        channel,
        role,
        RoleAccess {
            role: role.get(),
            allow,
            deny,
        },
        token,
        reason,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn restore_team_role_policy(
    ctx: &EventContext,
    channel: ChannelId,
    role: RoleId,
    baseline: RoleAccess,
    allow: Permissions,
    deny: Permissions,
    participant: bool,
    token: i64,
    reason: &Reason,
) -> Result<(), ModuleError> {
    let found = operation_channel(ctx, channel, token).await?;
    let overwrites = found
        .permission_overwrites
        .as_deref()
        .ok_or_else(|| ModuleError::other("the ticket channel has no permission overwrites"))?;
    let current = role_access(overwrites, role).unwrap_or_else(|| empty_access(role));
    let (allow, deny) = restore_team_policy(
        current.allow,
        current.deny,
        baseline.allow,
        baseline.deny,
        allow,
        deny,
    );
    let mut next = RoleAccess {
        role: role.get(),
        allow,
        deny,
    };
    if participant {
        next = adjust_access(next, baseline, true);
    }
    set_role_overwrite(ctx, channel, role, next, token, reason).await
}

async fn apply_team_user_policy(
    ctx: &EventContext,
    channel: ChannelId,
    user: UserId,
    allow: Permissions,
    deny: Permissions,
    token: i64,
    reason: &Reason,
) -> Result<(), ModuleError> {
    let found = operation_channel(ctx, channel, token).await?;
    let overwrites = found
        .permission_overwrites
        .as_deref()
        .ok_or_else(|| ModuleError::other("the ticket channel has no permission overwrites"))?;
    let current = user_access(overwrites, user).unwrap_or_else(|| empty_user_access(user));
    let (allow, deny) = apply_team_policy(current.allow, current.deny, allow, deny);
    set_user_overwrite(
        ctx,
        channel,
        user,
        UserAccess {
            user: user.get(),
            allow,
            deny,
        },
        token,
        reason,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn restore_team_user_policy(
    ctx: &EventContext,
    channel: ChannelId,
    user: UserId,
    baseline: UserAccess,
    allow: Permissions,
    deny: Permissions,
    participant: bool,
    token: i64,
    reason: &Reason,
) -> Result<(), ModuleError> {
    let found = operation_channel(ctx, channel, token).await?;
    let overwrites = found
        .permission_overwrites
        .as_deref()
        .ok_or_else(|| ModuleError::other("the ticket channel has no permission overwrites"))?;
    let current = user_access(overwrites, user).unwrap_or_else(|| empty_user_access(user));
    let (allow, deny) = restore_team_policy(
        current.allow,
        current.deny,
        baseline.allow,
        baseline.deny,
        allow,
        deny,
    );
    let mut next = UserAccess {
        user: user.get(),
        allow,
        deny,
    };
    if participant {
        next = adjust_user_access(next, baseline, true);
    }
    set_user_overwrite(ctx, channel, user, next, token, reason).await
}

async fn save_panel_baselines(
    ctx: &EventContext,
    channel: ChannelId,
    token: i64,
    role_accesses: &[RoleAccess],
    user_accesses: &[UserAccess],
) -> Result<(), ModuleError> {
    let key = ticket_key(channel);
    for _ in 0..5 {
        let Some(entry) = ctx
            .store()
            .entry(&key)
            .await
            .map_err(|error| stored(&error))?
        else {
            return Err(ModuleError::other("the ticket no longer exists"));
        };
        let mut ticket: Ticket = serde_json::from_value(entry.value)
            .map_err(|error| ModuleError::other(error.to_string()))?;
        let Some(PendingOperation::Panel {
            token: current,
            lease_until,
            role_baselines,
            user_baselines,
            ..
        }) = ticket.pending_operation.as_mut()
        else {
            return Err(ModuleError::other("the panel change is no longer active"));
        };
        if *current != token {
            return Err(ModuleError::other("the panel change is no longer active"));
        }
        *role_baselines = Some(role_accesses.to_vec());
        *user_baselines = Some(user_accesses.to_vec());
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
    Err(ModuleError::other(
        "the ticket changed while saving panel permissions",
    ))
}

async fn save_role_baseline(
    ctx: &EventContext,
    channel: ChannelId,
    token: i64,
    baseline: RoleAccess,
) -> Result<(), ModuleError> {
    let key = ticket_key(channel);
    for _ in 0..5 {
        let Some(entry) = ctx
            .store()
            .entry(&key)
            .await
            .map_err(|error| stored(&error))?
        else {
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
    Err(ModuleError::other(
        "the ticket changed while saving role permissions",
    ))
}

async fn save_user_baseline(
    ctx: &EventContext,
    channel: ChannelId,
    token: i64,
    baseline: UserAccess,
) -> Result<(), ModuleError> {
    let key = ticket_key(channel);
    for _ in 0..5 {
        let Some(entry) = ctx
            .store()
            .entry(&key)
            .await
            .map_err(|error| stored(&error))?
        else {
            return Err(ModuleError::other("the ticket no longer exists"));
        };
        let mut ticket: Ticket = serde_json::from_value(entry.value)
            .map_err(|error| ModuleError::other(error.to_string()))?;
        let Some(PendingOperation::ParticipantUser {
            token: current,
            lease_until,
            baseline: saved,
            ..
        }) = ticket.pending_operation.as_mut()
        else {
            return Err(ModuleError::other("the member change is no longer active"));
        };
        if *current != token {
            return Err(ModuleError::other("the member change is no longer active"));
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
    Err(ModuleError::other(
        "the ticket changed while saving member permissions",
    ))
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
        return Err(ModuleError::other(
            "the ticket channel is not a text channel",
        ));
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

#[allow(clippy::too_many_arguments)]
async fn finish_panel(
    ctx: &EventContext,
    channel: ChannelId,
    token: i64,
    panel: Option<NamedPanel>,
    settings: &Config,
    staff_roles: &[u64],
    staff_access: Option<&super::teams_survey::StaffAccessSnapshot>,
    role_baselines: &[RoleAccess],
    user_baselines: &[UserAccess],
) -> Result<(), ModuleError> {
    let key = ticket_key(channel);
    for _ in 0..5 {
        let Some(entry) = ctx
            .store()
            .entry(&key)
            .await
            .map_err(|error| stored(&error))?
        else {
            return Err(ModuleError::other("the ticket no longer exists"));
        };
        let mut ticket: Ticket = serde_json::from_value(entry.value)
            .map_err(|error| ModuleError::other(error.to_string()))?;
        if ticket
            .pending_operation
            .as_ref()
            .is_none_or(|operation| operation_token(operation) != token)
        {
            return Err(ModuleError::other("the panel change is no longer active"));
        }
        ticket.panel = panel.clone();
        ticket.settings = Some(settings.clone());
        ticket.staff_roles = Some(staff_roles.to_vec());
        ticket.staff_access = staff_access.cloned();
        ticket.panel_role_baselines = role_baselines.to_vec();
        let mut saved_user_baselines = user_baselines.to_vec();
        for baseline in &ticket.panel_user_baselines {
            if ticket.participant_users.contains(&baseline.user)
                && !saved_user_baselines
                    .iter()
                    .any(|saved| saved.user == baseline.user)
            {
                saved_user_baselines.push(*baseline);
            }
        }
        saved_user_baselines.sort_unstable_by_key(|saved| saved.user);
        ticket.panel_user_baselines = saved_user_baselines;
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
                if settings.close_after_days > 0
                    && let Err(error) = super::watch(
                        ctx,
                        channel,
                        ctx.now() + i64::from(settings.close_after_days) * super::DAY,
                    )
                    .await
                {
                    tracing::warn!(?error, %channel, "could not reschedule ticket idle close");
                }
                return Ok(());
            }
            Err(StorageError::Conflict) => {}
            Err(error) => return Err(stored(&error)),
        }
    }
    Err(ModuleError::other(
        "the ticket changed while saving its panel",
    ))
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
        let Some(entry) = ctx
            .store()
            .entry(&key)
            .await
            .map_err(|error| stored(&error))?
        else {
            return Err(ModuleError::other("the ticket no longer exists"));
        };
        let mut ticket: Ticket = serde_json::from_value(entry.value)
            .map_err(|error| ModuleError::other(error.to_string()))?;
        if ticket
            .pending_operation
            .as_ref()
            .is_none_or(|operation| operation_token(operation) != token)
        {
            return Err(ModuleError::other("the role change is no longer active"));
        }
        ticket
            .participant_roles
            .retain(|access| access.role != role.get());
        if add {
            ticket.participant_roles.push(baseline);
            ticket
                .participant_roles
                .sort_unstable_by_key(|access| access.role);
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
    Err(ModuleError::other(
        "the ticket changed while saving its participant role",
    ))
}

async fn commit_user(
    ctx: &EventContext,
    channel: ChannelId,
    token: i64,
    user: UserId,
    add: bool,
    baseline: Option<UserAccess>,
) -> Result<(), ModuleError> {
    let key = ticket_key(channel);
    for _ in 0..5 {
        let Some(entry) = ctx
            .store()
            .entry(&key)
            .await
            .map_err(|error| stored(&error))?
        else {
            return Err(ModuleError::other("the ticket no longer exists"));
        };
        let mut ticket: Ticket = serde_json::from_value(entry.value)
            .map_err(|error| ModuleError::other(error.to_string()))?;
        if ticket
            .pending_operation
            .as_ref()
            .is_none_or(|operation| operation_token(operation) != token)
        {
            return Err(ModuleError::other("the member change is no longer active"));
        }
        ticket
            .participant_users
            .retain(|saved| *saved != user.get());
        if add {
            ticket.participant_users.push(user.get());
            ticket.participant_users.sort_unstable();
            ticket.participant_users.dedup();
            if let Some(baseline) = baseline {
                ticket
                    .panel_user_baselines
                    .retain(|saved| saved.user != user.get());
                ticket.panel_user_baselines.push(baseline);
                ticket
                    .panel_user_baselines
                    .sort_unstable_by_key(|saved| saved.user);
            }
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
    Err(ModuleError::other(
        "the ticket changed while saving its participant member",
    ))
}

#[allow(clippy::too_many_lines)]
async fn apply_panel_forward(
    ctx: &EventContext,
    channel: ChannelId,
    token: i64,
) -> Result<(), ModuleError> {
    let ticket = renew_operation(ctx, channel, token).await?;
    let (
        panel,
        settings,
        saved_staff_roles,
        old_staff_roles,
        mut target_access,
        old_access,
        saved_role_baselines,
        saved_user_baselines,
    ) = match ticket.pending_operation.as_ref() {
        Some(PendingOperation::Panel {
            phase: PanelPhase::Forward,
            panel,
            settings,
            staff_roles,
            old_staff_roles,
            target_staff_access,
            old_staff_access,
            role_baselines,
            user_baselines,
            ..
        }) => (
            panel.as_deref().cloned(),
            settings.as_ref().clone(),
            staff_roles.clone(),
            old_staff_roles.clone(),
            target_staff_access.clone(),
            old_staff_access
                .clone()
                .or_else(|| ticket.staff_access.clone()),
            role_baselines.clone(),
            user_baselines.clone(),
        ),
        _ => return Err(ModuleError::other("the panel change is no longer active")),
    };

    if target_access.is_none()
        && panel
            .as_ref()
            .is_some_and(|panel| !panel.staff_teams.is_empty())
    {
        let base_roles = panel_roles(&settings);
        target_access = super::teams_survey::staff_access_snapshot(
            &settings.staff_teams,
            panel
                .as_ref()
                .map_or(&[][..], |panel| panel.staff_teams.as_slice()),
            &base_roles,
            ctx.guild_id.get(),
        )
        .map_err(|_| ModuleError::other("the target team access could not be restored"))?;
    }
    let target_staff_roles = target_access.as_ref().map_or_else(
        || saved_staff_roles.clone(),
        super::teams_survey::staff_roles,
    );
    let target_roles: Vec<_> = target_staff_roles
        .iter()
        .copied()
        .map(RoleId::new)
        .collect();
    let old_roles: Vec<_> = old_staff_roles.iter().copied().map(RoleId::new).collect();
    let old_team_roles = old_access
        .as_ref()
        .map(super::teams_survey::role_overwrites)
        .unwrap_or_default();
    let old_team_users = old_access
        .as_ref()
        .map(super::teams_survey::user_overwrites)
        .unwrap_or_default();
    let target_team_roles = target_access
        .as_ref()
        .map(super::teams_survey::role_overwrites)
        .unwrap_or_default();
    let target_team_users = target_access
        .as_ref()
        .map(super::teams_survey::user_overwrites)
        .unwrap_or_default();
    let reason = Reason::truncated(&ctx.t_in(ctx.locale, "tickets-reason-panel", &Args::new()));

    if ticket.thread {
        let parent = ticket
            .thread_parent
            .map(ChannelId::new)
            .ok_or_else(|| ModuleError::other("ticket thread parent is missing"))?;
        thread::ensure_thread_parent_with_access(
            ctx,
            parent,
            &target_roles,
            UserId::new(ticket.opener),
            &super::team_staff_users(target_access.as_ref()),
            target_access.as_ref(),
        )
        .await
        .map_err(|_| ModuleError::other("ticket thread staff access changed"))?;
        return finish_panel(
            ctx,
            channel,
            token,
            panel,
            &settings,
            &target_staff_roles,
            target_access.as_ref(),
            &[],
            &[],
        )
        .await;
    }

    let found = operation_channel(ctx, channel, token).await?;
    let overwrites = found
        .permission_overwrites
        .as_deref()
        .ok_or_else(|| ModuleError::other("the ticket channel has no permission overwrites"))?;
    let role_baselines = if let Some(baselines) = saved_role_baselines.clone() {
        baselines
    } else {
        target_roles
            .iter()
            .map(|role| {
                if ticket
                    .panel_role_baselines
                    .iter()
                    .any(|saved| saved.role == role.get())
                    || ticket
                        .participant_roles
                        .iter()
                        .any(|saved| saved.role == role.get())
                    || old_roles.contains(role)
                {
                    saved_role_access(&ticket, *role)
                } else {
                    role_access(overwrites, *role).unwrap_or_else(|| empty_access(*role))
                }
            })
            .collect::<Vec<_>>()
    };
    let opener = UserId::new(ticket.opener);
    let bot = ctx.bot();
    let mut target_users = target_team_users
        .iter()
        .map(|(user, _, _)| *user)
        .filter(|user| *user != opener && Some(*user) != bot)
        .collect::<Vec<_>>();
    target_users.sort_unstable();
    target_users.dedup();
    let user_baselines = if let Some(baselines) = saved_user_baselines.clone() {
        baselines
    } else {
        target_users
            .iter()
            .map(|user| {
                if ticket
                    .panel_user_baselines
                    .iter()
                    .any(|saved| saved.user == user.get())
                    || old_team_users.iter().any(|(staff, _, _)| staff == user)
                    || ticket.participant_users.contains(&user.get())
                {
                    saved_user_access(&ticket, *user)
                } else {
                    user_access(overwrites, *user).unwrap_or_else(|| empty_user_access(*user))
                }
            })
            .collect::<Vec<_>>()
    };
    if saved_role_baselines.is_none() || saved_user_baselines.is_none() {
        save_panel_baselines(ctx, channel, token, &role_baselines, &user_baselines).await?;
    }

    if let Some(snapshot) = target_access.as_ref() {
        for (role, allow, deny) in &target_team_roles {
            apply_team_role_policy(ctx, channel, *role, *allow, *deny, token, &reason).await?;
        }
        for (user, allow, deny) in &target_team_users {
            if *user == opener
                || Some(*user) == bot
                || ticket.participant_users.contains(&user.get())
            {
                continue;
            }
            apply_team_user_policy(ctx, channel, *user, *allow, *deny, token, &reason).await?;
        }
        let _ = snapshot;
    } else {
        if let Some(_old_snapshot) = old_access.as_ref() {
            for (role, allow, deny) in &old_team_roles {
                let baseline = ticket
                    .panel_role_baselines
                    .iter()
                    .find(|saved| saved.role == role.get())
                    .copied()
                    .unwrap_or_else(|| empty_access(*role));
                restore_team_role_policy(
                    ctx,
                    channel,
                    *role,
                    baseline,
                    *allow,
                    *deny,
                    ticket
                        .participant_roles
                        .iter()
                        .any(|saved| saved.role == role.get()),
                    token,
                    &reason,
                )
                .await?;
            }
            for (user, allow, deny) in &old_team_users {
                if *user == opener || Some(*user) == bot {
                    continue;
                }
                let baseline = saved_user_access(&ticket, *user);
                restore_team_user_policy(
                    ctx,
                    channel,
                    *user,
                    baseline,
                    *allow,
                    *deny,
                    ticket.participant_users.contains(&user.get()),
                    token,
                    &reason,
                )
                .await?;
            }
        }
        for role in &target_roles {
            let baseline = role_baselines
                .iter()
                .find(|saved| saved.role == role.get())
                .copied()
                .unwrap_or_else(|| empty_access(*role));
            set_role_access(ctx, channel, *role, baseline, true, token, &reason).await?;
        }
    }

    move_text_ticket(ctx, channel, settings.category(), token, &reason).await?;

    if target_access.is_some() {
        for (role, allow, deny) in &old_team_roles {
            if target_roles.contains(role) {
                continue;
            }
            let baseline = ticket
                .panel_role_baselines
                .iter()
                .find(|saved| saved.role == role.get())
                .copied()
                .unwrap_or_else(|| empty_access(*role));
            restore_team_role_policy(
                ctx,
                channel,
                *role,
                baseline,
                *allow,
                *deny,
                ticket
                    .participant_roles
                    .iter()
                    .any(|saved| saved.role == role.get()),
                token,
                &reason,
            )
            .await?;
        }
        for (user, allow, deny) in &old_team_users {
            if *user == opener || Some(*user) == bot || target_users.contains(user) {
                continue;
            }
            let baseline = saved_user_access(&ticket, *user);
            restore_team_user_policy(
                ctx,
                channel,
                *user,
                baseline,
                *allow,
                *deny,
                ticket.participant_users.contains(&user.get()),
                token,
                &reason,
            )
            .await?;
        }
    }
    if old_access.is_none() {
        for role in old_roles.iter().filter(|role| !target_roles.contains(role)) {
            if ticket
                .participant_roles
                .iter()
                .any(|saved| saved.role == role.get())
            {
                continue;
            }
            let baseline = ticket
                .panel_role_baselines
                .iter()
                .find(|saved| saved.role == role.get())
                .copied()
                .unwrap_or_else(|| empty_access(*role));
            set_role_access(ctx, channel, *role, baseline, false, token, &reason).await?;
        }
    }

    finish_panel(
        ctx,
        channel,
        token,
        panel,
        &settings,
        &target_staff_roles,
        target_access.as_ref(),
        &role_baselines,
        &user_baselines,
    )
    .await
}

async fn mark_panel_rollback(
    ctx: &EventContext,
    channel: ChannelId,
    token: i64,
) -> Result<(), ModuleError> {
    let key = ticket_key(channel);
    for _ in 0..5 {
        let Some(entry) = ctx
            .store()
            .entry(&key)
            .await
            .map_err(|error| stored(&error))?
        else {
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
    Err(ModuleError::other(
        "the ticket changed while preparing its rollback",
    ))
}

async fn clear_panel_operation(
    ctx: &EventContext,
    channel: ChannelId,
    token: i64,
) -> Result<(), ModuleError> {
    let key = ticket_key(channel);
    for _ in 0..5 {
        let Some(entry) = ctx
            .store()
            .entry(&key)
            .await
            .map_err(|error| stored(&error))?
        else {
            return Err(ModuleError::other("the ticket no longer exists"));
        };
        let mut ticket: Ticket = serde_json::from_value(entry.value)
            .map_err(|error| ModuleError::other(error.to_string()))?;
        if ticket
            .pending_operation
            .as_ref()
            .is_none_or(|operation| operation_token(operation) != token)
        {
            return Err(ModuleError::other(
                "the panel operation is no longer active",
            ));
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
    Err(ModuleError::other(
        "the ticket changed while clearing its panel operation",
    ))
}

#[allow(clippy::too_many_lines)]
async fn apply_panel_rollback(
    ctx: &EventContext,
    channel: ChannelId,
    token: i64,
) -> Result<(), ModuleError> {
    let ticket = renew_operation(ctx, channel, token).await?;
    let (
        settings,
        old_staff_roles,
        old_parent,
        target_access,
        old_access,
        target_baselines,
        target_user_baselines,
    ) = match ticket.pending_operation.as_ref() {
        Some(PendingOperation::Panel {
            settings,
            old_staff_roles,
            old_parent,
            target_staff_access,
            old_staff_access,
            role_baselines,
            user_baselines,
            phase: PanelPhase::Rollback,
            ..
        }) => (
            settings.as_ref().clone(),
            old_staff_roles.clone(),
            *old_parent,
            target_staff_access.clone(),
            old_staff_access
                .clone()
                .or_else(|| ticket.staff_access.clone()),
            role_baselines.clone(),
            user_baselines.clone(),
        ),
        _ => return Err(ModuleError::other("the panel rollback is no longer active")),
    };
    if ticket.thread {
        return clear_panel_operation(ctx, channel, token).await;
    }
    let reason = Reason::truncated(&ctx.t_in(ctx.locale, "tickets-reason-panel", &Args::new()));
    let old_roles: Vec<_> = old_staff_roles.iter().copied().map(RoleId::new).collect();
    let target_staff_roles = target_access.as_ref().map_or_else(
        || {
            panel_roles(&settings)
                .iter()
                .map(|role| role.get())
                .collect()
        },
        super::teams_survey::staff_roles,
    );
    let target_roles: Vec<_> = target_staff_roles
        .iter()
        .copied()
        .map(RoleId::new)
        .collect();
    let old_team_roles = old_access
        .as_ref()
        .map(super::teams_survey::role_overwrites)
        .unwrap_or_default();
    let old_team_users = old_access
        .as_ref()
        .map(super::teams_survey::user_overwrites)
        .unwrap_or_default();
    let target_team_users = target_access
        .as_ref()
        .map(super::teams_survey::user_overwrites)
        .unwrap_or_default();
    let opener = UserId::new(ticket.opener);
    let bot = ctx.bot();

    if old_access.is_some() {
        for (role, allow, deny) in &old_team_roles {
            apply_team_role_policy(ctx, channel, *role, *allow, *deny, token, &reason).await?;
        }
        for (user, allow, deny) in &old_team_users {
            if *user == opener
                || Some(*user) == bot
                || ticket.participant_users.contains(&user.get())
            {
                continue;
            }
            apply_team_user_policy(ctx, channel, *user, *allow, *deny, token, &reason).await?;
        }
    } else {
        for role in &old_roles {
            let baseline = ticket
                .panel_role_baselines
                .iter()
                .find(|saved| saved.role == role.get())
                .copied()
                .unwrap_or_else(|| empty_access(*role));
            set_role_access(ctx, channel, *role, baseline, true, token, &reason).await?;
        }
    }

    move_text_ticket(ctx, channel, old_parent.map(ChannelId::new), token, &reason).await?;

    let role_baselines = target_baselines.unwrap_or_default();
    for role in target_roles.iter().filter(|role| !old_roles.contains(role)) {
        let baseline = role_baselines
            .iter()
            .find(|saved| saved.role == role.get())
            .copied()
            .unwrap_or_else(|| empty_access(*role));
        if ticket
            .participant_roles
            .iter()
            .any(|saved| saved.role == role.get())
        {
            set_role_access(ctx, channel, *role, baseline, true, token, &reason).await?;
        } else {
            set_role_overwrite(ctx, channel, *role, baseline, token, &reason).await?;
        }
    }

    if target_access.is_some() {
        for (user, _, _) in &target_team_users {
            if *user == opener
                || Some(*user) == bot
                || old_team_users
                    .iter()
                    .any(|(old_user, _, _)| old_user == user)
                || ticket.participant_users.contains(&user.get())
            {
                continue;
            }
            let baseline = target_user_baselines
                .as_ref()
                .and_then(|baselines| baselines.iter().find(|saved| saved.user == user.get()))
                .copied()
                .unwrap_or_else(|| empty_user_access(*user));
            set_user_overwrite(ctx, channel, *user, baseline, token, &reason).await?;
        }
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
        }) => (RoleId::new(*role), *add, staff_roles.clone(), *baseline),
        _ => return Err(ModuleError::other("the role change is no longer active")),
    };
    if ticket.thread {
        return Err(ModuleError::other(
            "role participants are not supported in private ticket threads",
        ));
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

async fn is_protected_team_user(
    ctx: &EventContext,
    ticket: &Ticket,
    user: UserId,
) -> Result<bool, ModuleError> {
    let Some(snapshot) = ticket.staff_access.as_ref() else {
        return Ok(false);
    };
    if snapshot
        .teams
        .iter()
        .any(|team| team.users.contains(&user.get()))
    {
        return Ok(true);
    }
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
            return Err(ModuleError::other(
                "could not verify the ticket team member",
            ));
        }
        Err(error) => return Err(stored(&error)),
    };
    let staff_roles = super::teams_survey::staff_roles(snapshot);
    Ok(member
        .roles
        .iter()
        .any(|role| staff_roles.contains(&role.get())))
}

async fn apply_participant_user(
    ctx: &EventContext,
    channel: ChannelId,
    token: i64,
) -> Result<(), ModuleError> {
    let ticket = renew_operation(ctx, channel, token).await?;
    let (user, add, saved_baseline) = match ticket.pending_operation.as_ref() {
        Some(PendingOperation::ParticipantUser {
            user,
            add,
            baseline,
            ..
        }) => (UserId::new(*user), *add, *baseline),
        _ => return Err(ModuleError::other("the member change is no longer active")),
    };
    if is_protected_team_user(ctx, &ticket, user).await? {
        return Err(ModuleError::other(
            "ticket team staff cannot be managed as participants",
        ));
    }
    if ticket.thread {
        let parent = ticket
            .thread_parent
            .map(ChannelId::new)
            .ok_or_else(|| ModuleError::other("ticket thread parent is missing"))?;
        thread::change_member(ctx, channel, parent, user, add)
            .await
            .map_err(|error| ModuleError::other(format!("{error:?}")))?;
        return commit_user(ctx, channel, token, user, add, None).await;
    }

    let baseline = if let Some(baseline) = saved_baseline {
        baseline
    } else if let Some(baseline) = ticket
        .panel_user_baselines
        .iter()
        .find(|access| access.user == user.get())
        .copied()
    {
        baseline
    } else {
        let found = operation_channel(ctx, channel, token).await?;
        let overwrites = found
            .permission_overwrites
            .as_deref()
            .ok_or_else(|| ModuleError::other("the ticket channel has no permission overwrites"))?;
        user_access(overwrites, user).unwrap_or_else(|| empty_user_access(user))
    };
    if saved_baseline.is_none() {
        save_user_baseline(ctx, channel, token, baseline).await?;
    }
    let reason = Reason::truncated(&ctx.t_in(ctx.locale, "tickets-reason-member", &Args::new()));
    set_user_access(ctx, channel, user, baseline, add, token, &reason).await?;
    commit_user(ctx, channel, token, user, add, Some(baseline)).await
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
        Some(PendingOperation::Panel { .. }) => {
            match apply_panel_forward(ctx, channel, token).await {
                Ok(()) => Ok(ApplyResult::Done),
                Err(error) => {
                    tracing::warn!(?error, %channel, "panel change failed; restoring the previous panel");
                    mark_panel_rollback(ctx, channel, token).await?;
                    apply_panel_rollback(ctx, channel, token).await?;
                    Ok(ApplyResult::RolledBack)
                }
            }
        }
        Some(PendingOperation::ParticipantRole { .. }) => {
            apply_participant_role(ctx, channel, token).await?;
            Ok(ApplyResult::Done)
        }
        Some(PendingOperation::ParticipantUser { .. }) => {
            apply_participant_user(ctx, channel, token).await?;
            Ok(ApplyResult::Done)
        }
        None => Err(ModuleError::other(
            "the ticket operation is no longer active",
        )),
    }
}

#[allow(clippy::too_many_lines)]
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
    if !is_ticket_staff(&cx, &ticket, &config.for_ticket(&ticket)).await {
        return Err(UserError::message("tickets-staff-only"));
    }
    cx.defer(true).await?;
    if !super::valid_form_configuration(&config) {
        return cx
            .reply_private_t("tickets-form-invalid", &Args::new())
            .await;
    }
    if !super::teams_survey::valid_thread_staff_team_configuration(&config) {
        return cx
            .reply_private_t("tickets-team-thread-conflict", &Args::new())
            .await;
    }
    if Box::pin(resume_pending(ctx, channel))
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
    if !is_ticket_staff(&cx, &ticket, &current).await {
        return Err(UserError::message("tickets-staff-only"));
    }
    let (panel, settings) = if let Some(id) = args.panel {
        let panel = config.named(&id)?.clone();
        let settings = config.with_panel(&panel);
        (Some(panel), settings)
    } else {
        (None, config.clone())
    };
    let (staff_roles, target_staff_access) =
        validate_panel(ctx, &ticket, &settings, panel.as_ref()).await?;
    let old_staff_roles = ticket
        .staff_roles
        .clone()
        .unwrap_or_else(|| current.staff().iter().map(|role| role.get()).collect());
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
            panel: panel.clone().map(Box::new),
            settings: Box::new(settings.clone()),
            staff_roles,
            old_staff_roles,
            old_parent,
            target_staff_access,
            old_staff_access: ticket.staff_access.clone(),
            role_baselines: None,
            user_baselines: None,
        },
    )
    .await?;
    match run_owned(ctx, channel, token).await {
        Ok(ApplyResult::Done) => {
            let audit_key = format!("ticket-panel-{}", cx.invocation().interaction_id.get());
            if let Err(error) = super::audit::record_event(
                ctx,
                cx.user().id.get(),
                super::audit::AuditAction::TicketPanelChange,
                super::audit::AuditOutcome::Success,
                &audit_key,
            )
            .await
            {
                tracing::warn!(
                    ?error,
                    "could not record ticket panel operation audit event"
                );
            }
            let args = panel.as_ref().map_or_else(Args::new, |panel| {
                Args::new().with("panel", panel.id.clone())
            });
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
            cx.reply_private_t("tickets-panel-restored", &Args::new())
                .await
        }
        Err(error) => {
            tracing::warn!(?error, %channel, "panel operation remains pending");
            cx.reply_private_t("tickets-operation-pending", &Args::new())
                .await
        }
    }
}

#[allow(clippy::too_many_lines)]
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
    if !is_ticket_staff(&cx, &ticket, &config.for_ticket(&ticket)).await {
        return Err(UserError::message("tickets-staff-only"));
    }
    if ticket.thread {
        return Err(UserError::message("tickets-thread-role-unavailable"));
    }
    cx.defer(true).await?;
    if Box::pin(resume_pending(ctx, channel))
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
    if !is_ticket_staff(&cx, &ticket, &current).await {
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
    let staff_roles: Vec<u64> = ticket
        .staff_roles
        .clone()
        .unwrap_or_else(|| current.staff().iter().map(|role| role.get()).collect());
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
            let audit_action = if add {
                super::audit::AuditAction::TicketMemberAdd
            } else {
                super::audit::AuditAction::TicketMemberRemove
            };
            let audit_key = format!("ticket-role-{}", cx.invocation().interaction_id.get());
            if let Err(error) = super::audit::record_event(
                ctx,
                cx.user().id.get(),
                audit_action,
                super::audit::AuditOutcome::Success,
                &audit_key,
            )
            .await
            {
                tracing::warn!(?error, "could not record ticket role operation audit event");
            }
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
    Box::pin(change_role(cx, args, true)).await
}

pub(super) async fn remove_role(cx: CommandCx, args: RoleArgs) -> Result<(), UserError> {
    Box::pin(change_role(cx, args, false)).await
}

pub(super) async fn change_member(cx: CommandCx, user: UserId, add: bool) -> Result<(), UserError> {
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
    if !is_ticket_staff(&cx, &ticket, &config.for_ticket(&ticket)).await {
        return Err(UserError::message("tickets-staff-only"));
    }
    cx.defer(true).await?;
    if Box::pin(resume_pending(ctx, channel))
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
    if ticket.closed || ticket.closing.is_some() || !ticket.is_open() {
        return Err(UserError::message("tickets-not-a-ticket"));
    }
    if !is_ticket_staff(&cx, &ticket, &current).await {
        return Err(UserError::message("tickets-staff-only"));
    }
    match is_protected_team_user(ctx, &ticket, user).await {
        Ok(true) => {
            return cx
                .reply_private_t("tickets-team-member-protected", &Args::new())
                .await;
        }
        Err(error) => {
            tracing::warn!(?error, %user, "could not verify whether a ticket participant is team staff");
            return cx
                .reply_private_t("tickets-team-member-check-failed", &Args::new())
                .await;
        }
        Ok(false) => {}
    }
    let already_added = ticket.participant_users.contains(&user.get());
    if add && already_added {
        return cx
            .reply_private_t("tickets-member-already-added", &Args::new())
            .await;
    }
    if !add && !already_added {
        return cx
            .reply_private_t("tickets-member-not-added", &Args::new())
            .await;
    }
    let token = stage(
        ctx,
        channel,
        PendingOperation::ParticipantUser {
            token: 0,
            lease_until: 0,
            user: user.get(),
            add,
            baseline: None,
        },
    )
    .await?;
    match run_owned(ctx, channel, token).await {
        Ok(ApplyResult::Done) => {
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
        Ok(ApplyResult::RolledBack) => {
            cx.reply_private_t("tickets-operation-pending", &Args::new())
                .await
        }
        Err(error) => {
            tracing::warn!(?error, %channel, %user, "participant user operation remains pending");
            cx.reply_private_t("tickets-operation-pending", &Args::new())
                .await
        }
    }
}

pub(super) async fn waiting(cx: CommandCx, args: WaitingArgs) -> Result<(), UserError> {
    let ctx = cx.ctx();
    let channel = cx.invocation().channel_id;
    let config: Config = cx.config()?;
    cx.defer(true).await?;
    if Box::pin(resume_pending(ctx, channel))
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
        if !is_ticket_staff(&cx, &ticket, &config.for_ticket(&ticket)).await {
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
                if changed {
                    let audit_key =
                        format!("ticket-waiting-{}", cx.invocation().interaction_id.get());
                    if let Err(error) = super::audit::record_event(
                        ctx,
                        cx.user().id.get(),
                        super::audit::AuditAction::TicketWaitingUpdate,
                        super::audit::AuditOutcome::Success,
                        &audit_key,
                    )
                    .await
                    {
                        tracing::warn!(
                            ?error,
                            "could not record ticket waiting status audit event"
                        );
                    }
                }
                let message = if args.waiting {
                    "tickets-waiting-set"
                } else {
                    "tickets-waiting-cleared"
                };
                if changed
                    && let Err(error) = ctx
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
                return cx.reply_private_t(message, &Args::new()).await;
            }
            Err(StorageError::Conflict) => {}
            Err(error) => return Err(failed(&error)),
        }
    }
    Err(UserError::message("tickets-failed"))
}
