//! Experience for messages, levels, role rewards and a leaderboard.
//!
//! A message gives `xp_per_message` at most once per `cooldown_seconds`, so flooding does not pay.
//! The score is one row per member in the store; the cooldowns are in memory, so a restart may award once more.

#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

pub mod rules;

use std::collections::HashMap;
use std::sync::{Arc, Mutex, PoisonError};

use closure_core::action::{Action, MentionPolicy, Reason};
use closure_core::error::ModuleError;
use closure_core::event::{ChannelId, Event, EventKind, RoleId, UserId};
use closure_core::module::EventContext;
use closure_core::subscription::Flow;
use closure_sdk::{
    Args, Command, CommandArgs, CommandCx, CoreError, EmbedBuilder, Intents, InteractionReply,
    Level, Locale, ModuleBuilder, PermissionSpec, SdkModule, Subscription, UserError,
};
use serde::Deserialize;
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

const XP: &str = "xp";
/// The name of the score that holds when a member came into the voice channel they are in.
const SINCE: &str = "voice-since";
/// How many members the cooldowns remember before the ones that may get experience again are
/// forgotten.
const MAX_REMEMBERED: usize = 200_000;

fn stored(error: &closure_sdk::StorageError) -> ModuleError {
    ModuleError::other(error.to_string())
}

fn failed(error: &impl ToString) -> UserError {
    UserError::from(ModuleError::other(error.to_string()))
}

/// Last award per member, in memory: most messages fall inside the cooldown and cost no database access.
/// After a restart a member may get one award sooner than the cooldown; that is all it costs.
#[derive(Debug, Default)]
struct Cooldowns(Mutex<HashMap<(u64, u64), i64>>);

impl Cooldowns {
    /// Whether `user` may get experience now; when they may, they have to wait `wait` seconds
    /// from now.
    fn take(&self, guild: u64, user: u64, now: i64, wait: i64) -> bool {
        let mut last = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        if last
            .get(&(guild, user))
            .is_some_and(|before| now - before < wait)
        {
            return false;
        }
        if last.len() >= MAX_REMEMBERED {
            last.retain(|_, before| now - *before < wait);
        }
        last.insert((guild, user), now);
        true
    }
}

async fn experience(ctx: &EventContext, user: UserId) -> Result<u64, ModuleError> {
    let score = ctx
        .store()
        .score(XP, user.get())
        .await
        .map_err(|e| stored(&e))?;
    Ok(u64::try_from(score).unwrap_or(0))
}

/// Gives `user` `gain` experience and acts on the levels it takes them through.
async fn award(
    ctx: &EventContext,
    config: &Config,
    user: UserId,
    gain: u64,
    here: Option<ChannelId>,
) -> Result<(), ModuleError> {
    if gain == 0 {
        return Ok(());
    }
    let delta = i64::try_from(gain).unwrap_or(i64::MAX);
    let new = ctx
        .store()
        .score_add(XP, user.get(), delta)
        .await
        .map_err(|e| stored(&e))?;
    let total = new.min(MAX_TOTAL);
    if total < new {
        ctx.store()
            .score_put(XP, user.get(), total)
            .await
            .map_err(|e| stored(&e))?;
    }
    let before = u64::try_from(new.saturating_sub(delta)).unwrap_or(0);
    reached(
        ctx,
        config,
        user,
        before,
        u64::try_from(total).unwrap_or(0),
        here,
    )
    .await
}

/// Announces and rewards the levels `user` passed going from `before` to `total` experience.
async fn reached(
    ctx: &EventContext,
    config: &Config,
    user: UserId,
    before: u64,
    total: u64,
    here: Option<ChannelId>,
) -> Result<(), ModuleError> {
    let (old, new) = (progress(before).level, progress(total).level);
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
    let mut gave_all = true;
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
            gave_all = false;
        }
    }
    // The earlier roles go only when the new one is on: a member is never left with none.
    if config.replace_rewards && gave_all {
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
    // The best reward must be on the member before the earlier ones go: a reward that failed to
    // be given at its level is given now, and if it still cannot be, nothing is taken away.
    let Ok(member) = ctx.api.get_member(ctx.guild_id, user).await else {
        return Ok(());
    };
    for reward in config.rewards.iter().filter(|r| r.level == top) {
        let Some(role) = snowflake(&reward.role).map(RoleId::new) else {
            continue;
        };
        if member.roles.contains(&role) {
            continue;
        }
        let reason = Reason::new(ctx.t_in(ctx.locale, "leveling-reward-reason", &Args::new()))?;
        if let Err(error) = ctx.api.add_role(ctx.guild_id, user, role, reason).await {
            tracing::debug!(
                ?error,
                "could not give the best reward; the earlier ones stay"
            );
            return Ok(());
        }
    }
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

async fn on_message(
    cooldowns: Arc<Cooldowns>,
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
    if config.ignores(message.channel_id) {
        return Ok(Flow::Continue);
    }
    let wait = i64::from(config.cooldown_seconds);
    if !cooldowns.take(ctx.guild_id.get(), message.author.id.get(), ctx.now(), wait) {
        return Ok(Flow::Continue);
    }
    let gain = u64::from(config.xp_per_message)
        * config.percent(&message.author_roles, message.channel_id)
        / 100;
    award(
        &ctx,
        &config,
        message.author.id,
        gain,
        Some(message.channel_id),
    )
    .await?;
    Ok(Flow::Continue)
}

/// Experience for the time `user` was in `channel`, since they came in.
async fn settle(
    ctx: &EventContext,
    config: &Config,
    user: UserId,
    channel: ChannelId,
) -> Result<(), ModuleError> {
    let since = ctx
        .store()
        .score(SINCE, user.get())
        .await
        .map_err(|e| stored(&e))?;
    if since == 0 {
        return Ok(());
    }
    ctx.store()
        .score_put(SINCE, user.get(), 0)
        .await
        .map_err(|e| stored(&e))?;
    let minutes = ((ctx.now() - since) / 60).clamp(0, MAX_VOICE_MINUTES);
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
    award(ctx, config, user, gain, None).await
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
            .score_put(SINCE, user.id.get(), ctx.now())
            .await
            .map_err(|e| stored(&e))?;
    }
    Ok(Flow::Continue)
}

#[derive(CommandArgs)]
struct RankArgs {
    /// The member; yourself when left out.
    user: Option<UserId>,
    /// Yes: show the members with the most experience instead.
    top: Option<bool>,
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

/// The level of a member, or the members with the most experience.
async fn rank(cx: CommandCx, args: RankArgs) -> Result<(), UserError> {
    if args.top == Some(true) {
        return leaderboard(cx).await;
    }
    member_rank(cx, args.user).await
}

async fn member_rank(cx: CommandCx, user: Option<UserId>) -> Result<(), UserError> {
    let ctx = cx.ctx();
    let user = user.unwrap_or(cx.user().id);
    let mine = experience(ctx, user).await.map_err(|e| failed(&e))?;
    let Progress {
        level,
        into,
        needed,
    } = progress(mine);
    // The place and the number of members come from the index, not from reading the members.
    let above = ctx
        .store()
        .score_above(XP, i64::try_from(mine).unwrap_or(i64::MAX))
        .await
        .map_err(|e| failed(&e))?;
    let members = ctx.store().score_count(XP).await.map_err(|e| failed(&e))?;
    let position = above + 1;
    let say = |key: &str| cx.t(key, &Args::new());
    let embed = EmbedBuilder::new()
        .description(format!(
            "<@{user}>\n{}  {}/{}",
            bar(into, needed, BAR_WIDTH),
            grouped(into),
            grouped(needed)
        ))
        .field(say("leveling-level"), level.to_string(), true)
        .field(say("leveling-xp"), grouped(mine), true)
        .field(
            say("leveling-position"),
            format!("#{position} / {}", members.max(position)),
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

async fn leaderboard(cx: CommandCx) -> Result<(), UserError> {
    let all = cx
        .ctx()
        .store()
        .score_top(XP, BOARD_SIZE)
        .await
        .map_err(|e| failed(&e))?;
    if all.is_empty() {
        return cx.reply_private_t("leveling-empty", &Args::new()).await;
    }
    let lines: Vec<String> = all
        .iter()
        .enumerate()
        .map(|(at, (user, xp))| {
            let xp = u64::try_from(*xp).unwrap_or(0);
            format!(
                "{}. <@{user}>  ·  Lv {}  ·  {} XP",
                at + 1,
                progress(xp).level,
                grouped(xp)
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
    let before = experience(ctx, user).await.map_err(|e| failed(&e))?;
    let total = total.clamp(0, MAX_TOTAL);
    ctx.store()
        .score_put(XP, user.get(), total)
        .await
        .map_err(|e| failed(&e))?;
    let total = u64::try_from(total).unwrap_or(0);
    reached(ctx, &config, user, before, total, None)
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

/// Adds to the experience of a member as one step of the store, so that an addition at the same
/// moment as another (a message, a second moderator) is not lost.
async fn xp_add(cx: CommandCx, args: UserAmount) -> Result<(), UserError> {
    let ctx = cx.ctx();
    let config: Config = cx.config()?;
    let user = args.user;
    let new = ctx
        .store()
        .score_add(XP, user.get(), args.amount)
        .await
        .map_err(|e| failed(&e))?;
    let total = new.clamp(0, MAX_TOTAL);
    if total != new {
        ctx.store()
            .score_put(XP, user.get(), total)
            .await
            .map_err(|e| failed(&e))?;
    }
    let before = new.saturating_sub(args.amount).clamp(0, MAX_TOTAL);
    let total = u64::try_from(total).unwrap_or(0);
    reached(
        ctx,
        &config,
        user,
        u64::try_from(before).unwrap_or(0),
        total,
        None,
    )
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

async fn xp_set(cx: CommandCx, args: UserAmount) -> Result<(), UserError> {
    put(&cx, args.user, args.amount).await
}

async fn xp_reset(cx: CommandCx, args: OneUser) -> Result<(), UserError> {
    put(&cx, args.user, 0).await
}

/// # Errors
/// The module declaration is refused by the SDK.
#[allow(
    clippy::too_many_lines,
    reason = "the schema and the declaration, one after the other"
)]
pub fn builder() -> Result<ModuleBuilder, CoreError> {
    let cooldowns = Arc::new(Cooldowns::default());
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
                Command::<RankArgs>::new("rank")
                    .requires("leveling.view")
                    .handler(rank),
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
            .on(
                Subscription::new(EventKind::MessageCreated),
                move |ctx, event| on_message(Arc::clone(&cooldowns), ctx, event),
            )
            .on(Subscription::new(EventKind::VoiceStateChanged), on_voice)
            .catalog(Locale::En, EN)
            .catalog(Locale::Ja, JA),
    )
}

/// # Errors
/// The module declaration is refused by the SDK.
pub fn module() -> Result<SdkModule, CoreError> {
    builder()?.build()
}

#[cfg(test)]
mod tests;
