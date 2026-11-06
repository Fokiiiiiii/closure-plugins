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

pub mod links;
mod x;

use std::sync::Arc;

use anyhow::{Result, bail};
use closure_core::action::{Action, Outcome, Reason};
use closure_core::error::ModuleError;
use closure_core::event::{ChannelId, Event, EventKind, MessageInfo, RoleId};
use closure_core::module::EventContext;
use closure_core::subscription::Flow;
use closure_sdk::ui::{self, CustomId};
use closure_sdk::{
    Args, ComponentCx, CoreError, Intents, InteractionReply, Locale, ModuleBuilder, SdkModule,
    Subscription, UserError,
};
use plugin_kit::translate;
use plugin_kit::{Quoted, quote_embed};
use serde::Deserialize;
use serde_json::json;
use twilight_model::channel::message::Embed;
use twilight_model::channel::message::component::ButtonStyle;
use twilight_model::channel::{Channel, Message};
use twilight_model::guild::Permissions;
use twilight_util::permission_calculator::PermissionCalculator;

use crate::links::{MAX_PER_MESSAGE, MessageLink};

pub use plugin_kit::net::{Fetcher, Get};

const EN: &str = include_str!("../locales/en.ftl");
const JA: &str = include_str!("../locales/ja.ftl");
/// The most characters of a quoted text: three quotes have to fit in one message.
const QUOTE_CHARS: usize = 1200;
const VIEW: Permissions = Permissions::VIEW_CHANNEL.union(Permissions::READ_MESSAGE_HISTORY);

#[allow(
    clippy::struct_excessive_bools,
    reason = "the settings, one switch each"
)]
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, default)]
struct Config {
    message_links: bool,
    x_links: bool,
    /// Show the post a post replies to.
    replied_post: bool,
    /// Show the post a post quotes.
    quoted_post: bool,
    /// A button that translates the post.
    translate_button: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            message_links: true,
            x_links: true,
            replied_post: true,
            quoted_post: true,
            translate_button: true,
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

/// The service, asked about the post `id`.
async fn fetch_post(fetch: &dyn Get, id: u64) -> Result<x::Tweet> {
    let bytes = fetch
        .get(&format!("https://api.fxtwitter.com/status/{id}"))
        .await?;
    x::parse_tweet(&bytes)
}

fn labels(ctx: &EventContext) -> x::Labels {
    let say = |key: &str| ctx.t_in(ctx.locale, key, &Args::new());
    x::Labels {
        replied_to: say("linkexpand-replied-to"),
        quoted: say("linkexpand-quoted"),
        reposted_by: say("linkexpand-reposted-by"),
    }
}

#[allow(
    clippy::too_many_lines,
    reason = "one flow: find the links, expand them, answer"
)]
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
    let mut translate: Vec<u64> = Vec::new();
    let mut open: Option<String> = None;
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
        let words = labels(&ctx);
        for link in links::post_links(&message.content) {
            if budget == 0 {
                break;
            }
            let tweet = match fetch_post(&*fetch, link.id).await {
                Ok(tweet) => tweet,
                Err(error) => {
                    tracing::debug!(%error, "a post is not expanded");
                    continue;
                }
            };
            // Only the first post brings the post it answers: three threads would not fit.
            let parent = match tweet.parent_id() {
                Some(id) if config.replied_post && translate.is_empty() => {
                    fetch_post(&*fetch, id).await.ok()
                }
                _ => None,
            };
            let shown = x::expand(
                &tweet,
                parent.as_ref(),
                config.quoted_post,
                &words,
                QUOTE_CHARS,
            );
            embeds.extend(shown.embeds);
            videos.extend(shown.video);
            open.get_or_insert(shown.link);
            translate.push(link.id);
            budget -= 1;
        }
    }
    embeds.truncate(10);
    if embeds.is_empty() && videos.is_empty() {
        return Ok(Flow::Continue);
    }
    let say = |key: &str| ctx.t_in(ctx.locale, key, &Args::new());
    let mut buttons = Vec::new();
    if config.translate_button {
        let several = translate.len() > 1;
        for (at, id) in translate.iter().take(3).enumerate() {
            let label = if several {
                format!("{} {}", say("linkexpand-translate"), at + 1)
            } else {
                say("linkexpand-translate")
            };
            let custom = CustomId::new("translate").data(id.to_string());
            buttons.push(ui::button(
                ButtonStyle::Secondary,
                label,
                custom.encode(&ctx.module),
            ));
        }
    }
    if let Some(link) = open {
        buttons.push(ui::link_button(say("linkexpand-open"), link));
    }
    // Whoever posted the link can take the answer away again.
    let delete = CustomId::new("delete").only_for(message.author.id);
    buttons.push(ui::button(
        ButtonStyle::Danger,
        say("linkexpand-delete"),
        delete.encode(&ctx.module),
    ));
    ctx.api
        .execute(Action::PostMessage {
            channel_id: message.channel_id,
            message_id: None,
            reply_to: Some(message.id),
            content: (!videos.is_empty()).then(|| videos.join("\n")),
            embeds,
            components: vec![ui::row(buttons)],
        })
        .await?;
    Ok(Flow::Continue)
}

/// Translates the post of the button into the language of whoever pressed it.
async fn translate_click(fetch: Arc<dyn Get>, cx: ComponentCx) -> Result<(), UserError> {
    let Some(id) = cx.data().parse::<u64>().ok().filter(|id| *id > 0) else {
        return Err(UserError::message("linkexpand-translate-failed"));
    };
    cx.defer(true).await?;
    let to = cx.locale().tag();
    let tweet = fetch_post(&*fetch, id)
        .await
        .map_err(|_| UserError::message("linkexpand-translate-failed"))?;
    let done = translate::translate(&*fetch, &tweet.text, to)
        .await
        .map_err(|_| UserError::message("linkexpand-translate-failed"))?;
    let embed = closure_sdk::EmbedBuilder::new()
        .description(plugin_kit::truncate(&done.text, 3500))
        .footer(format!("{} → {to}", done.from))
        .build()
        .map_err(|_| UserError::message("linkexpand-translate-failed"))?;
    cx.reply_with(InteractionReply {
        content: None,
        embeds: vec![embed],
        ephemeral: true,
        components: Vec::new(),
    })
    .await
}

/// Takes the answer away; only the one who posted the link gets this far.
async fn delete_click(cx: ComponentCx) -> Result<(), UserError> {
    cx.defer_update().await?;
    let click = cx.click();
    let reason = Reason::new(cx.t("linkexpand-delete-reason", &Args::new()))
        .map_err(|_| UserError::message("linkexpand-translate-failed"))?;
    cx.api()
        .delete_message(click.channel_id, click.message_id, reason)
        .await
        .map_err(|_| UserError::message("linkexpand-translate-failed"))?;
    Ok(())
}

/// The module, with the network it reaches the services through.
///
/// # Errors
/// The module declaration is refused by the SDK.
pub fn builder_with(fetch: Arc<dyn Get>) -> Result<ModuleBuilder, CoreError> {
    let switch = |key: &str| {
        json!({
            "type": "boolean",
            "description": format!("config-linkexpand-{key}"),
            "x-ui": { "level": "basic" }
        })
    };
    let schema = json!({
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "message_links": switch("message_links"),
            "x_links": switch("x_links"),
            "replied_post": switch("replied_post"),
            "quoted_post": switch("quoted_post"),
            "translate_button": switch("translate_button")
        }
    });
    let for_message = Arc::clone(&fetch);
    Ok(
        ModuleBuilder::new("linkexpand", "Link expansion", env!("CARGO_PKG_VERSION"))?
            .capability(closure_sdk::Capability::SendMessages)
            .capability(closure_sdk::Capability::ReadMessages)
            .capability(closure_sdk::Capability::ReadGuild)
            .capability(closure_sdk::Capability::ManageMessages)
            .capability(closure_sdk::Capability::RespondToInteractions)
            .intents(Intents::GUILD_MESSAGES | Intents::MESSAGE_CONTENT)
            .config(
                schema,
                json!({
                    "message_links": true, "x_links": true, "replied_post": true,
                    "quoted_post": true, "translate_button": true
                }),
            )
            .on(
                Subscription::new(EventKind::MessageCreated),
                move |ctx, event| on_message(Arc::clone(&for_message), ctx, event),
            )
            .component("translate", move |cx| {
                translate_click(Arc::clone(&fetch), cx)
            })
            .component("delete", delete_click)
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
