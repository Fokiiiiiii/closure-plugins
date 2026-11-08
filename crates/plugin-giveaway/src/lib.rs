//! Giveaways: a message with a button to enter, and winners drawn when the time is up.
//!
//! `/giveaway start` posts the message; the button enters and leaves; the end draws the winners.
//! Entries are a set in the store (a row each, no lock), so the cost does not grow with the entrants.

#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

use closure_core::action::{Action, MentionPolicy, Outcome};
use closure_core::error::ModuleError;
use closure_core::event::{ChannelId, MessageId};
use closure_core::module::EventContext;
use closure_core::storage::{Expect, StorageError};
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
/// The most winners of one giveaway: a draw of this many is still one query.
const MAX_WINNERS: i64 = 10_000;
/// How many winners one message names: Discord takes 100 mentions and 2000 characters.
const NAMES_PER_MESSAGE: usize = 50;
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
    /// The result was told on the message; an end that stopped before that is finished by the
    /// next run of the job.
    #[serde(default)]
    announced: bool,
    /// Who was drawn, kept when the draw is made, so a telling that failed is tried again for the
    /// same winners and not for a new draw.
    #[serde(default)]
    drawn: Option<Vec<u64>>,
}

#[derive(Debug, Serialize, Deserialize)]
struct EndJob {
    message: u64,
}

fn giveaway_key(message: MessageId) -> String {
    format!("g:{message}")
}

fn entries_of(message: MessageId) -> String {
    format!("e-{message}")
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
    (digits.is_empty() && any && total >= MIN_SECONDS).then_some(total)
}

fn mentions(users: &[u64]) -> String {
    users
        .iter()
        .map(|id| format!("<@{id}>"))
        .collect::<Vec<_>>()
        .join(" ")
}

/// The winners as the message of the giveaway shows them: the first ones, and how many more.
fn shown(winners: &[u64]) -> String {
    if winners.len() <= NAMES_PER_MESSAGE {
        return mentions(winners);
    }
    format!(
        "{} (+{})",
        mentions(&winners[..NAMES_PER_MESSAGE]),
        winners.len() - NAMES_PER_MESSAGE
    )
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
#[allow(
    clippy::too_many_lines,
    reason = "one flow: claim the end, draw, edit the message, tell the winners"
)]
async fn finish(
    ctx: &EventContext,
    message: MessageId,
    redraw: bool,
) -> Result<Finish, ModuleError> {
    let key = giveaway_key(message);
    let Some(entry) = ctx.store().entry(&key).await.map_err(|e| stored(&e))? else {
        return Ok(Finish::Unknown);
    };
    let mut giveaway: Giveaway = serde_json::from_value(entry.value)
        .map_err(|error| ModuleError::other(error.to_string()))?;
    let proceed = if redraw {
        giveaway.ended
    } else {
        !giveaway.ended || !giveaway.announced
    };
    if !proceed {
        return Ok(Finish::WrongState);
    }
    if !redraw && !giveaway.ended {
        // Whoever writes the end first draws: the one who loses the race finds it ended.
        giveaway.ended = true;
        match ctx
            .store()
            .replace(&key, &giveaway, Expect::Revision(entry.revision), None)
            .await
        {
            Ok(_) => {}
            Err(StorageError::Conflict) => return Ok(Finish::WrongState),
            Err(error) => return Err(stored(&error)),
        }
    }
    let winners = if let (Some(drawn), false) = (&giveaway.drawn, redraw) {
        drawn.clone()
    } else {
        let winners = ctx
            .store()
            .set_sample(&entries_of(message), giveaway.winners as usize)
            .await
            .map_err(|e| stored(&e))?;
        if !redraw {
            giveaway.drawn = Some(winners.clone());
            ctx.store()
                .set(&key, &giveaway)
                .await
                .map_err(|e| stored(&e))?;
        }
        winners
    };
    let channel = ChannelId::new(giveaway.channel);
    let host = format!("<@{}>", giveaway.host);
    if !redraw {
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
                    .with("winners", shown(&winners))
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
        mark_announced(ctx, &key, &mut giveaway, redraw).await?;
        return Ok(Finish::NoEntries);
    }
    let words = if redraw {
        "giveaway-redrawn"
    } else {
        "giveaway-won"
    };
    let mut untold = false;
    // As many messages as the winners need, the first with the words.
    for (at, names) in winners.chunks(NAMES_PER_MESSAGE).enumerate() {
        let text = if at == 0 {
            ctx.t_in(
                ctx.locale,
                words,
                &Args::new()
                    .with("winners", mentions(names))
                    .with("prize", giveaway.prize.clone()),
            )
        } else {
            mentions(names)
        };
        let told = ctx
            .api
            .execute(Action::SendMessage {
                channel_id: channel,
                content: Some(text),
                embeds: Vec::new(),
                reply_to: (at == 0).then_some(message),
                mentions: MentionPolicy::Users(
                    names
                        .iter()
                        .map(|id| closure_core::event::UserId::new(*id))
                        .collect(),
                ),
            })
            .await;
        if let Err(error) = told {
            tracing::debug!(?error, "the winners of a giveaway were not announced");
            untold = true;
        }
    }
    // Done once they were told: a telling that failed is tried again, for the same winners.
    if untold && !redraw {
        return Err(ModuleError::other(
            "the winners of a giveaway were not told",
        ));
    }
    mark_announced(ctx, &key, &mut giveaway, redraw).await?;
    Ok(Finish::Drawn)
}

async fn mark_announced(
    ctx: &EventContext,
    key: &str,
    giveaway: &mut Giveaway,
    redraw: bool,
) -> Result<(), ModuleError> {
    if redraw {
        return Ok(());
    }
    giveaway.announced = true;
    ctx.store().set(key, giveaway).await.map_err(|e| stored(&e))
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
        announced: false,
        drawn: None,
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
    // The end is a job, which may run late: an entry after the time is too late all the same.
    if giveaway.ended || cx.now() >= giveaway.ends {
        return say_privately(&cx, cx.t("giveaway-over", &Args::new())).await;
    }
    let name = entries_of(message);
    let user = cx.user().id.get();
    // One statement decides: whoever was in leaves, whoever was not enters. The number of entries
    // is kept as a score beside the set, so an entry does not count them all again.
    let counter = format!("n-{message}");
    if cx
        .store()
        .set_remove(&name, user)
        .await
        .map_err(|e| failed(&e))?
    {
        let left = cx
            .store()
            .score_add(&counter, 0_u64, -1)
            .await
            .map_err(|e| failed(&e))?;
        if left < 0 {
            recount(&cx, &name, &counter).await?;
        }
        return say_privately(&cx, cx.t("giveaway-left", &Args::new())).await;
    }
    cx.store()
        .set_add(&name, user)
        .await
        .map_err(|e| failed(&e))?;
    let mut count = cx
        .store()
        .score_add(&counter, 0_u64, 1)
        .await
        .map_err(|e| failed(&e))?;
    if count == 1 {
        // The first entry, or a giveaway whose entries were made before the number was kept.
        count = recount(&cx, &name, &counter).await?;
    }
    say_privately(
        &cx,
        cx.t(
            "giveaway-joined",
            &Args::new().with("count", u64::try_from(count).unwrap_or(0)),
        ),
    )
    .await
}

async fn recount(cx: &ComponentCx, name: &str, counter: &str) -> Result<i64, UserError> {
    let real = cx.store().set_count(name).await.map_err(|e| failed(&e))?;
    let real = i64::try_from(real).unwrap_or(i64::MAX);
    cx.store()
        .score_put(counter, 0_u64, real)
        .await
        .map_err(|e| failed(&e))?;
    Ok(real)
}

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

/// # Errors
/// The module declaration is refused by the SDK.
pub fn module() -> Result<SdkModule, CoreError> {
    builder()?.build()
}

#[cfg(test)]
mod tests;
