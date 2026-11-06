//! Answers a link to a message of the server, or to a post on X, with a preview of it.
//!
//! - A link to a message of the same server is answered with a quote of it: who said it, what, the
//!   first picture, and a link back. The quote is made only when the person who posted the link
//!   can read that channel, and never for a link to another server.
//! - A link to a post on X is answered with its text, up to four pictures and its counts, through
//!   the `FxTwitter` service. A video is posted as a link, which Discord plays.
//!
//! A link written as `<https://...>` is left alone.

#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

mod fetch;
pub mod links;
mod x;

use std::sync::Arc;

use anyhow::{Result, bail};
use closure_core::action::{Action, MentionPolicy, Outcome};
use closure_core::error::ModuleError;
use closure_core::event::{ChannelId, Event, EventKind, MessageInfo, RoleId};
use closure_core::module::EventContext;
use closure_core::subscription::Flow;
use closure_sdk::{CoreError, Intents, Locale, ModuleBuilder, SdkModule, Subscription};
use plugin_kit::{Quoted, quote_embed};
use serde::Deserialize;
use serde_json::json;
use twilight_model::channel::message::Embed;
use twilight_model::channel::{Channel, Message};
use twilight_model::guild::Permissions;
use twilight_util::permission_calculator::PermissionCalculator;

use crate::links::{MAX_PER_MESSAGE, MessageLink};

pub use fetch::{Fetcher, Get};

const EN: &str = include_str!("../locales/en.ftl");
const JA: &str = include_str!("../locales/ja.ftl");
/// The most characters of a quoted text: three quotes have to fit in one message.
const QUOTE_CHARS: usize = 1200;
const VIEW: Permissions = Permissions::VIEW_CHANNEL.union(Permissions::READ_MESSAGE_HISTORY);

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, default)]
struct Config {
    message_links: bool,
    x_links: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            message_links: true,
            x_links: true,
        }
    }
}

fn avatar(user: &twilight_model::user::User) -> Option<String> {
    user.avatar
        .map(|hash| format!("https://cdn.discordapp.com/avatars/{}/{hash}.png", user.id))
}

/// What of a message a quote shows.
fn quoted(message: &Message, guild: u64, place: &str) -> Quoted {
    Quoted {
        author: message
            .author
            .global_name
            .clone()
            .unwrap_or_else(|| message.author.name.clone()),
        avatar: avatar(&message.author),
        content: message.content.clone(),
        timestamp: Some(message.timestamp.as_secs()),
        image: message
            .attachments
            .iter()
            .find(|a| {
                a.content_type
                    .as_deref()
                    .is_some_and(|k| k.starts_with("image/"))
            })
            .map(|a| a.url.clone()),
        jump: format!(
            "https://discord.com/channels/{guild}/{}/{}",
            message.channel_id, message.id
        ),
        place: format!("#{place}"),
    }
}

/// Whether a member with `roles` can read `channel` of the guild of `ctx`.
async fn can_read(
    ctx: &EventContext,
    member: closure_core::event::UserId,
    roles: &[RoleId],
    channel: &Channel,
) -> Result<bool> {
    let Outcome::Roles(all) = ctx
        .api
        .execute(Action::ListRoles {
            guild_id: ctx.guild_id,
        })
        .await?
    else {
        bail!("the roles of the server are not known");
    };
    let everyone = all
        .iter()
        .find(|role| role.id.cast() == ctx.guild_id)
        .map_or(Permissions::empty(), |role| role.permissions);
    let held: Vec<(RoleId, Permissions)> = all
        .iter()
        .filter(|role| roles.contains(&role.id))
        .map(|role| (role.id, role.permissions))
        .collect();
    let mut calculator = PermissionCalculator::new(ctx.guild_id, member, everyone, &held);
    if let Some(owner) = ctx.api.guild_owner().await? {
        calculator = calculator.owner_id(owner);
    }
    let overwrites = channel.permission_overwrites.as_deref().unwrap_or(&[]);
    Ok(calculator
        .in_channel(channel.kind, overwrites)
        .contains(VIEW))
}

/// The quote of the message `link` leads to, when the author of the link may read it.
async fn quote_of(ctx: &EventContext, source: &MessageInfo, link: &MessageLink) -> Result<Embed> {
    let channel_id = ChannelId::new(link.channel);
    let Outcome::Channel(channel) = ctx.api.execute(Action::GetChannel { channel_id }).await?
    else {
        bail!("the channel is not known");
    };
    // A thread takes its permissions from its parent, which is not looked at here.
    if channel.kind.is_thread() {
        bail!("a thread");
    }
    if !can_read(ctx, source.author.id, &source.author_roles, &channel).await? {
        bail!("the author cannot read that channel");
    }
    let target = ctx
        .api
        .get_message(
            channel_id,
            closure_core::event::MessageId::new(link.message),
        )
        .await?;
    let place = channel.name.as_deref().unwrap_or("channel");
    Ok(quote_embed(
        &quoted(&target, ctx.guild_id.get(), place),
        QUOTE_CHARS,
        &ctx.t_in(
            ctx.locale,
            "linkexpand-source",
            &closure_core::locale::Args::new(),
        ),
    ))
}

async fn on_message(
    fetch: Arc<dyn Get>,
    ctx: EventContext,
    event: Event,
) -> Result<Flow, ModuleError> {
    let Event::MessageCreated(message) = &event else {
        return Ok(Flow::Continue);
    };
    if message.author.bot || message.from_webhook {
        return Ok(Flow::Continue);
    }
    let config: Config = ctx.parse_config()?;
    let mut embeds: Vec<Embed> = Vec::new();
    let mut videos: Vec<String> = Vec::new();
    let mut budget = MAX_PER_MESSAGE;
    if config.message_links {
        for link in links::message_links(&message.content) {
            // Only the server the message is in: another server's messages are not for here.
            if link.guild != ctx.guild_id.get() || budget == 0 {
                continue;
            }
            match quote_of(&ctx, message, &link).await {
                Ok(embed) => {
                    embeds.push(embed);
                    budget -= 1;
                }
                Err(error) => tracing::debug!(%error, "a message link is not expanded"),
            }
        }
    }
    if config.x_links {
        for link in links::post_links(&message.content) {
            if budget == 0 {
                break;
            }
            let url = format!("https://api.fxtwitter.com/{}/status/{}", link.user, link.id);
            let post = match fetch.get(&url).await {
                Ok(bytes) => x::parse_tweet(&bytes),
                Err(error) => Err(error),
            };
            match post {
                Ok(tweet) => {
                    let shown = x::tweet_embeds(&tweet, QUOTE_CHARS);
                    embeds.extend(shown.embeds);
                    videos.extend(shown.video);
                    budget -= 1;
                }
                Err(error) => tracing::debug!(%error, "a post is not expanded"),
            }
        }
    }
    embeds.truncate(10);
    if embeds.is_empty() && videos.is_empty() {
        return Ok(Flow::Continue);
    }
    ctx.api
        .execute(Action::SendMessage {
            channel_id: message.channel_id,
            content: (!videos.is_empty()).then(|| videos.join("\n")),
            embeds,
            reply_to: Some(message.id),
            mentions: MentionPolicy::None,
        })
        .await?;
    Ok(Flow::Continue)
}

/// The module, with the network it reaches the `FxTwitter` service through.
///
/// # Errors
/// The module declaration is refused by the SDK.
pub fn builder_with(fetch: Arc<dyn Get>) -> Result<ModuleBuilder, CoreError> {
    let schema = json!({
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "message_links": {
                "type": "boolean",
                "description": "config-linkexpand-message_links",
                "x-ui": { "level": "basic" }
            },
            "x_links": {
                "type": "boolean",
                "description": "config-linkexpand-x_links",
                "x-ui": { "level": "basic" }
            }
        }
    });
    Ok(
        ModuleBuilder::new("linkexpand", "Link expansion", env!("CARGO_PKG_VERSION"))?
            .capability(closure_sdk::Capability::SendMessages)
            .capability(closure_sdk::Capability::ReadMessages)
            .capability(closure_sdk::Capability::ReadGuild)
            .intents(Intents::GUILD_MESSAGES | Intents::MESSAGE_CONTENT)
            .config(schema, json!({"message_links": true, "x_links": true}))
            .on(
                Subscription::new(EventKind::MessageCreated),
                move |ctx, event| on_message(Arc::clone(&fetch), ctx, event),
            )
            .catalog(Locale::En, EN)
            .catalog(Locale::Ja, JA)
            .enabled_by_default(),
    )
}

/// The module, reaching the network as it is.
///
/// # Errors
/// The module declaration is refused by the SDK.
pub fn module() -> Result<SdkModule, CoreError> {
    builder_with(Arc::new(Fetcher::new()))?.build()
}

#[cfg(test)]
mod tests;
