use std::time::Duration;

use closure_core::action::{Action, InteractionReply, Outcome};
use closure_core::error::ModuleError;
use closure_core::event::GuildId;
use closure_core::module::EventContext;
use closure_sdk::ui::{self, CustomId};
use closure_sdk::{Args, ComponentCx, ContextCx, EmbedBuilder, UserError};
use twilight_model::channel::Message;
use twilight_model::channel::message::Embed;
use twilight_model::guild::Permissions;

pub(super) const COMMAND_NAME: &str = "create-ticket-from-message";
pub(super) const PANEL_SELECT: &str = "msg-ticket";
const PANEL_SELECT_TTL_SECONDS: u64 = 15 * 60;

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub(super) struct MessageQuote {
    pub(super) guild_id: u64,
    pub(super) channel_id: u64,
    pub(super) message_id: u64,
    pub(super) author_id: u64,
    pub(super) author_name: String,
    pub(super) content: String,
    pub(super) attachments: Vec<MessageQuoteAttachment>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub(super) struct MessageQuoteAttachment {
    pub(super) filename: String,
    pub(super) url: String,
}

impl MessageQuote {
    fn from_discord(message: &Message, guild_id: GuildId) -> Self {
        Self {
            guild_id: message.guild_id.unwrap_or(guild_id).get(),
            channel_id: message.channel_id.get(),
            message_id: message.id.get(),
            author_id: message.author.id.get(),
            author_name: message.author.name.clone(),
            content: message.content.clone(),
            attachments: message
                .attachments
                .iter()
                .map(|attachment| MessageQuoteAttachment {
                    filename: attachment.filename.clone(),
                    url: attachment.url.clone(),
                })
                .collect(),
        }
    }
}

pub(super) fn source_url(quote: &MessageQuote) -> String {
    format!(
        "https://discord.com/channels/{}/{}/{}",
        quote.guild_id, quote.channel_id, quote.message_id
    )
}

pub(super) fn quote_embeds(
    ctx: &EventContext,
    quote: &MessageQuote,
) -> Result<Vec<Embed>, ModuleError> {
    let source = source_url(quote);
    let body = quote.content.replace(['\r', '\0'], "");
    let description = if body.is_empty() {
        format!("[Source message]({source})")
    } else {
        format!("{body}\n\n[Source message]({source})")
    };
    if description.chars().count() > 4096 {
        return Err(ModuleError::other(
            "source message quote exceeds embed limits",
        ));
    }
    let mut embeds = vec![
        EmbedBuilder::new()
            .title(ctx.t_in(
                ctx.locale,
                "tickets-message-source-quote",
                &Args::new().with("author", quote.author_name.clone()),
            ))
            .description(description)
            .build()
            .map_err(|error| ModuleError::other(error.to_string()))?,
    ];

    let mut fields = Vec::new();
    for attachment in &quote.attachments {
        let name = attachment.filename.replace(['\r', '\n', '\0'], " ");
        let url = &attachment.url;
        if format!("[{name}]({url})").chars().count() <= 1024 {
            let value = format!("[{name}]({url})");
            fields.push((name, value));
            continue;
        }
        let chunks = url.chars().collect::<Vec<_>>();
        let count = chunks.len().div_ceil(1000);
        for (part, chunk) in chunks.chunks(1000).enumerate() {
            let part_name = format!("{name} ({}/{count})", part + 1);
            fields.push((part_name, chunk.iter().collect()));
        }
    }
    for fields in fields.chunks(5) {
        let embed = fields.iter().fold(
            EmbedBuilder::new().title(ctx.t_in(
                ctx.locale,
                "tickets-message-source-attachments",
                &Args::new(),
            )),
            |embed, (name, value)| embed.field(name.clone(), value.clone(), false),
        );
        embeds.push(
            embed
                .build()
                .map_err(|error| ModuleError::other(error.to_string()))?,
        );
    }
    if embeds.len() > 10 {
        return Err(ModuleError::other(
            "source message attachments exceed embed limits",
        ));
    }
    Ok(embeds)
}

pub(super) async fn message_command(cx: ContextCx) -> Result<(), UserError> {
    cx.require("tickets.use").await?;
    let Some(message) = cx.target_message() else {
        return cx
            .reply_private_t("tickets-message-source-unavailable", &Args::new())
            .await;
    };
    let ctx = cx.ctx();
    if message.guild_id != ctx.guild_id {
        return cx
            .reply_private_t("tickets-message-source-unavailable", &Args::new())
            .await;
    }
    let config: super::Config = match cx.config() {
        Ok(config) => config,
        Err(error) => {
            tracing::warn!(?error, "ticket configuration could not be parsed");
            return cx
                .reply_private_t("tickets-invalid-config", &Args::new())
                .await;
        }
    };
    if config.panels.len() > 25 {
        return cx
            .reply_private_t("tickets-panel-page-invalid", &Args::new())
            .await;
    }
    let custom_id = CustomId::new(PANEL_SELECT)
        .only_for(cx.user().id)
        .expires_in(cx.now(), Duration::from_secs(PANEL_SELECT_TTL_SECONDS))
        .data(format!("{}:{}", message.channel_id.get(), message.id.get()))
        .encode(cx.module());
    let placeholder = cx.t("tickets-message-select-panel", &Args::new());
    let options = if config.panels.is_empty() {
        vec![(
            cx.t("tickets-message-default-panel", &Args::new()),
            "default".to_owned(),
            Some(cx.t("tickets-message-default-panel-description", &Args::new())),
        )]
    } else {
        config
            .panels
            .iter()
            .map(|panel| {
                let label = panel
                    .name
                    .as_deref()
                    .filter(|name| !name.trim().is_empty())
                    .unwrap_or(&panel.id)
                    .to_owned();
                let description = panel
                    .description
                    .as_deref()
                    .filter(|description| !description.trim().is_empty())
                    .map(str::to_owned);
                (label, format!("panel:{}", panel.id), description)
            })
            .collect()
    };
    cx.reply_with(InteractionReply {
        content: Some(placeholder.clone()),
        embeds: Vec::new(),
        ephemeral: true,
        components: vec![ui::row([ui::select_described(
            custom_id,
            Some(&placeholder),
            options,
            1,
            1,
        )])],
    })
    .await
}

pub(super) async fn select_panel(cx: ComponentCx) -> Result<(), UserError> {
    cx.require("tickets.use").await?;
    let values = cx.values();
    if values.len() != 1 {
        return cx
            .reply_private_t("tickets-panel-select-invalid", &Args::new())
            .await;
    }
    let Ok((source_channel, message_id)) = values_source_ids(&cx.data()) else {
        return cx
            .reply_private_t("tickets-message-source-unavailable", &Args::new())
            .await;
    };
    let ctx = cx.ctx();
    if cx.click().channel_id != source_channel
        || cx.click().guild_id != ctx.guild_id
        || !cx
            .click()
            .invoker_permissions
            .contains(Permissions::VIEW_CHANNEL)
    {
        return cx
            .reply_private_t("tickets-message-source-unavailable", &Args::new())
            .await;
    }
    let source = match ctx
        .api
        .execute(Action::GetMessage {
            channel_id: source_channel,
            message_id,
        })
        .await
    {
        Ok(Outcome::Message(message))
            if message.channel_id == source_channel
                && message.id == message_id
                && message
                    .guild_id
                    .is_none_or(|guild_id| guild_id == ctx.guild_id) =>
        {
            MessageQuote::from_discord(&message, ctx.guild_id)
        }
        Ok(_) => {
            return cx
                .reply_private_t("tickets-message-source-unavailable", &Args::new())
                .await;
        }
        Err(error) => {
            tracing::warn!(
                ?error,
                guild = %ctx.guild_id,
                "ticket source message could not be fetched"
            );
            return cx
                .reply_private_t("tickets-message-source-unavailable", &Args::new())
                .await;
        }
    };
    let selected = &values[0];
    let panel_id = if selected == "default" {
        None
    } else if let Some(panel_id) = selected.strip_prefix("panel:") {
        Some(panel_id.to_owned())
    } else {
        return cx
            .reply_private_t("tickets-panel-select-invalid", &Args::new())
            .await;
    };
    Box::pin(super::open_panel(cx, panel_id, Some(source))).await
}

fn values_source_ids(
    value: &str,
) -> Result<
    (
        closure_core::event::ChannelId,
        closure_core::event::MessageId,
    ),
    (),
> {
    let (channel, message) = value.split_once(':').ok_or(())?;
    let channel = channel.parse::<u64>().map_err(|_| ())?;
    let message = message.parse::<u64>().map_err(|_| ())?;
    if channel == 0 || message == 0 {
        return Err(());
    }
    Ok((
        closure_core::event::ChannelId::new(channel),
        closure_core::event::MessageId::new(message),
    ))
}
