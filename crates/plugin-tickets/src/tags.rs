use std::time::Duration;

use closure_core::action::{Action, MentionPolicy};
use closure_core::error::ModuleError;
use closure_core::module::{
    DashboardActionError, DashboardActionRequest, DashboardActionResult, DashboardScope,
    DynamicCommandAlias, DynamicCommandChangeError, EventContext,
};
use closure_core::permission::Actor;
use closure_core::storage::{Entry, Expect, MAX_VALUE_BYTES, StorageError};
use closure_sdk::{Args, Color, CommandArgs, CommandCx, EmbedBuilder, UserError};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use twilight_model::channel::message::Embed;

use super::SavedReply;

const PREFIX: &str = "ticket-tag:";
const PAGE_SIZE: usize = 25;
const SCAN_BATCH: usize = 1_000;
const MAX_ID_CHARS: usize = 16;
const MAX_BODY_CHARS: usize = 2_000;
const MAX_QUERY_CHARS: usize = 100;
const MAX_EMBED_CHARS: usize = 6_000;
const MAX_EMBED_FIELDS: usize = 25;
const TOMBSTONE_TTL: Duration = Duration::from_hours(720);

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct TagRecord {
    reply: SavedReply,
    #[serde(default)]
    embed: Option<TagEmbed>,
    #[serde(default)]
    article_id: Option<String>,
    #[serde(default)]
    custom_command_alias: bool,
    #[serde(default)]
    alias_sync_pending: bool,
    #[serde(default)]
    alias_sync_idempotency_key: Option<String>,
    #[serde(default)]
    deleted_from_revision: Option<i64>,
    #[serde(default)]
    created_by_idempotency_key: Option<String>,
    #[serde(default)]
    last_update_idempotency_key: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Default)]
#[serde(default, deny_unknown_fields)]
struct TagEmbed {
    title: Option<String>,
    color: Option<String>,
    description: Option<String>,
    author_name: Option<String>,
    author_url: Option<String>,
    author_icon_url: Option<String>,
    thumbnail_url: Option<String>,
    image_url: Option<String>,
    footer_text: Option<String>,
    footer_icon_url: Option<String>,
    footer_timestamp: bool,
    fields: Vec<TagEmbedField>,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
struct TagEmbedField {
    name: String,
    value: String,
    #[serde(default)]
    inline: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TagInput {
    id: String,
    body: String,
    #[serde(default)]
    embed: Option<TagEmbed>,
    #[serde(default)]
    article_id: Option<String>,
    #[serde(default)]
    custom_command_alias: bool,
}

#[derive(CommandArgs)]
pub(crate) struct TagCommandArgs {
    /// The tag ID to send in this ticket
    #[arg(max_length = 16)]
    id: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TagId {
    id: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TagClone {
    id: String,
    new_id: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct GalleryTagPayload {
    content: String,
    use_embed: bool,
    #[serde(default)]
    embed: Option<TagEmbed>,
}

const TAG_COMMAND_DESCRIPTION: &str = "command-tag";

pub(crate) async fn dynamic_command_aliases(
    ctx: EventContext,
) -> Result<Vec<DynamicCommandAlias>, ModuleError> {
    let mut after = PREFIX.to_owned();
    let mut aliases = Vec::new();
    loop {
        let entries = ctx
            .store()
            .list_after(PREFIX, &after, SCAN_BATCH)
            .await
            .map_err(|error| ModuleError::other(error.to_string()))?;
        let count = entries.len();
        if count == 0 {
            break;
        }
        for (key, entry) in entries {
            after.clone_from(&key);
            let record = decode_record(&key, entry)
                .map_err(|_| ModuleError::other("ticket tag storage is invalid"))?;
            if !record.reply.deleted && record.custom_command_alias {
                aliases.push(DynamicCommandAlias {
                    name: record.reply.id,
                    description_key: TAG_COMMAND_DESCRIPTION.to_owned(),
                });
            }
        }
        if count < SCAN_BATCH {
            break;
        }
    }
    Ok(aliases)
}

enum AliasSyncFailure {
    Unavailable,
    Rejected,
    Storage(DashboardActionError),
}

async fn refresh_alias_commands(ctx: &EventContext) -> Result<(), DynamicCommandChangeError> {
    ctx.notify_dynamic_command_changes().await
}

async fn sync_pending_alias(
    ctx: &EventContext,
    key: &str,
    record: &TagRecord,
    revision: i64,
) -> Result<(TagRecord, i64), AliasSyncFailure> {
    if !record.alias_sync_pending {
        return Ok((record.clone(), revision));
    }
    match refresh_alias_commands(ctx).await {
        Ok(()) => {}
        Err(DynamicCommandChangeError::Unavailable) => {
            return Err(AliasSyncFailure::Unavailable);
        }
        Err(DynamicCommandChangeError::Rejected) => {
            return Err(AliasSyncFailure::Rejected);
        }
    }
    let mut synced = record.clone();
    synced.alias_sync_pending = false;
    synced.alias_sync_idempotency_key = None;
    let revision = save_record(ctx, key, &synced, Expect::Revision(revision))
        .await
        .map_err(AliasSyncFailure::Storage)?;
    Ok((synced, revision))
}

fn alias_sync_error(error: AliasSyncFailure) -> DashboardActionError {
    match error {
        AliasSyncFailure::Unavailable => DashboardActionError::Unavailable,
        AliasSyncFailure::Rejected => {
            DashboardActionError::Invalid("tag command alias was rejected".to_owned())
        }
        AliasSyncFailure::Storage(error) => error,
    }
}

async fn retry_pending_alias(
    ctx: &EventContext,
    key: &str,
    record: &TagRecord,
    revision: i64,
) -> Result<(TagRecord, i64), DashboardActionError> {
    sync_pending_alias(ctx, key, record, revision)
        .await
        .map_err(alias_sync_error)
}

async fn rollback_alias_change(
    ctx: &EventContext,
    key: &str,
    mut rollback: TagRecord,
    failed_revision: i64,
    idempotency_key: &str,
) -> Result<(), DashboardActionError> {
    rollback.alias_sync_pending = true;
    rollback.alias_sync_idempotency_key = Some(idempotency_key.to_owned());
    rollback.last_update_idempotency_key = Some(idempotency_key.to_owned());
    let revision = save_record(ctx, key, &rollback, Expect::Revision(failed_revision)).await?;
    match sync_pending_alias(ctx, key, &rollback, revision).await {
        Ok(_) | Err(AliasSyncFailure::Unavailable) => Ok(()),
        Err(AliasSyncFailure::Rejected) => {
            tracing::warn!(tag_id = %rollback.reply.id, "host rejected restored tag alias state");
            Ok(())
        }
        Err(AliasSyncFailure::Storage(error)) => Err(error),
    }
}

pub(super) async fn tag_command(cx: CommandCx, args: TagCommandArgs) -> Result<(), UserError> {
    send_tag_by_id(cx, &args.id, false).await
}

pub(super) async fn dynamic_command_alias(cx: CommandCx, alias: String) -> Result<(), UserError> {
    send_tag_by_id(cx, &alias, true).await
}

async fn send_tag_by_id(cx: CommandCx, id: &str, require_alias: bool) -> Result<(), UserError> {
    cx.require("tickets.use").await?;
    if !valid_id(id) {
        return Err(UserError::message("tickets-tag-not-found"));
    }
    cx.defer(true).await?;
    let ctx = cx.ctx();
    let channel = cx.invocation().channel_id;
    super::staff_ticket(&cx, ctx, channel).await?;
    let Some((record, _)) = read_record(ctx, id)
        .await
        .map_err(|_| UserError::message("tickets-failed"))?
    else {
        return Err(UserError::message("tickets-tag-not-found"));
    };
    if record.reply.deleted || require_alias && !record.custom_command_alias {
        return Err(UserError::message("tickets-tag-not-found"));
    }

    let content = (!record.reply.body.trim().is_empty()).then(|| record.reply.body.clone());
    let embeds = record
        .embed
        .as_ref()
        .map(|embed| tag_embed(embed, ctx.now()).map(|embed| vec![embed]))
        .transpose()?
        .unwrap_or_default();
    if content.is_none() && embeds.is_empty() && record.article_id.is_none() {
        return Err(UserError::message("tickets-tag-invalid"));
    }
    if content.is_some() || !embeds.is_empty() {
        ctx.api
            .execute(Action::SendMessage {
                channel_id: channel,
                content,
                embeds,
                reply_to: None,
                mentions: MentionPolicy::None,
            })
            .await
            .map_err(|error| super::failed(&error))?;
    }
    if let Some(article_id) = record.article_id.as_deref()
        && let Err(error) = super::knowledge::send_article(ctx, channel, article_id, false).await
    {
        tracing::warn!(?error, channel = %channel, "could not send the published article linked by a tag");
    }
    cx.reply_private_t("tickets-tag-sent", &Args::new()).await
}

fn tag_embed(embed: &TagEmbed, now: i64) -> Result<Embed, UserError> {
    let mut builder = EmbedBuilder::new();
    if let Some(title) = embed.title.as_deref() {
        builder = builder.title(title);
    }
    if let Some(description) = embed.description.as_deref() {
        builder = builder.description(description);
    }
    if let Some(color) = embed.color.as_deref() {
        let color = u32::from_str_radix(&color[1..], 16)
            .map_err(|_| UserError::message("tickets-tag-invalid"))?;
        builder = builder.color(Color::Rgb(color));
    }
    if let Some(author) = embed.author_name.as_deref() {
        builder = builder.author(author);
    }
    if let Some(footer) = embed.footer_text.as_deref() {
        builder = builder.footer(footer);
    }
    if embed.footer_timestamp {
        builder = builder.timestamp(now);
    }
    for field in &embed.fields {
        builder = builder.field(field.name.clone(), field.value.clone(), field.inline);
    }

    let rendered = builder
        .build()
        .map_err(|_| UserError::message("tickets-tag-invalid"))?;
    let mut value =
        serde_json::to_value(rendered).map_err(|_| UserError::message("tickets-tag-invalid"))?;
    if let Some(author) = value.get_mut("author").and_then(Value::as_object_mut) {
        if let Some(url) = embed.author_url.as_ref() {
            author.insert("url".to_owned(), json!(url));
        }
        if let Some(icon) = embed.author_icon_url.as_ref() {
            author.insert("icon_url".to_owned(), json!(icon));
        }
    }
    if let Some(footer) = value.get_mut("footer").and_then(Value::as_object_mut)
        && let Some(icon) = embed.footer_icon_url.as_ref()
    {
        footer.insert("icon_url".to_owned(), json!(icon));
    }
    if let Some(thumbnail) = embed.thumbnail_url.as_ref() {
        value["thumbnail"] = json!({ "url": thumbnail });
    }
    if let Some(image) = embed.image_url.as_ref() {
        value["image"] = json!({ "url": image });
    }
    serde_json::from_value(value).map_err(|_| UserError::message("tickets-tag-invalid"))
}

pub(crate) async fn dashboard_state(
    ctx: &EventContext,
    _actor: &Actor,
    query: Option<&str>,
    cursor: Option<&str>,
) -> Result<DashboardActionResult, DashboardActionError> {
    let query = query.map(str::trim).filter(|query| !query.is_empty());
    if query.is_some_and(|query| {
        query.chars().count() > MAX_QUERY_CHARS || query.chars().any(char::is_control)
    }) {
        return Err(DashboardActionError::Invalid(
            "invalid tag search query".to_owned(),
        ));
    }
    let after = match cursor {
        Some(cursor) if valid_id(cursor) => tag_key(cursor),
        Some(_) => {
            return Err(DashboardActionError::Invalid(
                "invalid tag cursor".to_owned(),
            ));
        }
        None => PREFIX.to_owned(),
    };
    let query = query.map(str::to_lowercase);
    let entries = ctx
        .store()
        .list_after(PREFIX, &after, SCAN_BATCH)
        .await
        .map_err(storage_error)?;
    let entry_count = entries.len();
    let mut last_scanned = None;
    let mut tags = Vec::with_capacity(PAGE_SIZE);
    let mut more_results = false;

    for (key, entry) in entries {
        let revision = entry.revision;
        let record = decode_record(&key, entry)?;
        let id = record.reply.id.clone();
        if !record.reply.deleted && matches_query(&record, query.as_deref())? {
            if tags.len() == PAGE_SIZE {
                more_results = true;
                break;
            }
            tags.push(tag_value(ctx, &record, revision).await?);
        }
        last_scanned = Some(id);
    }
    let next_cursor = if more_results || entry_count == SCAN_BATCH {
        last_scanned
    } else {
        None
    };

    let article_state = super::knowledge::public_state(ctx, None, None, None).await?;
    let articles = article_state.value["articles"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|article| {
            Some(json!({
                "id": article["id"].as_str()?,
                "title": article["title"].as_str()?,
            }))
        })
        .collect::<Vec<_>>();

    Ok(DashboardActionResult {
        value: json!({
            "tags": tags,
            "articles": articles,
            "articles_next_cursor": article_state.value["next_cursor"],
            "next_cursor": next_cursor,
        }),
        revision: 0,
    })
}

pub(crate) async fn dashboard_action(
    ctx: &EventContext,
    _actor: &Actor,
    request: &DashboardActionRequest,
    can_manage: bool,
) -> Result<DashboardActionResult, DashboardActionError> {
    if !can_manage {
        return Err(DashboardActionError::Forbidden);
    }
    if !matches!(&request.scope, DashboardScope::Tags { .. })
        || !valid_idempotency_key(&request.idempotency_key)
        || request.expected_revision < 0
    {
        return Err(DashboardActionError::Invalid(
            "invalid tag action".to_owned(),
        ));
    }

    match request.action.as_str() {
        "tag.create" => {
            if request.expected_revision != 0 {
                return Err(DashboardActionError::Invalid(
                    "tag creation requires revision 0".to_owned(),
                ));
            }
            let input: TagInput = parse_data(&request.data)?;
            let record = input_record(input, &request.idempotency_key)?;
            if let Some((existing, revision)) = read_record(ctx, &record.reply.id).await?
                && !existing.reply.deleted
                && existing.created_by_idempotency_key.as_deref()
                    == Some(request.idempotency_key.as_str())
                && same_content(&existing, &record)
            {
                let (existing, revision) =
                    retry_pending_alias(ctx, &tag_key(&existing.reply.id), &existing, revision)
                        .await?;
                return tag_result(ctx, &existing, revision).await;
            }
            if let Some(article_id) = record.article_id.as_deref()
                && super::knowledge::public_article(ctx, article_id)
                    .await?
                    .is_none()
            {
                return Err(DashboardActionError::Invalid(
                    "linked article must be published".to_owned(),
                ));
            }
            create_record(ctx, record, &request.idempotency_key).await
        }
        "tag.update" => {
            let input: TagInput = parse_data(&request.data)?;
            update_record(
                ctx,
                input,
                request.expected_revision,
                &request.idempotency_key,
            )
            .await
        }
        "tag.clone" => {
            let input: TagClone = parse_data(&request.data)?;
            clone_record(
                ctx,
                &input.id,
                &input.new_id,
                request.expected_revision,
                &request.idempotency_key,
            )
            .await
        }
        "tag.delete" => {
            let input: TagId = parse_data(&request.data)?;
            delete_record(
                ctx,
                &input.id,
                request.expected_revision,
                &request.idempotency_key,
            )
            .await
        }
        _ => Err(DashboardActionError::Unsupported),
    }
}

pub(crate) async fn gallery_export(
    ctx: &EventContext,
    id: &str,
) -> Result<Value, DashboardActionError> {
    if !valid_id(id) {
        return Err(DashboardActionError::Invalid("invalid tag ID".to_owned()));
    }
    let Some((record, _)) = read_record(ctx, id).await? else {
        return Err(DashboardActionError::NotFound);
    };
    if record.reply.deleted {
        return Err(DashboardActionError::NotFound);
    }
    Ok(gallery_payload(&record))
}

pub(crate) async fn gallery_sources(
    ctx: &EventContext,
    query: Option<&str>,
    cursor: Option<&str>,
) -> Result<DashboardActionResult, DashboardActionError> {
    let query = query.map(str::trim).filter(|query| !query.is_empty());
    if query.is_some_and(|query| {
        query.chars().count() > MAX_QUERY_CHARS || query.chars().any(char::is_control)
    }) {
        return Err(DashboardActionError::Invalid(
            "invalid tag search query".to_owned(),
        ));
    }
    let after = match cursor {
        Some(cursor) if valid_id(cursor) => tag_key(cursor),
        Some(_) => {
            return Err(DashboardActionError::Invalid(
                "invalid tag cursor".to_owned(),
            ));
        }
        None => PREFIX.to_owned(),
    };
    let query = query.map(str::to_lowercase);
    let entries = ctx
        .store()
        .list_after(PREFIX, &after, SCAN_BATCH)
        .await
        .map_err(storage_error)?;
    let entry_count = entries.len();
    let mut last_scanned = None;
    let mut sources = Vec::with_capacity(PAGE_SIZE);
    let mut more_results = false;

    for (key, entry) in entries {
        let record = decode_record(&key, entry)?;
        let id = record.reply.id.clone();
        if !record.reply.deleted && matches_query(&record, query.as_deref())? {
            if sources.len() == PAGE_SIZE {
                more_results = true;
                break;
            }
            sources.push(json!({
                "kind": "tag",
                "id": id,
                "name": id,
                "payload": gallery_payload(&record),
            }));
        }
        last_scanned = Some(id);
    }
    let next_cursor = if more_results || entry_count == SCAN_BATCH {
        last_scanned
    } else {
        None
    };
    Ok(DashboardActionResult {
        value: json!({ "sources": sources, "next_cursor": next_cursor }),
        revision: 0,
    })
}

pub(crate) async fn gallery_import(
    ctx: &EventContext,
    id: &str,
    payload: &Value,
    idempotency_key: &str,
) -> Result<DashboardActionResult, DashboardActionError> {
    if !valid_id(id) || !valid_idempotency_key(idempotency_key) {
        return Err(DashboardActionError::Invalid(
            "invalid imported tag".to_owned(),
        ));
    }
    let payload: GalleryTagPayload = parse_data(payload)?;
    if payload.use_embed != payload.embed.is_some()
        || !valid_body(&payload.content)
        || !valid_embed(payload.embed.as_ref())
    {
        return Err(DashboardActionError::Invalid(
            "invalid tag template".to_owned(),
        ));
    }
    let record = new_record(
        id.to_owned(),
        payload.content,
        payload.embed,
        None,
        false,
        false,
        None,
        Some(idempotency_key.to_owned()),
        None,
    );
    create_record(ctx, record, idempotency_key).await
}

fn input_record(input: TagInput, idempotency_key: &str) -> Result<TagRecord, DashboardActionError> {
    if !valid_id(&input.id)
        || !valid_body(&input.body)
        || !valid_embed(input.embed.as_ref())
        || !valid_article_id_option(input.article_id.as_deref())
        || input.body.trim().is_empty() && input.embed.is_none() && input.article_id.is_none()
    {
        return Err(DashboardActionError::Invalid("invalid tag".to_owned()));
    }
    Ok(new_record(
        input.id,
        input.body,
        input.embed,
        input.article_id,
        input.custom_command_alias,
        false,
        None,
        Some(idempotency_key.to_owned()),
        Some(idempotency_key.to_owned()),
    ))
}

async fn create_record(
    ctx: &EventContext,
    mut record: TagRecord,
    idempotency_key: &str,
) -> Result<DashboardActionResult, DashboardActionError> {
    if record.custom_command_alias {
        record.alias_sync_pending = true;
        record.alias_sync_idempotency_key = Some(idempotency_key.to_owned());
    } else {
        record.alias_sync_pending = false;
        record.alias_sync_idempotency_key = None;
    }

    let key = tag_key(&record.reply.id);
    let expect = match ctx.store().entry(&key).await.map_err(storage_error)? {
        None => Expect::Missing,
        Some(entry) => {
            let revision = entry.revision;
            let current = decode_record(&key, entry)?;
            if current.reply.deleted
                && current.created_by_idempotency_key.as_deref() == Some(idempotency_key)
                && current.last_update_idempotency_key.as_deref() == Some(idempotency_key)
            {
                if current.alias_sync_pending {
                    let _ = retry_pending_alias(ctx, &key, &current, revision).await?;
                }
                return Err(DashboardActionError::Invalid(
                    "tag command alias was rejected".to_owned(),
                ));
            }
            if !current.reply.deleted {
                if current.created_by_idempotency_key.as_deref() == Some(idempotency_key)
                    && same_content(&current, &record)
                {
                    let (current, revision) =
                        retry_pending_alias(ctx, &key, &current, revision).await?;
                    return tag_result(ctx, &current, revision).await;
                }
                return Err(DashboardActionError::Conflict);
            }
            Expect::Revision(revision)
        }
    };
    let revision = save_record(ctx, &key, &record, expect).await?;
    if record.alias_sync_pending {
        match sync_pending_alias(ctx, &key, &record, revision).await {
            Ok((synced, revision)) => return tag_result(ctx, &synced, revision).await,
            Err(AliasSyncFailure::Unavailable) => {
                return Err(DashboardActionError::Unavailable);
            }
            Err(AliasSyncFailure::Storage(error)) => return Err(error),
            Err(AliasSyncFailure::Rejected) => {
                let rollback = TagRecord {
                    reply: SavedReply {
                        id: record.reply.id.clone(),
                        title: String::new(),
                        body: String::new(),
                        deleted: true,
                    },
                    embed: None,
                    article_id: None,
                    custom_command_alias: false,
                    alias_sync_pending: true,
                    alias_sync_idempotency_key: Some(idempotency_key.to_owned()),
                    deleted_from_revision: Some(revision),
                    created_by_idempotency_key: record.created_by_idempotency_key.clone(),
                    last_update_idempotency_key: Some(idempotency_key.to_owned()),
                };
                rollback_alias_change(ctx, &key, rollback, revision, idempotency_key).await?;
                return Err(DashboardActionError::Invalid(
                    "tag command alias was rejected".to_owned(),
                ));
            }
        }
    }
    tag_result(ctx, &record, revision).await
}

async fn update_record(
    ctx: &EventContext,
    input: TagInput,
    expected_revision: i64,
    idempotency_key: &str,
) -> Result<DashboardActionResult, DashboardActionError> {
    if expected_revision <= 0 || !valid_id(&input.id) {
        return Err(DashboardActionError::Invalid(
            "invalid tag revision".to_owned(),
        ));
    }
    let key = tag_key(&input.id);
    let Some((current, current_revision)) = read_record(ctx, &input.id).await? else {
        return Err(DashboardActionError::NotFound);
    };
    if current.reply.deleted {
        return Err(DashboardActionError::NotFound);
    }
    let mut record = input_record(input, idempotency_key)?;
    let replay = current.last_update_idempotency_key.as_deref() == Some(idempotency_key);
    let (current, current_revision) = if replay
        && current.alias_sync_pending
        && current.alias_sync_idempotency_key.as_deref() == Some(idempotency_key)
    {
        retry_pending_alias(ctx, &key, &current, current_revision).await?
    } else {
        (current, current_revision)
    };
    if replay {
        if same_content(&current, &record) {
            return tag_result(ctx, &current, current_revision).await;
        }
        return Err(DashboardActionError::Conflict);
    }
    if expected_revision != current_revision {
        return Err(DashboardActionError::PreconditionFailed {
            current: current_revision,
        });
    }
    if let Some(article_id) = record.article_id.as_deref()
        && current.article_id.as_deref() != Some(article_id)
        && super::knowledge::public_article(ctx, article_id)
            .await?
            .is_none()
    {
        return Err(DashboardActionError::Invalid(
            "linked article must be published".to_owned(),
        ));
    }

    let alias_changed = current.custom_command_alias != record.custom_command_alias;
    let needs_alias_sync = alias_changed || current.alias_sync_pending;
    if needs_alias_sync {
        record.alias_sync_pending = true;
        record.alias_sync_idempotency_key = Some(idempotency_key.to_owned());
    }
    let previous = current.clone();
    let record = TagRecord {
        created_by_idempotency_key: current.created_by_idempotency_key.clone(),
        ..record
    };
    let revision = save_record(ctx, &key, &record, Expect::Revision(current_revision)).await?;
    if needs_alias_sync {
        match sync_pending_alias(ctx, &key, &record, revision).await {
            Ok((synced, revision)) => return tag_result(ctx, &synced, revision).await,
            Err(AliasSyncFailure::Unavailable) => {
                return Err(DashboardActionError::Unavailable);
            }
            Err(AliasSyncFailure::Storage(error)) => return Err(error),
            Err(AliasSyncFailure::Rejected) => {
                rollback_alias_change(ctx, &key, previous, revision, idempotency_key).await?;
                return Err(DashboardActionError::Invalid(
                    "tag command alias was rejected".to_owned(),
                ));
            }
        }
    }
    tag_result(ctx, &record, revision).await
}

async fn clone_record(
    ctx: &EventContext,
    source_id: &str,
    new_id: &str,
    expected_revision: i64,
    idempotency_key: &str,
) -> Result<DashboardActionResult, DashboardActionError> {
    if !valid_id(source_id) || !valid_id(new_id) || source_id == new_id || expected_revision <= 0 {
        return Err(DashboardActionError::Invalid(
            "invalid tag clone".to_owned(),
        ));
    }
    if let Some((target, revision)) = read_record(ctx, new_id).await?
        && !target.reply.deleted
        && target.created_by_idempotency_key.as_deref() == Some(idempotency_key)
    {
        let (target, revision) =
            retry_pending_alias(ctx, &tag_key(new_id), &target, revision).await?;
        return tag_result(ctx, &target, revision).await;
    }
    let Some((source, source_revision)) = read_record(ctx, source_id).await? else {
        return Err(DashboardActionError::NotFound);
    };
    if source.reply.deleted {
        return Err(DashboardActionError::NotFound);
    }
    if source_revision != expected_revision {
        return Err(DashboardActionError::PreconditionFailed {
            current: source_revision,
        });
    }
    let mut clone = source;
    clone.reply.id = new_id.to_owned();
    clone.reply.title = new_id.to_owned();
    clone.reply.deleted = false;
    clone.deleted_from_revision = None;
    clone.created_by_idempotency_key = Some(idempotency_key.to_owned());
    clone.last_update_idempotency_key = None;
    clone.alias_sync_pending = false;
    clone.alias_sync_idempotency_key = None;
    create_record(ctx, clone, idempotency_key).await
}

async fn delete_record(
    ctx: &EventContext,
    id: &str,
    expected_revision: i64,
    idempotency_key: &str,
) -> Result<DashboardActionResult, DashboardActionError> {
    if !valid_id(id) || expected_revision <= 0 {
        return Err(DashboardActionError::Invalid(
            "invalid tag deletion".to_owned(),
        ));
    }
    let key = tag_key(id);
    let Some((current, current_revision)) = read_record(ctx, id).await? else {
        return Err(DashboardActionError::NotFound);
    };
    if current.reply.deleted {
        if current.deleted_from_revision == Some(expected_revision)
            && current.last_update_idempotency_key.as_deref() == Some(idempotency_key)
        {
            let (_, revision) = retry_pending_alias(ctx, &key, &current, current_revision).await?;
            return Ok(DashboardActionResult {
                value: json!({
                    "tag": { "id": id, "deleted": true, "revision": revision }
                }),
                revision,
            });
        }
        return Err(DashboardActionError::NotFound);
    }
    if current_revision != expected_revision {
        if current.last_update_idempotency_key.as_deref() == Some(idempotency_key) {
            return Err(DashboardActionError::Invalid(
                "tag command alias was rejected".to_owned(),
            ));
        }
        return Err(DashboardActionError::PreconditionFailed {
            current: current_revision,
        });
    }

    let previous = current.clone();
    let needs_alias_sync = current.custom_command_alias || current.alias_sync_pending;
    let record = TagRecord {
        reply: SavedReply {
            id: id.to_owned(),
            title: String::new(),
            body: String::new(),
            deleted: true,
        },
        embed: None,
        article_id: None,
        custom_command_alias: current.custom_command_alias,
        alias_sync_pending: needs_alias_sync,
        alias_sync_idempotency_key: needs_alias_sync.then(|| idempotency_key.to_owned()),
        deleted_from_revision: Some(current_revision),
        created_by_idempotency_key: current.created_by_idempotency_key.clone(),
        last_update_idempotency_key: Some(idempotency_key.to_owned()),
    };
    let revision = save_record(ctx, &key, &record, Expect::Revision(current_revision)).await?;
    if needs_alias_sync {
        match sync_pending_alias(ctx, &key, &record, revision).await {
            Ok((_, revision)) => {
                return Ok(DashboardActionResult {
                    value: json!({
                        "tag": { "id": id, "deleted": true, "revision": revision }
                    }),
                    revision,
                });
            }
            Err(AliasSyncFailure::Unavailable) => {
                return Err(DashboardActionError::Unavailable);
            }
            Err(AliasSyncFailure::Storage(error)) => return Err(error),
            Err(AliasSyncFailure::Rejected) => {
                rollback_alias_change(ctx, &key, previous, revision, idempotency_key).await?;
                return Err(DashboardActionError::Invalid(
                    "tag command alias was rejected".to_owned(),
                ));
            }
        }
    }
    Ok(DashboardActionResult {
        value: json!({
            "tag": { "id": id, "deleted": true, "revision": revision }
        }),
        revision,
    })
}

async fn read_record(
    ctx: &EventContext,
    id: &str,
) -> Result<Option<(TagRecord, i64)>, DashboardActionError> {
    let key = tag_key(id);
    let Some(entry) = ctx.store().entry(&key).await.map_err(storage_error)? else {
        return Ok(None);
    };
    let revision = entry.revision;
    let record = decode_record(&key, entry)?;
    Ok(Some((record, revision)))
}

fn decode_record(key: &str, entry: Entry) -> Result<TagRecord, DashboardActionError> {
    let record: TagRecord = serde_json::from_value(entry.value).map_err(|error| {
        tracing::warn!(?error, key = %key, "could not decode a saved tag");
        DashboardActionError::Internal("tag storage is invalid".to_owned())
    })?;
    if key != tag_key(&record.reply.id) || !valid_record(&record) {
        return Err(DashboardActionError::Internal(
            "tag storage is invalid".to_owned(),
        ));
    }
    Ok(record)
}

async fn save_record(
    ctx: &EventContext,
    key: &str,
    record: &TagRecord,
    expect: Expect,
) -> Result<i64, DashboardActionError> {
    let value = serde_json::to_vec(record)
        .map_err(|_| DashboardActionError::Internal("tag could not be encoded".to_owned()))?;
    if !valid_record(record) || value.len() > MAX_VALUE_BYTES {
        return Err(DashboardActionError::Invalid(
            "tag exceeds storage limits".to_owned(),
        ));
    }
    let ttl = record.reply.deleted.then_some(TOMBSTONE_TTL);
    ctx.store()
        .replace(key, record, expect, ttl)
        .await
        .map_err(storage_error)
}

async fn tag_result(
    ctx: &EventContext,
    record: &TagRecord,
    revision: i64,
) -> Result<DashboardActionResult, DashboardActionError> {
    Ok(DashboardActionResult {
        value: json!({
            "tag": tag_value(ctx, record, revision).await?,
        }),
        revision,
    })
}

async fn tag_value(
    ctx: &EventContext,
    record: &TagRecord,
    revision: i64,
) -> Result<Value, DashboardActionError> {
    let article_title = match record.article_id.as_deref() {
        Some(id) => super::knowledge::public_article(ctx, id)
            .await?
            .map(|article| article.title),
        None => None,
    };
    Ok(json!({
        "id": record.reply.id,
        "type": tag_type(record),
        "body": record.reply.body,
        "embed": record.embed,
        "article_id": record.article_id,
        "article_title": article_title,
        "custom_command_alias": record.custom_command_alias,
        "alias_sync_pending": record.alias_sync_pending,
        "revision": revision,
    }))
}

fn tag_type(record: &TagRecord) -> &'static str {
    match (!record.reply.body.trim().is_empty(), record.embed.is_some()) {
        (true, true) => "Embed + Text",
        (false, true) => "Embed",
        _ => "Text",
    }
}

fn gallery_payload(record: &TagRecord) -> Value {
    json!({
        "content": record.reply.body,
        "use_embed": record.embed.is_some(),
        "embed": record.embed,
    })
}

#[allow(
    clippy::too_many_arguments,
    reason = "this private constructor maps every persisted tag field explicitly"
)]
fn new_record(
    id: String,
    body: String,
    embed: Option<TagEmbed>,
    article_id: Option<String>,
    custom_command_alias: bool,
    alias_sync_pending: bool,
    alias_sync_idempotency_key: Option<String>,
    created_by_idempotency_key: Option<String>,
    last_update_idempotency_key: Option<String>,
) -> TagRecord {
    TagRecord {
        reply: SavedReply {
            title: id.clone(),
            id,
            body,
            deleted: false,
        },
        embed,
        article_id,
        custom_command_alias,
        alias_sync_pending,
        alias_sync_idempotency_key,
        deleted_from_revision: None,
        created_by_idempotency_key,
        last_update_idempotency_key,
    }
}

fn matches_query(record: &TagRecord, query: Option<&str>) -> Result<bool, DashboardActionError> {
    let Some(query) = query else {
        return Ok(true);
    };
    let embed = serde_json::to_string(&record.embed).map_err(|error| {
        tracing::warn!(?error, id = %record.reply.id, "could not search a saved tag");
        DashboardActionError::Internal("tag search is unavailable".to_owned())
    })?;
    Ok(record.reply.id.to_lowercase().contains(query)
        || record.reply.body.to_lowercase().contains(query)
        || record
            .article_id
            .as_deref()
            .is_some_and(|id| id.to_lowercase().contains(query))
        || embed.to_lowercase().contains(query))
}

fn same_content(left: &TagRecord, right: &TagRecord) -> bool {
    left.reply.id == right.reply.id
        && left.reply.title == right.reply.title
        && left.reply.body == right.reply.body
        && left.embed == right.embed
        && left.article_id == right.article_id
        && left.custom_command_alias == right.custom_command_alias
        && left.reply.deleted == right.reply.deleted
}

fn valid_record(record: &TagRecord) -> bool {
    record
        .created_by_idempotency_key
        .as_deref()
        .is_none_or(valid_idempotency_key)
        && record
            .last_update_idempotency_key
            .as_deref()
            .is_none_or(valid_idempotency_key)
        && record
            .alias_sync_idempotency_key
            .as_deref()
            .is_none_or(valid_idempotency_key)
        && record.alias_sync_pending == record.alias_sync_idempotency_key.is_some()
        && if record.reply.deleted {
            valid_id(&record.reply.id)
                && record.reply.title.is_empty()
                && record.reply.body.is_empty()
                && record.embed.is_none()
                && record.article_id.is_none()
                && record
                    .deleted_from_revision
                    .is_some_and(|revision| revision > 0)
        } else {
            valid_id(&record.reply.id)
                && record.reply.title == record.reply.id
                && valid_body(&record.reply.body)
                && valid_embed(record.embed.as_ref())
                && valid_article_id_option(record.article_id.as_deref())
                && (!record.reply.body.trim().is_empty()
                    || record.embed.is_some()
                    || record.article_id.is_some())
                && record.deleted_from_revision.is_none()
        }
}

fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.chars().count() <= MAX_ID_CHARS
        && id.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'_' | b'-')
        })
}

fn valid_article_id_option(id: Option<&str>) -> bool {
    id.is_none_or(|id| {
        (1..=32).contains(&id.len())
            && id
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
    })
}

fn valid_body(body: &str) -> bool {
    body.chars().count() <= MAX_BODY_CHARS
        && !body.chars().any(|character| {
            character == '\0' || character.is_control() && !matches!(character, '\n' | '\r' | '\t')
        })
}

fn valid_embed(embed: Option<&TagEmbed>) -> bool {
    let Some(embed) = embed else {
        return true;
    };
    let strings = [
        (embed.title.as_deref(), 256),
        (embed.description.as_deref(), 4_096),
        (embed.author_name.as_deref(), 256),
        (embed.footer_text.as_deref(), 2_048),
    ];
    let urls = [
        embed.author_url.as_deref(),
        embed.author_icon_url.as_deref(),
        embed.thumbnail_url.as_deref(),
        embed.image_url.as_deref(),
        embed.footer_icon_url.as_deref(),
    ];
    let total_text_chars = strings
        .iter()
        .filter_map(|(value, _)| *value)
        .map(|value| value.chars().count())
        .sum::<usize>()
        + embed
            .fields
            .iter()
            .map(|field| field.name.chars().count() + field.value.chars().count())
            .sum::<usize>();
    embed.color.as_deref().is_none_or(valid_color)
        && strings
            .iter()
            .all(|(value, max)| value.is_none_or(|value| valid_text(value, *max)))
        && urls.iter().all(|url| url.is_none_or(valid_url))
        && embed.fields.len() <= MAX_EMBED_FIELDS
        && embed
            .fields
            .iter()
            .all(|field| valid_text(&field.name, 256) && valid_text(&field.value, 1_024))
        && total_text_chars <= MAX_EMBED_CHARS
        // The renderer builds the embed before adding author, footer and images, so those alone cannot render.
        && (embed.title.is_some() || embed.description.is_some() || !embed.fields.is_empty())
        && serde_json::to_vec(embed).is_ok_and(|value| value.len() <= MAX_EMBED_CHARS)
}

fn valid_text(value: &str, max_chars: usize) -> bool {
    !value.trim().is_empty()
        && value.chars().count() <= max_chars
        && !value.chars().any(|character| {
            character == '\0' || character.is_control() && !matches!(character, '\n' | '\r' | '\t')
        })
}

fn valid_url(value: &str) -> bool {
    value.len() <= 2_048
        && !value.chars().any(char::is_whitespace)
        && !value.chars().any(char::is_control)
        && (value
            .strip_prefix("https://")
            .is_some_and(|host| !host.is_empty())
            || value
                .strip_prefix("http://")
                .is_some_and(|host| !host.is_empty()))
}

fn valid_color(value: &str) -> bool {
    value.len() == 7
        && value.starts_with('#')
        && value[1..].bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn valid_idempotency_key(key: &str) -> bool {
    (1..=64).contains(&key.len())
        && key
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
}

fn tag_key(id: &str) -> String {
    format!("{PREFIX}{id}")
}

fn parse_data<T: for<'de> Deserialize<'de>>(data: &Value) -> Result<T, DashboardActionError> {
    serde_json::from_value(data.clone())
        .map_err(|error| DashboardActionError::Invalid(error.to_string()))
}

fn storage_error(error: StorageError) -> DashboardActionError {
    match error {
        StorageError::Conflict => DashboardActionError::Conflict,
        StorageError::TooLarge { .. } => {
            DashboardActionError::Invalid("tag exceeds storage limits".to_owned())
        }
        error => {
            tracing::warn!(?error, "could not access saved tags");
            DashboardActionError::Internal("tag storage is unavailable".to_owned())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{TagEmbed, TagEmbedField, TagRecord, valid_embed, valid_id, valid_record};

    #[test]
    fn tag_validation_matches_dashboard_limits() {
        assert!(valid_id("refund_policy"));
        assert!(valid_id("tag-1"));
        assert!(!valid_id("Refund"));
        assert!(!valid_id("a tag"));
        assert!(!valid_id("12345678901234567"));

        let mut embed = Some(TagEmbed::default());
        assert!(!valid_embed(embed.as_ref()));
        embed = Some(TagEmbed {
            title: Some("Policy".to_owned()),
            color: Some("#336699".to_owned()),
            fields: vec![TagEmbedField {
                name: "Eligibility".to_owned(),
                value: "Within 30 days".to_owned(),
                inline: false,
            }],
            ..TagEmbed::default()
        });
        assert!(valid_embed(embed.as_ref()));

        let tag = TagRecord {
            reply: super::SavedReply {
                id: "policy".to_owned(),
                title: "policy".to_owned(),
                body: String::new(),
                deleted: false,
            },
            embed,
            article_id: None,
            custom_command_alias: false,
            alias_sync_pending: false,
            alias_sync_idempotency_key: None,
            deleted_from_revision: None,
            created_by_idempotency_key: Some("request_1".to_owned()),
            last_update_idempotency_key: None,
        };
        assert!(valid_record(&tag));
    }
}

#[cfg(test)]
mod embed_render_tests {
    use super::{TagEmbed, TagEmbedField, tag_embed, valid_embed};

    fn accepted_embeds() -> Vec<TagEmbed> {
        vec![
            TagEmbed {
                title: Some("Policy".to_owned()),
                ..TagEmbed::default()
            },
            TagEmbed {
                description: Some("Body".to_owned()),
                ..TagEmbed::default()
            },
            TagEmbed {
                fields: vec![TagEmbedField {
                    name: "Name".to_owned(),
                    value: "Value".to_owned(),
                    inline: false,
                }],
                ..TagEmbed::default()
            },
        ]
    }

    #[test]
    fn every_embed_the_validator_accepts_renders() {
        let candidates = [
            TagEmbed {
                author_name: Some("Author".to_owned()),
                image_url: Some("https://example.com/image.png".to_owned()),
                ..TagEmbed::default()
            },
            TagEmbed {
                footer_text: Some("Footer".to_owned()),
                thumbnail_url: Some("https://example.com/thumb.png".to_owned()),
                ..TagEmbed::default()
            },
        ];
        for embed in candidates {
            assert!(
                !valid_embed(Some(&embed)),
                "validator must reject {embed:?}"
            );
        }
        for embed in accepted_embeds() {
            assert!(valid_embed(Some(&embed)));
            assert!(
                tag_embed(&embed, 0).is_ok(),
                "renderer must accept {embed:?}"
            );
        }
    }

    #[test]
    fn validator_acceptance_implies_render_success() {
        let author_and_image = TagEmbed {
            author_name: Some("Author".to_owned()),
            image_url: Some("https://example.com/image.png".to_owned()),
            ..TagEmbed::default()
        };
        assert!(!valid_embed(Some(&author_and_image)) || tag_embed(&author_and_image, 0).is_ok());
    }
}
