//! Experience for messages, levels, role rewards and a leaderboard.
//!
//! A member gets `xp_per_message` experience for a message, at most once in `cooldown_seconds`, so
//! that flooding does not pay. The levels take more experience as they go up (see [`rules`]).
//! Time in a voice channel gives experience too, by the minute, when the member leaves it. Roles
//! and channels can make the experience worth more or less. Reaching a level can be announced and
//! can give a role, which can take the roles of the levels before it away.
//!
//! shortcut: the leaderboard and the rank read the first thousand members the module keeps (the
//! limit of one listing); upgrade when a server has more members than that who talk.

#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

pub mod rules;

use closure_core::action::{Action, MentionPolicy, Reason};
use closure_core::error::ModuleError;
use closure_core::event::{ChannelId, Event, EventKind, RoleId, UserId};
use closure_core::module::EventContext;
use closure_core::subscription::Flow;
use closure_sdk::{
    Args, Command, CommandArgs, CommandCx, CoreError, EmbedBuilder, Intents, InteractionReply,
    Level, Locale, ModuleBuilder, PermissionSpec, SdkModule, Subscription, UserError,
};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::rules::{Progress, bar, grouped, progress};

const EN: &str = include_str!("../locales/en.ftl");
const JA: &str = include_str!("../locales/ja.ftl");
/// The most experience one member can have: far past any level that matters.
const MAX_TOTAL: i64 = 1_000_000_000;
/// A time in voice counts for at most this many minutes: a member who is left in a channel for
/// days is not rewarded for it.
const MAX_VOICE_MINUTES: i64 = 12 * 60;
const BOARD_SIZE: usize = 10;
const BAR_WIDTH: usize = 12;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, default)]
struct Config {
    xp_per_message: u32,
    cooldown_seconds: u32,
    ignored_channels: Vec<String>,
    announce: bool,
    /// Where level-ups are announced; the channel of the message when none.
    announce_channel: Option<String>,
    rewards: Vec<Reward>,
    /// Experience for each minute in a voice channel; none when 0.
    voice_xp_per_minute: u32,
    /// The roles that make experience worth more or less; the best of the member's counts.
    role_multipliers: Vec<RoleMultiplier>,
    /// The channels where experience is worth more or less.
    channel_multipliers: Vec<ChannelMultiplier>,
    /// A reward role takes the reward roles of the levels before it away.
    replace_rewards: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            xp_per_message: 20,
            cooldown_seconds: 60,
            ignored_channels: Vec::new(),
            announce: true,
            announce_channel: None,
            rewards: Vec::new(),
            voice_xp_per_minute: 5,
            role_multipliers: Vec::new(),
            channel_multipliers: Vec::new(),
            replace_rewards: false,
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Reward {
    level: u32,
    role: String,
}

/// A percentage of the experience: 100 is as it is, 0 gives none.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RoleMultiplier {
    role: String,
    percent: u32,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ChannelMultiplier {
    channel: String,
    percent: u32,
}

fn snowflake(text: &str) -> Option<u64> {
    text.parse::<u64>().ok().filter(|id| *id > 0)
}

impl Config {
    fn ignores(&self, channel: ChannelId) -> bool {
        self.ignored_channels
            .iter()
            .any(|c| snowflake(c) == Some(channel.get()))
    }

    /// What the experience of `roles` in `channel` is worth, in percent: the best of the roles,
    /// then the channel's on top.
    fn percent(&self, roles: &[RoleId], channel: ChannelId) -> u64 {
        let by_role = self
            .role_multipliers
            .iter()
            .filter(|m| roles.iter().any(|r| snowflake(&m.role) == Some(r.get())))
            .map(|m| u64::from(m.percent))
            .max()
            .unwrap_or(100);
        let by_channel = self
            .channel_multipliers
            .iter()
            .find(|m| snowflake(&m.channel) == Some(channel.get()))
            .map_or(100, |m| u64::from(m.percent));
        by_role * by_channel / 100
    }

    fn announce_to(&self) -> Option<ChannelId> {
        self.announce_channel
            .as_deref()
            .and_then(snowflake)
            .map(ChannelId::new)
    }
}

/// What the module keeps of a member.
#[derive(Debug, Default, Serialize, Deserialize)]
struct Profile {
    xp: u64,
    /// Unix seconds of the last message that gave experience.
    last: i64,
}

fn key(user: UserId) -> String {
    format!("xp:{user}")
}

fn stored(error: &closure_sdk::StorageError) -> ModuleError {
    ModuleError::other(error.to_string())
}

fn failed(error: &impl ToString) -> UserError {
    UserError::from(ModuleError::other(error.to_string()))
}

async fn profile(ctx: &EventContext, user: UserId) -> Result<Profile, ModuleError> {
    Ok(ctx
        .store()
        .get::<Profile>(&key(user))
        .await
        .map_err(|e| stored(&e))?
        .unwrap_or_default())
}

/// Every member the module keeps, with their experience, most first.
async fn standings(ctx: &EventContext) -> Result<Vec<(UserId, u64)>, ModuleError> {
    let entries = ctx
        .store()
        .list("xp:", 1000)
        .await
        .map_err(|e| stored(&e))?;
    let mut all: Vec<(UserId, u64)> = entries
        .into_iter()
        .filter_map(|(key, entry)| {
            let id = key.strip_prefix("xp:")?.parse::<u64>().ok()?;
            let xp = entry.value.get("xp")?.as_u64()?;
            (id > 0).then(|| (UserId::new(id), xp))
        })
        .collect();
    all.sort_by_key(|(id, xp)| (std::cmp::Reverse(*xp), id.get()));
    Ok(all)
}

/// Puts `user` at `total` experience, announcing and rewarding the levels they pass on the way up.
async fn set_total(
    ctx: &EventContext,
    config: &Config,
    user: UserId,
    before: &Profile,
    total: u64,
    last: i64,
    here: Option<ChannelId>,
) -> Result<(), ModuleError> {
    if total == 0 {
        ctx.store()
            .delete(&key(user))
            .await
            .map_err(|e| stored(&e))?;
    } else {
        ctx.store()
            .set(&key(user), &Profile { xp: total, last })
            .await
            .map_err(|e| stored(&e))?;
    }
    let (old, new) = (progress(before.xp).level, progress(total).level);
    if new <= old {
        return Ok(());
    }
    if config.announce
        && let Some(channel) = config.announce_to().or(here)
    {
        let text = ctx.t_in(
            ctx.locale,
            "leveling-up",
            &Args::new()
                .with("user", format!("<@{user}>"))
                .with("level", u64::from(new)),
        );
        let sent = ctx
            .api
            .execute(Action::SendMessage {
                channel_id: channel,
                content: Some(text),
                embeds: Vec::new(),
                reply_to: None,
                mentions: MentionPolicy::None,
            })
            .await;
        if let Err(error) = sent {
            tracing::debug!(?error, "could not announce a level");
        }
    }
    for reward in config
        .rewards
        .iter()
        .filter(|r| r.level > old && r.level <= new)
    {
        let Some(role) = snowflake(&reward.role).map(RoleId::new) else {
            continue;
        };
        let reason = Reason::new(ctx.t_in(ctx.locale, "leveling-reward-reason", &Args::new()))?;
        if let Err(error) = ctx.api.add_role(ctx.guild_id, user, role, reason).await {
            tracing::debug!(?error, "could not give a reward");
        }
    }
    if config.replace_rewards {
        take_earlier_rewards_away(ctx, config, user, new).await?;
    }
    Ok(())
}

/// Takes the roles of the rewards below the best one that `level` has reached away.
async fn take_earlier_rewards_away(
    ctx: &EventContext,
    config: &Config,
    user: UserId,
    level: u32,
) -> Result<(), ModuleError> {
    let Some(top) = config
        .rewards
        .iter()
        .filter(|r| r.level <= level)
        .map(|r| r.level)
        .max()
    else {
        return Ok(());
    };
    for reward in config.rewards.iter().filter(|r| r.level < top) {
        let Some(role) = snowflake(&reward.role).map(RoleId::new) else {
            continue;
        };
        // The role of the best reward may stand for a lower level too: it stays.
        if config
            .rewards
            .iter()
            .any(|r| r.level == top && r.role == reward.role)
        {
            continue;
        }
        let reason = Reason::new(ctx.t_in(ctx.locale, "leveling-reward-reason", &Args::new()))?;
        if let Err(error) = ctx.api.remove_role(ctx.guild_id, user, role, reason).await {
            tracing::debug!(?error, "could not take a reward away");
        }
    }
    Ok(())
}

async fn on_message(ctx: EventContext, event: Event) -> Result<Flow, ModuleError> {
    let Event::MessageCreated(message) = &event else {
        return Ok(Flow::Continue);
    };
    if message.author.bot || message.from_webhook {
        return Ok(Flow::Continue);
    }
    let config: Config = ctx.parse_config()?;
    if config.ignores(message.channel_id) {
        return Ok(Flow::Continue);
    }
    let now = ctx.now();
    let before = profile(&ctx, message.author.id).await?;
    if now - before.last < i64::from(config.cooldown_seconds) {
        return Ok(Flow::Continue);
    }
    let gain = u64::from(config.xp_per_message)
        * config.percent(&message.author_roles, message.channel_id)
        / 100;
    let total = before.xp.saturating_add(gain).min(MAX_TOTAL.unsigned_abs());
    set_total(
        &ctx,
        &config,
        message.author.id,
        &before,
        total,
        now,
        Some(message.channel_id),
    )
    .await?;
    Ok(Flow::Continue)
}

/// When a member joined the voice channel they are in.
#[derive(Debug, Serialize, Deserialize)]
struct Sitting {
    since: i64,
}

fn voice_key(user: UserId) -> String {
    format!("voice:{user}")
}

/// Experience for the time `user` was in `channel`, since they came in.
async fn settle(
    ctx: &EventContext,
    config: &Config,
    user: UserId,
    channel: ChannelId,
) -> Result<(), ModuleError> {
    let Some(sitting) = ctx
        .store()
        .get::<Sitting>(&voice_key(user))
        .await
        .map_err(|e| stored(&e))?
    else {
        return Ok(());
    };
    ctx.store()
        .delete(&voice_key(user))
        .await
        .map_err(|e| stored(&e))?;
    let minutes = ((ctx.now() - sitting.since) / 60).clamp(0, MAX_VOICE_MINUTES);
    if config.voice_xp_per_minute == 0 || minutes == 0 || config.ignores(channel) {
        return Ok(());
    }
    // The roles are asked for only when they make a difference.
    let roles = if config.role_multipliers.is_empty() {
        Vec::new()
    } else {
        match ctx.api.get_member(ctx.guild_id, user).await {
            Ok(member) => member.roles,
            Err(error) => {
                tracing::debug!(?error, "the roles of a member are not known");
                Vec::new()
            }
        }
    };
    let gain = minutes.unsigned_abs()
        * u64::from(config.voice_xp_per_minute)
        * config.percent(&roles, channel)
        / 100;
    let before = profile(ctx, user).await?;
    let total = before.xp.saturating_add(gain).min(MAX_TOTAL.unsigned_abs());
    set_total(ctx, config, user, &before, total, before.last, None).await
}

async fn on_voice(ctx: EventContext, event: Event) -> Result<Flow, ModuleError> {
    let Event::VoiceStateChanged {
        user,
        before,
        after,
        ..
    } = &event
    else {
        return Ok(Flow::Continue);
    };
    if user.bot || before == after {
        return Ok(Flow::Continue);
    }
    let config: Config = ctx.parse_config()?;
    if let Some(channel) = before {
        settle(&ctx, &config, user.id, *channel).await?;
    }
    if after.is_some() {
        ctx.store()
            .set(&voice_key(user.id), &Sitting { since: ctx.now() })
            .await
            .map_err(|e| stored(&e))?;
    }
    Ok(Flow::Continue)
}

#[derive(CommandArgs)]
struct MaybeUser {
    /// The member; yourself when left out.
    user: Option<UserId>,
}

#[derive(CommandArgs)]
struct UserAmount {
    /// The member.
    user: UserId,
    /// How much experience.
    amount: i64,
}

#[derive(CommandArgs)]
struct OneUser {
    /// The member.
    user: UserId,
}

async fn rank(cx: CommandCx, args: MaybeUser) -> Result<(), UserError> {
    let ctx = cx.ctx();
    let user = args.user.unwrap_or(cx.user().id);
    let mine = profile(ctx, user).await.map_err(|e| failed(&e))?;
    let Progress {
        level,
        into,
        needed,
    } = progress(mine.xp);
    let all = standings(ctx).await.map_err(|e| failed(&e))?;
    let position = all.iter().filter(|(_, xp)| *xp > mine.xp).count() + 1;
    let say = |key: &str| cx.t(key, &Args::new());
    let embed = EmbedBuilder::new()
        .description(format!(
            "<@{user}>\n{}  {}/{}",
            bar(into, needed, BAR_WIDTH),
            grouped(into),
            grouped(needed)
        ))
        .field(say("leveling-level"), level.to_string(), true)
        .field(say("leveling-xp"), grouped(mine.xp), true)
        .field(
            say("leveling-position"),
            format!("#{position} / {}", all.len().max(position)),
            true,
        )
        .build()
        .map_err(|e| failed(&e))?;
    cx.reply_with(InteractionReply {
        content: None,
        embeds: vec![embed],
        ephemeral: false,
        components: Vec::new(),
    })
    .await
}

async fn leaderboard(cx: CommandCx, (): ()) -> Result<(), UserError> {
    let all = standings(cx.ctx()).await.map_err(|e| failed(&e))?;
    if all.is_empty() {
        return cx.reply_private_t("leveling-empty", &Args::new()).await;
    }
    let lines: Vec<String> = all
        .iter()
        .take(BOARD_SIZE)
        .enumerate()
        .map(|(at, (user, xp))| {
            format!(
                "{}. <@{user}>  ·  Lv {}  ·  {} XP",
                at + 1,
                progress(*xp).level,
                grouped(*xp)
            )
        })
        .collect();
    let embed = EmbedBuilder::new()
        .title(cx.t("leveling-leaderboard", &Args::new()))
        .description(lines.join("\n"))
        .build()
        .map_err(|e| failed(&e))?;
    cx.reply_with(InteractionReply {
        content: None,
        embeds: vec![embed],
        ephemeral: false,
        components: Vec::new(),
    })
    .await
}

/// Puts `user` at `total` (clamped to what is allowed) and says so.
async fn put(cx: &CommandCx, user: UserId, total: i64) -> Result<(), UserError> {
    let ctx = cx.ctx();
    let config: Config = cx.config()?;
    let before = profile(ctx, user).await.map_err(|e| failed(&e))?;
    let total = u64::try_from(total.clamp(0, MAX_TOTAL)).unwrap_or(0);
    set_total(ctx, &config, user, &before, total, before.last, None)
        .await
        .map_err(|e| failed(&e))?;
    cx.reply_private_t(
        "leveling-set",
        &Args::new()
            .with("user", format!("<@{user}>"))
            .with("xp", grouped(total))
            .with("level", u64::from(progress(total).level)),
    )
    .await
}

async fn xp_add(cx: CommandCx, args: UserAmount) -> Result<(), UserError> {
    let before = profile(cx.ctx(), args.user).await.map_err(|e| failed(&e))?;
    let now = i64::try_from(before.xp).unwrap_or(i64::MAX);
    put(&cx, args.user, now.saturating_add(args.amount)).await
}

async fn xp_set(cx: CommandCx, args: UserAmount) -> Result<(), UserError> {
    put(&cx, args.user, args.amount).await
}

async fn xp_reset(cx: CommandCx, args: OneUser) -> Result<(), UserError> {
    put(&cx, args.user, 0).await
}

/// The module.
///
/// # Errors
/// The module declaration is refused by the SDK.
#[allow(
    clippy::too_many_lines,
    reason = "the schema and the declaration, one after the other"
)]
pub fn builder() -> Result<ModuleBuilder, CoreError> {
    let snowflake = json!({"type": "string", "pattern": "^[0-9]{1,20}$"});
    let schema = json!({
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "xp_per_message": {
                "type": "integer", "minimum": 1, "maximum": 1000,
                "description": "config-leveling-xp_per_message",
                "x-ui": { "level": "basic" }
            },
            "cooldown_seconds": {
                "type": "integer", "minimum": 0, "maximum": 3600,
                "description": "config-leveling-cooldown_seconds",
                "x-ui": { "level": "basic" }
            },
            "ignored_channels": {
                "type": "array", "maxItems": 50, "items": snowflake.clone(),
                "description": "config-leveling-ignored_channels",
                "x-ui": { "level": "basic", "widget": "channel" }
            },
            "announce": {
                "type": "boolean",
                "description": "config-leveling-announce",
                "x-ui": { "level": "basic" }
            },
            "announce_channel": {
                "anyOf": [snowflake.clone(), { "type": "null" }],
                "description": "config-leveling-announce_channel",
                "x-ui": { "level": "basic", "widget": "channel" }
            },
            "voice_xp_per_minute": {
                "type": "integer", "minimum": 0, "maximum": 1000,
                "description": "config-leveling-voice_xp_per_minute",
                "x-ui": { "level": "basic" }
            },
            "role_multipliers": {
                "type": "array", "maxItems": 25,
                "description": "config-leveling-role_multipliers",
                "x-ui": { "level": "basic" },
                "items": {
                    "type": "object",
                    "additionalProperties": false,
                    "required": ["role", "percent"],
                    "properties": {
                        "role": snowflake.clone(),
                        "percent": { "type": "integer", "minimum": 0, "maximum": 1000 }
                    }
                }
            },
            "channel_multipliers": {
                "type": "array", "maxItems": 25,
                "description": "config-leveling-channel_multipliers",
                "x-ui": { "level": "basic" },
                "items": {
                    "type": "object",
                    "additionalProperties": false,
                    "required": ["channel", "percent"],
                    "properties": {
                        "channel": snowflake.clone(),
                        "percent": { "type": "integer", "minimum": 0, "maximum": 1000 }
                    }
                }
            },
            "replace_rewards": {
                "type": "boolean",
                "description": "config-leveling-replace_rewards",
                "x-ui": { "level": "basic" }
            },
            "rewards": {
                "type": "array", "maxItems": 25,
                "description": "config-leveling-rewards",
                "x-ui": { "level": "basic" },
                "items": {
                    "type": "object",
                    "additionalProperties": false,
                    "required": ["level", "role"],
                    "properties": {
                        "level": { "type": "integer", "minimum": 1, "maximum": 1000 },
                        "role": snowflake
                    }
                }
            }
        }
    });
    Ok(
        ModuleBuilder::new("leveling", "Leveling", env!("CARGO_PKG_VERSION"))?
            .capability(closure_sdk::Capability::Storage)
            .capability(closure_sdk::Capability::SendMessages)
            .capability(closure_sdk::Capability::ManageRoles)
            .capability(closure_sdk::Capability::RespondToInteractions)
            .capability(closure_sdk::Capability::ReadGuild)
            .intents(Intents::GUILD_MESSAGES | Intents::GUILD_VOICE_STATES)
            .permission(PermissionSpec::new(
                "leveling.view",
                Level::GENERAL,
                "permission-leveling-view",
            )?)
            .permission(PermissionSpec::new(
                "leveling.manage",
                Level::MODERATOR,
                "permission-leveling-manage",
            )?)
            .config(
                schema,
                json!({
                    "xp_per_message": 20, "cooldown_seconds": 60, "ignored_channels": [],
                    "announce": true, "announce_channel": null, "rewards": [],
                    "voice_xp_per_minute": 5, "role_multipliers": [], "channel_multipliers": [],
                    "replace_rewards": false
                }),
            )
            .command(
                Command::<MaybeUser>::new("rank")
                    .requires("leveling.view")
                    .handler(rank),
            )
            .command(
                Command::<()>::new("leaderboard")
                    .requires("leveling.view")
                    .handler(leaderboard),
            )
            .group(
                "xp",
                [
                    Command::<UserAmount>::new("add")
                        .requires("leveling.manage")
                        .handler(xp_add),
                    Command::<UserAmount>::new("set")
                        .requires("leveling.manage")
                        .handler(xp_set),
                    Command::<OneUser>::new("reset")
                        .requires("leveling.manage")
                        .handler(xp_reset),
                ],
            )
            .on(Subscription::new(EventKind::MessageCreated), on_message)
            .on(Subscription::new(EventKind::VoiceStateChanged), on_voice)
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
