//! A starboard: a message that gets enough of an emoji (a star by default) is posted to a channel,
//! and its count follows the reactions.
//!
//! A message in an age-restricted channel, in a thread, or in a channel that not everybody can
//! read is never posted. The author's own star counts only when the server says so.

#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

pub mod rules;

use std::sync::Arc;

use closure_core::action::{Action, Emoji, MentionPolicy, Outcome, Reason};
use closure_core::error::{DiscordError, ModuleError};
use closure_core::event::{ChannelId, Event, EventKind, GuildId, MessageId, UserId};
use closure_core::locale::Args;
use closure_core::module::EventContext;
use closure_core::subscription::Flow;
use closure_sdk::{CoreError, Intents, Locale, ModuleBuilder, SdkModule, Subscription};
use plugin_kit::{Quoted, quote_embed};
use serde::Deserialize;
use serde_json::json;
use twilight_model::channel::message::EmojiReactionType;
use twilight_model::channel::{Channel, Message};
use twilight_model::guild::Permissions;
use twilight_util::permission_calculator::PermissionCalculator;

use crate::rules::Step;

const EN: &str = include_str!("../locales/en.ftl");
const JA: &str = include_str!("../locales/ja.ftl");
/// The most characters of a quoted text.
const QUOTE_CHARS: usize = 2400;
const DEFAULT_EMOJI: &str = "⭐";

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, default)]
struct Config {
    /// The channel the posts go to; the starboard is off without one.
    channel: Option<String>,
    threshold: u32,
    emoji: String,
    self_star: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            channel: None,
            threshold: 3,
            emoji: DEFAULT_EMOJI.to_owned(),
            self_star: false,
        }
    }
}

impl Config {
    fn board(&self) -> Option<ChannelId> {
        self.channel
            .as_deref()
            .and_then(|text| text.parse::<u64>().ok())
            .filter(|id| *id > 0)
            .map(ChannelId::new)
    }
}

/// Where the post of a starred message is.
#[derive(Debug)]
struct Posted {
    board: u64,
    post: u64,
}

/// The name of the score that holds, for a starred message, the id of its post on the board.
const POSTS: &str = "post";
/// The name of the score that holds the board a post was made on.
const BOARDS: &str = "post-board";
/// How many locks take turns: reactions to the same message wait for each other, those to
/// messages that fall in other places do not.
const LOCKS: u64 = 256;

struct Locks(Vec<tokio::sync::Mutex<()>>);

impl Locks {
    fn new() -> Self {
        Self((0..LOCKS).map(|_| tokio::sync::Mutex::new(())).collect())
    }

    fn of(&self, message: MessageId) -> &tokio::sync::Mutex<()> {
        &self.0[usize::try_from(message.get() % LOCKS).unwrap_or(0)]
    }
}

impl std::fmt::Debug for Locks {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Locks").finish_non_exhaustive()
    }
}

/// The post of `source`, when it has one: kept as a score, a row for each starred message that
/// costs the same however many there are.
async fn posted_of(
    ctx: &EventContext,
    source: MessageId,
    board: ChannelId,
) -> Result<Option<Posted>, ModuleError> {
    let post = ctx
        .store()
        .score(POSTS, source.get())
        .await
        .map_err(|e| stored(&e))?;
    // The board the post was made on, kept with it: a post is managed where it is, whatever the
    // setting says now. Posts made before this was kept are taken to be on the board of now.
    let made_on = ctx
        .store()
        .score(BOARDS, source.get())
        .await
        .map_err(|e| stored(&e))?;
    Ok(u64::try_from(post)
        .ok()
        .filter(|post| *post > 0)
        .map(|post| Posted {
            board: u64::try_from(made_on)
                .ok()
                .filter(|board| *board > 0)
                .unwrap_or_else(|| board.get()),
            post,
        }))
}

async fn forget(ctx: &EventContext, source: MessageId) -> Result<(), ModuleError> {
    for name in [POSTS, BOARDS] {
        ctx.store()
            .score_put(name, source.get(), 0)
            .await
            .map_err(|e| stored(&e))?;
    }
    Ok(())
}

/// Takes the post of `posted` off its board. Gone already is as good as removed; a failure that
/// may pass (a busy Discord, a missing permission) is an error, and the post stays recorded so
/// that the next try finds it and no second post is made beside it.
async fn take_off(ctx: &EventContext, posted: &Posted) -> Result<(), ModuleError> {
    let reason = Reason::new(ctx.t_in(ctx.locale, "starboard-reason", &Args::new()))?;
    match ctx
        .api
        .delete_message(
            ChannelId::new(posted.board),
            MessageId::new(posted.post),
            reason,
        )
        .await
    {
        Ok(()) | Err(DiscordError::NotFound) => Ok(()),
        Err(error) => Err(error.into()),
    }
}

fn stored(error: &closure_sdk::StorageError) -> ModuleError {
    ModuleError::other(error.to_string())
}

/// The emoji of the configuration as a request names it.
fn emoji_of(configured: &str) -> Emoji {
    match (rules::custom_id(configured), rules::custom_name(configured)) {
        (Some(id), name) => Emoji::Custom {
            id: closure_core::event::EmojiId::new(id),
            name: name.map(str::to_owned),
        },
        _ => Emoji::Unicode {
            value: configured.trim().to_owned(),
        },
    }
}

/// The stars of a message: the reactions of the configured emoji, without the author's own unless
/// the server counts it.
async fn count_stars(
    ctx: &EventContext,
    message: &Message,
    config: &Config,
) -> Result<u32, ModuleError> {
    let counted = message
        .reactions
        .iter()
        .find(|reaction| match &reaction.emoji {
            EmojiReactionType::Unicode { name } => {
                rules::emoji_matches(&config.emoji, Some(name), None)
            }
            EmojiReactionType::Custom { id, .. } => {
                rules::emoji_matches(&config.emoji, None, Some(id.get()))
            }
        });
    let mut stars = counted.map_or(0, |r| u32::try_from(r.count).unwrap_or(u32::MAX));
    if !config.self_star && stars > 0 && author_reacted(ctx, message, config).await? {
        stars -= 1;
    }
    Ok(stars)
}

/// Whether the author reacted with the emoji. Discord lists the reactors by user id, so the first
/// one after the id just below the author's is the author, if the author is among them: one
/// request, however many people reacted.
async fn author_reacted(
    ctx: &EventContext,
    message: &Message,
    config: &Config,
) -> Result<bool, ModuleError> {
    let author = message.author.id;
    let Outcome::Users(users) = ctx
        .api
        .execute(Action::GetReactionUsers {
            channel_id: message.channel_id,
            message_id: message.id,
            emoji: emoji_of(&config.emoji),
            after: UserId::new_checked(author.get().saturating_sub(1)),
            limit: 1,
        })
        .await?
    else {
        return Ok(false);
    };
    Ok(users.first().is_some_and(|user| user.id == author))
}

/// Whether everybody in the server can read `channel`.
async fn is_public(ctx: &EventContext, channel: &Channel) -> Result<bool, ModuleError> {
    let Outcome::Roles(roles) = ctx
        .api
        .execute(Action::ListRoles {
            guild_id: ctx.guild_id,
        })
        .await?
    else {
        return Ok(false);
    };
    let everyone = roles
        .iter()
        .find(|role| role.id.cast() == ctx.guild_id)
        .map_or(Permissions::empty(), |role| role.permissions);
    let overwrites = channel.permission_overwrites.as_deref().unwrap_or(&[]);
    let nobody: UserId = UserId::new(1);
    Ok(
        PermissionCalculator::new(ctx.guild_id, nobody, everyone, &[])
            .in_channel(channel.kind, overwrites)
            .contains(Permissions::VIEW_CHANNEL),
    )
}

/// Whether what is said in `channel` may be shown to all. What was said for some people only, or
/// for adults only, is not. Asked again whenever a post is made or changed: a channel that was
/// public may not be any longer.
async fn shareable(ctx: &EventContext, channel: &Channel) -> Result<bool, ModuleError> {
    Ok(channel.nsfw != Some(true) && !channel.kind.is_thread() && is_public(ctx, channel).await?)
}

fn quoted(message: &Message, guild: GuildId, place: &str) -> Quoted {
    Quoted {
        author: message
            .author
            .global_name
            .clone()
            .unwrap_or_else(|| message.author.name.clone()),
        avatar: message.author.avatar.map(|hash| {
            format!(
                "https://cdn.discordapp.com/avatars/{}/{hash}.png",
                message.author.id
            )
        }),
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

async fn post(
    ctx: &EventContext,
    board: ChannelId,
    source: &Message,
    config: &Config,
    stars: u32,
) -> Result<(), ModuleError> {
    let Outcome::Channel(channel) = ctx
        .api
        .execute(Action::GetChannel {
            channel_id: source.channel_id,
        })
        .await?
    else {
        return Ok(());
    };
    if !shareable(ctx, &channel).await? {
        return Ok(());
    }
    let place = channel.name.as_deref().unwrap_or("channel");
    let embed = quote_embed(
        &quoted(source, ctx.guild_id, place),
        QUOTE_CHARS,
        &ctx.t_in(ctx.locale, "starboard-source", &Args::new()),
    );
    let sent = ctx
        .api
        .execute(Action::SendMessage {
            channel_id: board,
            content: Some(rules::count_line(
                &config.emoji,
                stars,
                source.channel_id.get(),
            )),
            embeds: vec![embed],
            reply_to: None,
            mentions: MentionPolicy::None,
        })
        .await?;
    if let Outcome::Message(sent) = sent {
        ctx.store()
            .score_put(
                POSTS,
                source.id.get(),
                i64::try_from(sent.id.get()).unwrap_or(0),
            )
            .await
            .map_err(|e| stored(&e))?;
        ctx.store()
            .score_put(
                BOARDS,
                source.id.get(),
                i64::try_from(board.get()).unwrap_or(0),
            )
            .await
            .map_err(|e| stored(&e))?;
    }
    Ok(())
}

async fn on_reaction(
    locks: Arc<Locks>,
    ctx: EventContext,
    event: Event,
) -> Result<Flow, ModuleError> {
    let (Event::ReactionAdded(info) | Event::ReactionRemoved(info)) = &event else {
        return Ok(Flow::Continue);
    };
    let config: Config = ctx.parse_config()?;
    let Some(board) = config.board() else {
        return Ok(Flow::Continue);
    };
    let (unicode, custom) = match &info.emoji {
        Emoji::Unicode { value } => (Some(value.as_str()), None),
        Emoji::Custom { id, .. } => (None, Some(id.get())),
    };
    if !rules::emoji_matches(&config.emoji, unicode, custom) || info.channel_id == board {
        return Ok(Flow::Continue);
    }
    // One at a time for a message, so that two stars at once do not post twice.
    let _one_at_a_time = locks.of(info.message_id).lock().await;
    let source = ctx
        .api
        .get_message(info.channel_id, info.message_id)
        .await?;
    let stars = count_stars(&ctx, &source, &config).await?;
    let posted = posted_of(&ctx, source.id, board).await?;
    match rules::decide(stars, config.threshold, posted.is_some()) {
        Step::Nothing => {}
        Step::Post => post(&ctx, board, &source, &config, stars).await?,
        Step::Update => {
            if let Some(posted) = posted {
                let line = rules::count_line(&config.emoji, stars, source.channel_id.get());
                let edited = ctx
                    .api
                    .execute(Action::EditMessage {
                        channel_id: ChannelId::new(posted.board),
                        message_id: MessageId::new(posted.post),
                        content: Some(line),
                        embeds: None,
                    })
                    .await;
                match edited {
                    Ok(_) => {}
                    // The post was deleted by hand: forget it, so that the next star posts it again.
                    Err(DiscordError::NotFound) => forget(&ctx, source.id).await?,
                    // Anything else (a busy Discord, a missing permission) may pass: the post is
                    // still there, and posting it a second time would be worse.
                    Err(error) => return Err(error.into()),
                }
            }
        }
        Step::Remove => {
            if let Some(posted) = posted {
                take_off(&ctx, &posted).await?;
                forget(&ctx, source.id).await?;
            }
        }
    }
    Ok(Flow::Continue)
}

/// A message that was starred is deleted, or changed: its post follows. A deleted source takes its
/// post away; an edited one has the quote of the post made again.
async fn on_source_change(
    locks: Arc<Locks>,
    ctx: EventContext,
    event: Event,
) -> Result<Flow, ModuleError> {
    let config: Config = ctx.parse_config()?;
    let Some(board) = config.board() else {
        return Ok(Flow::Continue);
    };
    match &event {
        Event::MessageDeleted {
            channel_id,
            message_id,
            ..
        } if *channel_id != board => remove_post(&ctx, &locks, board, *message_id).await?,
        Event::MessagesBulkDeleted {
            channel_id,
            message_ids,
            ..
        } if *channel_id != board => {
            for message_id in message_ids {
                remove_post(&ctx, &locks, board, *message_id).await?;
            }
        }
        Event::MessageUpdated(message) if message.channel_id != board => {
            let _one_at_a_time = locks.of(message.id).lock().await;
            let Some(posted) = posted_of(&ctx, message.id, board).await? else {
                return Ok(Flow::Continue);
            };
            let source = match ctx.api.get_message(message.channel_id, message.id).await {
                Ok(source) => source,
                Err(DiscordError::NotFound) => return Ok(Flow::Continue),
                Err(error) => return Err(error.into()),
            };
            let Outcome::Channel(channel) = ctx
                .api
                .execute(Action::GetChannel {
                    channel_id: source.channel_id,
                })
                .await?
            else {
                return Ok(Flow::Continue);
            };
            // The channel is not public any more: what was quoted from it comes off the board, and
            // what was said since is not put there.
            if !shareable(&ctx, &channel).await? {
                take_off(&ctx, &posted).await?;
                forget(&ctx, message.id).await?;
                return Ok(Flow::Continue);
            }
            let place = channel.name.as_deref().unwrap_or("channel");
            let embed = quote_embed(
                &quoted(&source, ctx.guild_id, place),
                QUOTE_CHARS,
                &ctx.t_in(ctx.locale, "starboard-source", &Args::new()),
            );
            let edited = ctx
                .api
                .execute(Action::EditMessage {
                    channel_id: ChannelId::new(posted.board),
                    message_id: MessageId::new(posted.post),
                    content: None,
                    embeds: Some(vec![embed]),
                })
                .await;
            match edited {
                Ok(_) => {}
                Err(DiscordError::NotFound) => forget(&ctx, message.id).await?,
                Err(error) => return Err(error.into()),
            }
        }
        _ => {}
    }
    Ok(Flow::Continue)
}

/// Takes the post of `source` off the board, if it has one.
async fn remove_post(
    ctx: &EventContext,
    locks: &Locks,
    board: ChannelId,
    source: MessageId,
) -> Result<(), ModuleError> {
    let _one_at_a_time = locks.of(source).lock().await;
    let Some(posted) = posted_of(ctx, source, board).await? else {
        return Ok(());
    };
    take_off(ctx, &posted).await?;
    forget(ctx, source).await
}

/// The module.
///
/// # Errors
/// The module declaration is refused by the SDK.
pub fn builder() -> Result<ModuleBuilder, CoreError> {
    let schema = json!({
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "channel": {
                "anyOf": [{ "type": "string", "pattern": "^[0-9]{1,20}$" }, { "type": "null" }],
                "description": "config-starboard-channel",
                "x-ui": { "level": "basic", "widget": "channel" }
            },
            "threshold": {
                "type": "integer",
                "minimum": 1,
                "maximum": 1_000_000,
                "description": "config-starboard-threshold",
                "x-ui": { "level": "basic" }
            },
            "emoji": {
                "type": "string",
                "minLength": 1,
                "maxLength": 64,
                "description": "config-starboard-emoji",
                "x-ui": { "level": "basic" }
            },
            "self_star": {
                "type": "boolean",
                "description": "config-starboard-self_star",
                "x-ui": { "level": "basic" }
            }
        }
    });
    let locks = Arc::new(Locks::new());
    Ok(
        ModuleBuilder::new("starboard", "Starboard", env!("CARGO_PKG_VERSION"))?
            .capability(closure_sdk::Capability::SendMessages)
            .capability(closure_sdk::Capability::ReadMessages)
            .capability(closure_sdk::Capability::ManageMessages)
            .capability(closure_sdk::Capability::ReadGuild)
            .capability(closure_sdk::Capability::Storage)
            .intents(Intents::GUILD_MESSAGE_REACTIONS | Intents::GUILD_MESSAGES)
            .config(
                schema,
                json!({"channel": null, "threshold": 3, "emoji": DEFAULT_EMOJI, "self_star": false}),
            )
            .on(Subscription::new(EventKind::ReactionAdded), {
                let locks = Arc::clone(&locks);
                move |ctx, event| on_reaction(Arc::clone(&locks), ctx, event)
            })
            .on(Subscription::new(EventKind::ReactionRemoved), {
                let locks = Arc::clone(&locks);
                move |ctx, event| on_reaction(Arc::clone(&locks), ctx, event)
            })
            .on(Subscription::new(EventKind::MessageDeleted), {
                let locks = Arc::clone(&locks);
                move |ctx, event| on_source_change(Arc::clone(&locks), ctx, event)
            })
            .on(Subscription::new(EventKind::MessagesBulkDeleted), {
                let locks = Arc::clone(&locks);
                move |ctx, event| on_source_change(Arc::clone(&locks), ctx, event)
            })
            .on(Subscription::new(EventKind::MessageUpdated), move |ctx, event| {
                on_source_change(Arc::clone(&locks), ctx, event)
            })
            .catalog(Locale::En, EN)
            .catalog(Locale::Ja, JA),
    )
}

/// The module.
///
/// # Errors
/// The module declaration is refused by the SDK.
pub fn module() -> Result<SdkModule, CoreError> {
    builder()?.build()
}

#[cfg(test)]
mod tests;
