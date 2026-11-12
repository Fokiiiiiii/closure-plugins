use super::{
    Action, Args, ChannelId, ChannelType, CommandCx, Config, DAY, DiscordError, EventContext,
    Expect, InteractionRef, ModuleError, Outcome, Permissions, Reason, RoleId, StorageError,
    ThreadPatch, Ticket, UserError, UserId, acquire_open_lock, all_tickets,
    cancel_close_request_timeout, closing_key, failed, left_close_key, list_user_reservations,
    open_counts, owns_open_lock, read_open_state, release_open_lock,
    remove_missing_closing_tickets, set_open_score, stored, teams_survey, ticket_key, ticket_ttl,
    watch,
};
use twilight_model::channel::permission_overwrite::PermissionOverwriteType;
use twilight_util::permission_calculator::PermissionCalculator;

const THREAD_STAFF_PERMISSIONS: Permissions = Permissions::VIEW_CHANNEL
    .union(Permissions::READ_MESSAGE_HISTORY)
    .union(Permissions::SEND_MESSAGES_IN_THREADS)
    .union(Permissions::MANAGE_THREADS);
const THREAD_MEMBER_PERMISSIONS: Permissions = Permissions::VIEW_CHANNEL
    .union(Permissions::READ_MESSAGE_HISTORY)
    .union(Permissions::SEND_MESSAGES_IN_THREADS);
const THREAD_BOT_PERMISSIONS: Permissions = THREAD_MEMBER_PERMISSIONS
    .union(Permissions::CREATE_PRIVATE_THREADS)
    .union(Permissions::MANAGE_THREADS);
const THREAD_PRIVATE_VIEW_PERMISSIONS: Permissions =
    Permissions::VIEW_CHANNEL.union(Permissions::MANAGE_THREADS);
const THREAD_PARENT_VIEW_GRANTS: Permissions =
    THREAD_PRIVATE_VIEW_PERMISSIONS.union(Permissions::ADMINISTRATOR);
const MAX_THREAD_PERMISSION_CHECKS: usize = 65_536;

enum ReopenResult {
    Reopened,
    GlobalLimit(u32),
    PanelLimit(u32),
    Busy,
    PrivacyBusy,
    Erased,
    Stale,
}

/// Effective permissions for one member in a guild channel.
fn effective_permissions(
    ctx: &EventContext,
    channel: &twilight_model::channel::Channel,
    member: UserId,
    member_roles: &[RoleId],
    all_roles: &[twilight_model::guild::Role],
    owner: Option<UserId>,
) -> Permissions {
    let everyone = all_roles
        .iter()
        .find(|role| role.id.cast() == ctx.guild_id)
        .map_or(Permissions::empty(), |role| role.permissions);
    let held: Vec<_> = all_roles
        .iter()
        .filter(|role| member_roles.contains(&role.id))
        .map(|role| (role.id, role.permissions))
        .collect();
    let mut calculator = PermissionCalculator::new(ctx.guild_id, member, everyone, &held);
    if let Some(owner) = owner {
        calculator = calculator.owner_id(owner);
    }
    calculator.in_channel(
        channel.kind,
        channel.permission_overwrites.as_deref().unwrap_or(&[]),
    )
}

/// Whether any combination of unconfigured roles can expose a private thread in this parent.
fn unstaffed_roles_can_view_private_threads(
    ctx: &EventContext,
    channel: &twilight_model::channel::Channel,
    roles: &[twilight_model::guild::Role],
    staff_roles: &[RoleId],
    bot: UserId,
) -> bool {
    let Some(everyone_role) = roles.iter().find(|role| role.id.cast() == ctx.guild_id) else {
        return true;
    };
    if everyone_role
        .permissions
        .contains(Permissions::ADMINISTRATOR)
    {
        return false;
    }
    let everyone_id = everyone_role.id;
    let everyone = effective_permissions(ctx, channel, UserId::new(1), &[], roles, None);
    if everyone.contains(Permissions::ADMINISTRATOR)
        || everyone.contains(THREAD_PRIVATE_VIEW_PERMISSIONS)
    {
        return true;
    }

    let mut view_roles = Vec::new();
    let mut manage_roles = Vec::new();
    if everyone.contains(Permissions::VIEW_CHANNEL) {
        view_roles.push(everyone_id);
    }
    if everyone.contains(Permissions::MANAGE_THREADS) {
        manage_roles.push(everyone_id);
    }
    for role in roles {
        let bot_role = role.tags.as_ref().and_then(|tags| tags.bot_id) == Some(bot);
        if role.id == everyone_id
            || staff_roles.contains(&role.id)
            || role.permissions.contains(Permissions::ADMINISTRATOR)
            || bot_role
        {
            continue;
        }
        let effective =
            effective_permissions(ctx, channel, UserId::new(1), &[role.id], roles, None);
        if effective.contains(Permissions::ADMINISTRATOR) {
            return true;
        }
        if effective.contains(THREAD_PRIVATE_VIEW_PERMISSIONS) {
            return true;
        }
        if effective.contains(Permissions::VIEW_CHANNEL) {
            view_roles.push(role.id);
        }
        if effective.contains(Permissions::MANAGE_THREADS) {
            manage_roles.push(role.id);
        }
    }

    if view_roles.len().saturating_mul(manage_roles.len()) > MAX_THREAD_PERMISSION_CHECKS {
        return true;
    }
    if view_roles.is_empty() || manage_roles.is_empty() {
        return false;
    }
    for view_role in &view_roles {
        for manage_role in &manage_roles {
            if view_role == manage_role {
                continue;
            }
            let held: Vec<_> = [*view_role, *manage_role]
                .into_iter()
                .filter(|role| *role != everyone_id)
                .collect();
            let effective = effective_permissions(ctx, channel, UserId::new(1), &held, roles, None);
            if effective.contains(Permissions::ADMINISTRATOR)
                || effective.contains(THREAD_PRIVATE_VIEW_PERMISSIONS)
            {
                return true;
            }
        }
    }
    false
}

/// Validates named team members as well as the configured role staff in the parent channel.
#[allow(
    clippy::too_many_lines,
    reason = "one check must prove the parent, staff, opener, team users, and bot can use the thread"
)]
pub(super) async fn ensure_thread_parent_with_access(
    ctx: &EventContext,
    parent_id: ChannelId,
    staff_roles: &[RoleId],
    opener: UserId,
    additional_members: &[UserId],
    staff_access: Option<&teams_survey::StaffAccessSnapshot>,
) -> Result<(), UserError> {
    let channel = match ctx
        .api
        .execute(Action::GetChannel {
            channel_id: parent_id,
        })
        .await
    {
        Ok(Outcome::Channel(channel)) => channel,
        Ok(_) | Err(DiscordError::NotFound) => {
            return Err(UserError::message("tickets-thread-channel-invalid"));
        }
        Err(error) => {
            tracing::warn!(?error, %parent_id, "could not read the ticket thread parent");
            return Err(UserError::message("tickets-thread-access-unknown"));
        }
    };
    // ApiClient scopes GetChannel to this guild even when Discord omits guild_id in the payload.
    if channel.kind != ChannelType::GuildText
        || channel.guild_id.is_some_and(|guild| guild != ctx.guild_id)
    {
        return Err(UserError::message("tickets-thread-channel-invalid"));
    }
    let all_roles = match ctx
        .api
        .execute(Action::ListRoles {
            guild_id: ctx.guild_id,
        })
        .await
    {
        Ok(Outcome::Roles(roles)) => roles,
        Ok(_) => return Err(UserError::message("tickets-thread-access-unknown")),
        Err(error) => {
            tracing::warn!(?error, guild = %ctx.guild_id, "could not read ticket staff roles");
            return Err(UserError::message("tickets-thread-access-unknown"));
        }
    };
    let bot = ctx
        .bot()
        .ok_or_else(|| UserError::message("tickets-thread-access-unknown"))?;
    let all_overwrites = channel.permission_overwrites.as_deref().unwrap_or(&[]);
    let role_overwrites: Vec<_> = all_overwrites
        .iter()
        .filter(|overwrite| overwrite.kind == PermissionOverwriteType::Role)
        .copied()
        .collect();
    let mut role_channel = (*channel).clone();
    role_channel.permission_overwrites = Some(role_overwrites.clone());
    let team_role_overwrites = staff_access
        .map(teams_survey::thread_role_overwrites)
        .unwrap_or_default();

    for staff_role in staff_roles {
        let Some(role) = all_roles.iter().find(|role| role.id == *staff_role) else {
            return Err(UserError::message("tickets-thread-access-required"));
        };
        if role.permissions.contains(Permissions::ADMINISTRATOR) {
            continue;
        }
        let (team_allow, team_deny, required) = if staff_access.is_some() {
            let Some((_, allow, deny)) = team_role_overwrites
                .iter()
                .find(|(role, _, _)| role == staff_role)
            else {
                return Err(UserError::message("tickets-team-thread-permissions"));
            };
            let base = Permissions::VIEW_CHANNEL
                .union(Permissions::READ_MESSAGE_HISTORY)
                .union(Permissions::MANAGE_THREADS);
            (
                *allow,
                *deny,
                base.union(allow.intersection(Permissions::SEND_MESSAGES_IN_THREADS)),
            )
        } else {
            (
                Permissions::empty(),
                Permissions::empty(),
                THREAD_STAFF_PERMISSIONS,
            )
        };
        let effective = effective_permissions(
            ctx,
            &role_channel,
            UserId::new(1),
            &[*staff_role],
            &all_roles,
            None,
        );
        if !effective.contains(required) {
            return Err(UserError::message(if staff_access.is_some() {
                "tickets-team-thread-permissions"
            } else {
                "tickets-thread-access-required"
            }));
        }
        if staff_access.is_some()
            && (!effective.contains(team_allow) || effective.intersects(team_deny))
        {
            return Err(UserError::message("tickets-team-thread-permissions"));
        }

        // A different held role can deny inherited permissions; prove the staff role can override it.
        let own_allow = role_overwrites
            .iter()
            .filter(|overwrite| {
                overwrite.id.get() == staff_role.get()
                    && overwrite.kind == PermissionOverwriteType::Role
            })
            .fold(Permissions::empty(), |allowed, overwrite| {
                allowed | overwrite.allow
            });
        let other_denials = role_overwrites
            .iter()
            .filter(|overwrite| {
                overwrite.kind == PermissionOverwriteType::Role
                    && overwrite.id.get() != staff_role.get()
                    && overwrite.id.get() != ctx.guild_id.get()
            })
            .fold(Permissions::empty(), |denied, overwrite| {
                denied | overwrite.deny
            });
        if !(other_denials & required & !own_allow).is_empty() {
            return Err(UserError::message(if staff_access.is_some() {
                "tickets-team-thread-permissions"
            } else {
                "tickets-thread-access-required"
            }));
        }
    }

    if unstaffed_roles_can_view_private_threads(ctx, &role_channel, &all_roles, staff_roles, bot) {
        return Err(UserError::message("tickets-thread-parent-exposed"));
    }

    let owner = match ctx.api.guild_owner().await {
        Ok(owner) => owner,
        Err(error) => {
            tracing::warn!(?error, guild = %ctx.guild_id, "could not read the guild owner for thread permissions");
            return Err(UserError::message("tickets-thread-access-unknown"));
        }
    };
    for overwrite in all_overwrites {
        if overwrite.deny.intersects(THREAD_STAFF_PERMISSIONS)
            || overwrite.allow.intersects(THREAD_PARENT_VIEW_GRANTS)
        {
            match overwrite.kind {
                PermissionOverwriteType::Member => {
                    let member_id = UserId::new(overwrite.id.get());
                    let member = match ctx
                        .api
                        .execute(Action::GetMember {
                            guild_id: ctx.guild_id,
                            user_id: member_id,
                        })
                        .await
                    {
                        Ok(Outcome::Member(member)) => member,
                        Ok(Outcome::None) => {
                            return Err(UserError::message("tickets-thread-access-unknown"));
                        }
                        Err(DiscordError::NotFound) => continue,
                        Err(error) => {
                            tracing::warn!(?error, %member_id, "could not verify a thread staff member");
                            return Err(UserError::message("tickets-thread-access-unknown"));
                        }
                        _ => return Err(UserError::message("tickets-thread-access-unknown")),
                    };
                    let member_is_staff = additional_members.contains(&member_id)
                        || member.roles.iter().any(|role| staff_roles.contains(role));
                    let member_is_admin = owner == Some(member_id)
                        || member.roles.iter().any(|held| {
                            all_roles.iter().any(|role| {
                                role.id == *held
                                    && role.permissions.contains(Permissions::ADMINISTRATOR)
                            })
                        });
                    let permissions = effective_permissions(
                        ctx,
                        &channel,
                        member_id,
                        &member.roles,
                        &all_roles,
                        owner,
                    );
                    if member_is_staff && !member_is_admin {
                        let (team_allow, team_deny) = staff_access.map_or(
                            (Permissions::empty(), Permissions::empty()),
                            |snapshot| {
                                teams_survey::thread_member_overwrite(
                                    snapshot,
                                    member_id.get(),
                                    &member.roles,
                                )
                            },
                        );
                        let mut required = THREAD_MEMBER_PERMISSIONS;
                        if staff_access.is_some() {
                            required =
                                Permissions::VIEW_CHANNEL.union(Permissions::READ_MESSAGE_HISTORY);
                            if member.roles.iter().any(|role| staff_roles.contains(role)) {
                                required = required.union(Permissions::MANAGE_THREADS);
                            }
                            required = required.union(
                                team_allow.intersection(Permissions::SEND_MESSAGES_IN_THREADS),
                            );
                        }
                        if !permissions.contains(required)
                            || (staff_access.is_some()
                                && (!permissions.contains(team_allow)
                                    || permissions.intersects(team_deny)))
                        {
                            return Err(UserError::message(if staff_access.is_some() {
                                "tickets-team-thread-permissions"
                            } else {
                                "tickets-thread-access-required"
                            }));
                        }
                    }
                    if !member_is_staff
                        && member_id != bot
                        && !member_is_admin
                        && (permissions.contains(THREAD_PRIVATE_VIEW_PERMISSIONS)
                            || permissions.contains(Permissions::ADMINISTRATOR))
                    {
                        return Err(UserError::message("tickets-thread-parent-exposed"));
                    }
                }
                PermissionOverwriteType::Unknown(_) => {
                    return Err(UserError::message("tickets-thread-access-unknown"));
                }
                PermissionOverwriteType::Role => {}
                _ => return Err(UserError::message("tickets-thread-access-unknown")),
            }
        }
    }

    let member = match ctx
        .api
        .execute(Action::GetMember {
            guild_id: ctx.guild_id,
            user_id: opener,
        })
        .await
    {
        Ok(Outcome::Member(member)) => member,
        Ok(_) | Err(DiscordError::NotFound) => {
            return Err(UserError::message("tickets-thread-member-access"));
        }
        Err(error) => {
            tracing::warn!(?error, %opener, "could not verify the ticket opener's access to threads");
            return Err(UserError::message("tickets-thread-access-unknown"));
        }
    };
    if !effective_permissions(ctx, &channel, opener, &member.roles, &all_roles, owner)
        .contains(THREAD_MEMBER_PERMISSIONS)
    {
        return Err(UserError::message("tickets-thread-member-access"));
    }
    for additional in additional_members {
        if *additional == opener || *additional == bot {
            continue;
        }
        let member = match ctx
            .api
            .execute(Action::GetMember {
                guild_id: ctx.guild_id,
                user_id: *additional,
            })
            .await
        {
            Ok(Outcome::Member(member)) => member,
            Ok(_) | Err(DiscordError::NotFound) => {
                return Err(UserError::message("tickets-thread-member-access"));
            }
            Err(error) => {
                tracing::warn!(?error, %additional, "could not verify a named ticket team member");
                return Err(UserError::message("tickets-thread-access-unknown"));
            }
        };
        let permissions =
            effective_permissions(ctx, &channel, *additional, &member.roles, &all_roles, owner);
        let (team_allow, team_deny) =
            staff_access.map_or((Permissions::empty(), Permissions::empty()), |snapshot| {
                teams_survey::thread_member_overwrite(snapshot, additional.get(), &member.roles)
            });
        let member_is_admin = owner == Some(*additional)
            || member.roles.iter().any(|held| {
                all_roles.iter().any(|role| {
                    role.id == *held && role.permissions.contains(Permissions::ADMINISTRATOR)
                })
            });
        let required = if staff_access.is_some() {
            let base = Permissions::VIEW_CHANNEL.union(Permissions::READ_MESSAGE_HISTORY);
            base.union(team_allow.intersection(Permissions::SEND_MESSAGES_IN_THREADS))
        } else {
            THREAD_MEMBER_PERMISSIONS
        };
        if !member_is_admin
            && (!permissions.contains(required)
                || (staff_access.is_some()
                    && (!permissions.contains(team_allow) || permissions.intersects(team_deny))))
        {
            return Err(UserError::message(if staff_access.is_some() {
                "tickets-team-thread-permissions"
            } else {
                "tickets-thread-member-access"
            }));
        }
    }
    let bot_member = match ctx
        .api
        .execute(Action::GetMember {
            guild_id: ctx.guild_id,
            user_id: bot,
        })
        .await
    {
        Ok(Outcome::Member(member)) => member,
        Ok(_) | Err(DiscordError::NotFound) => {
            return Err(UserError::message("tickets-thread-access-unknown"));
        }
        Err(error) => {
            tracing::warn!(?error, %bot, "could not verify the bot's access to threads");
            return Err(UserError::message("tickets-thread-access-unknown"));
        }
    };
    if !effective_permissions(ctx, &channel, bot, &bot_member.roles, &all_roles, owner)
        .contains(THREAD_BOT_PERMISSIONS)
    {
        return Err(UserError::message("tickets-thread-access-required"));
    }
    Ok(())
}

/// Effective access a member has in the parent of a private ticket thread.
pub(super) async fn thread_parent_permissions(
    ctx: &EventContext,
    parent_id: ChannelId,
    user: UserId,
) -> Result<Permissions, UserError> {
    let channel = match ctx
        .api
        .execute(Action::GetChannel {
            channel_id: parent_id,
        })
        .await
    {
        Ok(Outcome::Channel(channel))
            if channel.kind == ChannelType::GuildText
                && channel.guild_id.is_none_or(|guild| guild == ctx.guild_id) =>
        {
            channel
        }
        Ok(_) | Err(DiscordError::NotFound) => {
            return Err(UserError::message("tickets-thread-channel-invalid"));
        }
        Err(error) => {
            tracing::warn!(?error, %parent_id, "could not read the thread parent for a ticket member");
            return Err(UserError::message("tickets-thread-access-unknown"));
        }
    };
    let member = match ctx
        .api
        .execute(Action::GetMember {
            guild_id: ctx.guild_id,
            user_id: user,
        })
        .await
    {
        Ok(Outcome::Member(member)) => member,
        Ok(_) | Err(DiscordError::NotFound) => return Ok(Permissions::empty()),
        Err(error) => {
            tracing::warn!(?error, %user, "could not verify access for a ticket member");
            return Err(UserError::message("tickets-thread-access-unknown"));
        }
    };
    let all_roles = match ctx
        .api
        .execute(Action::ListRoles {
            guild_id: ctx.guild_id,
        })
        .await
    {
        Ok(Outcome::Roles(roles)) => roles,
        Ok(_) => return Err(UserError::message("tickets-thread-access-unknown")),
        Err(error) => {
            tracing::warn!(?error, guild = %ctx.guild_id, "could not read ticket staff roles");
            return Err(UserError::message("tickets-thread-access-unknown"));
        }
    };
    let owner = match ctx.api.guild_owner().await {
        Ok(owner) => owner,
        Err(error) => {
            tracing::warn!(?error, guild = %ctx.guild_id, "could not read the guild owner for thread permissions");
            return Err(UserError::message("tickets-thread-access-unknown"));
        }
    };
    Ok(effective_permissions(
        ctx,
        &channel,
        user,
        &member.roles,
        &all_roles,
        owner,
    ))
}

pub(super) async fn add_member(
    ctx: &EventContext,
    thread_id: ChannelId,
    user_id: UserId,
) -> Result<(), ModuleError> {
    ctx.api
        .execute(Action::AddThreadMember { thread_id, user_id })
        .await
        .map(|_| ())
        .map_err(|error| super::stored(&error))
}

pub(super) async fn archive(
    ctx: &EventContext,
    thread_id: ChannelId,
    reason: &str,
) -> Result<(), ModuleError> {
    ctx.api
        .execute(Action::EditThread {
            thread_id,
            patch: ThreadPatch {
                archived: Some(true),
                locked: Some(true),
                ..ThreadPatch::default()
            },
            reason: Reason::truncated(reason),
        })
        .await
        .map(|_| ())
        .map_err(|error| super::stored(&error))
}

pub(super) async fn unarchive(
    ctx: &EventContext,
    thread_id: ChannelId,
    reason: &str,
) -> Result<(), ModuleError> {
    ctx.api
        .execute(Action::EditThread {
            thread_id,
            patch: ThreadPatch {
                archived: Some(false),
                locked: Some(false),
                ..ThreadPatch::default()
            },
            reason: Reason::truncated(reason),
        })
        .await
        .map(|_| ())
        .map_err(|error| super::stored(&error))
}

pub(super) async fn change_member(
    ctx: &EventContext,
    thread_id: ChannelId,
    parent_id: ChannelId,
    user_id: UserId,
    add: bool,
) -> Result<(), UserError> {
    let permissions = thread_parent_permissions(ctx, parent_id, user_id).await?;
    if add && !permissions.contains(THREAD_MEMBER_PERMISSIONS) {
        return Err(UserError::message("tickets-thread-member-access"));
    }
    if !add && permissions.contains(Permissions::MANAGE_THREADS) {
        return Err(UserError::message("tickets-thread-member-cannot-remove"));
    }
    let action = if add {
        Action::AddThreadMember { thread_id, user_id }
    } else {
        Action::RemoveThreadMember { thread_id, user_id }
    };
    ctx.api
        .execute(action)
        .await
        .map_err(|error| super::failed(&error))?;
    Ok(())
}

pub(super) async fn rename(
    ctx: &EventContext,
    thread_id: ChannelId,
    name: &str,
    reason: Reason,
) -> Result<(), UserError> {
    let action = Action::EditThread {
        thread_id,
        patch: ThreadPatch {
            name: Some(name.to_owned()),
            ..ThreadPatch::default()
        },
        reason,
    };
    action
        .validate()
        .map_err(|_| UserError::message("tickets-invalid-name"))?;
    ctx.api
        .execute(action)
        .await
        .map_err(|error| super::failed(&error))?;
    Ok(())
}

async fn rollback_reopened_ticket(
    ctx: &EventContext,
    channel: ChannelId,
    number: i64,
    opener: u64,
    generation: i64,
    reason: &str,
) -> Result<(), ModuleError> {
    archive(ctx, channel, reason)
        .await
        .map_err(|error| ModuleError::other(error.to_string()))?;
    let key = ticket_key(channel);
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
        if ticket.number != number
            || ticket.opener != opener
            || ticket.close_generation != generation
            || ticket.closed
        {
            return Ok(());
        }
        ticket.closed = true;
        ticket.closing = None;
        ticket.close_request = None;
        super::refresh_ticket_form_answer_ttl(ctx, &ticket)
            .await
            .map_err(|error| stored(&error))?;
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
        "ticket changed while a privacy-fenced reopen was being rolled back",
    ))
}

#[allow(
    clippy::too_many_lines,
    reason = "ticket reopen coordinates CAS, locks, history leases, and rollback in one operation"
)]
pub(super) async fn reopen_ticket(cx: CommandCx, (): ()) -> Result<(), UserError> {
    cx.require("tickets.manage").await?;
    cx.defer(true).await?;
    let ctx = cx.ctx();
    let channel = cx.invocation().channel_id;
    let history_token = cx.invocation().interaction_id.get();
    let key = ticket_key(channel);
    let Some(ticket) = ctx
        .store()
        .get::<Ticket>(&key)
        .await
        .map_err(|error| failed(&error))?
    else {
        return Err(UserError::message("tickets-not-a-ticket"));
    };
    if !ticket.thread {
        return cx
            .reply_private_t("tickets-reopen-thread-only", &Args::new())
            .await;
    }
    if !ticket.closed || ticket.closing.is_some() {
        return cx.reply_private_t("tickets-not-closed", &Args::new()).await;
    }
    let parent_id = ticket
        .thread_parent
        .map(ChannelId::new)
        .ok_or_else(|| UserError::message("tickets-thread-access-unknown"))?;
    let original: Config = cx.config()?;
    let config = original.for_ticket(&ticket);
    let mut staff: Vec<RoleId> = ticket.staff_access.as_ref().map_or_else(
        || {
            ticket.staff_roles.as_ref().map_or_else(
                || config.staff(),
                |roles| roles.iter().copied().map(RoleId::new).collect(),
            )
        },
        |snapshot| {
            teams_survey::staff_roles(snapshot)
                .into_iter()
                .map(RoleId::new)
                .collect()
        },
    );
    staff.sort_unstable();
    staff.dedup();
    let staff_users = super::team_staff_users(ticket.staff_access.as_ref());
    ensure_thread_parent_with_access(
        ctx,
        parent_id,
        &staff,
        UserId::new(ticket.opener),
        &staff_users,
        ticket.staff_access.as_ref(),
    )
    .await?;

    let opener = ticket.opener;
    let panel_id = ticket.panel.as_ref().map(|panel| panel.id.as_str());
    let panel_limit = ticket.panel.as_ref().map_or(0, |saved| {
        original
            .panels
            .iter()
            .find(|panel| panel.id == saved.id)
            .map_or(saved.max_open, |panel| panel.max_open)
    });
    let global_limit = original.max_open;
    let member = match ctx
        .api
        .execute(Action::GetMember {
            guild_id: ctx.guild_id,
            user_id: UserId::new(opener),
        })
        .await
    {
        Ok(Outcome::Member(member)) => Some(member),
        Ok(Outcome::None) | Err(DiscordError::NotFound) => None,
        Err(error) => return Err(failed(&error)),
        _ => None,
    };
    let legacy_staff_roles = ticket
        .staff_roles
        .clone()
        .unwrap_or_else(|| staff.iter().map(|role| role.get()).collect());
    let opener_is_staff = member.as_ref().is_some_and(|member| {
        teams_survey::may_access(
            ticket.staff_access.as_ref(),
            &legacy_staff_roles,
            opener,
            &member.roles,
            false,
        )
    });
    let opener_is_owner = ctx
        .api
        .guild_owner()
        .await
        .map_err(|error| failed(&error))?
        .is_some_and(|owner| owner.get() == opener);
    let bypass_limits = opener_is_staff || opener_is_owner;
    let Some(lock) = acquire_open_lock(ctx, opener)
        .await
        .map_err(|error| failed(&error))?
    else {
        return cx.reply_private_t("tickets-opening", &Args::new()).await;
    };

    let mut history_lease = None;
    let result = async {
        let (_, state) = read_open_state(ctx, opener).await?;
        if !owns_open_lock(&state, lock, ctx.now()) {
            return Ok(ReopenResult::Busy);
        }
        let mut tickets = all_tickets(ctx).await?;
        remove_missing_closing_tickets(ctx, opener, &mut tickets).await?;
        let reservations = list_user_reservations(ctx, opener).await?;
        let (open_count, panel_count) = open_counts(opener, panel_id, &tickets, &reservations);
        set_open_score(ctx, opener, open_count).await?;
        if !bypass_limits {
            if global_limit > 0 && open_count >= u64::from(global_limit) {
                return Ok(ReopenResult::GlobalLimit(global_limit));
            }
            if panel_limit > 0 && panel_count >= u64::from(panel_limit) {
                return Ok(ReopenResult::PanelLimit(panel_limit));
            }
        }
        let entry = ctx
            .store()
            .entry(&key)
            .await
            .map_err(|error| stored(&error))?
            .ok_or_else(|| ModuleError::other("the closed ticket no longer exists"))?;
        let mut current: Ticket = serde_json::from_value(entry.value)
            .map_err(|error| ModuleError::other(error.to_string()))?;
        if current.number != ticket.number
            || current.opener != opener
            || !current.thread
            || !current.closed
            || current.closing.is_some()
            || current.close_generation != ticket.close_generation
        {
            return Ok(ReopenResult::Stale);
        }
        history_lease = match super::acquire_history_reopen_lease(
            ctx,
            channel,
            &current,
            history_token,
        )
        .await?
        {
            super::HistoryReopenAcquire::Legacy => None,
            super::HistoryReopenAcquire::Acquired(lease) => Some(lease),
            super::HistoryReopenAcquire::Busy => return Ok(ReopenResult::PrivacyBusy),
            super::HistoryReopenAcquire::Erased => return Ok(ReopenResult::Erased),
        };
        let next_generation = current
            .close_generation
            .checked_add(1)
            .ok_or_else(|| ModuleError::other("ticket close generation is exhausted"))?;
        if let Some(lease) = history_lease.as_ref()
            && !super::renew_history_reopen_lease(ctx, lease).await?
        {
            return Ok(ReopenResult::Stale);
        }
        if let Err(error) = unarchive(ctx, channel, &cx.t("tickets-reason-reopen", &Args::new())).await {
            if let Err(rollback_error) =
                archive(ctx, channel, &cx.t("tickets-reason-reopen-rollback", &Args::new())).await
            {
                tracing::warn!(?rollback_error, %channel, "could not restore a closed ticket thread");
            }
            return Err(error);
        }
        if let Some(lease) = history_lease.as_ref()
            && !super::renew_history_reopen_lease(ctx, lease).await?
        {
            if let Err(error) =
                archive(ctx, channel, &cx.t("tickets-reason-reopen-rollback", &Args::new())).await
            {
                tracing::warn!(?error, %channel, "could not restore a closed ticket thread");
            }
            return Ok(ReopenResult::Stale);
        }
        if let Err(error) = add_member(ctx, channel, UserId::new(opener)).await {
            if let Err(rollback_error) =
                archive(ctx, channel, &cx.t("tickets-reason-reopen-rollback", &Args::new())).await
            {
                tracing::warn!(?rollback_error, %channel, "could not restore a closed ticket thread");
            }
            return Err(error);
        }
        let (_, state) = read_open_state(ctx, opener).await?;
        if !owns_open_lock(&state, lock, ctx.now()) {
            if let Err(error) =
                archive(ctx, channel, &cx.t("tickets-reason-reopen-rollback", &Args::new())).await
            {
                tracing::warn!(?error, %channel, "could not restore a closed ticket thread");
            }
            return Ok(ReopenResult::Busy);
        }
        if let Some(lease) = history_lease.as_ref()
            && !super::renew_history_reopen_lease(ctx, lease).await?
        {
            if let Err(error) =
                archive(ctx, channel, &cx.t("tickets-reason-reopen-rollback", &Args::new())).await
            {
                tracing::warn!(?error, %channel, "could not restore a closed ticket thread");
            }
            return Ok(ReopenResult::Stale);
        }
        current.closed = false;
        current.closing = None;
        current.close_request = None;
        current.close_generation = next_generation;
        super::refresh_ticket_form_answer_ttl(ctx, &current).await?;
        if let Some(lease) = history_lease.as_ref()
            && !super::renew_history_reopen_lease(ctx, lease).await?
        {
            if let Err(error) =
                archive(ctx, channel, &cx.t("tickets-reason-reopen-rollback", &Args::new())).await
            {
                tracing::warn!(?error, %channel, "could not restore a closed ticket thread");
            }
            return Ok(ReopenResult::Stale);
        }
        match ctx
            .store()
            .replace(
                &key,
                &current,
                Expect::Revision(entry.revision),
                Some(ticket_ttl(&current, ctx.now())),
            )
            .await
        {
            Ok(_) => {
                if let Some(lease) = history_lease.take() {
                    match super::release_history_reopen_lease(ctx, &lease).await {
                        Ok(true) => {}
                        Ok(false) => {
                            if let Err(error) = rollback_reopened_ticket(
                                ctx,
                                channel,
                                current.number,
                                opener,
                                next_generation,
                                &cx.t("tickets-reason-reopen-rollback", &Args::new()),
                            )
                            .await
                            {
                                tracing::warn!(?error, %channel, "could not restore a stale privacy-fenced reopen");
                                return Err(error);
                            }
                            return Ok(ReopenResult::Stale);
                        }
                        Err(error) => {
                            if let Err(rollback_error) = rollback_reopened_ticket(
                                ctx,
                                channel,
                                current.number,
                                opener,
                                next_generation,
                                &cx.t("tickets-reason-reopen-rollback", &Args::new()),
                            )
                            .await
                            {
                                tracing::warn!(?rollback_error, %channel, "could not restore a failed privacy-fenced reopen");
                            }
                            return Err(error);
                        }
                    }
                }
                if let Some((_, stored_ticket)) =
                    tickets.iter_mut().find(|(saved_channel, _)| *saved_channel == channel)
                {
                    *stored_ticket = current.clone();
                } else {
                    tickets.push((channel, current.clone()));
                }
                let reservations = list_user_reservations(ctx, opener).await?;
                let (open_count, _) = open_counts(opener, None, &tickets, &reservations);
                if let Err(error) = set_open_score(ctx, opener, open_count).await {
                    tracing::warn!(?error, opener, "could not refresh the reopened ticket count");
                }
                if let Err(error) = super::automation::start(ctx, channel).await {
                    tracing::warn!(?error, %channel, "ticket automation timers could not be restarted");
                }
                Ok(ReopenResult::Reopened)
            }
            Err(StorageError::Conflict) => {
                if let Err(error) =
                    archive(ctx, channel, &cx.t("tickets-reason-reopen-rollback", &Args::new())).await
                {
                    tracing::warn!(?error, %channel, "could not restore a closed ticket thread");
                }
                Ok(ReopenResult::Stale)
            }
            Err(error) => {
                if let Err(rollback_error) =
                    archive(ctx, channel, &cx.t("tickets-reason-reopen-rollback", &Args::new())).await
                {
                    tracing::warn!(?rollback_error, %channel, "could not restore a closed ticket thread");
                }
                Err(stored(&error))
            }
        }
    }
    .await;
    if let Some(lease) = history_lease.take()
        && let Err(error) = super::release_history_reopen_lease(ctx, &lease).await
    {
        tracing::warn!(?error, %channel, "could not release the ticket history reopen lease");
    }
    release_open_lock(ctx, opener, lock)
        .await
        .map_err(|error| failed(&error))?;
    match result.map_err(|error| failed(&error))? {
        ReopenResult::Reopened => {
            if let Err(error) = super::add_team_users_to_thread(ctx, channel, &ticket).await {
                tracing::warn!(?error, %channel, "ticket team members could not be restored to the thread");
            }
            if let Some(request) = &ticket.close_request {
                cancel_close_request_timeout(ctx, channel, request).await;
            }
            let _ = ctx.scheduler().cancel(&closing_key(channel)).await;
            let _ = ctx.scheduler().cancel(&left_close_key(channel)).await;
            if config.close_after_days > 0 {
                let _ = watch(
                    ctx,
                    channel,
                    ctx.now() + i64::from(config.close_after_days) * DAY,
                )
                .await;
            }
            cx.reply_private_t("tickets-reopened", &Args::new()).await
        }
        ReopenResult::GlobalLimit(max) => {
            cx.reply_private_t("tickets-limit", &Args::new().with("max", i64::from(max)))
                .await
        }
        ReopenResult::PanelLimit(max) => {
            cx.reply_private_t(
                "tickets-panel-limit",
                &Args::new().with("max", i64::from(max)),
            )
            .await
        }
        ReopenResult::Busy => {
            cx.reply_private_t("tickets-opening-pending", &Args::new())
                .await
        }
        ReopenResult::PrivacyBusy => {
            cx.reply_private_t("tickets-data-operation-busy", &Args::new())
                .await
        }
        ReopenResult::Erased => {
            cx.reply_private_t("tickets-history-erased-opener", &Args::new())
                .await
        }
        ReopenResult::Stale => {
            cx.reply_private_t("tickets-not-a-ticket", &Args::new())
                .await
        }
    }
}
