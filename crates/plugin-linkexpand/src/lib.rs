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
use closure_core::action::{Action, FileUpload, Outcome, Reason};
use closure_core::error::ModuleError;
use closure_core::event::{ChannelId, Event, EventKind, MessageId, MessageInfo, RoleId};
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
use twilight_model::channel::message::component::ButtonStyle;
use twilight_model::channel::message::embed::EmbedThumbnail;
use twilight_model::channel::message::{Component, Embed};
use twilight_model::channel::{Channel, Message};
use twilight_model::guild::Permissions;
use twilight_util::permission_calculator::PermissionCalculator;

use crate::links::MessageLink;

pub use plugin_kit::net::{Fetcher, Get};

const EN: &str = include_str!("../locales/en.ftl");
const JA: &str = include_str!("../locales/ja.ftl");
/// The most characters of a quoted text: three quotes have to fit in one message.
const QUOTE_CHARS: usize = 1200;
const VIEW: Permissions = Permissions::VIEW_CHANNEL.union(Permissions::READ_MESSAGE_HISTORY);

/// How the posts of a thread are laid out in Discord.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Layout {
    /// One message with an embed for each post.
    #[default]
    Embeds,
    /// A message for each post, each answering the one before it.
    Chain,
}

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
    /// Hide the preview Discord made of the link, which the answer repeats.
    hide_original: bool,
    layout: Layout,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            message_links: true,
            x_links: true,
            replied_post: true,
            quoted_post: true,
            translate_button: true,
            hide_original: true,
            layout: Layout::Embeds,
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

/// One message of the answer: an embed, and what is attached to it.
struct Unit {
    embed: Embed,
    photos: Vec<String>,
    video: Option<String>,
    /// The link of the post, for a video that is too large to attach.
    link: Option<String>,
    /// Whether this is what the writer linked, and not the post it answers or quotes.
    main: bool,
}

/// The most messages one answer is made of. The button that takes the answer away names the others
/// in its `custom_id`, which has room for three ids besides its own.
const MAX_UNITS: usize = 4;
/// The most bytes of files in one message: Discord's limit, with room to spare.
const MAX_FILES_BYTES: usize = 9_000_000;

fn extension_of(url: &str) -> &str {
    let name = url.split('?').next().unwrap_or(url);
    match name.rsplit('.').next() {
        Some(ext @ ("jpg" | "jpeg" | "png" | "gif" | "webp" | "mp4")) => ext,
        _ => "jpg",
    }
}

/// The pictures and the video of a unit as files, and a player link when the video does not fit.
async fn attachments(fetch: &dyn Get, unit: &Unit) -> (Vec<FileUpload>, Option<String>) {
    let mut files = Vec::new();
    let mut room = MAX_FILES_BYTES;
    for (at, url) in unit.photos.iter().enumerate() {
        if let Ok(data) = fetch.get_large(url, room).await {
            room -= data.len();
            files.push(FileUpload {
                filename: format!("photo-{}.{}", at + 1, extension_of(url)),
                data,
            });
        }
    }
    let mut player = None;
    if let Some(url) = &unit.video {
        match fetch.get_large(url, room).await {
            Ok(data) => files.push(FileUpload {
                filename: format!("video.{}", extension_of(url)),
                data,
            }),
            Err(error) => {
                tracing::debug!(%error, "a video is not attached");
                player = unit.link.as_deref().map(x::player_link);
            }
        }
    }
    (files, player)
}

/// The buttons under the last message of the answer.
fn buttons(
    ctx: &EventContext,
    config: &Config,
    posted_by: closure_core::event::UserId,
    translate: &[u64],
    open: Option<&str>,
    earlier: &[MessageId],
) -> Vec<Component> {
    let say = |key: &str| ctx.t_in(ctx.locale, key, &Args::new());
    let mut row = Vec::new();
    if config.translate_button {
        for (at, id) in translate.iter().take(3).enumerate() {
            let label = if translate.len() > 1 {
                format!("{} {}", say("linkexpand-translate"), at + 1)
            } else {
                say("linkexpand-translate")
            };
            let custom = CustomId::new("translate").data(id.to_string());
            row.push(ui::button(
                ButtonStyle::Secondary,
                label,
                custom.encode(&ctx.module),
            ));
        }
    }
    if let Some(link) = open {
        row.push(ui::link_button(say("linkexpand-open"), link));
    }
    // Whoever posted the link can take the whole answer away again.
    let ids: Vec<String> = earlier.iter().map(ToString::to_string).collect();
    let delete = CustomId::new("delete")
        .only_for(posted_by)
        .data(ids.join(","));
    row.push(ui::button(
        ButtonStyle::Danger,
        say("linkexpand-delete"),
        delete.encode(&ctx.module),
    ));
    vec![ui::row(row)]
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
    let mut units: Vec<Unit> = Vec::new();
    let mut translate: Vec<u64> = Vec::new();
    let mut open: Option<String> = None;
    if config.message_links {
        for link in links::message_links(&message.content) {
            // Only the server the message is in: another server's messages are not for here.
            if link.guild != ctx.guild_id.get() || units.len() >= MAX_UNITS {
                continue;
            }
            match quote_of(&ctx, message, &link).await {
                Ok(embed) => units.push(Unit {
                    embed,
                    photos: Vec::new(),
                    video: None,
                    link: None,
                    main: true,
                }),
                Err(error) => tracing::debug!(%error, "a message link is not expanded"),
            }
        }
    }
    let mut expanded_post = false;
    if config.x_links {
        let words = labels(&ctx);
        for link in links::post_links(&message.content) {
            if units.len() >= MAX_UNITS {
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
            // The quoted post comes whole, so that it has its counts.
            let full = match tweet.quote_id() {
                Some(id) if config.quoted_post => fetch_post(&*fetch, id).await.ok(),
                _ => None,
            };
            let quoted = if config.quoted_post {
                full.as_ref().or(tweet.quote.as_deref())
            } else {
                None
            };
            let mut shown = x::pieces(&tweet, parent.as_ref(), quoted, &words, QUOTE_CHARS);
            // What does not fit goes: the post that was linked stays.
            while units.len() + shown.len() > MAX_UNITS {
                if let Some(at) = shown.iter().position(|p| !p.main) {
                    shown.remove(at);
                } else {
                    break;
                }
            }
            for piece in shown {
                if piece.main {
                    open.get_or_insert_with(|| piece.link.clone());
                }
                units.push(Unit {
                    embed: piece.embed,
                    photos: piece.photos,
                    video: piece.video,
                    link: Some(piece.link),
                    main: piece.main,
                });
            }
            translate.push(link.id);
            expanded_post = true;
        }
    }
    if units.is_empty() {
        return Ok(Flow::Continue);
    }
    match config.layout {
        Layout::Chain => {
            send_chain(
                &ctx,
                &*fetch,
                message,
                &units,
                &config,
                &translate,
                open.as_deref(),
            )
            .await?;
        }
        Layout::Embeds => {
            send_embeds(
                &ctx,
                &*fetch,
                message,
                units,
                &config,
                &translate,
                open.as_deref(),
            )
            .await?;
        }
    }
    if expanded_post && config.hide_original {
        // Discord's own preview of the link would repeat the answer; this needs Manage Messages.
        let hidden = ctx
            .api
            .execute(Action::SuppressEmbeds {
                channel_id: message.channel_id,
                message_id: message.id,
            })
            .await;
        if let Err(error) = hidden {
            tracing::debug!(?error, "the preview of the link stays");
        }
    }
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

/// Takes the answer away, all of its messages; only the one who posted the link gets this far.
async fn delete_click(cx: ComponentCx) -> Result<(), UserError> {
    cx.defer_update().await?;
    let click = cx.click();
    let reason = Reason::new(cx.t("linkexpand-delete-reason", &Args::new()))
        .map_err(|_| UserError::message("linkexpand-translate-failed"))?;
    // The messages before this one are named in the button; they may be gone already.
    for id in cx
        .data()
        .split(',')
        .filter_map(|id| id.parse::<u64>().ok())
        .filter(|id| *id > 0)
        .take(MAX_UNITS - 1)
    {
        let _ = cx
            .api()
            .delete_message(click.channel_id, MessageId::new(id), reason.clone())
            .await;
    }
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
            "translate_button": switch("translate_button"),
            "hide_original": switch("hide_original"),
            "layout": {
                "type": "string",
                "enum": ["embeds", "chain"],
                "description": "config-linkexpand-layout",
                "x-ui": { "level": "basic" }
            }
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
                    "quoted_post": true, "translate_button": true, "hide_original": true,
                    "layout": "embeds"
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
/// All of the answer in one message: an embed for each post, and the pictures and the video of
/// what was linked attached. A post shown for the sake of another has its first picture as a small
/// one in its embed, so that it is clear whose it is.
async fn send_embeds(
    ctx: &EventContext,
    fetch: &dyn Get,
    message: &MessageInfo,
    units: Vec<Unit>,
    config: &Config,
    translate: &[u64],
    open: Option<&str>,
) -> Result<(), ModuleError> {
    let mut files = Vec::new();
    let mut player = None;
    let mut embeds = Vec::new();
    for mut unit in units {
        if unit.main {
            let (more, link) = attachments(fetch, &unit).await;
            files.extend(more);
            player = player.or(link);
        } else if let Some(url) = unit
            .photos
            .first()
            .filter(|url| url.starts_with("https://"))
        {
            unit.embed.thumbnail = Some(EmbedThumbnail {
                url: url.clone(),
                proxy_url: None,
                height: None,
                width: None,
            });
        }
        embeds.push(unit.embed);
    }
    ctx.api
        .execute(Action::PostMessage {
            channel_id: message.channel_id,
            message_id: None,
            reply_to: Some(message.id),
            content: player,
            embeds,
            components: buttons(ctx, config, message.author.id, translate, open, &[]),
            files,
        })
        .await?;
    Ok(())
}

/// The answer as a message for each post, each answering the one before it; the buttons are under
/// the last one and take them all away.
async fn send_chain(
    ctx: &EventContext,
    fetch: &dyn Get,
    message: &MessageInfo,
    units: &[Unit],
    config: &Config,
    translate: &[u64],
    open: Option<&str>,
) -> Result<(), ModuleError> {
    let mut previous = message.id;
    let mut sent: Vec<MessageId> = Vec::new();
    for (at, unit) in units.iter().enumerate() {
        let (files, player) = attachments(fetch, unit).await;
        let components = if at + 1 == units.len() {
            buttons(ctx, config, message.author.id, translate, open, &sent)
        } else {
            Vec::new()
        };
        let posted = ctx
            .api
            .execute(Action::PostMessage {
                channel_id: message.channel_id,
                message_id: None,
                reply_to: Some(previous),
                content: player,
                embeds: vec![unit.embed.clone()],
                components,
                files,
            })
            .await?;
        if let Outcome::Message(posted) = posted {
            previous = posted.id;
            sent.push(posted.id);
        }
    }
    Ok(())
}
