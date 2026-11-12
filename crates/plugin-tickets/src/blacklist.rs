use closure_core::action::Action;
use closure_core::error::DiscordError;
use closure_core::event::{RoleId, UserId};
use closure_core::module::EventContext;
use closure_core::storage::{Expect, StorageError};
use closure_sdk::{Args, CommandArgs, CommandCx, UserError};
use serde::{Deserialize, Serialize};

use super::{Config, failed, is_staff_command};

const PREFIX: &str = "ticket-blacklist";
const CAS_ATTEMPTS: usize = 5;
const PAGE_SIZE: usize = 25;

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub(super) enum Target {
    User { id: String },
    Role { id: String },
}

impl Target {
    fn user(user: UserId) -> Self {
        Self::User {
            id: user.get().to_string(),
        }
    }

    fn role(role: RoleId) -> Self {
        Self::Role {
            id: role.get().to_string(),
        }
    }

    fn kind(&self) -> &'static str {
        match self {
            Self::User { .. } => "user",
            Self::Role { .. } => "role",
        }
    }

    fn id(&self) -> &str {
        match self {
            Self::User { id } | Self::Role { id } => id,
        }
    }

    fn canonical(&self, guild_id: u64) -> Result<Self, UserError> {
        let id = self
            .id()
            .parse::<u64>()
            .ok()
            .filter(|id| *id > 0)
            .ok_or_else(|| UserError::message("tickets-blacklist-target-invalid"))?;
        if matches!(self, Self::Role { .. }) && id == guild_id {
            return Err(UserError::message("tickets-blacklist-target-invalid"));
        }
        Ok(match self {
            Self::User { .. } => Self::User { id: id.to_string() },
            Self::Role { .. } => Self::Role { id: id.to_string() },
        })
    }

    fn label(&self, cx: &CommandCx) -> String {
        format!(
            "{} {}",
            cx.t(
                match self {
                    Self::User { .. } => "tickets-blacklist-kind-user",
                    Self::Role { .. } => "tickets-blacklist-kind-role",
                },
                &Args::new(),
            ),
            self.id()
        )
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub(super) struct State {
    pub(super) entries: Vec<Target>,
}

#[derive(Debug, Clone)]
pub(super) struct Snapshot {
    /// The store revision used for dashboard ETag and If-Match.
    pub(super) revision: Option<i64>,
    pub(super) state: State,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum RevisionGuard {
    /// Retry conflicts against the latest value; used by Discord commands.
    Any,
    /// The dashboard read that this write follows found no state value.
    Missing,
    /// The dashboard read that this write follows returned this revision.
    Revision(i64),
}

#[derive(Debug, Clone)]
pub(super) struct ChangeResult {
    pub(super) snapshot: Snapshot,
    pub(super) changed: bool,
}

#[derive(Debug, Clone, Copy)]
enum Operation {
    Add,
    Remove,
}

#[derive(CommandArgs)]
pub(super) struct UserArgs {
    /// The member to block or unblock
    user: UserId,
    /// Limit the entry to one ticket panel
    #[arg(max_length = 32)]
    panel: Option<String>,
}

#[derive(CommandArgs)]
pub(super) struct RoleArgs {
    /// The role to block or unblock
    role: RoleId,
    /// Limit the entry to one ticket panel
    #[arg(max_length = 32)]
    panel: Option<String>,
}

#[derive(CommandArgs)]
pub(super) struct ScopeArgs {
    /// Show entries for one ticket panel
    #[arg(max_length = 32)]
    panel: Option<String>,
    /// The page number
    page: Option<i64>,
}

fn key(ctx: &EventContext, panel: Option<&str>) -> String {
    match panel {
        Some(panel) => format!("{PREFIX}:{}:panel:{panel}:state", ctx.guild_id.get()),
        None => format!("{PREFIX}:{}:guild:state", ctx.guild_id.get()),
    }
}

pub(super) fn scope_config(config: &Config, panel: Option<&str>) -> Result<Config, UserError> {
    match panel {
        Some(id) => Ok(config.with_panel(config.named(id)?)),
        None => Ok(config.clone()),
    }
}

pub(super) fn staff_for_actor(
    config: &Config,
    effective_roles: &[RoleId],
    tickets_manage: bool,
) -> bool {
    tickets_manage
        || config
            .staff()
            .iter()
            .any(|role| effective_roles.contains(role))
}

pub(super) async fn dashboard_state(
    ctx: &EventContext,
    config: &Config,
    effective_roles: &[RoleId],
    tickets_manage: bool,
    panel: Option<&str>,
) -> Result<Snapshot, UserError> {
    let scope = scope_config(config, panel)?;
    if !staff_for_actor(&scope, effective_roles, tickets_manage) {
        return Err(UserError::message("tickets-staff-only"));
    }
    state(ctx, panel).await
}

pub(super) async fn dashboard_change(
    ctx: &EventContext,
    config: &Config,
    effective_roles: &[RoleId],
    tickets_manage: bool,
    panel: Option<&str>,
    target: Target,
    add: bool,
    guard: RevisionGuard,
) -> Result<ChangeResult, UserError> {
    let scope = scope_config(config, panel)?;
    if !staff_for_actor(&scope, effective_roles, tickets_manage) {
        return Err(UserError::message("tickets-staff-only"));
    }
    change(ctx, panel, target, add, guard).await
}

pub(super) async fn state(ctx: &EventContext, panel: Option<&str>) -> Result<Snapshot, UserError> {
    let key = key(ctx, panel);
    let entries = ctx.store().list(&key, 1).await.map_err(|error| {
        tracing::warn!(?error, guild = %ctx.guild_id, "could not read ticket blacklist");
        UserError::message("tickets-failed")
    })?;
    let entry = entries
        .into_iter()
        .find(|(stored_key, _)| stored_key == &key)
        .map(|(_, entry)| entry);
    let mut state = match entry.as_ref() {
        Some(entry) => serde_json::from_value(entry.value.clone()).map_err(|error| {
            tracing::warn!(?error, guild = %ctx.guild_id, "could not decode ticket blacklist");
            UserError::message("tickets-failed")
        })?,
        None => State::default(),
    };
    for target in &mut state.entries {
        *target = target.canonical(ctx.guild_id.get()).map_err(|error| {
            tracing::warn!(?error, guild = %ctx.guild_id, "ticket blacklist contains an invalid target");
            UserError::message("tickets-failed")
        })?;
    }
    state.entries.sort_unstable();
    Ok(Snapshot {
        revision: entry.map(|entry| entry.revision),
        state,
    })
}

async fn validate_add_target(ctx: &EventContext, target: &Target) -> Result<(), UserError> {
    match target {
        Target::User { id } => {
            let user_id = UserId::new(
                id.parse()
                    .map_err(|_| UserError::message("tickets-blacklist-target-invalid"))?,
            );
            match ctx
                .api
                .execute(Action::GetMember {
                    guild_id: ctx.guild_id,
                    user_id,
                })
                .await
            {
                Ok(closure_core::action::Outcome::Member(_)) => Ok(()),
                Ok(_) | Err(DiscordError::NotFound) => {
                    Err(UserError::message("tickets-blacklist-user-unavailable"))
                }
                Err(error) => Err(failed(&error)),
            }
        }
        Target::Role { id } => {
            let role_id = RoleId::new(
                id.parse()
                    .map_err(|_| UserError::message("tickets-blacklist-target-invalid"))?,
            );
            let closure_core::action::Outcome::Roles(roles) = ctx
                .api
                .execute(Action::ListRoles {
                    guild_id: ctx.guild_id,
                })
                .await
                .map_err(|error| failed(&error))?
            else {
                return Err(UserError::message("tickets-failed"));
            };
            if roles.iter().any(|role| role.id == role_id) {
                Ok(())
            } else {
                Err(UserError::message("tickets-blacklist-role-unavailable"))
            }
        }
    }
}

pub(super) async fn change(
    ctx: &EventContext,
    panel: Option<&str>,
    target: Target,
    add: bool,
    guard: RevisionGuard,
) -> Result<ChangeResult, UserError> {
    let target = target.canonical(ctx.guild_id.get())?;
    let operation = if add {
        validate_add_target(ctx, &target).await?;
        Operation::Add
    } else {
        Operation::Remove
    };
    let key = key(ctx, panel);
    for _ in 0..CAS_ATTEMPTS {
        let current = state(ctx, panel).await?;
        let expect = match guard {
            RevisionGuard::Any => current.revision.map_or(Expect::Missing, Expect::Revision),
            RevisionGuard::Missing if current.revision.is_none() => Expect::Missing,
            RevisionGuard::Revision(revision) if current.revision == Some(revision) => {
                Expect::Revision(revision)
            }
            RevisionGuard::Missing | RevisionGuard::Revision(_) => {
                return Err(UserError::message("tickets-blacklist-conflict"));
            }
        };

        let mut state = current.state.clone();
        let was_present = state.entries.contains(&target);
        let changed = match operation {
            Operation::Add if !was_present => {
                state.entries.push(target.clone());
                state.entries.sort_unstable();
                true
            }
            Operation::Remove if was_present => {
                state.entries.retain(|entry| entry != &target);
                true
            }
            Operation::Add | Operation::Remove => false,
        };
        if !changed {
            return Ok(ChangeResult {
                snapshot: current,
                changed: false,
            });
        }

        match ctx.store().replace(&key, &state, expect, None).await {
            Ok(_) => {
                return Ok(ChangeResult {
                    snapshot: self::state(ctx, panel).await?,
                    changed: true,
                });
            }
            Err(StorageError::Conflict) if guard == RevisionGuard::Any => continue,
            Err(StorageError::Conflict) => {
                return Err(UserError::message("tickets-blacklist-conflict"));
            }
            Err(error) => {
                tracing::warn!(?error, guild = %ctx.guild_id, "could not update ticket blacklist");
                return Err(UserError::message("tickets-failed"));
            }
        }
    }
    Err(UserError::message("tickets-failed"))
}

pub(super) async fn is_blocked(
    ctx: &EventContext,
    user: UserId,
    effective_roles: &[RoleId],
    panel: Option<&str>,
) -> Result<bool, UserError> {
    let matches = |state: &State| {
        state.entries.iter().any(|target| match target {
            Target::User { id } => id == &user.get().to_string(),
            Target::Role { id } => effective_roles
                .iter()
                .any(|role| id == &role.get().to_string()),
        })
    };
    if matches(&state(ctx, None).await?.state) {
        return Ok(true);
    }
    if let Some(panel) = panel
        && matches(&state(ctx, Some(panel)).await?.state)
    {
        return Ok(true);
    }
    Ok(false)
}

pub(super) async fn is_blocked_member(
    ctx: &EventContext,
    user: UserId,
    panel: Option<&str>,
) -> Result<bool, UserError> {
    let effective_roles = match ctx
        .api
        .execute(Action::GetMember {
            guild_id: ctx.guild_id,
            user_id: user,
        })
        .await
    {
        Ok(closure_core::action::Outcome::Member(member)) => member.roles,
        Ok(_) | Err(DiscordError::NotFound) => Vec::new(),
        Err(error) => return Err(failed(&error)),
    };
    is_blocked(ctx, user, &effective_roles, panel).await
}

fn scope_label(cx: &CommandCx, config: &Config, panel: Option<&str>) -> Result<String, UserError> {
    match panel {
        Some(id) => Ok(config
            .named(id)?
            .name
            .clone()
            .unwrap_or_else(|| id.to_owned())),
        None => Ok(cx.t("tickets-blacklist-scope-guild", &Args::new())),
    }
}

async fn modify(
    cx: CommandCx,
    panel: Option<String>,
    target: Target,
    add: bool,
) -> Result<(), UserError> {
    let config: Config = cx.config()?;
    let scope = scope_config(&config, panel.as_deref())?;
    if !is_staff_command(&cx, &scope).await {
        return cx.reply_private_t("tickets-staff-only", &Args::new()).await;
    }
    cx.defer(true).await?;
    let result = change(
        cx.ctx(),
        panel.as_deref(),
        target.clone(),
        add,
        RevisionGuard::Any,
    )
    .await?;
    if result.changed {
        let audit_key = format!("ticket-blacklist-{}", cx.invocation().interaction_id.get());
        let audit_action = if add {
            super::audit::AuditAction::BlacklistAdd
        } else {
            super::audit::AuditAction::BlacklistRemove
        };
        if let Err(error) = super::audit::record_event(
            cx.ctx(),
            cx.user().id.get(),
            audit_action,
            super::audit::AuditOutcome::Success,
            &audit_key,
        )
        .await
        {
            tracing::warn!(?error, "could not record ticket blacklist audit event");
        }
    }
    let scope = scope_label(&cx, &config, panel.as_deref())?;
    let target_label = target.label(&cx);
    let key = match (add, result.changed) {
        (true, true) => "tickets-blacklist-added",
        (true, false) => "tickets-blacklist-already",
        (false, true) => "tickets-blacklist-removed",
        (false, false) => "tickets-blacklist-not-listed",
    };
    cx.reply_private_t(
        key,
        &Args::new()
            .with("target", target_label)
            .with("scope", scope),
    )
    .await
}

pub(super) async fn add_user(cx: CommandCx, args: UserArgs) -> Result<(), UserError> {
    modify(cx, args.panel, Target::user(args.user), true).await
}

pub(super) async fn remove_user(cx: CommandCx, args: UserArgs) -> Result<(), UserError> {
    modify(cx, args.panel, Target::user(args.user), false).await
}

pub(super) async fn add_role(cx: CommandCx, args: RoleArgs) -> Result<(), UserError> {
    modify(cx, args.panel, Target::role(args.role), true).await
}

pub(super) async fn remove_role(cx: CommandCx, args: RoleArgs) -> Result<(), UserError> {
    modify(cx, args.panel, Target::role(args.role), false).await
}

pub(super) async fn list(cx: CommandCx, args: ScopeArgs) -> Result<(), UserError> {
    let config: Config = cx.config()?;
    let scope = scope_config(&config, args.panel.as_deref())?;
    if !is_staff_command(&cx, &scope).await {
        return cx.reply_private_t("tickets-staff-only", &Args::new()).await;
    }
    cx.defer(true).await?;
    let snapshot = state(cx.ctx(), args.panel.as_deref()).await?;
    let entries = &snapshot.state.entries;
    let scope_label = scope_label(&cx, &config, args.panel.as_deref())?;
    if entries.is_empty() {
        return cx
            .reply_private_t(
                "tickets-blacklist-empty",
                &Args::new().with("scope", scope_label),
            )
            .await;
    }
    let pages = entries.len().div_ceil(PAGE_SIZE);
    let Some(page) = usize::try_from(args.page.unwrap_or(1))
        .ok()
        .and_then(|page| page.checked_sub(1))
        .filter(|page| *page < pages)
    else {
        return cx
            .reply_private_t(
                "tickets-blacklist-page-not-found",
                &Args::new()
                    .with("page", args.page.unwrap_or(1).to_string())
                    .with("pages", pages.to_string()),
            )
            .await;
    };
    let mut content = cx.t(
        "tickets-blacklist-page",
        &Args::new()
            .with("scope", scope_label)
            .with("page", (page + 1).to_string())
            .with("pages", pages.to_string()),
    );
    for target in entries.iter().skip(page * PAGE_SIZE).take(PAGE_SIZE) {
        content.push('\n');
        content.push_str(
            &cx.t(
                "tickets-blacklist-entry",
                &Args::new()
                    .with(
                        "kind",
                        cx.t(
                            match target {
                                Target::User { .. } => "tickets-blacklist-kind-user",
                                Target::Role { .. } => "tickets-blacklist-kind-role",
                            },
                            &Args::new(),
                        ),
                    )
                    .with("id", target.id()),
            ),
        );
    }
    cx.reply_private(content).await
}
