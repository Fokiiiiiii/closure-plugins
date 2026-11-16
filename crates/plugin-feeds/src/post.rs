//! Posting to the channel a subscription names, and checking the bot may post there.

use closure_core::action::{Action, MentionPolicy, Outcome};
use closure_core::error::{DiscordError, ModuleError};
use closure_core::event::{ChannelId, RoleId};
use closure_core::module::EventContext;
use closure_core::storage::{Expect, StorageError};
use twilight_model::guild::Permissions;
use twilight_util::permission_calculator::PermissionCalculator;

use crate::model::{self, POSTED_TTL, Subscription};

/// Whether the bot may post in a channel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Access {
    Allowed,
    /// The channel does not exist in this server.
    Missing,
    Denied,
}

/// Why an item could not be posted.
#[derive(Debug)]
pub enum PostError {
    /// The channel is gone or the bot may not post there any longer: the subscription is paused,
    /// with the key of the reason.
    Gone(&'static str),
    Failed(ModuleError),
}

/// # Errors
/// Discord cannot be asked, or a failure that may pass happens.
pub async fn bot_can_post(ctx: &EventContext, channel: ChannelId) -> Result<Access, ModuleError> {
    let Some(bot) = ctx.bot() else {
        return Ok(Access::Denied);
    };
    let channel = match ctx
        .api
        .execute(Action::GetChannel {
            channel_id: channel,
        })
        .await
    {
        Ok(Outcome::Channel(found)) => *found,
        Ok(_) | Err(DiscordError::NotFound) => return Ok(Access::Missing),
        Err(error) => return Err(error.into()),
    };
    if channel.guild_id != Some(ctx.guild_id) {
        return Ok(Access::Missing);
    }
    let member = match ctx
        .api
        .execute(Action::GetMember {
            guild_id: ctx.guild_id,
            user_id: bot,
        })
        .await
    {
        Ok(Outcome::Member(member)) => *member,
        Ok(_) | Err(DiscordError::NotFound) => return Ok(Access::Denied),
        Err(error) => return Err(error.into()),
    };
    let Outcome::Roles(roles) = ctx
        .api
        .execute(Action::ListRoles {
            guild_id: ctx.guild_id,
        })
        .await?
    else {
        return Ok(Access::Denied);
    };
    let everyone = roles
        .iter()
        .find(|role| role.id.cast() == ctx.guild_id)
        .map_or(Permissions::empty(), |role| role.permissions);
    let member_roles: Vec<_> = roles
        .iter()
        .filter(|role| member.roles.contains(&role.id))
        .map(|role| (role.id, role.permissions))
        .collect();
    let overwrites = channel.permission_overwrites.as_deref().unwrap_or(&[]);
    let needed = Permissions::VIEW_CHANNEL
        | if channel.kind.is_thread() {
            Permissions::SEND_MESSAGES_IN_THREADS
        } else {
            Permissions::SEND_MESSAGES
        };
    let granted = PermissionCalculator::new(ctx.guild_id, bot, everyone, &member_roles)
        .in_channel(channel.kind, overwrites);
    Ok(if granted.contains(needed) {
        Access::Allowed
    } else {
        Access::Denied
    })
}

/// Sends `text` to `channel`, with the role mentioned when there is one, and nobody else.
///
/// # Errors
/// Discord refuses or cannot be asked.
pub async fn send(
    ctx: &EventContext,
    channel: ChannelId,
    role: Option<RoleId>,
    text: &str,
) -> Result<(), DiscordError> {
    let content = match role {
        Some(role) => format!("<@&{role}> {text}"),
        None => text.to_owned(),
    };
    let mentions = role.map_or(MentionPolicy::None, |role| MentionPolicy::Roles(vec![role]));
    ctx.api
        .execute(Action::SendMessage {
            channel_id: channel,
            content: Some(content),
            embeds: Vec::new(),
            reply_to: None,
            mentions,
            silent: false,
        })
        .await
        .map(drop)
}

/// Posts `text` once for `marker` (an item, or a summary of items): the post is recorded before it
/// is made, so a second look does not make it again. A post that fails is not recorded.
///
/// # Errors
/// See [`PostError`].
pub async fn post_once(
    ctx: &EventContext,
    subscription: &Subscription,
    marker: &str,
    text: &str,
) -> Result<(), PostError> {
    let key = model::posted_key(&subscription.id, marker);
    match ctx
        .store()
        .replace(&key, &true, Expect::Missing, Some(POSTED_TTL))
        .await
    {
        Ok(_) => {}
        // Recorded already: it was posted (or is being posted) by an earlier look.
        Err(StorageError::Conflict) => return Ok(()),
        Err(error) => return Err(PostError::Failed(ModuleError::other(error.to_string()))),
    }
    let Err(error) = send(ctx, subscription.channel, subscription.role, text).await else {
        return Ok(());
    };
    if let Err(forget) = ctx.store().delete(&key).await {
        tracing::debug!(?forget, "a post that failed could not be forgotten");
    }
    match error {
        DiscordError::NotFound => Err(PostError::Gone("feed-paused-channel-missing")),
        DiscordError::Forbidden(_) => Err(PostError::Gone("feed-paused-channel-denied")),
        other => Err(PostError::Failed(other.into())),
    }
}
