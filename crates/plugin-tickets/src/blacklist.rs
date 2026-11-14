use closure_core::action::Action;
use closure_core::error::DiscordError;
use closure_core::event::{RoleId, UserId};
use closure_core::module::EventContext;
use closure_core::storage::{Expect, StorageError, Store};
use closure_sdk::{Args, CommandArgs, CommandCx, UserError};
use serde::{Deserialize, Serialize};

use super::{Config, failed, is_staff_command};

const PREFIX: &str = "ticket-blacklist";
const READ_PAGE: usize = 500;
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
    /// The store revision used for dashboard `ETag` and `If-Match`.
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

#[allow(clippy::too_many_arguments)]
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
    let store = ctx.store();
    let guild = ctx.guild_id.get();
    let entries = stored_targets(store, guild, panel)
        .await
        .map_err(|error| store_failed(ctx, &error))?;
    let revision = store
        .entry(&meta_key(&scope(guild, panel)))
        .await
        .map_err(|error| store_failed(ctx, &error))?
        .map(|entry| entry.revision);
    Ok(Snapshot {
        revision,
        state: State { entries },
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
    let guild = ctx.guild_id.get();
    let target = target.canonical(guild)?;
    if add {
        validate_add_target(ctx, &target).await?;
    }
    let store = ctx.store();
    let meta = meta_key(&scope(guild, panel));
    let revision = store
        .entry(&meta)
        .await
        .map_err(|error| store_failed(ctx, &error))?
        .map(|entry| entry.revision);
    let guarded = match guard {
        RevisionGuard::Any => true,
        RevisionGuard::Missing => revision.is_none(),
        RevisionGuard::Revision(expected) => revision == Some(expected),
    };
    if !guarded {
        return Err(UserError::message("tickets-blacklist-conflict"));
    }
    let changed = write_target(store, guild, panel, &target, add)
        .await
        .map_err(|error| store_failed(ctx, &error))?;
    if changed {
        store
            .replace(&meta, &true, Expect::Any, None)
            .await
            .map_err(|error| store_failed(ctx, &error))?;
    }
    Ok(ChangeResult {
        snapshot: state(ctx, panel).await?,
        changed,
    })
}

pub(super) async fn is_blocked(
    ctx: &EventContext,
    user: UserId,
    effective_roles: &[RoleId],
    panel: Option<&str>,
) -> Result<bool, UserError> {
    let store = ctx.store();
    let guild = ctx.guild_id.get();
    if is_listed(store, guild, None, user, effective_roles)
        .await
        .map_err(|error| store_failed(ctx, &error))?
    {
        return Ok(true);
    }
    match panel {
        Some(panel) => is_listed(store, guild, Some(panel), user, effective_roles)
            .await
            .map_err(|error| store_failed(ctx, &error)),
        None => Ok(false),
    }
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

fn scope(guild: u64, panel: Option<&str>) -> String {
    match panel {
        Some(panel) => format!("{PREFIX}:{guild}:panel:{panel}"),
        None => format!("{PREFIX}:{guild}:guild"),
    }
}

/// The single record that older builds wrote; it is still read and moved away on the next write.
fn legacy_key(guild: u64, panel: Option<&str>) -> String {
    format!("{}:state", scope(guild, panel))
}

fn entries_prefix(scope: &str) -> String {
    format!("{scope}:entry:")
}

fn meta_key(scope: &str) -> String {
    format!("{scope}:meta")
}

fn entry_key(scope: &str, target: &Target) -> String {
    match target {
        Target::User { id } => format!("{scope}:entry:user:{id}"),
        Target::Role { id } => format!("{scope}:entry:role:{id}"),
    }
}

fn valid_target(target: &Target, guild: u64) -> Result<Target, StorageError> {
    target
        .canonical(guild)
        .map_err(|_| StorageError::Value("invalid blacklist target".to_owned()))
}

fn member_of(target: &Target, user: UserId, roles: &[RoleId]) -> bool {
    match target {
        Target::User { id } => id == &user.get().to_string(),
        Target::Role { id } => roles.iter().any(|role| id == &role.get().to_string()),
    }
}

/// Every record that can make `user` blocked here: one per user and per effective role.
fn member_keys(scope: &str, user: UserId, roles: &[RoleId]) -> Vec<String> {
    std::iter::once(entry_key(scope, &Target::user(user)))
        .chain(
            roles
                .iter()
                .map(|role| entry_key(scope, &Target::role(*role))),
        )
        .collect()
}

/// All targets of one scope, from per-entry records plus the legacy record, deduplicated.
async fn stored_targets(
    store: &Store,
    guild: u64,
    panel: Option<&str>,
) -> Result<Vec<Target>, StorageError> {
    let prefix = entries_prefix(&scope(guild, panel));
    let mut targets = Vec::new();
    let mut after = prefix.clone();
    loop {
        let page = store.list_after(&prefix, &after, READ_PAGE).await?;
        let full = page.len() == READ_PAGE;
        for (key, entry) in page {
            let target = serde_json::from_value::<Target>(entry.value)
                .map_err(|error| StorageError::Value(error.to_string()))?;
            targets.push(valid_target(&target, guild)?);
            after = key;
        }
        if !full {
            break;
        }
    }
    if let Some(legacy) = store.get::<State>(&legacy_key(guild, panel)).await? {
        for target in legacy.entries {
            targets.push(valid_target(&target, guild)?);
        }
    }
    targets.sort_unstable();
    targets.dedup();
    Ok(targets)
}

/// Moves the legacy record into per-entry records. Entries are written before the legacy record
/// is deleted, so an interruption leaves both, which reads treat as one set.
async fn migrate_legacy(
    store: &Store,
    guild: u64,
    panel: Option<&str>,
) -> Result<(), StorageError> {
    let legacy = legacy_key(guild, panel);
    let Some(record) = store.get::<State>(&legacy).await? else {
        return Ok(());
    };
    let scope = scope(guild, panel);
    for target in record.entries {
        let target = valid_target(&target, guild)?;
        store
            .replace(&entry_key(&scope, &target), &target, Expect::Any, None)
            .await?;
    }
    store.delete(&legacy).await?;
    Ok(())
}

/// Adds or removes one target. Returns whether the stored set changed.
async fn write_target(
    store: &Store,
    guild: u64,
    panel: Option<&str>,
    target: &Target,
    add: bool,
) -> Result<bool, StorageError> {
    migrate_legacy(store, guild, panel).await?;
    let key = entry_key(&scope(guild, panel), target);
    if !add {
        return store.delete(&key).await;
    }
    match store.replace(&key, target, Expect::Missing, None).await {
        Ok(_) => Ok(true),
        Err(StorageError::Conflict) => Ok(false),
        Err(error) => Err(error),
    }
}

async fn is_listed(
    store: &Store,
    guild: u64,
    panel: Option<&str>,
    user: UserId,
    roles: &[RoleId],
) -> Result<bool, StorageError> {
    for key in member_keys(&scope(guild, panel), user, roles) {
        if store.entry(&key).await?.is_some() {
            return Ok(true);
        }
    }
    Ok(match store.get::<State>(&legacy_key(guild, panel)).await? {
        Some(record) => record
            .entries
            .iter()
            .any(|target| member_of(target, user, roles)),
        None => false,
    })
}

fn store_failed(ctx: &EventContext, error: &StorageError) -> UserError {
    tracing::warn!(?error, guild = %ctx.guild_id, "could not use ticket blacklist");
    UserError::message("tickets-failed")
}

#[cfg(test)]
mod tests {
    use closure_core::storage::Store;
    use closure_sdk::testing::Harness;

    use super::*;

    const GUILD: u64 = 1;
    const BASE: u64 = 1_000_000_000_000_000_000;

    fn user(id: u64) -> Target {
        Target::User { id: id.to_string() }
    }

    #[tokio::test]
    async fn adds_past_the_old_single_record_limit() {
        let harness = Harness::new(crate::builder().unwrap()).unwrap();
        let store: Store = harness.store();
        for n in 0..800 {
            assert!(
                write_target(&store, GUILD, None, &user(BASE + n), true)
                    .await
                    .unwrap(),
                "entry {n} was not added"
            );
        }
        assert_eq!(
            stored_targets(&store, GUILD, None).await.unwrap().len(),
            800
        );
    }

    #[tokio::test]
    async fn legacy_record_is_read_and_moved_on_write() {
        let harness = Harness::new(crate::builder().unwrap()).unwrap();
        let store: Store = harness.store();
        let legacy = legacy_key(GUILD, None);
        let old = State {
            entries: vec![user(5)],
        };
        store
            .replace(&legacy, &old, Expect::Any, None)
            .await
            .unwrap();
        assert_eq!(
            stored_targets(&store, GUILD, None).await.unwrap(),
            vec![user(5)]
        );

        assert!(
            write_target(&store, GUILD, None, &user(6), true)
                .await
                .unwrap()
        );

        assert!(store.entry(&legacy).await.unwrap().is_none());
        assert_eq!(
            stored_targets(&store, GUILD, None).await.unwrap(),
            vec![user(5), user(6)]
        );
    }

    #[tokio::test]
    async fn interrupted_migration_does_not_duplicate_entries() {
        let harness = Harness::new(crate::builder().unwrap()).unwrap();
        let store: Store = harness.store();
        let legacy = legacy_key(GUILD, None);
        let old = State {
            entries: vec![user(5)],
        };
        store
            .replace(&legacy, &old, Expect::Any, None)
            .await
            .unwrap();
        let scope = scope(GUILD, None);
        store
            .replace(&entry_key(&scope, &user(5)), &user(5), Expect::Any, None)
            .await
            .unwrap();
        assert_eq!(
            stored_targets(&store, GUILD, None).await.unwrap(),
            vec![user(5)]
        );

        assert!(
            write_target(&store, GUILD, None, &user(7), true)
                .await
                .unwrap()
        );

        assert!(store.entry(&legacy).await.unwrap().is_none());
        assert_eq!(
            stored_targets(&store, GUILD, None).await.unwrap(),
            vec![user(5), user(7)]
        );
    }

    #[tokio::test]
    async fn direct_membership_reads_only_the_candidate_keys() {
        let harness = Harness::new(crate::builder().unwrap()).unwrap();
        let store: Store = harness.store();
        let scope = scope(GUILD, None);
        let roles = [RoleId::new(9), RoleId::new(10)];
        assert_eq!(
            member_keys(&scope, UserId::new(5), &roles).len(),
            1 + roles.len()
        );

        for n in 0..3 {
            write_target(&store, GUILD, None, &user(BASE + n), true)
                .await
                .unwrap();
        }
        let role = Target::Role { id: "9".to_owned() };
        write_target(&store, GUILD, None, &role, true)
            .await
            .unwrap();

        assert!(
            is_listed(&store, GUILD, None, UserId::new(BASE + 2), &roles)
                .await
                .unwrap()
        );
        assert!(
            is_listed(&store, GUILD, None, UserId::new(5), &roles)
                .await
                .unwrap()
        );
        assert!(
            !is_listed(&store, GUILD, None, UserId::new(5), &roles[1..])
                .await
                .unwrap()
        );
    }
}
