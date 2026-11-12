use std::collections::{BTreeMap, BTreeSet};

use super::*;

const DATA_SCAN_BATCH: usize = 50;
const EXPORT_PART_BYTES: usize = 1024 * 1024;
const EXPORT_FILES_PER_DM: usize = 8;

#[derive(CommandArgs)]
pub(super) struct ExportMyData {}

#[derive(CommandArgs)]
pub(super) struct EraseMyData {
    /// Confirm anonymizing your closed ticket data
    confirm: bool,
}

const DATA_OPERATION_PREFIX: &str = "ticket-data-operation:";
const DATA_OPERATION_TTL: std::time::Duration = std::time::Duration::from_secs(15 * 60);

#[derive(Debug, Serialize, Deserialize)]
struct DataOperationLease {
    token: u64,
}

fn data_operation_key(user: UserId) -> String {
    format!("{DATA_OPERATION_PREFIX}{}", user.get())
}

async fn acquire_data_operation(
    ctx: &EventContext,
    user: UserId,
    token: u64,
) -> Result<bool, ModuleError> {
    let key = data_operation_key(user);
    for _ in 0..5 {
        if ctx
            .store()
            .entry(&key)
            .await
            .map_err(|error| stored(&error))?
            .is_some()
        {
            return Ok(false);
        }
        match ctx
            .store()
            .replace(
                &key,
                &DataOperationLease { token },
                Expect::Missing,
                Some(DATA_OPERATION_TTL),
            )
            .await
        {
            Ok(_) => return Ok(true),
            Err(StorageError::Conflict) => {}
            Err(error) => return Err(stored(&error)),
        }
    }
    Ok(false)
}

async fn renew_data_operation(
    ctx: &EventContext,
    user: UserId,
    token: u64,
) -> Result<(), ModuleError> {
    let key = data_operation_key(user);
    let Some(entry) = ctx
        .store()
        .entry(&key)
        .await
        .map_err(|error| stored(&error))?
    else {
        return Err(ModuleError::other("ticket data operation lock expired"));
    };
    let current: DataOperationLease = serde_json::from_value(entry.value)
        .map_err(|error| ModuleError::other(error.to_string()))?;
    if current.token != token {
        return Err(ModuleError::other("ticket data operation lock changed"));
    }
    ctx.store()
        .replace(
            &key,
            &current,
            Expect::Revision(entry.revision),
            Some(DATA_OPERATION_TTL),
        )
        .await
        .map_err(|error| stored(&error))?;
    Ok(())
}

async fn release_data_operation(
    ctx: &EventContext,
    user: UserId,
    token: u64,
) -> Result<(), ModuleError> {
    let key = data_operation_key(user);
    let Some(entry) = ctx
        .store()
        .entry(&key)
        .await
        .map_err(|error| stored(&error))?
    else {
        return Ok(());
    };
    let current: DataOperationLease = serde_json::from_value(entry.value)
        .map_err(|error| ModuleError::other(error.to_string()))?;
    if current.token != token {
        return Ok(());
    }
    match ctx
        .store()
        .replace(
            &key,
            &current,
            Expect::Revision(entry.revision),
            Some(std::time::Duration::from_secs(1)),
        )
        .await
    {
        Ok(_) | Err(StorageError::Conflict) => Ok(()),
        Err(error) => Err(stored(&error)),
    }
}

const HISTORY_SPAN_PREFIX: &str = "ticket-history-span:";

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub(super) struct TranscriptRichMessage {
    pub(super) author_name: String,
    pub(super) created_at: Option<String>,
    pub(super) attachments: Vec<TranscriptRichAttachment>,
    pub(super) embeds: Vec<TranscriptRichEmbed>,
    pub(super) components: Vec<TranscriptRichComponent>,
    pub(super) mentions: Vec<TranscriptRichMention>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub(super) struct TranscriptRichAttachment {
    pub(super) url: String,
    pub(super) filename: String,
    pub(super) content_type: Option<String>,
    pub(super) size: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub(super) struct TranscriptRichEmbed {
    pub(super) title: Option<String>,
    pub(super) description: Option<String>,
    pub(super) url: Option<String>,
    pub(super) color: Option<u32>,
    pub(super) fields: Vec<TranscriptRichEmbedField>,
    pub(super) author: Option<TranscriptRichEmbedAuthor>,
    pub(super) footer: Option<TranscriptRichEmbedFooter>,
    pub(super) image_url: Option<String>,
    pub(super) thumbnail_url: Option<String>,
    pub(super) timestamp: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub(super) struct TranscriptRichEmbedField {
    pub(super) name: String,
    pub(super) value: String,
    pub(super) inline: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub(super) struct TranscriptRichEmbedAuthor {
    pub(super) name: String,
    pub(super) url: Option<String>,
    pub(super) icon_url: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub(super) struct TranscriptRichEmbedFooter {
    pub(super) text: String,
    pub(super) icon_url: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub(super) struct TranscriptRichComponent {
    pub(super) kind: String,
    pub(super) label: Option<String>,
    pub(super) style: Option<String>,
    pub(super) url: Option<String>,
    pub(super) disabled: bool,
    pub(super) placeholder: Option<String>,
    pub(super) options: Vec<TranscriptRichComponentOption>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub(super) struct TranscriptRichComponentOption {
    pub(super) label: String,
    pub(super) description: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub(super) struct TranscriptRichMention {
    pub(super) kind: String,
    pub(super) id: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(super) enum TranscriptSpanPart {
    Message { message_id: u64, author_id: u64 },
    Opener { user_id: u64 },
    FormAnswers { user_id: u64 },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(super) struct TranscriptSpan {
    pub(super) start: u32,
    pub(super) end: u32,
    pub(super) part: TranscriptSpanPart,
    #[serde(default)]
    pub(super) rich: Option<TranscriptRichMessage>,
}

#[derive(Debug)]
pub(super) struct TranscriptSpanRange {
    pub(super) start: usize,
    pub(super) end: usize,
    pub(super) part: TranscriptSpanPart,
    pub(super) rich: Option<TranscriptRichMessage>,
}

#[derive(Debug)]
pub(super) struct TranscriptArchive {
    pub(super) text: String,
    pub(super) truncated: bool,
    pub(super) messages: Vec<twilight_model::channel::Message>,
    pub(super) spans: Vec<TranscriptSpanRange>,
}

pub(super) fn history_span_key(channel: u64, attempt: i64, page: usize) -> String {
    format!("{HISTORY_SPAN_PREFIX}{channel}:{attempt}:{page:04}")
}

fn history_privacy_generation_matches(history: &TicketHistory, generation: u64) -> bool {
    history.status == HistoryStatus::Closed && history.privacy_generation == generation
}

pub(super) async fn history_privacy_generation_current(
    ctx: &EventContext,
    key: &str,
    generation: u64,
) -> Result<bool, ModuleError> {
    let latest: Option<TicketHistory> =
        ctx.store().get(key).await.map_err(|error| stored(&error))?;
    Ok(latest.is_some_and(|history| history_privacy_generation_matches(&history, generation)))
}

async fn ticket_messages(
    ctx: &EventContext,
    channel: ChannelId,
) -> Result<(Vec<twilight_model::channel::Message>, bool), DiscordError> {
    let mut messages = Vec::new();
    let mut cursor = None;
    let mut cut = false;
    for page_number in 0..TRANSCRIPT_PAGES {
        let page = ctx.api.get_messages(channel, cursor, 100).await?;
        let Some(oldest) = page.last().map(|message| message.id) else {
            break;
        };
        let full = page.len() == 100;
        messages.extend(page);
        if !full {
            break;
        }
        if page_number + 1 == TRANSCRIPT_PAGES {
            let older = ctx
                .api
                .get_messages(channel, Some(MessageCursor::Before(oldest)), 1)
                .await?;
            cut = transcript_is_incomplete(messages.len(), !older.is_empty());
            break;
        }
        cursor = Some(MessageCursor::Before(oldest));
    }
    messages.reverse();
    Ok((messages, cut))
}

fn message_quote_for_post<'a>(
    ticket: &'a Ticket,
    posted_message_id: u64,
    guild_id: u64,
) -> Option<&'a message_ticket::MessageQuote> {
    ticket.message_quote.as_ref().filter(|quote| {
        ticket.message_quote_message_id == Some(posted_message_id) && quote.guild_id == guild_id
    })
}

fn ticket_quote_for_user<'a>(
    ticket: &'a Ticket,
    user: UserId,
    guild_id: u64,
) -> Option<&'a message_ticket::MessageQuote> {
    ticket
        .message_quote
        .as_ref()
        .filter(|quote| quote.guild_id == guild_id && quote.author_id == user.get())
}

pub(super) async fn capture_transcript(
    ctx: &EventContext,
    channel: ChannelId,
    ticket: &Ticket,
) -> Result<TranscriptArchive, DiscordError> {
    let (messages, cut) = ticket_messages(ctx, channel).await?;
    let mut text = String::new();
    let mut spans = Vec::new();
    let header_start = text.len();
    text.push_str(
        &ctx.t_in(
            ctx.locale,
            "tickets-transcript-head",
            &Args::new()
                .with("number", ticket.number)
                .with("opener", ticket.opener.to_string()),
        ),
    );
    let header_end = text.len();
    text.push('\n');
    spans.push(TranscriptSpanRange {
        start: header_start,
        end: header_end,
        part: TranscriptSpanPart::Opener {
            user_id: ticket.opener,
        },
        rich: None,
    });
    if cut {
        text.push_str(&ctx.t_in(
            ctx.locale,
            "tickets-transcript-cut",
            &Args::new().with("count", messages.len() as u64),
        ));
        text.push('\n');
    }
    for message in &messages {
        let quote = message_quote_for_post(ticket, message.id.get(), ctx.guild_id.get());
        let (source_message_id, author_id, author_name, content, attached) =
            if let Some(quote) = quote {
                (
                    quote.message_id,
                    quote.author_id,
                    quote.author_name.as_str(),
                    quote.content.as_str(),
                    quote
                        .attachments
                        .iter()
                        .map(|file| file.url.as_str())
                        .collect::<Vec<_>>()
                        .join(" "),
                )
            } else {
                (
                    message.id.get(),
                    message.author.id.get(),
                    message.author.name.as_str(),
                    message.content.as_str(),
                    message
                        .attachments
                        .iter()
                        .map(|file| file.url.as_str())
                        .collect::<Vec<_>>()
                        .join(" "),
                )
            };
        let start = text.len();
        let _ = write!(
            text,
            "[{}] {} ({}): {} {attached}",
            made_at(source_message_id),
            author_name.replace(['\n', '\r'], " "),
            author_id,
            content.replace(['\n', '\r'], " ")
        );
        let rich = transcript_rich_message(
            message,
            author_name,
            source_message_id,
            content,
            quote.map(|quote| quote.attachments.as_slice()),
        );
        let end = text.len();
        text.push('\n');
        spans.push(TranscriptSpanRange {
            start,
            end,
            part: TranscriptSpanPart::Message {
                message_id: source_message_id,
                author_id,
            },
            rich,
        });
    }
    Ok(TranscriptArchive {
        text,
        truncated: cut,
        messages,
        spans,
    })
}

fn transcript_rich_message(
    message: &twilight_model::channel::Message,
    author_name: &str,
    source_message_id: u64,
    source_content: &str,
    quoted_attachments: Option<&[message_ticket::MessageQuoteAttachment]>,
) -> Option<TranscriptRichMessage> {
    let raw = serde_json::to_value(message).ok()?;
    let attachments: Vec<_> = if let Some(quoted_attachments) = quoted_attachments {
        quoted_attachments
            .iter()
            .map(|attachment| TranscriptRichAttachment {
                url: attachment.url.clone(),
                filename: attachment.filename.clone(),
                content_type: None,
                size: None,
            })
            .collect()
    } else {
        raw.get("attachments")
            .and_then(serde_json::Value::as_array)
            .map(|attachments| {
                attachments
                    .iter()
                    .filter_map(transcript_rich_attachment)
                    .collect()
            })
            .unwrap_or_default()
    };
    let embeds = raw
        .get("embeds")
        .and_then(serde_json::Value::as_array)
        .map(|embeds| {
            embeds
                .iter()
                .filter_map(transcript_rich_embed)
                .collect::<Vec<TranscriptRichEmbed>>()
        })
        .unwrap_or_default();
    let components = raw
        .get("components")
        .map(transcript_rich_components)
        .unwrap_or_default();
    let mentions =
        transcript_rich_mentions(&raw, source_content, &author_name, &embeds, &components);
    if attachments.is_empty() && embeds.is_empty() && components.is_empty() && mentions.is_empty() {
        return None;
    }
    Some(TranscriptRichMessage {
        author_name: author_name.replace(['\n', '\r'], " "),
        created_at: Some(made_at(source_message_id).to_string()),
        attachments,
        embeds,
        components,
        mentions,
    })
}

fn transcript_rich_attachment(value: &serde_json::Value) -> Option<TranscriptRichAttachment> {
    let object = value.as_object()?;
    Some(TranscriptRichAttachment {
        url: object.get("url")?.as_str()?.to_owned(),
        filename: object.get("filename")?.as_str()?.to_owned(),
        content_type: object
            .get("content_type")
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned),
        size: object.get("size").and_then(serde_json::Value::as_u64),
    })
}

fn transcript_rich_embed(value: &serde_json::Value) -> Option<TranscriptRichEmbed> {
    let object = value.as_object()?;
    let fields = object
        .get("fields")
        .and_then(serde_json::Value::as_array)
        .map(|fields| {
            fields
                .iter()
                .filter_map(transcript_rich_embed_field)
                .collect()
        })
        .unwrap_or_default();
    let author = object
        .get("author")
        .and_then(serde_json::Value::as_object)
        .map(|author| TranscriptRichEmbedAuthor {
            name: author
                .get("name")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default()
                .to_owned(),
            url: author
                .get("url")
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned),
            icon_url: author
                .get("icon_url")
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned),
        });
    let footer = object
        .get("footer")
        .and_then(serde_json::Value::as_object)
        .map(|footer| TranscriptRichEmbedFooter {
            text: footer
                .get("text")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default()
                .to_owned(),
            icon_url: footer
                .get("icon_url")
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned),
        });
    Some(TranscriptRichEmbed {
        title: object
            .get("title")
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned),
        description: object
            .get("description")
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned),
        url: object
            .get("url")
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned),
        color: object
            .get("color")
            .and_then(serde_json::Value::as_u64)
            .and_then(|color| u32::try_from(color).ok()),
        fields,
        author,
        footer,
        image_url: object
            .get("image")
            .and_then(serde_json::Value::as_object)
            .and_then(|image| image.get("url"))
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned),
        thumbnail_url: object
            .get("thumbnail")
            .and_then(serde_json::Value::as_object)
            .and_then(|thumbnail| thumbnail.get("url"))
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned),
        timestamp: object
            .get("timestamp")
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned),
    })
}

fn transcript_rich_embed_field(value: &serde_json::Value) -> Option<TranscriptRichEmbedField> {
    let object = value.as_object()?;
    Some(TranscriptRichEmbedField {
        name: object.get("name")?.as_str()?.to_owned(),
        value: object.get("value")?.as_str()?.to_owned(),
        inline: object
            .get("inline")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false),
    })
}

fn transcript_rich_components(value: &serde_json::Value) -> Vec<TranscriptRichComponent> {
    let mut components = Vec::new();
    append_transcript_rich_components(value, &mut components);
    components
}

fn append_transcript_rich_components(
    value: &serde_json::Value,
    components: &mut Vec<TranscriptRichComponent>,
) {
    match value {
        serde_json::Value::Array(values) => {
            for value in values {
                append_transcript_rich_components(value, components);
            }
        }
        serde_json::Value::Object(object) => {
            let kind = object
                .get("type")
                .or_else(|| object.get("component_type"))
                .map(transcript_component_kind)
                .unwrap_or_default();
            let options = object
                .get("options")
                .and_then(serde_json::Value::as_array)
                .map(|options| {
                    options
                        .iter()
                        .filter_map(|option| {
                            let option = option.as_object()?;
                            Some(TranscriptRichComponentOption {
                                label: option.get("label")?.as_str()?.to_owned(),
                                description: option
                                    .get("description")
                                    .and_then(serde_json::Value::as_str)
                                    .map(str::to_owned),
                            })
                        })
                        .collect()
                })
                .unwrap_or_default();
            if !kind.is_empty() && kind != "action_row" {
                components.push(TranscriptRichComponent {
                    kind,
                    label: object
                        .get("label")
                        .and_then(serde_json::Value::as_str)
                        .map(str::to_owned),
                    style: object.get("style").and_then(transcript_component_value),
                    url: object
                        .get("url")
                        .and_then(serde_json::Value::as_str)
                        .map(str::to_owned),
                    disabled: object
                        .get("disabled")
                        .and_then(serde_json::Value::as_bool)
                        .unwrap_or(false),
                    placeholder: object
                        .get("placeholder")
                        .and_then(serde_json::Value::as_str)
                        .map(str::to_owned),
                    options,
                });
            }
            if let Some(children) = object.get("components") {
                append_transcript_rich_components(children, components);
            }
            if let Some(accessory) = object.get("accessory") {
                append_transcript_rich_components(accessory, components);
            }
        }
        _ => {}
    }
}

fn transcript_component_kind(value: &serde_json::Value) -> String {
    match transcript_component_value(value).as_deref() {
        Some("1") => "action_row".to_owned(),
        Some("2") => "button".to_owned(),
        Some("3") => "string_select".to_owned(),
        Some("4") => "text_input".to_owned(),
        Some("5") => "user_select".to_owned(),
        Some("6") => "role_select".to_owned(),
        Some("7") => "mentionable_select".to_owned(),
        Some("8") => "channel_select".to_owned(),
        Some("9") => "section".to_owned(),
        Some("10") => "text_display".to_owned(),
        Some("11") => "thumbnail".to_owned(),
        Some("12") => "media_gallery".to_owned(),
        Some("13") => "file".to_owned(),
        Some("14") => "separator".to_owned(),
        Some("17") => "container".to_owned(),
        Some(kind) => kind.to_owned(),
        None => String::new(),
    }
}

fn transcript_component_value(value: &serde_json::Value) -> Option<String> {
    value
        .as_str()
        .map(str::to_owned)
        .or_else(|| value.as_u64().map(|number| number.to_string()))
}

fn transcript_rich_mentions(
    message: &serde_json::Value,
    content: &str,
    author_name: &str,
    embeds: &[TranscriptRichEmbed],
    components: &[TranscriptRichComponent],
) -> Vec<TranscriptRichMention> {
    let mut mentions = BTreeSet::new();
    for (key, kind) in [
        ("mentions", "user"),
        ("mention_roles", "role"),
        ("mention_channels", "channel"),
    ] {
        if let Some(values) = message.get(key).and_then(serde_json::Value::as_array) {
            for value in values {
                let id = value
                    .get("id")
                    .and_then(transcript_id_value)
                    .or_else(|| transcript_id_value(value));
                if let Some(id) = id.filter(|id| *id != 0) {
                    mentions.insert((kind.to_owned(), id));
                }
            }
        }
    }
    collect_transcript_text_mentions(content, &mut mentions);
    collect_transcript_text_mentions(author_name, &mut mentions);
    for embed in embeds {
        for text in [
            embed.title.as_deref(),
            embed.description.as_deref(),
            embed.url.as_deref(),
            embed.author.as_ref().map(|author| author.name.as_str()),
            embed
                .author
                .as_ref()
                .and_then(|author| author.url.as_deref()),
            embed
                .author
                .as_ref()
                .and_then(|author| author.icon_url.as_deref()),
            embed.footer.as_ref().map(|footer| footer.text.as_str()),
            embed
                .footer
                .as_ref()
                .and_then(|footer| footer.icon_url.as_deref()),
            embed.image_url.as_deref(),
            embed.thumbnail_url.as_deref(),
            embed.timestamp.as_deref(),
        ]
        .into_iter()
        .flatten()
        {
            collect_transcript_text_mentions(text, &mut mentions);
        }
        for field in &embed.fields {
            collect_transcript_text_mentions(&field.name, &mut mentions);
            collect_transcript_text_mentions(&field.value, &mut mentions);
        }
    }
    for component in components {
        for text in [
            component.label.as_deref(),
            component.style.as_deref(),
            component.url.as_deref(),
            component.placeholder.as_deref(),
        ]
        .into_iter()
        .flatten()
        {
            collect_transcript_text_mentions(text, &mut mentions);
        }
        for option in &component.options {
            collect_transcript_text_mentions(&option.label, &mut mentions);
            if let Some(description) = option.description.as_deref() {
                collect_transcript_text_mentions(description, &mut mentions);
            }
        }
    }
    mentions
        .into_iter()
        .map(|(kind, id)| TranscriptRichMention { kind, id })
        .collect()
}

fn transcript_id_value(value: &serde_json::Value) -> Option<u64> {
    value
        .as_u64()
        .or_else(|| value.as_str().and_then(|id| id.parse().ok()))
}

fn collect_transcript_text_mentions(content: &str, mentions: &mut BTreeSet<(String, u64)>) {
    let bytes = content.as_bytes();
    let mut cursor = 0;
    while cursor + 2 < bytes.len() {
        if bytes[cursor] != b'<' {
            cursor += 1;
            continue;
        }
        let (kind, start) = match bytes[cursor + 1] {
            b'@' => {
                let mut start = cursor + 2;
                let kind = match bytes.get(start) {
                    Some(b'&') => {
                        start += 1;
                        "role"
                    }
                    Some(b'!') => {
                        start += 1;
                        "user"
                    }
                    _ => "user",
                };
                (kind, start)
            }
            b'#' => ("channel", cursor + 2),
            _ => {
                cursor += 1;
                continue;
            }
        };
        let mut end = start;
        let mut id = 0_u64;
        let mut valid = true;
        while end < bytes.len() && bytes[end].is_ascii_digit() {
            let byte = bytes[end];
            id = match id
                .checked_mul(10)
                .and_then(|id| id.checked_add(u64::from(byte - b'0')))
            {
                Some(id) => id,
                None => {
                    valid = false;
                    break;
                }
            };
            end += 1;
        }
        if valid && end > start && bytes.get(end) == Some(&b'>') && id != 0 {
            mentions.insert((kind.to_owned(), id));
            cursor = end + 1;
        } else {
            cursor += 1;
        }
    }
}

pub(super) fn append_form_answers(
    ctx: &EventContext,
    transcript: &mut TranscriptArchive,
    form_answers: &[(String, String)],
    opener: u64,
) {
    if form_answers.is_empty() {
        return;
    }
    let start = transcript.text.len();
    let _ = writeln!(
        transcript.text,
        "\n{}",
        ctx.t_in(ctx.locale, "tickets-transcript-answers-title", &Args::new(),)
    );
    for (question, answer) in form_answers {
        let _ = writeln!(
            transcript.text,
            "- {}: {}",
            question.replace(['\n', '\r'], " "),
            answer.replace(['\n', '\r'], " "),
        );
    }
    let end = transcript.text.len();
    transcript.spans.push(TranscriptSpanRange {
        start,
        end,
        part: TranscriptSpanPart::FormAnswers { user_id: opener },
        rich: None,
    });
}

pub(super) async fn write_history_span_pages(
    ctx: &EventContext,
    channel: ChannelId,
    attempt: i64,
    text: &str,
    pages: &[(usize, usize)],
    spans: &[TranscriptSpanRange],
    expires_at: i64,
) -> Result<u32, ModuleError> {
    let page_count =
        u32::try_from(pages.len()).map_err(|error| ModuleError::other(error.to_string()))?;
    let ttl = history_ttl_until(expires_at, ctx.now())?;
    for (page, (page_start, page_end)) in pages.iter().enumerate() {
        let mut page_spans = Vec::new();
        for span in spans {
            let start = span.start.max(*page_start);
            let end = span.end.min(*page_end);
            if start >= end {
                continue;
            }
            page_spans.push(TranscriptSpan {
                start: u32::try_from(text[*page_start..start].chars().count())
                    .map_err(|error| ModuleError::other(error.to_string()))?,
                end: u32::try_from(text[*page_start..end].chars().count())
                    .map_err(|error| ModuleError::other(error.to_string()))?,
                part: span.part.clone(),
                rich: if start == span.start {
                    span.rich.clone()
                } else {
                    None
                },
            });
        }
        let serialized = serde_json::to_vec(&page_spans)
            .map_err(|error| ModuleError::other(error.to_string()))?;
        if serialized.len() > HISTORY_VALUE_BYTES {
            for span in &mut page_spans {
                span.rich = None;
            }
            let spans_without_rich = serde_json::to_vec(&page_spans)
                .map_err(|error| ModuleError::other(error.to_string()))?;
            if spans_without_rich.len() > HISTORY_VALUE_BYTES {
                remove_history_span_pages(ctx, channel.get(), attempt, page_count).await;
                return Err(ModuleError::other("ticket transcript spans are too large"));
            }
        }
        if let Err(error) = ctx
            .store()
            .set_for(
                &history_span_key(channel.get(), attempt, page),
                &page_spans,
                ttl,
            )
            .await
        {
            remove_history_span_pages(ctx, channel.get(), attempt, page_count).await;
            return Err(stored(&error));
        }
    }
    Ok(page_count)
}

pub(super) async fn remove_history_span_pages(
    ctx: &EventContext,
    channel: u64,
    attempt: i64,
    pages: u32,
) {
    for page in 0..pages {
        if let Err(error) = ctx
            .store()
            .delete(&history_span_key(channel, attempt, page as usize))
            .await
        {
            tracing::warn!(
                ?error,
                channel,
                page,
                "could not remove an incomplete ticket transcript span page"
            );
        }
    }
}

pub(super) async fn validate_history_span_pages(
    ctx: &EventContext,
    channel: u64,
    attempt: i64,
    content_pages: u32,
    span_pages: u32,
) -> Result<(), ModuleError> {
    if span_pages == 0 || span_pages != content_pages {
        return if span_pages == 0 {
            Ok(())
        } else {
            Err(ModuleError::other("ticket transcript spans are incomplete"))
        };
    }
    for page in 0..span_pages {
        let page = usize::try_from(page).map_err(|error| ModuleError::other(error.to_string()))?;
        let spans: Vec<TranscriptSpan> = ctx
            .store()
            .get(&history_span_key(channel, attempt, page))
            .await
            .map_err(|error| stored(&error))?
            .ok_or_else(|| ModuleError::other("ticket transcript spans are missing"))?;
        let text: String = ctx
            .store()
            .get(&history_content_key(channel, attempt, page))
            .await
            .map_err(|error| stored(&error))?
            .ok_or_else(|| ModuleError::other("ticket transcript page is missing"))?;
        let chars = text.chars().count();
        if spans.iter().any(|span| {
            span.start >= span.end || usize::try_from(span.end).map_or(true, |end| end > chars)
        }) {
            return Err(ModuleError::other("ticket transcript spans are invalid"));
        }
    }
    Ok(())
}

#[derive(Default)]
struct ExportStats {
    legacy_histories: usize,
    log_copies: usize,
    staff_reasons_omitted: usize,
    active_read_errors: usize,
    active_truncated_tickets: usize,
    truncated_histories: usize,
    pending_histories: usize,
    closed_retained_tickets: usize,
    closed_retained_messages: usize,
    private_note_pages_not_exported: usize,
    private_note_scan_incomplete: usize,
    knowledge_suggestion_states_not_exported: usize,
    suggestion_tickets_seen: BTreeSet<(u64, i64)>,
    note_channels_seen: BTreeSet<u64>,
    malformed_records: usize,
    own_messages: usize,
}

impl ExportStats {
    fn complete(&self) -> bool {
        self.legacy_histories == 0
            && self.log_copies == 0
            && self.staff_reasons_omitted == 0
            && self.active_read_errors == 0
            && self.active_truncated_tickets == 0
            && self.truncated_histories == 0
            && self.pending_histories == 0
            && self.closed_retained_tickets == 0
            && self.closed_retained_messages == 0
            && self.private_note_pages_not_exported == 0
            && self.private_note_scan_incomplete == 0
            && self.knowledge_suggestion_states_not_exported == 0
            && self.malformed_records == 0
    }
}

struct ExportFiles {
    user_id: UserId,
    operation_token: u64,
    guild_id: closure_core::event::GuildId,
    current: Vec<u8>,
    current_started: bool,
    files: Vec<FileUpload>,
    part: usize,
    sent_parts: usize,
}

impl ExportFiles {
    fn new(user_id: UserId, guild_id: closure_core::event::GuildId, operation_token: u64) -> Self {
        let mut files = Self {
            user_id,
            operation_token,
            guild_id,
            current: Vec::new(),
            current_started: false,
            files: Vec::new(),
            part: 0,
            sent_parts: 0,
        };
        files.start_part();
        files
    }

    fn start_part(&mut self) {
        self.part += 1;
        self.current = format!(
            "{{\"kind\":\"header\",\"schema_version\":1,\"guild_id\":{},\"user_id\":{},\"part\":{}}}\n",
            self.guild_id.get(),
            self.user_id.get(),
            self.part
        )
        .into_bytes();
        self.current_started = true;
    }

    async fn renew(&self, ctx: &EventContext) -> Result<(), ModuleError> {
        renew_data_operation(ctx, self.user_id, self.operation_token).await
    }

    async fn add(
        &mut self,
        ctx: &EventContext,
        value: serde_json::Value,
    ) -> Result<(), ModuleError> {
        let mut line =
            serde_json::to_vec(&value).map_err(|error| ModuleError::other(error.to_string()))?;
        line.push(b'\n');
        if line.len() + 128 > EXPORT_PART_BYTES {
            return Err(ModuleError::other(
                "one ticket data row is too large to export",
            ));
        }
        if !self.current_started {
            self.start_part();
        }
        if self.current.len().saturating_add(line.len()) > EXPORT_PART_BYTES {
            self.flush_current(ctx).await?;
            self.start_part();
        }
        self.current.extend(line);
        Ok(())
    }

    async fn flush_current(&mut self, ctx: &EventContext) -> Result<(), ModuleError> {
        if !self.current_started {
            return Ok(());
        }
        let data = std::mem::take(&mut self.current);
        self.current_started = false;
        self.files.push(FileUpload {
            filename: format!("ticket-data-{:04}.jsonl", self.part),
            data,
        });
        if self.files.len() >= EXPORT_FILES_PER_DM {
            self.send_batch(ctx).await?;
        }
        Ok(())
    }

    async fn send_batch(&mut self, ctx: &EventContext) -> Result<(), ModuleError> {
        if self.files.is_empty() {
            return Ok(());
        }
        self.renew(ctx).await?;
        let files = std::mem::take(&mut self.files);
        let count = files.len();
        if let Err(error) = ctx
            .api
            .execute(Action::SendDirectMessage {
                guild_id: self.guild_id,
                user_id: self.user_id,
                content: None,
                embeds: Vec::new(),
                components: Vec::new(),
                files,
            })
            .await
        {
            tracing::warn!(
                ?error,
                user_id = self.user_id.get(),
                "could not send ticket data export"
            );
            return Err(stored(&error));
        }
        self.sent_parts += count;
        Ok(())
    }

    async fn finish(
        &mut self,
        ctx: &EventContext,
        mut manifest: serde_json::Value,
    ) -> Result<(), ModuleError> {
        if !self.current_started {
            self.start_part();
        }
        self.flush_current(ctx).await?;
        self.send_batch(ctx).await?;
        manifest["parts"] = serde_json::json!(self.part);
        let data =
            serde_json::to_vec(&manifest).map_err(|error| ModuleError::other(error.to_string()))?;
        self.renew(ctx).await?;
        if let Err(error) = ctx
            .api
            .execute(Action::SendDirectMessage {
                guild_id: self.guild_id,
                user_id: self.user_id,
                content: None,
                embeds: Vec::new(),
                components: Vec::new(),
                files: vec![FileUpload {
                    filename: "ticket-data-manifest.json".to_owned(),
                    data,
                }],
            })
            .await
        {
            tracing::warn!(
                ?error,
                user_id = self.user_id.get(),
                "could not send ticket data manifest"
            );
            return Err(stored(&error));
        }
        Ok(())
    }
}

pub(super) async fn export_my_data(cx: CommandCx, _args: ExportMyData) -> Result<(), UserError> {
    cx.defer(true).await?;
    let user = cx.user().id;
    let ctx = cx.ctx();
    let token = cx.invocation().interaction_id.get();
    match acquire_data_operation(ctx, user, token).await {
        Ok(true) => {}
        Ok(false) => {
            return cx
                .reply_private_t("tickets-data-operation-busy", &Args::new())
                .await;
        }
        Err(error) => {
            tracing::warn!(
                ?error,
                user_id = user.get(),
                "could not lock ticket data export"
            );
            return cx
                .reply_private_t(
                    "tickets-data-export-failed",
                    &Args::new().with("parts", "0"),
                )
                .await;
        }
    }
    let mut stats = ExportStats::default();
    let mut files = ExportFiles::new(user, ctx.guild_id, token);

    let result = async {
        export_open_tickets(ctx, user, &mut files, &mut stats).await?;
        export_closed_histories(ctx, user, &mut files, &mut stats).await?;
        files
            .finish(
                ctx,
                serde_json::json!({
                "kind": "manifest",
                "schema_version": 1,
                "guild_id": ctx.guild_id.get(),
                "user_id": user.get(),
                "complete": stats.complete(),
                "legacy_histories_not_exported": stats.legacy_histories,
                "staff_log_copies_not_exported": stats.log_copies,
                "staff_authored_close_reasons_omitted": stats.staff_reasons_omitted,
                "active_ticket_read_errors": stats.active_read_errors,
                "active_ticket_transcripts_truncated": stats.active_truncated_tickets,
                "closed_ticket_transcripts_truncated": stats.truncated_histories,
                "unfinished_histories": stats.pending_histories,
                "closed_retained_tickets_not_erased": stats.closed_retained_tickets,
                "closed_retained_messages_not_erased": stats.closed_retained_messages,
                "unreadable_records": stats.malformed_records,
                "own_messages_exported": stats.own_messages,
                    "private_note_pages_not_exported": stats.private_note_pages_not_exported,
                "private_note_scan_incomplete": stats.private_note_scan_incomplete,
                "knowledge_suggestion_states_not_exported":
                    stats.knowledge_suggestion_states_not_exported
                }),
            )
            .await
    }
    .await;
    if let Err(error) = release_data_operation(ctx, user, token).await {
        tracing::warn!(
            ?error,
            user_id = user.get(),
            "could not release ticket data export lock"
        );
    }

    if let Err(error) = result {
        tracing::warn!(
            ?error,
            sent_parts = files.sent_parts,
            "ticket data export was incomplete"
        );
        let key = if files.sent_parts == 0 {
            "tickets-data-export-failed"
        } else {
            "tickets-data-export-partial"
        };
        return cx
            .reply_private_t(
                key,
                &Args::new().with("parts", files.sent_parts.to_string()),
            )
            .await;
    }
    cx.reply_private_t(
        "tickets-data-export-sent",
        &Args::new().with("parts", files.part.to_string()),
    )
    .await
}

async fn ticket_note_count(
    ctx: &EventContext,
    channel: ChannelId,
) -> Result<(usize, bool), ModuleError> {
    let pages = ctx
        .store()
        .list(&note_pages_prefix(channel), NOTE_PAGE_LIMIT)
        .await
        .map_err(|error| stored(&error))?;
    let count = pages.len();
    Ok((count, count == NOTE_PAGE_LIMIT))
}

async fn record_export_notes(
    ctx: &EventContext,
    channel: ChannelId,
    stats: &mut ExportStats,
) -> Result<(), ModuleError> {
    if !stats.note_channels_seen.insert(channel.get()) {
        return Ok(());
    }
    match ticket_note_count(ctx, channel).await {
        Ok((count, more_possible)) => {
            stats.private_note_pages_not_exported += count;
            if more_possible {
                stats.private_note_scan_incomplete += 1;
            }
        }
        Err(error) => {
            tracing::warn!(?error, channel = %channel, "could not count private ticket note pages");
            stats.private_note_scan_incomplete += 1;
        }
    }
    Ok(())
}

async fn export_ticket_suggestion_state(
    ctx: &EventContext,
    channel: ChannelId,
    ticket_number: i64,
    ticket_state: &str,
    files: &mut ExportFiles,
    stats: &mut ExportStats,
) -> Result<(), ModuleError> {
    if !stats
        .suggestion_tickets_seen
        .insert((channel.get(), ticket_number))
    {
        return Ok(());
    }
    files.renew(ctx).await?;
    let key = format!("ticket-knowledge-suggestion:{}", channel.get());
    let Some(entry) = ctx
        .store()
        .entry(&key)
        .await
        .map_err(|error| stored(&error))?
    else {
        return Ok(());
    };
    let Some(suggestion) = entry.value.as_object() else {
        stats.knowledge_suggestion_states_not_exported += 1;
        return Ok(());
    };
    let (Some(guild_id), Some(channel_id), Some(stored_ticket_number)) = (
        suggestion
            .get("guild_id")
            .and_then(serde_json::Value::as_u64),
        suggestion
            .get("channel_id")
            .and_then(serde_json::Value::as_u64),
        suggestion
            .get("ticket_number")
            .and_then(serde_json::Value::as_i64),
    ) else {
        stats.knowledge_suggestion_states_not_exported += 1;
        return Ok(());
    };
    if guild_id != ctx.guild_id.get()
        || channel_id != channel.get()
        || stored_ticket_number != ticket_number
    {
        return Ok(());
    }
    let (
        Some(terms),
        Some(candidates),
        Some(expires_at),
        Some(complete),
        Some(notified),
        Some(cancelled),
    ) = (
        suggestion
            .get("terms")
            .and_then(serde_json::Value::as_array),
        suggestion
            .get("candidates")
            .and_then(serde_json::Value::as_array),
        suggestion
            .get("expires_at")
            .and_then(serde_json::Value::as_i64),
        suggestion
            .get("complete")
            .and_then(serde_json::Value::as_bool),
        suggestion
            .get("notified")
            .and_then(serde_json::Value::as_bool),
        suggestion
            .get("cancelled")
            .and_then(serde_json::Value::as_bool),
    )
    else {
        stats.knowledge_suggestion_states_not_exported += 1;
        return Ok(());
    };
    let Some(terms) = terms
        .iter()
        .map(serde_json::Value::as_str)
        .collect::<Option<Vec<_>>>()
    else {
        stats.knowledge_suggestion_states_not_exported += 1;
        return Ok(());
    };
    let mut candidate_rows = Vec::with_capacity(candidates.len());
    for candidate in candidates {
        let Some(candidate) = candidate.as_object() else {
            stats.knowledge_suggestion_states_not_exported += 1;
            return Ok(());
        };
        let (Some(id), Some(score)) = (
            candidate.get("id").and_then(serde_json::Value::as_str),
            candidate.get("score").and_then(serde_json::Value::as_u64),
        ) else {
            stats.knowledge_suggestion_states_not_exported += 1;
            return Ok(());
        };
        candidate_rows.push(serde_json::json!({ "id": id, "score": score }));
    }
    if terms.is_empty() && candidate_rows.is_empty() {
        return Ok(());
    }
    files
        .add(
            ctx,
            serde_json::json!({
                "kind": "knowledge_suggestion_state",
                "state": ticket_state,
                "ticket_number": ticket_number,
                "query_terms": terms,
                "candidates": candidate_rows,
                "expires_at": expires_at,
                "complete": complete,
                "notified": notified,
                "cancelled": cancelled
            }),
        )
        .await?;
    Ok(())
}

async fn cancel_owner_ticket_suggestions(
    ctx: &EventContext,
    channel: ChannelId,
    ticket_number: i64,
) -> Result<(), ModuleError> {
    let key = format!("ticket-knowledge-suggestion:{}", channel.get());
    for _ in 0..5 {
        knowledge::cancel_ticket_suggestions(ctx, channel, ticket_number).await?;
        let Some(entry) = ctx
            .store()
            .entry(&key)
            .await
            .map_err(|error| stored(&error))?
        else {
            return Ok(());
        };
        let Some(state) = entry.value.as_object() else {
            return Err(ModuleError::other(
                "ticket knowledge suggestion state is invalid",
            ));
        };
        let (Some(guild_id), Some(channel_id), Some(stored_ticket_number)) = (
            state.get("guild_id").and_then(serde_json::Value::as_u64),
            state.get("channel_id").and_then(serde_json::Value::as_u64),
            state
                .get("ticket_number")
                .and_then(serde_json::Value::as_i64),
        ) else {
            return Err(ModuleError::other(
                "ticket knowledge suggestion state is invalid",
            ));
        };
        if guild_id != ctx.guild_id.get()
            || channel_id != channel.get()
            || stored_ticket_number != ticket_number
        {
            return Ok(());
        }
        let terms_empty = state
            .get("terms")
            .and_then(serde_json::Value::as_array)
            .is_some_and(Vec::is_empty);
        let candidates_empty = state
            .get("candidates")
            .and_then(serde_json::Value::as_array)
            .is_some_and(Vec::is_empty);
        let cancelled = state
            .get("cancelled")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false);
        if terms_empty && candidates_empty && cancelled {
            return Ok(());
        }
    }
    Err(ModuleError::other(
        "ticket knowledge suggestion terms could not be cleared",
    ))
}

async fn record_erase_notes(
    ctx: &EventContext,
    channel: ChannelId,
    stats: &mut EraseStats,
) -> Result<(), ModuleError> {
    if !stats.note_channels_seen.insert(channel.get()) {
        return Ok(());
    }
    match ticket_note_count(ctx, channel).await {
        Ok((count, more_possible)) => {
            stats.staff_notes += count;
            if more_possible {
                stats.staff_note_scan_incomplete += 1;
            }
        }
        Err(error) => {
            tracing::warn!(?error, channel = %channel, "could not count private ticket note pages");
            stats.staff_note_scan_incomplete += 1;
        }
    }
    Ok(())
}

async fn export_open_tickets(
    ctx: &EventContext,
    user: UserId,
    files: &mut ExportFiles,
    stats: &mut ExportStats,
) -> Result<(), ModuleError> {
    let mut after = String::new();
    loop {
        files.renew(ctx).await?;
        let entries = ctx
            .store()
            .list_after("ticket:", &after, DATA_SCAN_BATCH)
            .await
            .map_err(|error| stored(&error))?;
        let entries_len = entries.len();
        for (key, entry) in entries {
            after = key.clone();
            files.renew(ctx).await?;
            let Some(channel) = key
                .strip_prefix("ticket:")
                .and_then(|value| value.parse::<u64>().ok())
            else {
                stats.malformed_records += 1;
                continue;
            };
            let ticket: Ticket = match serde_json::from_value(entry.value) {
                Ok(ticket) => ticket,
                Err(error) => {
                    tracing::warn!(?error, channel, "could not read a ticket for export");
                    stats.malformed_records += 1;
                    continue;
                }
            };
            let closed_retained = ticket.closed;
            let owns_ticket = ticket.opener == user.get();
            let assigned_to_user = ticket.claimed_by == Some(user.get());
            let ticket_state = if closed_retained {
                "closed_retained"
            } else {
                "open"
            };
            if owns_ticket {
                export_ticket_suggestion_state(
                    ctx,
                    ChannelId::new(channel),
                    ticket.number,
                    ticket_state,
                    files,
                    stats,
                )
                .await?;
            }
            let (messages, truncated) = match ticket_messages(ctx, ChannelId::new(channel)).await {
                Ok(messages) => messages,
                Err(error) => {
                    tracing::warn!(?error, channel, "could not read a ticket for export");
                    stats.active_read_errors += 1;
                    let quote = ticket_quote_for_user(&ticket, user, ctx.guild_id.get());
                    let quote_owned = quote.is_some();
                    if closed_retained && (owns_ticket || assigned_to_user || quote_owned) {
                        stats.closed_retained_tickets += 1;
                    }
                    if let Some(quote) = quote {
                        stats.own_messages += 1;
                        if closed_retained {
                            stats.closed_retained_messages += 1;
                        }
                        let lifecycle = if closed_retained {
                            "closed_retained"
                        } else {
                            "open"
                        };
                        files
                            .add(
                                ctx,
                                serde_json::json!({
                                    "kind": "message",
                                    "state": lifecycle,
                                    "ticket_number": owns_ticket.then_some(ticket.number),
                                    "created_at": made_at(quote.message_id),
                                    "content": quote.content,
                                    "attachments": quote.attachments.iter().map(|attachment| {
                                        serde_json::json!({
                                            "filename": attachment.filename,
                                            "url": attachment.url
                                        })
                                    }).collect::<Vec<_>>()
                                }),
                            )
                            .await?;
                    }
                    if owns_ticket || assigned_to_user || quote_owned {
                        record_export_notes(ctx, ChannelId::new(channel), stats).await?;
                    }
                    continue;
                }
            };
            if truncated && !closed_retained {
                stats.active_truncated_tickets += 1;
            }
            let lifecycle = if closed_retained {
                "closed_retained"
            } else {
                "open"
            };
            let form_answers = if owns_ticket {
                advanced_forms::load_ticket_form_answers(ctx, &ticket)
                    .await
                    .map_err(|error| stored(&error))?
            } else {
                Vec::new()
            };
            if owns_ticket {
                files
                    .add(
                        ctx,
                        serde_json::json!({
                            "kind": "ticket",
                            "state": lifecycle,
                            "ticket_number": ticket.number,
                            "created_at": ticket.created,
                            "labels": &ticket.labels,
                            "assigned_to_you": assigned_to_user
                        }),
                    )
                    .await?;
                for (index, (question, answer)) in form_answers.iter().enumerate() {
                    files
                        .add(
                            ctx,
                            serde_json::json!({
                                "kind": "form_answer",
                                "state": lifecycle,
                                "ticket_number": ticket.number,
                                "index": index,
                                "question": question,
                                "answer": answer
                            }),
                        )
                        .await?;
                }
            } else if assigned_to_user {
                files
                    .add(
                        ctx,
                        serde_json::json!({
                            "kind": "ticket_role",
                            "state": lifecycle,
                            "ticket_number": None::<i64>,
                            "role": "assignee"
                        }),
                    )
                    .await?;
            }
            let mut own_message_count = 0;
            for message in messages {
                if message_quote_for_post(&ticket, message.id.get(), ctx.guild_id.get()).is_some()
                    || message.author.id != user
                {
                    continue;
                }
                own_message_count += 1;
                stats.own_messages += 1;
                if closed_retained {
                    stats.closed_retained_messages += 1;
                }
                files
                    .add(
                        ctx,
                        serde_json::json!({
                            "kind": "message",
                            "state": lifecycle,
                            "ticket_number": owns_ticket.then_some(ticket.number),
                            "created_at": made_at(message.id.get()),
                            "content": message.content,
                            "attachments": message.attachments.iter().map(|attachment| {
                                serde_json::json!({
                                    "filename": attachment.filename,
                                    "url": attachment.url
                                })
                            }).collect::<Vec<_>>()
                        }),
                    )
                    .await?;
            }
            if let Some(quote) = ticket_quote_for_user(&ticket, user, ctx.guild_id.get()) {
                own_message_count += 1;
                stats.own_messages += 1;
                if closed_retained {
                    stats.closed_retained_messages += 1;
                }
                files
                    .add(
                        ctx,
                        serde_json::json!({
                            "kind": "message",
                            "state": lifecycle,
                            "ticket_number": owns_ticket.then_some(ticket.number),
                            "created_at": made_at(quote.message_id),
                            "content": quote.content,
                            "attachments": quote.attachments.iter().map(|attachment| {
                                serde_json::json!({
                                    "filename": attachment.filename,
                                    "url": attachment.url
                                })
                            }).collect::<Vec<_>>()
                        }),
                    )
                    .await?;
            }
            let related = owns_ticket || assigned_to_user || own_message_count > 0;
            if closed_retained && related {
                stats.closed_retained_tickets += 1;
            }
            if related {
                record_export_notes(ctx, ChannelId::new(channel), stats).await?;
            }
        }
        if entries_len < DATA_SCAN_BATCH {
            break;
        }
    }
    Ok(())
}

async fn export_closed_histories(
    ctx: &EventContext,
    user: UserId,
    files: &mut ExportFiles,
    stats: &mut ExportStats,
) -> Result<(), ModuleError> {
    let mut after = String::new();
    loop {
        files.renew(ctx).await?;
        let entries = ctx
            .store()
            .list_after(HISTORY_PREFIX, &after, DATA_SCAN_BATCH)
            .await
            .map_err(|error| stored(&error))?;
        let entries_len = entries.len();
        for (key, entry) in entries {
            after = key.clone();
            files.renew(ctx).await?;
            let history: TicketHistory = match serde_json::from_value(entry.value) {
                Ok(history) => history,
                Err(error) => {
                    tracing::warn!(?error, "could not read a ticket history for export");
                    stats.malformed_records += 1;
                    continue;
                }
            };
            if history.log_copy_sent && history.log_copy_channel.is_some() {
                stats.log_copies += 1;
            }
            if !history.reason.is_empty() {
                stats.staff_reasons_omitted += 1;
            }
            if history.status != HistoryStatus::Closed {
                stats.pending_histories += 1;
                let owns_ticket = !history.opener_erased && history.opener == user.get();
                let related_by_role = owns_ticket
                    || history.claimed_by == Some(user.get())
                    || history.closer == Some(user.get());
                if owns_ticket {
                    export_ticket_suggestion_state(
                        ctx,
                        ChannelId::new(history.channel_id),
                        history.number,
                        "pending",
                        files,
                        stats,
                    )
                    .await?;
                }
                if related_by_role {
                    record_export_notes(ctx, ChannelId::new(history.channel_id), stats).await?;
                }
                continue;
            }
            if history.transcript_truncated {
                stats.truncated_histories += 1;
            }
            let legacy = history.transcript_span_pages != history.transcript_pages;
            if legacy {
                stats.legacy_histories += 1;
            }

            let mut roles = Vec::new();
            let owns_ticket = !history.opener_erased && history.opener == user.get();
            if owns_ticket {
                export_ticket_suggestion_state(
                    ctx,
                    ChannelId::new(history.channel_id),
                    history.number,
                    "closed",
                    files,
                    stats,
                )
                .await?;
                roles.push("opener");
            }
            if history.claimed_by == Some(user.get()) {
                roles.push("assignee");
            }
            if history.closer == Some(user.get()) {
                roles.push("closer");
            }
            if !roles.is_empty() {
                files
                    .add(
                        ctx,
                        if owns_ticket {
                            serde_json::json!({
                                "kind": "ticket",
                                "state": "closed",
                                "ticket_number": history.number,
                                "created_at": history.created,
                                "closed_at": history.closed,
                                "roles": &roles,
                                "labels": &history.labels,
                                "rating": history.rating.as_ref().map(|rating| {
                                    serde_json::json!({
                                        "score": rating.score,
                                        "created_at": rating.created
                                    })
                                }),
                            })
                        } else {
                            serde_json::json!({
                                "kind": "ticket_role",
                                "state": "closed",
                                "ticket_number": None::<i64>,
                                "roles": &roles
                            })
                        },
                    )
                    .await?;
            }
            if legacy {
                if !roles.is_empty() {
                    record_export_notes(ctx, ChannelId::new(history.channel_id), stats).await?;
                }
                continue;
            }
            let own = match own_history_content(ctx, &history, user, owns_ticket, files).await {
                Ok(own) => own,
                Err(error) => {
                    tracing::warn!(
                        ?error,
                        number = history.number,
                        "could not read ticket history messages for export"
                    );
                    stats.malformed_records += 1;
                    continue;
                }
            };
            let related =
                !roles.is_empty() || !own.messages.is_empty() || own.form_answers_text.is_some();
            if related {
                record_export_notes(ctx, ChannelId::new(history.channel_id), stats).await?;
            }
            for (message_id, content, rich) in own.messages {
                stats.own_messages += 1;
                files
                    .add(
                        ctx,
                        serde_json::json!({
                            "kind": "message",
                            "state": "closed",
                            "ticket_number": owns_ticket.then_some(history.number),
                            "created_at": made_at(message_id),
                            "content": content,
                            "rich": rich
                        }),
                    )
                    .await?;
            }
            if let Some(form_text) = own.form_answers_text {
                let chars: Vec<char> = form_text.chars().collect();
                for (chunk_index, chunk) in chars.chunks(16_000).enumerate() {
                    let chunk: String = chunk.iter().collect();
                    files
                        .add(
                            ctx,
                            serde_json::json!({
                                "kind": "form_answers",
                                "state": "closed",
                                "ticket_number": history.number,
                                "part": chunk_index,
                                "content": chunk
                            }),
                        )
                        .await?;
                }
            }
        }
        if entries_len < DATA_SCAN_BATCH {
            break;
        }
    }
    Ok(())
}

struct OwnHistoryContent {
    messages: Vec<(u64, String, Vec<TranscriptRichMessage>)>,
    form_answers_text: Option<String>,
}

async fn own_history_content(
    ctx: &EventContext,
    history: &TicketHistory,
    user: UserId,
    owner: bool,
    files: &ExportFiles,
) -> Result<OwnHistoryContent, ModuleError> {
    let mut messages = BTreeMap::<u64, String>::new();
    let mut rich_messages = BTreeMap::<u64, Vec<TranscriptRichMessage>>::new();
    let mut form_answers_text = String::new();
    for page in 0..history.transcript_span_pages {
        files.renew(ctx).await?;
        let page = usize::try_from(page).map_err(|error| ModuleError::other(error.to_string()))?;
        let spans: Vec<TranscriptSpan> = ctx
            .store()
            .get(&history_span_key(
                history.channel_id,
                history.transcript_attempt,
                page,
            ))
            .await
            .map_err(|error| stored(&error))?
            .ok_or_else(|| ModuleError::other("ticket transcript spans are missing"))?;
        let has_owned_data = spans.iter().any(|span| match &span.part {
            TranscriptSpanPart::Message { author_id, .. } => *author_id == user.get(),
            TranscriptSpanPart::FormAnswers { user_id } => owner && *user_id == user.get(),
            TranscriptSpanPart::Opener { .. } => false,
        });
        if !has_owned_data {
            continue;
        }
        let text: String = ctx
            .store()
            .get(&history_content_key(
                history.channel_id,
                history.transcript_attempt,
                page,
            ))
            .await
            .map_err(|error| stored(&error))?
            .ok_or_else(|| ModuleError::other("ticket transcript page is missing"))?;
        let chars: Vec<char> = text.chars().collect();
        for span in spans {
            let start = usize::try_from(span.start)
                .map_err(|error| ModuleError::other(error.to_string()))?;
            let end =
                usize::try_from(span.end).map_err(|error| ModuleError::other(error.to_string()))?;
            if start >= end || end > chars.len() {
                return Err(ModuleError::other("ticket transcript span is invalid"));
            }
            match span.part {
                TranscriptSpanPart::Message {
                    message_id,
                    author_id,
                } if author_id == user.get() => {
                    messages
                        .entry(message_id)
                        .or_default()
                        .extend(chars[start..end].iter().copied());
                    if let Some(rich) = span.rich {
                        rich_messages.entry(message_id).or_default().push(rich);
                    }
                }
                TranscriptSpanPart::FormAnswers { user_id } if owner && user_id == user.get() => {
                    form_answers_text.extend(chars[start..end].iter().copied());
                }
                _ => {}
            }
        }
    }
    if owner {
        files.renew(ctx).await?;
        if let Some(answers) = teams_survey::exit_survey_answers_for_export(ctx, history, user)
            .await
            .map_err(|error| stored(&error))?
        {
            if !form_answers_text.is_empty() {
                form_answers_text.push('\n');
            }
            form_answers_text.push_str(&ctx.t_in(
                ctx.locale,
                "tickets-exit-survey-export-title",
                &Args::new(),
            ));
            for (question, answer) in answers {
                form_answers_text.push_str(&format!("\n- {question}: {answer}"));
            }
        }
    }
    Ok(OwnHistoryContent {
        messages: messages
            .into_iter()
            .map(|(message_id, content)| {
                let rich = rich_messages.remove(&message_id).unwrap_or_default();
                (message_id, content, rich)
            })
            .collect(),
        form_answers_text: (!form_answers_text.is_empty()).then_some(form_answers_text),
    })
}

#[derive(Default)]
struct EraseStats {
    eligible_histories: usize,
    closed_retained_tickets: usize,
    closed_retained_messages: usize,
    anonymized_histories: usize,
    redacted_messages: usize,
    legacy_histories: usize,
    legacy_related: usize,
    pending_histories: usize,
    active_owner_tickets: usize,
    active_assigned_tickets: usize,
    active_user_messages: usize,
    active_read_errors: usize,
    active_truncated_tickets: usize,
    external_log_copies: usize,
    staff_reasons_retained: usize,
    staff_notes: usize,
    staff_note_scan_incomplete: usize,
    note_channels_seen: BTreeSet<u64>,
    malformed_records: usize,
    pointers_removed: usize,
}

struct HistoryErase {
    matched: bool,
    updated: bool,
    legacy: bool,
    pending: bool,
    log_copy: bool,
    staff_reason: bool,
    redacted_messages: usize,
    pointer_removed: bool,
    channel_id: Option<u64>,
    active_reopened: bool,
    active_owner: bool,
    active_assigned: bool,
}

pub(super) async fn erase_my_data(cx: CommandCx, args: EraseMyData) -> Result<(), UserError> {
    cx.defer(true).await?;
    let user = cx.user().id;
    let ctx = cx.ctx();
    let token = cx.invocation().interaction_id.get();
    match acquire_data_operation(ctx, user, token).await {
        Ok(true) => {}
        Ok(false) => {
            return cx
                .reply_private_t("tickets-data-operation-busy", &Args::new())
                .await;
        }
        Err(error) => {
            tracing::warn!(
                ?error,
                user_id = user.get(),
                "could not lock ticket data anonymization"
            );
            return cx
                .reply_private_t("tickets-data-operation-failed", &Args::new())
                .await;
        }
    }
    let mut stats = EraseStats::default();

    let result = async {
        inspect_open_tickets(ctx, user, token, args.confirm, &mut stats).await?;
        erase_closed_histories(ctx, user, token, args.confirm, &mut stats).await
    }
    .await;
    if let Err(error) = release_data_operation(ctx, user, token).await {
        tracing::warn!(
            ?error,
            user_id = user.get(),
            "could not release ticket data anonymization lock"
        );
    }

    if let Err(error) = result {
        tracing::warn!(
            ?error,
            changed = stats.anonymized_histories,
            "ticket data anonymization was incomplete"
        );
        return cx
            .reply_private_t("tickets-data-erase-partial", &erase_args(&stats))
            .await;
    }

    let key = if args.confirm {
        "tickets-data-erase-result"
    } else {
        "tickets-data-erase-preview"
    };
    cx.reply_private_t(key, &erase_args(&stats)).await
}

fn erase_args(stats: &EraseStats) -> Args {
    Args::new()
        .with(
            "records",
            if stats.eligible_histories == 0 {
                stats.anonymized_histories.to_string()
            } else {
                stats.eligible_histories.to_string()
            },
        )
        .with("changed", stats.anonymized_histories.to_string())
        .with("messages", stats.redacted_messages.to_string())
        .with("legacy", stats.legacy_histories.to_string())
        .with("legacy_related", stats.legacy_related.to_string())
        .with("pending", stats.pending_histories.to_string())
        .with("closed_retained", stats.closed_retained_tickets.to_string())
        .with(
            "closed_retained_messages",
            stats.closed_retained_messages.to_string(),
        )
        .with("active_tickets", stats.active_owner_tickets.to_string())
        .with("active_assigned", stats.active_assigned_tickets.to_string())
        .with("active_messages", stats.active_user_messages.to_string())
        .with("active_errors", stats.active_read_errors.to_string())
        .with(
            "active_truncated",
            stats.active_truncated_tickets.to_string(),
        )
        .with("logs", stats.external_log_copies.to_string())
        .with("reasons", stats.staff_reasons_retained.to_string())
        .with("staff_notes", stats.staff_notes.to_string())
        .with(
            "staff_note_scan_incomplete",
            stats.staff_note_scan_incomplete.to_string(),
        )
        .with("unreadable", stats.malformed_records.to_string())
        .with("pointers", stats.pointers_removed.to_string())
}

async fn inspect_open_tickets(
    ctx: &EventContext,
    user: UserId,
    token: u64,
    confirm: bool,
    stats: &mut EraseStats,
) -> Result<(), ModuleError> {
    let mut after = String::new();
    loop {
        renew_data_operation(ctx, user, token).await?;
        let entries = ctx
            .store()
            .list_after("ticket:", &after, DATA_SCAN_BATCH)
            .await
            .map_err(|error| stored(&error))?;
        let entries_len = entries.len();
        for (key, entry) in entries {
            after = key.clone();
            renew_data_operation(ctx, user, token).await?;
            let Some(channel) = key
                .strip_prefix("ticket:")
                .and_then(|value| value.parse::<u64>().ok())
            else {
                stats.malformed_records += 1;
                continue;
            };
            let ticket: Ticket = match serde_json::from_value(entry.value) {
                Ok(ticket) => ticket,
                Err(error) => {
                    tracing::warn!(?error, channel, "could not inspect a ticket");
                    stats.malformed_records += 1;
                    continue;
                }
            };
            let owner = ticket.opener == user.get();
            let assigned = ticket.claimed_by == Some(user.get());
            let channel_id = ChannelId::new(channel);
            if confirm && owner {
                cancel_owner_ticket_suggestions(ctx, channel_id, ticket.number).await?;
            }
            let (messages, truncated) = match ticket_messages(ctx, channel_id).await {
                Ok(messages) => messages,
                Err(error) => {
                    tracing::warn!(?error, channel, "could not inspect a ticket");
                    let quote_owned =
                        ticket_quote_for_user(&ticket, user, ctx.guild_id.get()).is_some();
                    if ticket.closed && (owner || assigned || quote_owned) {
                        stats.closed_retained_tickets += 1;
                    } else if owner {
                        stats.active_owner_tickets += 1;
                    } else if assigned {
                        stats.active_assigned_tickets += 1;
                    }
                    if quote_owned {
                        if ticket.closed {
                            stats.closed_retained_messages += 1;
                        } else {
                            stats.active_user_messages += 1;
                        }
                    }
                    stats.active_read_errors += 1;
                    if owner || assigned || quote_owned {
                        record_erase_notes(ctx, channel_id, stats).await?;
                    }
                    continue;
                }
            };
            if truncated && !ticket.closed {
                stats.active_truncated_tickets += 1;
            }
            if !ticket.closed && owner {
                stats.active_owner_tickets += 1;
            }
            if !ticket.closed && assigned {
                stats.active_assigned_tickets += 1;
            }
            let own_message_count = messages
                .iter()
                .filter(|message| {
                    message_quote_for_post(&ticket, message.id.get(), ctx.guild_id.get()).is_none()
                        && message.author.id == user
                })
                .count();
            let own_messages = own_message_count
                + if ticket_quote_for_user(&ticket, user, ctx.guild_id.get()).is_some() {
                    1
                } else {
                    0
                };
            stats.active_user_messages += if ticket.closed { 0 } else { own_messages };
            stats.closed_retained_messages += if ticket.closed { own_messages } else { 0 };
            if ticket.closed && (owner || assigned || own_messages > 0) {
                stats.closed_retained_tickets += 1;
            }
            if owner || assigned || own_messages > 0 {
                record_erase_notes(ctx, channel_id, stats).await?;
            }
        }
        if entries_len < DATA_SCAN_BATCH {
            break;
        }
    }
    Ok(())
}

async fn erase_closed_histories(
    ctx: &EventContext,
    user: UserId,
    token: u64,
    confirm: bool,
    stats: &mut EraseStats,
) -> Result<(), ModuleError> {
    let mut after = String::new();
    loop {
        renew_data_operation(ctx, user, token).await?;
        let entries = ctx
            .store()
            .list_after(HISTORY_PREFIX, &after, DATA_SCAN_BATCH)
            .await
            .map_err(|error| stored(&error))?;
        let entries_len = entries.len();
        for (key, _) in &entries {
            after = key.clone();
            renew_data_operation(ctx, user, token).await?;
            match erase_history(ctx, key, user, token, confirm).await {
                Ok(result) => {
                    if result.matched {
                        if let Some(channel) = result.channel_id {
                            record_erase_notes(ctx, ChannelId::new(channel), stats).await?;
                        }
                    }
                    if result.legacy {
                        stats.legacy_histories += 1;
                    }
                    if result.matched && result.legacy {
                        stats.legacy_related += 1;
                    }
                    if result.active_reopened {
                        if result.active_owner {
                            stats.active_owner_tickets += 1;
                        }
                        if result.active_assigned {
                            stats.active_assigned_tickets += 1;
                        }
                    }
                    if result.pending {
                        stats.pending_histories += 1;
                    }
                    if result.matched && result.log_copy {
                        stats.external_log_copies += 1;
                    }
                    if result.matched && result.staff_reason {
                        stats.staff_reasons_retained += 1;
                    }
                    if result.matched && !result.legacy && !result.pending {
                        stats.eligible_histories += 1;
                    }
                    if result.updated {
                        stats.anonymized_histories += 1;
                        stats.redacted_messages += result.redacted_messages;
                    }
                    if result.pointer_removed {
                        stats.pointers_removed += 1;
                    }
                }
                Err(error) => {
                    tracing::warn!(?error, key, "could not anonymize a ticket history");
                    return Err(error);
                }
            }
        }
        if entries_len < DATA_SCAN_BATCH {
            break;
        }
    }
    Ok(())
}

fn can_resume_anonymization(history: &TicketHistory, user: UserId) -> bool {
    history.status == HistoryStatus::Anonymizing && history.anonymizing_user == Some(user.get())
}

async fn renew_history_anonymizing_lease(
    ctx: &EventContext,
    key: &str,
    user: UserId,
    token: u64,
    generation: u64,
) -> Result<bool, ModuleError> {
    for _ in 0..5 {
        let Some(entry) = ctx
            .store()
            .entry(key)
            .await
            .map_err(|error| stored(&error))?
        else {
            return Ok(false);
        };
        let mut history: TicketHistory = serde_json::from_value(entry.value)
            .map_err(|error| ModuleError::other(error.to_string()))?;
        let now = ctx.now();
        if history.status != HistoryStatus::Anonymizing
            || history.anonymizing_user != Some(user.get())
            || history.operation_token != Some(token)
            || history.privacy_generation != generation
            || history.operation_until <= now
        {
            return Ok(false);
        }
        history.operation_until = now
            .checked_add(HISTORY_OPERATION_LEASE_SECONDS)
            .ok_or_else(|| ModuleError::other("ticket history operation deadline overflowed"))?;
        let ttl = history_ttl(&history, now)?;
        match ctx
            .store()
            .replace(key, &history, Expect::Revision(entry.revision), Some(ttl))
            .await
        {
            Ok(_) => return Ok(true),
            Err(StorageError::Conflict) => {}
            Err(error) => return Err(stored(&error)),
        }
    }
    Ok(false)
}

async fn release_history_anonymizing_lease(
    ctx: &EventContext,
    key: &str,
    user: UserId,
    token: u64,
    generation: u64,
) -> Result<bool, ModuleError> {
    for _ in 0..5 {
        let Some(entry) = ctx
            .store()
            .entry(key)
            .await
            .map_err(|error| stored(&error))?
        else {
            return Ok(false);
        };
        let mut history: TicketHistory = serde_json::from_value(entry.value)
            .map_err(|error| ModuleError::other(error.to_string()))?;
        if history.status != HistoryStatus::Anonymizing
            || history.anonymizing_user != Some(user.get())
            || history.operation_token != Some(token)
            || history.privacy_generation != generation
        {
            return Ok(false);
        }
        history.status = HistoryStatus::Closed;
        history.anonymizing_user = None;
        history.operation_token = None;
        history.operation_until = 0;
        let ttl = history_ttl(&history, ctx.now())?;
        match ctx
            .store()
            .replace(key, &history, Expect::Revision(entry.revision), Some(ttl))
            .await
        {
            Ok(_) => return Ok(true),
            Err(StorageError::Conflict) => {}
            Err(error) => return Err(stored(&error)),
        }
    }
    Ok(false)
}

async fn erase_history(
    ctx: &EventContext,
    key: &str,
    user: UserId,
    operation_token: u64,
    dry_run: bool,
) -> Result<HistoryErase, ModuleError> {
    for _ in 0..5 {
        renew_data_operation(ctx, user, operation_token).await?;
        let Some(entry) = ctx
            .store()
            .entry(key)
            .await
            .map_err(|error| stored(&error))?
        else {
            return Ok(empty_history_erase());
        };
        let mut history: TicketHistory = serde_json::from_value(entry.value)
            .map_err(|error| ModuleError::other(error.to_string()))?;
        let now = ctx.now();
        let resumed = can_resume_anonymization(&history, user)
            && (history.operation_until <= now || history.operation_token == Some(operation_token));
        let expired_reopening =
            history.status == HistoryStatus::Reopening && history.operation_until <= now;
        let owner = !history.opener_erased && history.opener == user.get();
        let metadata_match =
            owner || history.claimed_by == Some(user.get()) || history.closer == Some(user.get());
        let legacy = history.transcript_span_pages != history.transcript_pages;
        let busy = match history.status {
            HistoryStatus::Closed => {
                history.operation_token.is_some() && history.operation_until > now
            }
            HistoryStatus::Reopening => !expired_reopening,
            HistoryStatus::Anonymizing => !resumed,
            HistoryStatus::Closing => true,
        };
        let pending = busy || (dry_run && expired_reopening);
        let (span_match, redacted_messages) = if legacy || pending || resumed {
            (false, 0)
        } else {
            scan_user_spans(ctx, &history, user, owner, operation_token).await?
        };
        let matched = metadata_match || span_match || resumed;
        let result = HistoryErase {
            matched,
            updated: false,
            legacy,
            pending,
            log_copy: matched && history.log_copy_sent && history.log_copy_channel.is_some(),
            staff_reason: matched && !history.reason.is_empty(),
            redacted_messages,
            pointer_removed: false,
            channel_id: Some(history.channel_id),
            active_reopened: false,
            active_owner: false,
            active_assigned: false,
        };
        if !matched || dry_run || legacy || pending {
            return Ok(result);
        }

        let generation = if resumed {
            history.privacy_generation
        } else {
            let Some(generation) = history.privacy_generation.checked_add(1) else {
                return Err(ModuleError::other("ticket privacy generation is exhausted"));
            };
            generation
        };
        let operation_until = ctx
            .now()
            .checked_add(HISTORY_OPERATION_LEASE_SECONDS)
            .ok_or_else(|| ModuleError::other("ticket history operation deadline overflowed"))?;
        history.status = HistoryStatus::Anonymizing;
        history.anonymizing_user = Some(user.get());
        history.privacy_generation = generation;
        history.operation_token = Some(operation_token);
        history.operation_until = operation_until;
        let ttl = history_ttl(&history, ctx.now())?;
        match ctx
            .store()
            .replace(key, &history, Expect::Revision(entry.revision), Some(ttl))
            .await
        {
            Ok(_) => {}
            Err(StorageError::Conflict) => continue,
            Err(error) => return Err(stored(&error)),
        }
        if owner {
            cancel_owner_ticket_suggestions(
                ctx,
                ChannelId::new(history.channel_id),
                history.number,
            )
            .await?;
        }

        let ticket_key = format!("ticket:{}", history.channel_id);
        let current_ticket: Option<Ticket> = ctx
            .store()
            .get(&ticket_key)
            .await
            .map_err(|error| stored(&error))?;
        if let Some(ticket) = current_ticket.filter(|ticket| !ticket.closed) {
            let _ = release_history_anonymizing_lease(ctx, key, user, operation_token, generation)
                .await?;
            return Ok(HistoryErase {
                pending: true,
                active_reopened: true,
                active_owner: ticket.opener == user.get(),
                active_assigned: ticket.claimed_by == Some(user.get()),
                ..result
            });
        }

        if !renew_history_anonymizing_lease(ctx, key, user, operation_token, generation).await? {
            return Ok(HistoryErase {
                pending: true,
                ..result
            });
        }
        let owner = !history.opener_erased && history.opener == user.get();
        if owner {
            teams_survey::delete_exit_survey(ctx, &history)
                .await
                .map_err(|error| stored(&error))?;
        }
        let transcript_bytes =
            redact_history_pages(ctx, &history, user, owner, operation_token, key, generation)
                .await?;
        remove_user_spans(ctx, &history, user, owner, operation_token, key, generation).await?;

        let mut finalized = false;
        for _ in 0..5 {
            renew_data_operation(ctx, user, operation_token).await?;
            if !renew_history_anonymizing_lease(ctx, key, user, operation_token, generation).await?
            {
                return Ok(HistoryErase {
                    pending: true,
                    ..result
                });
            }
            let Some(entry) = ctx
                .store()
                .entry(key)
                .await
                .map_err(|error| stored(&error))?
            else {
                return Err(ModuleError::other(
                    "ticket history disappeared during anonymization",
                ));
            };
            let mut updated: TicketHistory = serde_json::from_value(entry.value)
                .map_err(|error| ModuleError::other(error.to_string()))?;
            if updated.status == HistoryStatus::Closed
                && updated.privacy_generation == generation
                && updated.anonymizing_user.is_none()
                && updated.operation_token.is_none()
                && updated.operation_until == 0
            {
                finalized = true;
                break;
            }
            if updated.status != HistoryStatus::Anonymizing
                || updated.anonymizing_user != Some(user.get())
                || updated.operation_token != Some(operation_token)
                || updated.privacy_generation != generation
                || updated.operation_until <= ctx.now()
            {
                return Ok(HistoryErase {
                    pending: true,
                    ..result
                });
            }
            updated.transcript_bytes = transcript_bytes;
            if !updated.opener_erased && updated.opener == user.get() {
                updated.opener = 0;
                updated.opener_erased = true;
                updated.labels.clear();
                updated.rating = None;
            }
            if updated.claimed_by == Some(user.get()) {
                updated.claimed_by = None;
            }
            if updated.closer == Some(user.get()) {
                updated.closer = None;
            }
            updated.status = HistoryStatus::Closed;
            updated.anonymizing_user = None;
            updated.operation_token = None;
            updated.operation_until = 0;
            let ttl = history_ttl(&updated, ctx.now())?;
            match ctx
                .store()
                .replace(key, &updated, Expect::Revision(entry.revision), Some(ttl))
                .await
            {
                Ok(_) => {
                    finalized = true;
                    break;
                }
                Err(StorageError::Conflict) => continue,
                Err(error) => return Err(stored(&error)),
            }
        }
        if !finalized {
            return Err(ModuleError::other(
                "ticket history changed while it was being anonymized",
            ));
        }
        let mut pointer_removed = false;
        if owner {
            let pointer = history_id_key(ChannelId::new(history.channel_id));
            let current: Option<String> = ctx
                .store()
                .get(&pointer)
                .await
                .map_err(|error| stored(&error))?;
            if current.as_deref() == Some(key) {
                ctx.store()
                    .delete(&pointer)
                    .await
                    .map_err(|error| stored(&error))?;
                pointer_removed = true;
            }
        }
        return Ok(HistoryErase {
            updated: true,
            pointer_removed,
            ..result
        });
    }
    Err(ModuleError::other(
        "ticket history changed while it was being anonymized",
    ))
}

fn empty_history_erase() -> HistoryErase {
    HistoryErase {
        matched: false,
        updated: false,
        legacy: false,
        pending: false,
        log_copy: false,
        staff_reason: false,
        redacted_messages: 0,
        pointer_removed: false,
        channel_id: None,
        active_reopened: false,
        active_owner: false,
        active_assigned: false,
    }
}

async fn scan_user_spans(
    ctx: &EventContext,
    history: &TicketHistory,
    user: UserId,
    owner: bool,
    operation_token: u64,
) -> Result<(bool, usize), ModuleError> {
    let mut message_ids = BTreeSet::new();
    let mut matched = false;
    for page in 0..history.transcript_span_pages {
        renew_data_operation(ctx, user, operation_token).await?;
        let page = usize::try_from(page).map_err(|error| ModuleError::other(error.to_string()))?;
        let spans: Vec<TranscriptSpan> = ctx
            .store()
            .get(&history_span_key(
                history.channel_id,
                history.transcript_attempt,
                page,
            ))
            .await
            .map_err(|error| stored(&error))?
            .ok_or_else(|| ModuleError::other("ticket transcript spans are missing"))?;
        for span in spans {
            match span.part {
                TranscriptSpanPart::Message {
                    message_id,
                    author_id,
                } if author_id == user.get() => {
                    matched = true;
                    message_ids.insert(message_id);
                }
                TranscriptSpanPart::Opener { user_id }
                | TranscriptSpanPart::FormAnswers { user_id }
                    if owner && user_id == user.get() =>
                {
                    matched = true;
                }
                _ => {}
            }
        }
    }
    Ok((matched, message_ids.len()))
}

async fn redact_history_pages(
    ctx: &EventContext,
    history: &TicketHistory,
    user: UserId,
    owner: bool,
    operation_token: u64,
    history_key: &str,
    generation: u64,
) -> Result<u64, ModuleError> {
    let ttl = history_ttl(history, ctx.now())?;
    let mut bytes = 0_u64;
    for page in 0..history.transcript_pages {
        renew_data_operation(ctx, user, operation_token).await?;
        if !renew_history_anonymizing_lease(ctx, history_key, user, operation_token, generation)
            .await?
        {
            return Err(ModuleError::other(
                "ticket history anonymization lease was lost",
            ));
        }
        let page = usize::try_from(page).map_err(|error| ModuleError::other(error.to_string()))?;
        let content_key = history_content_key(history.channel_id, history.transcript_attempt, page);
        let span_key = history_span_key(history.channel_id, history.transcript_attempt, page);
        let content: String = ctx
            .store()
            .get(&content_key)
            .await
            .map_err(|error| stored(&error))?
            .ok_or_else(|| ModuleError::other("ticket transcript page is missing"))?;
        let spans: Vec<TranscriptSpan> = ctx
            .store()
            .get(&span_key)
            .await
            .map_err(|error| stored(&error))?
            .ok_or_else(|| ModuleError::other("ticket transcript spans are missing"))?;
        let redacted = redact_page(&content, &spans, user, owner)?;
        if redacted != content {
            if !renew_history_anonymizing_lease(ctx, history_key, user, operation_token, generation)
                .await?
            {
                return Err(ModuleError::other(
                    "ticket history anonymization lease was lost",
                ));
            }
            ctx.store()
                .set_for(&content_key, &redacted, ttl)
                .await
                .map_err(|error| stored(&error))?;
        }
        bytes = bytes
            .checked_add(
                u64::try_from(redacted.len())
                    .map_err(|error| ModuleError::other(error.to_string()))?,
            )
            .ok_or_else(|| ModuleError::other("ticket transcript is too long"))?;
    }
    Ok(bytes)
}

fn redact_page(
    content: &str,
    spans: &[TranscriptSpan],
    user: UserId,
    owner: bool,
) -> Result<String, ModuleError> {
    let mut chars: Vec<char> = content.chars().collect();
    for span in spans {
        if !span_belongs_to_user(&span.part, user, owner) {
            continue;
        }
        let start =
            usize::try_from(span.start).map_err(|error| ModuleError::other(error.to_string()))?;
        let end =
            usize::try_from(span.end).map_err(|error| ModuleError::other(error.to_string()))?;
        if start >= end || end > chars.len() {
            return Err(ModuleError::other("ticket transcript span is invalid"));
        }
        for character in &mut chars[start..end] {
            if *character != '\n' {
                *character = ' ';
            }
        }
    }
    Ok(chars.into_iter().collect())
}

fn span_belongs_to_user(part: &TranscriptSpanPart, user: UserId, owner: bool) -> bool {
    match part {
        TranscriptSpanPart::Message { author_id, .. } => *author_id == user.get(),
        TranscriptSpanPart::Opener { user_id } | TranscriptSpanPart::FormAnswers { user_id } => {
            owner && *user_id == user.get()
        }
    }
}

async fn remove_user_spans(
    ctx: &EventContext,
    history: &TicketHistory,
    user: UserId,
    owner: bool,
    operation_token: u64,
    history_key: &str,
    generation: u64,
) -> Result<(), ModuleError> {
    let ttl = history_ttl(history, ctx.now())?;
    for page in 0..history.transcript_span_pages {
        renew_data_operation(ctx, user, operation_token).await?;
        if !renew_history_anonymizing_lease(ctx, history_key, user, operation_token, generation)
            .await?
        {
            return Err(ModuleError::other(
                "ticket history anonymization lease was lost",
            ));
        }
        let page = usize::try_from(page).map_err(|error| ModuleError::other(error.to_string()))?;
        let key = history_span_key(history.channel_id, history.transcript_attempt, page);
        let spans: Vec<TranscriptSpan> = ctx
            .store()
            .get(&key)
            .await
            .map_err(|error| stored(&error))?
            .ok_or_else(|| ModuleError::other("ticket transcript spans are missing"))?;
        let mut kept = Vec::with_capacity(spans.len());
        let mut changed = false;
        for mut span in spans {
            if span_belongs_to_user(&span.part, user, owner) {
                changed = true;
                continue;
            }
            if span
                .rich
                .as_ref()
                .is_some_and(|rich| rich.mentions.iter().any(|mention| mention.id == user.get()))
            {
                span.rich = None;
                changed = true;
            }
            kept.push(span);
        }
        if changed {
            if !renew_history_anonymizing_lease(ctx, history_key, user, operation_token, generation)
                .await?
            {
                return Err(ModuleError::other(
                    "ticket history anonymization lease was lost",
                ));
            }
            ctx.store()
                .set_for(&key, &kept, ttl)
                .await
                .map_err(|error| stored(&error))?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn span(text: &str, target: &str, part: TranscriptSpanPart) -> TranscriptSpan {
        let byte_start = text.find(target).expect("test span is present");
        let start = text[..byte_start].chars().count();
        let end = start + target.chars().count();
        TranscriptSpan {
            start: u32::try_from(start).expect("test span start fits"),
            end: u32::try_from(end).expect("test span end fits"),
            part,
            rich: None,
        }
    }

    fn transcript_spans(text: &str) -> Vec<TranscriptSpan> {
        vec![
            span(
                text,
                "header: user 7",
                TranscriptSpanPart::Opener { user_id: 7 },
            ),
            span(
                text,
                "staff (9): preserve",
                TranscriptSpanPart::Message {
                    message_id: 9,
                    author_id: 9,
                },
            ),
            span(
                text,
                "user (7): private attachment\ncontinued private user line",
                TranscriptSpanPart::Message {
                    message_id: 7,
                    author_id: 7,
                },
            ),
            span(
                text,
                "form answer: secret",
                TranscriptSpanPart::FormAnswers { user_id: 7 },
            ),
        ]
    }

    #[test]
    fn anonymizing_a_participant_preserves_the_opener_and_other_messages() {
        let text = "header: user 7\nstaff (9): preserve\nuser (7): private attachment\ncontinued private user line\nform answer: secret\n";
        let redacted = redact_page(text, &transcript_spans(text), UserId::new(7), false)
            .expect("well-formed spans are redacted");
        assert!(redacted.contains("header: user 7"));
        assert!(redacted.contains("staff (9): preserve"));
        assert!(redacted.contains("form answer: secret"));
        assert!(!redacted.contains("user (7): private attachment"));
        assert!(!redacted.contains("continued private user line"));
        assert_eq!(redacted.chars().count(), text.chars().count());
    }

    #[test]
    fn anonymizing_the_opener_also_removes_their_form_answers() {
        let text = "header: user 7\nstaff (9): preserve\nuser (7): private attachment\ncontinued private user line\nform answer: secret\n";
        let redacted = redact_page(text, &transcript_spans(text), UserId::new(7), true)
            .expect("well-formed spans are redacted");
        assert!(!redacted.contains("header: user 7"));
        assert!(redacted.contains("staff (9): preserve"));
        assert!(!redacted.contains("user (7): private attachment"));
        assert!(!redacted.contains("continued private user line"));
        assert!(!redacted.contains("form answer: secret"));
    }

    #[test]
    fn redaction_uses_character_offsets_and_covers_multiline_utf8_messages() {
        let text = "🧪 header\nstaff (9): 保持\nuser (7): 私有データ\n続き λ";
        let spans = [span(
            text,
            "user (7): 私有データ\n続き λ",
            TranscriptSpanPart::Message {
                message_id: 7,
                author_id: 7,
            },
        )];
        let redacted = redact_page(text, &spans, UserId::new(7), false)
            .expect("well-formed UTF-8 spans are redacted");
        assert!(redacted.contains("🧪 header"));
        assert!(redacted.contains("staff (9): 保持"));
        assert!(!redacted.contains("私有データ"));
        assert!(!redacted.contains("続き λ"));
        assert_eq!(redacted.chars().count(), text.chars().count());
    }

    #[test]
    fn a_retry_after_a_partial_page_write_is_idempotent() {
        let page_one = "staff: 保持\nuser (7): private";
        let page_two = "continued λ and attachment";
        let part = TranscriptSpanPart::Message {
            message_id: 7,
            author_id: 7,
        };
        let first_span = span(page_one, "user (7): private", part.clone());
        let second_span = span(page_two, "continued λ and attachment", part);
        let first_write = redact_page(
            page_one,
            std::slice::from_ref(&first_span),
            UserId::new(7),
            false,
        )
        .expect("first page is redacted");
        let resumed_first = redact_page(
            &first_write,
            std::slice::from_ref(&first_span),
            UserId::new(7),
            false,
        )
        .expect("retry is idempotent");
        let resumed_second = redact_page(page_two, &[second_span], UserId::new(7), false)
            .expect("retry redacts the remaining page");
        assert_eq!(first_write, resumed_first);
        assert!(resumed_first.contains("staff: 保持"));
        assert!(!resumed_first.contains("private"));
        assert!(!resumed_second.contains("continued λ and attachment"));
    }

    fn sample_history(
        status: HistoryStatus,
        privacy_generation: u64,
        anonymizing_user: Option<u64>,
    ) -> TicketHistory {
        TicketHistory {
            version: 1,
            expires_at: 1,
            channel_id: 1,
            number: 1,
            opener: 7,
            opener_erased: false,
            privacy_generation,
            anonymizing_user,
            operation_token: None,
            operation_until: 0,
            claimed_by: None,
            created: 0,
            closed: 1,
            closer: None,
            reason: String::new(),
            panel: None,
            survey_integrations: Vec::new(),
            labels: Vec::new(),
            rating: None,
            rating_prompt_attempted: false,
            staff_access: None,
            staff_roles: Vec::new(),
            status,
            log_copy_sent: true,
            log_copy_channel: None,
            log_copy_message: None,
            attempt: 1,
            transcript_attempt: 1,
            transcript_pages: 0,
            transcript_index_pages: 0,
            transcript_span_pages: 0,
            transcript_char_count: 0,
            transcript_bytes: 0,
            transcript_messages: 0,
            transcript_truncated: false,
        }
    }

    #[test]
    fn history_reads_are_fenced_by_status_and_privacy_generation() {
        let closed = sample_history(HistoryStatus::Closed, 3, None);
        let anonymizing = sample_history(HistoryStatus::Anonymizing, 3, Some(7));
        assert!(history_privacy_generation_matches(&closed, 3));
        assert!(!history_privacy_generation_matches(&closed, 4));
        assert!(!history_privacy_generation_matches(&anonymizing, 3));
    }

    #[test]
    fn only_the_requesting_user_can_resume_an_anonymization() {
        let anonymizing = sample_history(HistoryStatus::Anonymizing, 3, Some(7));
        assert!(can_resume_anonymization(&anonymizing, UserId::new(7)));
        assert!(!can_resume_anonymization(&anonymizing, UserId::new(8)));
        assert!(!can_resume_anonymization(
            &sample_history(HistoryStatus::Closed, 3, None),
            UserId::new(7)
        ));
    }
}
