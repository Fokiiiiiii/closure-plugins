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
use closure_core::action::{Action, FileUpload, MAX_FILES, Outcome, Reason};
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
use twilight_model::channel::message::embed::{EmbedField, EmbedImage};
use twilight_model::channel::message::{Component, Embed};
use twilight_model::channel::{Channel, Message};
use twilight_model::guild::Permissions;
use twilight_util::permission_calculator::PermissionCalculator;

use crate::links::MessageLink;

pub use plugin_kit::net::{Budget, Fetcher, Get};

const EN: &str = include_str!("../locales/en.ftl");
const JA: &str = include_str!("../locales/ja.ftl");
/// The most characters of the text of one post.
const QUOTE_CHARS: usize = 1200;
/// The text that all the embeds of an answer may hold together: Discord allows 6000 characters in
/// all, with the names, titles and footers among them.
const TEXT_BUDGET: usize = 4400;
/// The most embeds in a message, Discord's limit.
const MAX_EMBEDS: usize = 10;
/// How far up a conversation is followed, and how many quotes inside quotes.
const MAX_ANCESTORS: usize = 8;
const MAX_QUOTES: usize = 5;

/// The characters of text for each of `posts`; a translation under a post takes half of them.
fn text_limit(posts: usize, shared: bool) -> usize {
    let each = (TEXT_BUDGET / posts.max(1)).clamp(120, QUOTE_CHARS);
    if shared { each / 2 } else { each }
}
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
    /// Add the post in the language of the server under it, when it is in another one.
    auto_translate: bool,
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
            auto_translate: true,
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
    // What the bot posts is read by everybody who reads where it is posted: a message of a channel
    // that not everybody may read is quoted only in that same channel.
    if channel_id != source.channel_id
        && !can_read(ctx, closure_core::event::UserId::new(1), &[], &channel).await?
    {
        bail!("the channel is not one everybody may read");
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

/// `post` with the posts it answers, up to the first of the conversation, and the posts it quotes,
/// one inside the other. What cannot be had (a post that is gone, or private) ends the chain.
async fn walk(fetch: &dyn Get, post: x::Tweet, replied: bool, quoted: bool) -> x::Thread {
    let mut thread = x::Thread::new(post);
    let mut seen: Vec<u64> = status_id(&thread.post.url).into_iter().collect();
    if replied {
        let mut next = thread.post.parent_id();
        while let Some(id) = next {
            if seen.contains(&id) || thread.ancestors.len() >= MAX_ANCESTORS {
                break;
            }
            let Ok(parent) = fetch_post(fetch, id).await else {
                break;
            };
            seen.push(id);
            next = parent.parent_id();
            thread.ancestors.insert(0, parent);
        }
    }
    if quoted {
        let mut from = thread.post.clone();
        while thread.quotes.len() < MAX_QUOTES {
            let Some(id) = from.quote_id() else {
                break;
            };
            if seen.contains(&id) {
                break;
            }
            seen.push(id);
            // The post whole, so that it has its counts; what the one before it holds will do.
            let whole = match fetch_post(fetch, id).await {
                Ok(whole) => whole,
                Err(_) => match from.quote.as_deref() {
                    Some(inside) => inside.clone(),
                    None => break,
                },
            };
            thread.quotes.push(whole.clone());
            from = whole;
        }
    }
    thread
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
    /// The link of the post.
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

/// The pictures of a unit as files, and the address of its video, which is posted as a link.
///
/// `room` is what a message can still carry in files, and is used up by what is attached.
async fn attachments(
    fetch: &dyn Get,
    unit: &Unit,
    room: &mut usize,
) -> (Vec<FileUpload>, Option<String>) {
    let mut files = Vec::new();
    for (at, url) in unit.photos.iter().enumerate() {
        match fetch.get_large(url, *room).await {
            Ok(data) => {
                *room -= data.len();
                files.push(FileUpload {
                    filename: format!("photo-{}.{}", at + 1, extension_of(url)),
                    data,
                });
            }
            Err(error) => tracing::warn!(%error, "a picture is not attached"),
        }
    }
    // A video is not downloaded: its address at the video servers of X is the link, and Discord
    // plays that.
    let player = unit.video.clone();
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
    if config.translate_button && !translate.is_empty() {
        // One button for all of the posts, named in base 36 to fit what a button can carry.
        let ids: Vec<String> = translate.iter().map(|id| to_base36(*id)).collect();
        let custom = CustomId::new("translate").data(ids.join(","));
        row.push(ui::button(
            ButtonStyle::Secondary,
            say("linkexpand-translate"),
            custom.encode(&ctx.module),
        ));
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
/// What one message may ask of the network, in calls and in time.
const EVENT_CALLS: usize = 30;
const EVENT_TIME: std::time::Duration = std::time::Duration::from_secs(20);

async fn on_message(
    fetch: Arc<dyn Get>,
    ctx: EventContext,
    event: Event,
) -> Result<Flow, ModuleError> {
    // A message with many links, or a long thread of quotes, gets a part of the network and of the
    // time, not all it asks for: the handler's own limit is not met by waiting on the calls.
    let budget = Budget::new(&*fetch, EVENT_CALLS);
    if let Ok(result) = tokio::time::timeout(EVENT_TIME, expand(&budget, ctx, event)).await {
        result
    } else {
        tracing::debug!("a message took too long to expand; it is left alone");
        Ok(Flow::Continue)
    }
}

#[allow(
    clippy::too_many_lines,
    reason = "one flow: the links of a message, the posts, the pieces, the answer"
)]
async fn expand(get: &dyn Get, ctx: EventContext, event: Event) -> Result<Flow, ModuleError> {
    let Event::MessageCreated(message) = &event else {
        return Ok(Flow::Continue);
    };
    if message.author.bot || message.from_webhook {
        return Ok(Flow::Continue);
    }
    let config: Config = ctx.parse_config()?;
    let mut units: Vec<Unit> = Vec::new();
    let mut open: Option<String> = None;
    // One message holds ten embeds; an answer of messages is as long as its button can name.
    let cap = match config.layout {
        Layout::Chain => MAX_UNITS,
        Layout::Embeds => MAX_EMBEDS,
    };
    if config.message_links {
        for link in links::message_links(&message.content) {
            // Only the server the message is in: another server's messages are not for here.
            if link.guild != ctx.guild_id.get() || units.len() >= cap {
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
    let mut threads: Vec<x::Thread> = Vec::new();
    let mut planned = units.len();
    if config.x_links {
        for link in links::post_links(&message.content) {
            if planned >= cap {
                break;
            }
            let tweet = match fetch_post(get, link.id).await {
                Ok(tweet) => tweet,
                Err(error) => {
                    tracing::debug!(%error, "a post is not expanded");
                    continue;
                }
            };
            let mut thread = walk(get, tweet, config.replied_post, config.quoted_post).await;
            // What does not fit goes, from the far ends: the post that was linked stays.
            thread.fit(cap - planned);
            planned += thread.size();
            threads.push(thread);
        }
    }
    let expanded_post = !threads.is_empty();
    let limit = text_limit(planned, config.auto_translate);
    let words = labels(&ctx);
    for thread in &threads {
        for piece in x::pieces(thread, &words, limit) {
            if piece.main {
                open.get_or_insert_with(|| piece.link.clone());
            }
            let embed = if config.auto_translate {
                with_translation(get, &ctx, piece.embed, limit).await
            } else {
                piece.embed
            };
            units.push(Unit {
                embed,
                photos: piece.photos,
                video: piece.video,
                link: Some(piece.link),
                main: piece.main,
            });
        }
    }
    if units.is_empty() {
        return Ok(Flow::Continue);
    }
    let mut translate: Vec<u64> = Vec::new();
    // The button translates what was linked, with what is around it as the click finds it.
    for id in units
        .iter()
        .filter(|unit| unit.main)
        .filter_map(|unit| unit.link.as_deref().and_then(status_id))
    {
        if !translate.contains(&id) {
            translate.push(id);
        }
    }
    match config.layout {
        Layout::Chain => {
            send_chain(
                &ctx,
                get,
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
                get,
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

/// The embed of a post with its translation under it, when the post is in another language than
/// the server's; a service that does not answer leaves the embed as it is.
async fn with_translation(
    fetch: &dyn Get,
    ctx: &EventContext,
    mut embed: Embed,
    limit: usize,
) -> Embed {
    let to = ctx.locale.tag();
    let Some(text) = embed.description.clone() else {
        return embed;
    };
    let Ok(done) = translate::translate(fetch, &text, to).await else {
        return embed;
    };
    if done.from.split('-').next() == to.split('-').next() || done.text.trim() == text.trim() {
        return embed;
    }
    let label = ctx.t_in(ctx.locale, "linkexpand-translated", &Args::new());
    embed.fields.push(EmbedField {
        inline: false,
        name: format!("{label} ({} → {to})", done.from),
        value: plugin_kit::truncate(&done.text, limit),
    });
    embed
}

/// The number at the end of the address of a post.
fn status_id(link: &str) -> Option<u64> {
    let (_, tail) = link.rsplit_once("/status/")?;
    tail.split(|c: char| !c.is_ascii_digit())
        .next()?
        .parse()
        .ok()
}

fn to_base36(mut number: u64) -> String {
    const DIGITS: &[u8; 36] = b"0123456789abcdefghijklmnopqrstuvwxyz";
    let mut out = Vec::new();
    loop {
        out.push(DIGITS[(number % 36) as usize]);
        number /= 36;
        if number == 0 {
            break;
        }
    }
    out.reverse();
    String::from_utf8_lossy(&out).into_owned()
}

/// Translates the posts of the button and the posts around them, each into the language of whoever
/// pressed it.
async fn translate_click(fetch: Arc<dyn Get>, cx: ComponentCx) -> Result<(), UserError> {
    let failed = || UserError::message("linkexpand-translate-failed");
    let ids: Vec<u64> = cx
        .data()
        .split(',')
        .filter_map(|id| u64::from_str_radix(id, 36).ok())
        .filter(|id| *id > 0)
        .take(MAX_UNITS)
        .collect();
    if ids.is_empty() {
        return Err(failed());
    }
    cx.defer(true).await?;
    let to = cx.locale().tag();
    let mut posts: Vec<x::Tweet> = Vec::new();
    for id in ids {
        let Ok(tweet) = fetch_post(&*fetch, id).await else {
            continue;
        };
        let thread = walk(&*fetch, tweet, true, true).await;
        posts.extend(thread.ancestors);
        posts.push(thread.post);
        posts.extend(thread.quotes);
    }
    posts.truncate(MAX_EMBEDS);
    let each = (TEXT_BUDGET / posts.len().max(1)).clamp(120, 1300);
    let mut embeds = Vec::new();
    for tweet in &posts {
        // A post that cannot be translated is left out; the others still are told.
        let Ok(done) = translate::translate(&*fetch, &tweet.text, to).await else {
            continue;
        };
        let text = format!(
            "**{} (@{})**\n{}",
            tweet.author.name,
            tweet.author.screen_name,
            plugin_kit::truncate(&done.text, each)
        );
        let embed = closure_sdk::EmbedBuilder::new()
            .description(text)
            .footer(format!("{} → {to}", done.from))
            .build()
            .map_err(|_| failed())?;
        embeds.push(embed);
    }
    if embeds.is_empty() {
        return Err(failed());
    }
    cx.reply_with(InteractionReply {
        content: None,
        embeds,
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
            "auto_translate": switch("auto_translate"),
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
                    "auto_translate": true,
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
/// Posts one message of the answer, as a reply to `reply_to` when there is one, and tells its id.
/// Only the first message of an answer replies, to the message that had the link: the messages of
/// the answer do not reply to one another.
async fn post(
    ctx: &EventContext,
    channel_id: ChannelId,
    reply_to: Option<MessageId>,
    content: Option<String>,
    embeds: Vec<Embed>,
    components: Vec<Component>,
    files: Vec<FileUpload>,
) -> Result<Option<MessageId>, ModuleError> {
    let posted = ctx
        .api
        .execute(Action::PostMessage {
            channel_id,
            message_id: None,
            reply_to,
            content,
            embeds,
            components,
            files,
        })
        .await?;
    Ok(match posted {
        Outcome::Message(posted) => Some(posted.id),
        _ => None,
    })
}

/// The videos of the answer, as a message of their own that has nothing but their links: Discord
/// makes no player of a link in a message that has embeds, but it does of one that has not.
async fn post_videos(
    ctx: &EventContext,
    message: &MessageInfo,
    reply_to: Option<MessageId>,
    videos: Vec<String>,
    components: Vec<Component>,
) -> Result<Option<MessageId>, ModuleError> {
    post(
        ctx,
        message.channel_id,
        reply_to,
        Some(videos.join("\n")),
        Vec::new(),
        components,
        Vec::new(),
    )
    .await
}

/// All of the answer in one message: an embed for each post, and the pictures of what was linked
/// attached. A post shown for the sake of another has its first picture in its embed, under its
/// text, so that it is clear whose it is. The videos come first, as a message of their own.
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
    let mut room = MAX_FILES_BYTES;
    let mut videos: Vec<String> = Vec::new();
    let mut embeds = Vec::new();
    for mut unit in units {
        if unit.main {
            let (more, video) = attachments(fetch, &unit, &mut room).await;
            files.extend(more);
            videos.extend(video);
        } else {
            videos.extend(unit.video.clone());
            if let Some(url) = unit
                .photos
                .first()
                .filter(|url| url.starts_with("https://"))
            {
                unit.embed.image = Some(EmbedImage {
                    url: url.clone(),
                    proxy_url: None,
                    height: None,
                    width: None,
                });
            }
        }
        embeds.push(unit.embed);
    }
    let by = message.author.id;
    // The videos come first, as a message of their own; the message with the buttons names it.
    let mut earlier: Vec<MessageId> = Vec::new();
    // The first message of the answer replies to the link; the others do not reply at all.
    let mut reply_to = Some(message.id);
    if !videos.is_empty()
        && let Some(id) = post_videos(ctx, message, reply_to, videos, Vec::new()).await?
    {
        earlier.push(id);
        reply_to = None;
    }
    // Discord takes ten files in a message: what is over is left out, not the whole answer.
    files.truncate(MAX_FILES);
    post(
        ctx,
        message.channel_id,
        reply_to,
        None,
        embeds,
        buttons(ctx, config, by, translate, open, &earlier),
        files,
    )
    .await?;
    Ok(())
}

/// The answer as the videos in a message of their own, then a message for each post; the buttons
/// are under the last one and take them all away.
async fn send_chain(
    ctx: &EventContext,
    fetch: &dyn Get,
    message: &MessageInfo,
    units: &[Unit],
    config: &Config,
    translate: &[u64],
    open: Option<&str>,
) -> Result<(), ModuleError> {
    let by = message.author.id;
    // The message of the videos is one of the messages the button names.
    let units = if units.iter().any(|unit| unit.video.is_some()) {
        &units[..units.len().min(MAX_UNITS - 1)]
    } else {
        units
    };
    let mut posts = Vec::new();
    let mut videos: Vec<String> = Vec::new();
    for unit in units {
        let (files, video) = attachments(fetch, unit, &mut MAX_FILES_BYTES.clone()).await;
        videos.extend(video);
        posts.push((unit, files));
    }
    let mut reply_to = Some(message.id);
    let mut sent: Vec<MessageId> = Vec::new();
    if !videos.is_empty()
        && let Some(id) = post_videos(ctx, message, reply_to, videos, Vec::new()).await?
    {
        reply_to = None;
        sent.push(id);
    }
    let count = posts.len();
    for (at, (unit, files)) in posts.into_iter().enumerate() {
        let components = if at + 1 == count {
            buttons(ctx, config, by, translate, open, &sent)
        } else {
            Vec::new()
        };
        let posted = post(
            ctx,
            message.channel_id,
            reply_to,
            None,
            vec![unit.embed.clone()],
            components,
            files,
        )
        .await?;
        reply_to = None;
        if let Some(id) = posted {
            sent.push(id);
        }
    }
    Ok(())
}
