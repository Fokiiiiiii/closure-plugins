//! Giveaways: a message with a button to enter, and winners drawn when the time is up.
//!
//! `/giveaway start` posts the message and schedules its end. Pressing the button enters, and
//! pressing it again leaves. At the end the winners are drawn from those who are in, the message
//! tells them and the button goes. `/giveaway end` ends one at once and `/giveaway reroll` draws
//! again from a giveaway that is over.
//!
//! shortcut: the draw reads the first thousand entries the module keeps (the limit of one
//! listing); upgrade when a giveaway has more who enter.

#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

use closure_core::action::{Action, MentionPolicy, Outcome};
use closure_core::error::ModuleError;
use closure_core::event::{ChannelId, MessageId};
use closure_core::module::EventContext;
use closure_sdk::ui::{self, CustomId};
use closure_sdk::{
    Args, Command, CommandArgs, CommandCx, ComponentCx, CoreError, EmbedBuilder, InteractionReply,
    JobCx, Level, Locale, ModuleBuilder, PermissionSpec, SdkModule, UserError,
};
use serde::{Deserialize, Serialize};
use twilight_model::channel::message::component::ButtonStyle;
use twilight_model::channel::message::{Component, Embed};

const EN: &str = include_str!("../locales/en.ftl");
const JA: &str = include_str!("../locales/ja.ftl");
const END: &str = "end";
const MIN_SECONDS: i64 = 60;
const MAX_SECONDS: i64 = 30 * 24 * 3600;
const MAX_WINNERS: i64 = 20;
/// The most entries one draw reads: the limit of one listing of the store.
const MAX_ENTRIES: usize = 1000;
const COLOR: u32 = 0x00F5_B301;

/// What the module keeps of a giveaway, under the id of its message.
#[derive(Debug, Serialize, Deserialize)]
struct Giveaway {
    channel: u64,
    prize: String,
    winners: u32,
    /// Unix seconds.
    ends: i64,
    host: u64,
    #[serde(default)]
    ended: bool,
}

#[derive(Debug, Serialize, Deserialize)]
struct EndJob {
    message: u64,
}

fn giveaway_key(message: MessageId) -> String {
    format!("g:{message}")
}

fn entry_prefix(message: MessageId) -> String {
    format!("e:{message}:")
}

fn stored(error: &closure_sdk::StorageError) -> ModuleError {
    ModuleError::other(error.to_string())
}

fn failed(error: &impl ToString) -> UserError {
    UserError::from(ModuleError::other(error.to_string()))
}

/// A time such as `30m`, `2h`, `1d` or `1h30m`, in seconds; none when it is not one or is outside
/// what a giveaway may last.
fn span(text: &str) -> Option<i64> {
    let mut total = 0_i64;
    let mut digits = String::new();
    let mut any = false;
    for c in text.trim().chars() {
        if c.is_ascii_digit() {
            digits.push(c);
            continue;
        }
        let number: i64 = digits.parse().ok()?;
        digits.clear();
        let unit = match c.to_ascii_lowercase() {
            'm' => 60,
            'h' => 3600,
            'd' => 86_400,
            _ => return None,
        };
        total = total.checked_add(number.checked_mul(unit)?)?;
        any = true;
    }
    (digits.is_empty() && any && (MIN_SECONDS..=MAX_SECONDS).contains(&total)).then_some(total)
}

/// `count` of the `entrants`, chosen at random and each at most once.
fn draw(entrants: &[u64], count: usize, rng: &mut fastrand::Rng) -> Vec<u64> {
    let mut pool = entrants.to_vec();
    rng.shuffle(&mut pool);
    pool.truncate(count);
    pool
}

fn mentions(users: &[u64]) -> String {
    users
        .iter()
        .map(|id| format!("<@{id}>"))
        .collect::<Vec<_>>()
        .join(" ")
}

fn embed_of(title: &str, text: String) -> Result<Embed, ModuleError> {
    EmbedBuilder::new()
        .title(format!("🎉 {title}"))
        .description(text)
        .color(closure_sdk::Color::Rgb(COLOR))
        .build()
        .map_err(|error| ModuleError::other(error.to_string()))
}

fn enter_button(ctx: &EventContext) -> Component {
    ui::row(vec![ui::button(
        ButtonStyle::Success,
        ctx.t_in(ctx.locale, "giveaway-enter", &Args::new()),
        CustomId::new("join").encode(&ctx.module),
    )])
}

async fn entrants(ctx: &EventContext, message: MessageId) -> Result<Vec<u64>, ModuleError> {
    let entries = ctx
        .store()
        .list(&entry_prefix(message), MAX_ENTRIES)
        .await
        .map_err(|e| stored(&e))?;
    Ok(entries
        .into_iter()
        .filter_map(|(key, _)| key.rsplit(':').next()?.parse().ok())
        .collect())
}

/// How a giveaway ended, or why it could not.
#[derive(Debug, PartialEq, Eq)]
enum Finish {
    Drawn,
    NoEntries,
    Unknown,
    /// Ending one that is over already, or drawing again one that still runs.
    WrongState,
}

/// Ends the giveaway on `message` and draws its winners, or draws again when `redraw`.
async fn finish(
    ctx: &EventContext,
    message: MessageId,
    redraw: bool,
) -> Result<Finish, ModuleError> {
    let Some(mut giveaway) = ctx
        .store()
        .get::<Giveaway>(&giveaway_key(message))
        .await
        .map_err(|e| stored(&e))?
    else {
        return Ok(Finish::Unknown);
    };
    if giveaway.ended != redraw {
        return Ok(Finish::WrongState);
    }
    let winners = draw(
        &entrants(ctx, message).await?,
        giveaway.winners as usize,
        &mut fastrand::Rng::new(),
    );
    let channel = ChannelId::new(giveaway.channel);
    let host = format!("<@{}>", giveaway.host);
    if !redraw {
        giveaway.ended = true;
        ctx.store()
            .set(&giveaway_key(message), &giveaway)
            .await
            .map_err(|e| stored(&e))?;
        let text = if winners.is_empty() {
            ctx.t_in(
                ctx.locale,
                "giveaway-ended-nobody",
                &Args::new().with("host", host),
            )
        } else {
            ctx.t_in(
                ctx.locale,
                "giveaway-ended-winners",
                &Args::new()
                    .with("winners", mentions(&winners))
                    .with("host", host),
            )
        };
        // The message may be gone: the draw still counts.
        let edited = ctx
            .api
            .execute(Action::PostMessage {
                channel_id: channel,
                message_id: Some(message),
                reply_to: None,
                content: None,
                embeds: vec![embed_of(&giveaway.prize, text)?],
                components: Vec::new(),
                files: Vec::new(),
            })
            .await;
        if let Err(error) = edited {
            tracing::debug!(?error, "the message of a giveaway was not edited");
        }
    }
    if winners.is_empty() {
        return Ok(Finish::NoEntries);
    }
    let key = if redraw {
        "giveaway-redrawn"
    } else {
        "giveaway-won"
    };
    let text = ctx.t_in(
        ctx.locale,
        key,
        &Args::new()
            .with("winners", mentions(&winners))
            .with("prize", giveaway.prize.clone()),
    );
    let told = ctx
        .api
        .execute(Action::SendMessage {
            channel_id: channel,
            content: Some(text),
            embeds: Vec::new(),
            reply_to: Some(message),
            mentions: MentionPolicy::Users(
                winners
                    .iter()
                    .map(|id| closure_core::event::UserId::new(*id))
                    .collect(),
            ),
        })
        .await;
    if let Err(error) = told {
        tracing::debug!(?error, "the winners of a giveaway were not announced");
    }
    Ok(Finish::Drawn)
}

#[derive(CommandArgs)]
struct Start {
    /// What is given away.
    #[arg(max_length = 200)]
    prize: String,
    /// How long it runs: 30m, 2h or 1d.
    #[arg(max_length = 20)]
    duration: String,
    /// How many winners; 1 when left out.
    winners: Option<i64>,
    /// Where it is posted; this channel when left out.
    channel: Option<ChannelId>,
}

#[derive(CommandArgs)]
struct OfMessage {
    /// The id of the message of the giveaway.
    #[arg(max_length = 20)]
    message: String,
}

async fn start(cx: CommandCx, args: Start) -> Result<(), UserError> {
    let Some(seconds) = span(&args.duration) else {
        return cx
            .reply_private_t("giveaway-bad-duration", &Args::new())
            .await;
    };
    let winners = args.winners.unwrap_or(1);
    let Some(winners) = u32::try_from(winners)
        .ok()
        .filter(|n| (1..=MAX_WINNERS.unsigned_abs()).contains(&u64::from(*n)))
    else {
        return cx
            .reply_private_t("giveaway-bad-winners", &Args::new())
            .await;
    };
    let ctx = cx.ctx();
    let channel = args.channel.unwrap_or(cx.invocation().channel_id);
    let giveaway = Giveaway {
        channel: channel.get(),
        prize: args.prize,
        winners,
        ends: cx.now() + seconds,
        host: cx.user().id.get(),
        ended: false,
    };
    let text = ctx.t_in(
        ctx.locale,
        "giveaway-open",
        &Args::new()
            .with("ends", format!("<t:{}:R>", giveaway.ends))
            .with("winners", u64::from(winners))
            .with("host", format!("<@{}>", giveaway.host)),
    );
    let posted = ctx
        .api
        .execute(Action::PostMessage {
            channel_id: channel,
            message_id: None,
            reply_to: None,
            content: None,
            embeds: vec![embed_of(&giveaway.prize, text).map_err(|e| failed(&e))?],
            components: vec![enter_button(ctx)],
            files: Vec::new(),
        })
        .await
        .map_err(|e| failed(&e))?;
    let Outcome::Message(message) = posted else {
        return Err(UserError::message("giveaway-failed"));
    };
    let id = message.id;
    ctx.store()
        .set(&giveaway_key(id), &giveaway)
        .await
        .map_err(|e| failed(&e))?;
    cx.scheduler()
        .job(END, &EndJob { message: id.get() })
        .map_err(|e| failed(&e))?
        .key(&format!("end-{id}"))
        .at(giveaway.ends)
        .await
        .map_err(|e| failed(&e))?;
    let link = format!(
        "https://discord.com/channels/{}/{}/{id}",
        cx.guild_id(),
        channel
    );
    cx.reply_private_t("giveaway-started", &Args::new().with("link", link))
        .await
}

async fn end_job(cx: JobCx) -> Result<(), ModuleError> {
    let job: EndJob = cx.payload()?;
    finish(cx.ctx(), MessageId::new(job.message), false)
        .await
        .map(drop)
}

/// The message a command names, when it is a message id.
fn message_of(args: &OfMessage) -> Option<MessageId> {
    args.message
        .trim()
        .parse::<u64>()
        .ok()
        .filter(|id| *id > 0)
        .map(MessageId::new)
}

async fn end_now(cx: CommandCx, args: OfMessage) -> Result<(), UserError> {
    let Some(message) = message_of(&args) else {
        return cx.reply_private_t("giveaway-not-found", &Args::new()).await;
    };
    let outcome = finish(cx.ctx(), message, false)
        .await
        .map_err(|e| failed(&e))?;
    let key = match outcome {
        Finish::Drawn | Finish::NoEntries => "giveaway-ended",
        Finish::Unknown => "giveaway-not-found",
        Finish::WrongState => "giveaway-over",
    };
    cx.reply_private_t(key, &Args::new()).await
}

async fn reroll(cx: CommandCx, args: OfMessage) -> Result<(), UserError> {
    let Some(message) = message_of(&args) else {
        return cx.reply_private_t("giveaway-not-found", &Args::new()).await;
    };
    let outcome = finish(cx.ctx(), message, true)
        .await
        .map_err(|e| failed(&e))?;
    let key = match outcome {
        Finish::Drawn => "giveaway-ended",
        Finish::NoEntries => "giveaway-no-entries",
        Finish::Unknown => "giveaway-not-found",
        Finish::WrongState => "giveaway-not-over",
    };
    cx.reply_private_t(key, &Args::new()).await
}

async fn say_privately(cx: &ComponentCx, text: String) -> Result<(), UserError> {
    cx.reply_with(InteractionReply {
        content: Some(text),
        embeds: Vec::new(),
        ephemeral: true,
        components: Vec::new(),
    })
    .await
}

/// Pressing the button enters, and pressing it again leaves.
async fn join(cx: ComponentCx) -> Result<(), UserError> {
    let message = cx.click().message_id;
    let Some(giveaway) = cx
        .store()
        .get::<Giveaway>(&giveaway_key(message))
        .await
        .map_err(|e| failed(&e))?
    else {
        return say_privately(&cx, cx.t("giveaway-not-found", &Args::new())).await;
    };
    if giveaway.ended {
        return say_privately(&cx, cx.t("giveaway-over", &Args::new())).await;
    }
    let key = format!("{}{}", entry_prefix(message), cx.user().id);
    let in_already = cx.store().get::<u8>(&key).await.map_err(|e| failed(&e))?;
    if in_already.is_some() {
        cx.store().delete(&key).await.map_err(|e| failed(&e))?;
        return say_privately(&cx, cx.t("giveaway-left", &Args::new())).await;
    }
    cx.store().set(&key, &1_u8).await.map_err(|e| failed(&e))?;
    let count = entrants(cx.ctx(), message)
        .await
        .map_err(|e| failed(&e))?
        .len();
    say_privately(
        &cx,
        cx.t("giveaway-joined", &Args::new().with("count", count as u64)),
    )
    .await
}

/// The module.
///
/// # Errors
/// The module declaration is refused by the SDK.
pub fn builder() -> Result<ModuleBuilder, CoreError> {
    Ok(
        ModuleBuilder::new("giveaway", "Giveaway", env!("CARGO_PKG_VERSION"))?
            .capability(closure_sdk::Capability::Storage)
            .capability(closure_sdk::Capability::SendMessages)
            .capability(closure_sdk::Capability::RespondToInteractions)
            .permission(PermissionSpec::new(
                "giveaway.manage",
                Level::MODERATOR,
                "permission-giveaway-manage",
            )?)
            .group(
                "giveaway",
                [
                    Command::<Start>::new("start")
                        .requires("giveaway.manage")
                        .handler(start),
                    Command::<OfMessage>::new("end")
                        .requires("giveaway.manage")
                        .handler(end_now),
                    Command::<OfMessage>::new("reroll")
                        .requires("giveaway.manage")
                        .handler(reroll),
                ],
            )
            .component("join", join)
            .job(END, end_job)
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
