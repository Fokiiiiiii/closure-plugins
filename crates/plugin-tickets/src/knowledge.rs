use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use closure_core::action::{Action, MentionPolicy};
use closure_core::error::{DiscordError, ModuleError};
use closure_core::event::{ChannelId, UserId};
use closure_core::module::EventContext;
use closure_core::storage::{Entry, Expect, MAX_VALUE_BYTES, StorageError};
use closure_sdk::{
    Args, CommandArgs, CommandCx, DirectMessage, EmbedBuilder, InteractionReply, JobCx, Locale,
    Sent, UserError,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use twilight_model::channel::message::Embed;

const PREFIX: &str = "ticket-knowledge:";
const TOMBSTONE_TTL: std::time::Duration = std::time::Duration::from_hours(720);
const PAGE_SIZE: usize = 8;
const ARTICLE_SCAN_BATCH: usize = 1000;
const DASHBOARD_PAGE_SIZE: usize = 25;
pub(crate) const KNOWLEDGE_SEARCH_JOB: &str = "knowledge-search";
pub(crate) const KNOWLEDGE_SUGGESTION_JOB: &str = "knowledge-suggestion";
const SUGGESTION_PREFIX: &str = "ticket-knowledge-suggestion:";
const SUGGESTION_JOB_DELAY: Duration = Duration::from_millis(250);
const SUGGESTION_STATE_TTL: Duration = Duration::from_hours(24);
const SUGGESTION_BATCHES_PER_JOB: usize = 10;
const SEARCH_PREFIX: &str = "ticket-knowledge-search:";
const SEARCH_REQUEST_PREFIX: &str = "ticket-knowledge-search-request:";
const SEARCH_TTL: Duration = Duration::from_hours(24);
const SEARCH_EMPTY_RESULT_TTL: Duration = Duration::from_secs(60);
const SEARCH_JOB_DELAY: Duration = Duration::from_millis(250);
const PUBLIC_CURSOR_PREFIX: &str = "ticket-knowledge-public-cursor:";
const PUBLIC_CURSOR_TTL: Duration = Duration::from_mins(15);
static PUBLIC_CURSOR_SEQUENCE: AtomicU64 = AtomicU64::new(0);
const MAX_ID_CHARS: usize = 32;
const MAX_TITLE_CHARS: usize = 100;
const MAX_BODY_CHARS: usize = 4096;
const MAX_CATEGORY_CHARS: usize = 100;
const MAX_KEYWORD_CHARS: usize = 100;
const MAX_QUERY_CHARS: usize = 100;
const MAX_TICKET_SUGGESTIONS: usize = 3;

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
struct Article {
    id: String,
    title: String,
    body: String,
    #[serde(default)]
    categories: Vec<String>,
    #[serde(default)]
    keywords: Vec<String>,
    // Legacy records have no publication flag, so they remain private drafts.
    #[serde(default)]
    published: bool,
    #[serde(default)]
    custom_embed: Option<Value>,
    #[serde(default)]
    deleted: bool,
    #[serde(default)]
    deleted_from_revision: Option<i64>,
    #[serde(default)]
    created_by_idempotency_key: Option<String>,
    #[serde(default)]
    last_update_idempotency_key: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct PublicKnowledgeArticle {
    pub(crate) id: String,
    pub(crate) title: String,
    pub(crate) body: String,
    pub(crate) categories: Vec<String>,
    pub(crate) keywords: Vec<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct KnowledgeSearchResult {
    id: String,
    title: String,
}

// shortcut: preserve the serialized flag shape; group flags only with a storage migration.
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct KnowledgeSearchState {
    owner: u64,
    token: String,
    query: String,
    locale: Locale,
    after_key: String,
    page: usize,
    matched: usize,
    results: Vec<KnowledgeSearchResult>,
    expires_at: i64,
    complete: bool,
    pages_known: bool,
    notified: bool,
    dm_closed: bool,
    cancelled: bool,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct KnowledgeSearchRequest {
    owner: u64,
    query: String,
    page: usize,
    token: String,
    expires_at: i64,
}

struct SearchLookup {
    request_revision: Option<i64>,
    state_revision: Option<i64>,
    state: Option<KnowledgeSearchState>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct KnowledgeSearchJob {
    owner: u64,
    token: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct KnowledgeSuggestionCandidate {
    id: String,
    score: usize,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct KnowledgeSuggestionState {
    guild_id: u64,
    channel_id: u64,
    ticket_number: i64,
    panel_id: Option<String>,
    token: String,
    terms: Vec<String>,
    after_key: String,
    candidates: Vec<KnowledgeSuggestionCandidate>,
    expires_at: i64,
    complete: bool,
    notified: bool,
    cancelled: bool,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct KnowledgeSuggestionJob {
    channel_id: u64,
    token: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct PublicCursorState {
    token: String,
    query: Option<String>,
    category: Option<String>,
    after_key: String,
    expires_at: i64,
}

#[derive(CommandArgs)]
pub(crate) struct SearchArgs {
    /// Search article IDs, titles, and content
    #[arg(max_length = 100)]
    query: String,
    /// The result page, starting at 1
    page: Option<i64>,
}

#[derive(CommandArgs)]
pub(crate) struct ViewArgs {
    /// The article ID
    #[arg(max_length = 32)]
    id: String,
}

#[derive(CommandArgs)]
pub(crate) struct AddArgs {
    /// A unique lowercase ID using letters, numbers, or hyphens
    #[arg(max_length = 32)]
    id: String,
    /// The article title
    #[arg(max_length = 100)]
    title: String,
    /// The article content
    #[arg(max_length = 1700)]
    body: String,
}

#[derive(CommandArgs)]
pub(crate) struct EditArgs {
    /// The article ID
    #[arg(max_length = 32)]
    id: String,
    /// The new article title
    #[arg(max_length = 100)]
    title: String,
    /// The new article content
    #[arg(max_length = 1700)]
    body: String,
}

#[derive(CommandArgs)]
pub(crate) struct DeleteArgs {
    /// The article ID
    #[arg(max_length = 32)]
    id: String,
}

pub(crate) async fn search(cx: CommandCx, args: SearchArgs) -> Result<(), UserError> {
    let query = args.query.trim().to_lowercase();
    if query.is_empty() || query.chars().count() > MAX_QUERY_CHARS {
        return Err(UserError::message("tickets-knowledge-invalid-query"));
    }
    let Some(page) = args
        .page
        .unwrap_or(1)
        .try_into()
        .ok()
        .and_then(|page: usize| page.checked_sub(1))
        .filter(|page: &usize| page.checked_mul(PAGE_SIZE).is_some())
    else {
        return Err(UserError::message("tickets-knowledge-page-not-found"));
    };
    cx.defer(true).await?;
    let ctx = cx.ctx();
    let owner = cx.user().id.get();
    let now = ctx.now();
    let lookup = load_search_lookup(ctx, owner, &query, page, now).await?;

    if let Some(mut state) = lookup.state {
        let state_key = knowledge_search_state_key(owner, &state.token);
        let (Some(state_revision), Some(request_revision)) =
            (lookup.state_revision, lookup.request_revision)
        else {
            return start_search(&cx, ctx, owner, &query, page, lookup.request_revision, now).await;
        };
        if state.cancelled || state.expires_at <= now {
            return restart_search(
                &cx,
                ctx,
                owner,
                &query,
                page,
                &state_key,
                state,
                state_revision,
                request_revision,
                now,
            )
            .await;
        }
        if state.complete {
            if state.results.is_empty() {
                return reply_empty_search(&cx, &state).await;
            }
            if refresh_search_results(ctx, &mut state).await? {
                state.notified = true;
                if let Err(error) =
                    persist_search_state(ctx, &state_key, &state, Expect::Revision(state_revision))
                        .await
                {
                    tracing::warn!(
                        ?error,
                        owner,
                        "could not mark cached knowledge results as read"
                    );
                }
                return reply_search_results(&cx, &state).await;
            }
            return restart_search(
                &cx,
                ctx,
                owner,
                &query,
                page,
                &state_key,
                state,
                state_revision,
                request_revision,
                now,
            )
            .await;
        }
        schedule_search_job(ctx, &state, SEARCH_JOB_DELAY).await?;
        let request_key = knowledge_search_request_key(owner, &query, page);
        let request = KnowledgeSearchRequest {
            owner,
            query: query.clone(),
            page,
            token: state.token.clone(),
            expires_at: state.expires_at,
        };
        match refresh_search_request(ctx, &request_key, &request, request_revision, &state).await {
            Ok(_) | Err(StorageError::Conflict) => {}
            Err(error) => return Err(storage_error(&error)),
        }
        return reply_search_queued(&cx, &query, page + 1).await;
    }

    start_search(&cx, ctx, owner, &query, page, lookup.request_revision, now).await
}

// shortcut: keep the persisted search worker flow together; split a phase if it grows further.
#[allow(clippy::too_many_lines)]
pub(crate) async fn search_job(cx: JobCx) -> Result<(), ModuleError> {
    let job: KnowledgeSearchJob = cx.payload()?;
    let ctx = cx.ctx();
    let state_key = knowledge_search_state_key(job.owner, &job.token);
    let Some(state_entry) = ctx
        .store()
        .entry(&state_key)
        .await
        .map_err(|error| storage_module_error(&error))?
    else {
        return Ok(());
    };
    let mut state: KnowledgeSearchState = serde_json::from_value(state_entry.value.clone())
        .map_err(|error| ModuleError::other(error.to_string()))?;
    let now = ctx.now();
    if !valid_search_state(&state, job.owner, &job.token, now) || state.cancelled || state.notified
    {
        return Ok(());
    }
    let request_key = knowledge_search_request_key(state.owner, &state.query, state.page);
    let Some(request_entry) = ctx
        .store()
        .entry(&request_key)
        .await
        .map_err(|error| storage_module_error(&error))?
    else {
        return Ok(());
    };
    let request: KnowledgeSearchRequest = serde_json::from_value(request_entry.value.clone())
        .map_err(|error| ModuleError::other(error.to_string()))?;
    if request.owner != state.owner
        || request.token != state.token
        || request.query != state.query
        || request.page != state.page
        || request.expires_at <= now
    {
        return Ok(());
    }

    let mut revision = state_entry.revision;
    let mut request_revision = request_entry.revision;
    if !state.complete {
        let scan_complete = scan_search_batch(ctx, &mut state)
            .await
            .map_err(|_| ModuleError::other("knowledge search scan failed"))?;
        if state.results.len() == PAGE_SIZE {
            state.complete = true;
            state.pages_known = scan_complete;
        } else if scan_complete {
            state.complete = true;
            state.pages_known = true;
        }
        state.expires_at = search_expiry(now, state.complete && state.results.is_empty())
            .map_err(|_| ModuleError::other("knowledge search expiry was out of range"))?;
        revision =
            match persist_search_state(ctx, &state_key, &state, Expect::Revision(revision)).await {
                Ok(revision) => revision,
                Err(StorageError::Conflict) => return Ok(()),
                Err(error) => return Err(storage_module_error(&error)),
            };
        let updated_request = KnowledgeSearchRequest {
            expires_at: state.expires_at,
            ..request.clone()
        };
        request_revision = match refresh_search_request(
            ctx,
            &request_key,
            &updated_request,
            request_revision,
            &state,
        )
        .await
        {
            Ok(revision) => revision,
            Err(StorageError::Conflict) => {
                schedule_search_job(ctx, &state, SEARCH_JOB_DELAY)
                    .await
                    .map_err(|_| {
                        ModuleError::other("knowledge search job could not be scheduled")
                    })?;
                return Ok(());
            }
            Err(error) => return Err(storage_module_error(&error)),
        };
    }

    if state.complete {
        finish_search_job(
            &cx,
            ctx,
            &state_key,
            state,
            revision,
            &request_key,
            &request,
            request_revision,
        )
        .await
    } else {
        schedule_search_job(ctx, &state, SEARCH_JOB_DELAY)
            .await
            .map_err(|_| ModuleError::other("knowledge search job could not be scheduled"))
    }
}

pub(crate) async fn view(cx: CommandCx, args: ViewArgs) -> Result<(), UserError> {
    if !valid_id(&args.id) {
        return Err(UserError::message("tickets-knowledge-invalid-id"));
    }
    cx.defer(true).await?;
    let Some((article, _)) = read_article(cx.ctx(), &args.id).await? else {
        return Err(UserError::message("tickets-knowledge-not-found"));
    };
    if !article.published {
        return Err(UserError::message("tickets-knowledge-not-found"));
    }
    let heading = cx.t(
        "tickets-knowledge-view",
        &Args::new()
            .with("id", article.id.clone())
            .with("title", article.title.clone()),
    );
    for embed in article_view_embeds(&heading, &article.body)? {
        cx.reply_with(InteractionReply {
            embeds: vec![embed],
            ephemeral: true,
            ..InteractionReply::default()
        })
        .await?;
    }
    Ok(())
}

// Discord caps an embed description at 4096 characters; a longer body continues in more embeds.
const ARTICLE_DESCRIPTION_CHARS: usize = 4096;

fn article_view_embeds(heading: &str, body: &str) -> Result<Vec<Embed>, UserError> {
    let characters: Vec<char> = body.chars().collect();
    let mut parts: Vec<String> = characters
        .chunks(ARTICLE_DESCRIPTION_CHARS)
        .map(|part| part.iter().collect())
        .collect();
    if parts.is_empty() {
        parts.push(String::new());
    }
    parts
        .into_iter()
        .enumerate()
        .map(|(index, part)| {
            let mut builder = EmbedBuilder::new();
            if index == 0 {
                builder = builder.title(heading);
            }
            if !part.is_empty() {
                builder = builder.description(part);
            }
            builder
                .build()
                .map_err(|_| UserError::message("tickets-failed"))
        })
        .collect()
}

#[cfg(test)]
mod article_view_tests {
    use super::*;

    fn description_text(embeds: &[Embed]) -> String {
        embeds
            .iter()
            .filter_map(|embed| embed.description.as_deref())
            .collect()
    }

    #[test]
    fn full_length_article_is_shown_in_one_embed_with_its_title() {
        let body = "b".repeat(4096);
        let Ok(embeds) = article_view_embeds("[id] title", &body) else {
            panic!("a 4096-character article must render");
        };
        assert_eq!(embeds.len(), 1);
        assert_eq!(embeds[0].title.as_deref(), Some("[id] title"));
        assert_eq!(description_text(&embeds), body);
    }

    #[test]
    fn body_beyond_one_description_continues_without_loss() {
        let body = format!("{}{}", "a".repeat(4096), "z".repeat(10));
        let Ok(embeds) = article_view_embeds("[id] title", &body) else {
            panic!("a longer article must still render");
        };
        assert_eq!(embeds.len(), 2);
        assert!(
            embeds
                .iter()
                .all(|embed| embed.description.as_deref().map_or(0, str::len) <= 4096)
        );
        assert_eq!(description_text(&embeds), body);
    }
}

async fn record_knowledge_success(
    cx: &CommandCx,
    ctx: &EventContext,
    action: crate::audit::AuditAction,
    key_prefix: &str,
) {
    let dedupe_key = format!("{key_prefix}-{}", cx.interaction_id().get());
    if crate::audit::record_event(
        ctx,
        cx.user().id.get(),
        action,
        crate::audit::AuditOutcome::Success,
        &dedupe_key,
    )
    .await
    .is_err()
    {
        tracing::warn!(guild = %ctx.guild_id, "could not record knowledge article audit event");
    }
}

pub(crate) async fn schedule_ticket_suggestions(
    ctx: &EventContext,
    channel: ChannelId,
    ticket_number: i64,
    panel_id: Option<&str>,
    answers: &[(String, String)],
) -> Result<(), ModuleError> {
    let terms = suggestion_terms(answers);
    if terms.is_empty() {
        return Ok(());
    }
    if ticket_number <= 0
        || panel_id.is_some_and(|id| !valid_id(id))
        || ctx.guild_id.get() == 0
        || channel.get() == 0
    {
        return Err(ModuleError::other(
            "ticket knowledge suggestions could not be scheduled",
        ));
    }
    let now = ctx.now();
    let query = terms.join(" ");
    let token = knowledge_search_token(
        ctx.guild_id.get(),
        &query,
        PREFIX,
        0,
        now,
        &channel.get().to_string(),
    );
    let key = suggestion_state_key(channel.get());
    let mut state = KnowledgeSuggestionState {
        guild_id: ctx.guild_id.get(),
        channel_id: channel.get(),
        ticket_number,
        panel_id: panel_id.map(str::to_owned),
        token,
        terms,
        after_key: PREFIX.to_owned(),
        candidates: Vec::new(),
        expires_at: now.saturating_add(SUGGESTION_STATE_TTL.as_secs().cast_signed()),
        complete: false,
        notified: false,
        cancelled: false,
    };
    let expect = match ctx
        .store()
        .entry(&key)
        .await
        .map_err(|error| storage_module_error(&error))?
    {
        None => Expect::Missing,
        Some(entry) => {
            let previous: KnowledgeSuggestionState = serde_json::from_value(entry.value)
                .map_err(|_| ModuleError::other("ticket knowledge suggestion state is invalid"))?;
            if previous.notified {
                return Ok(());
            }
            if previous.expires_at > now {
                if previous.guild_id != state.guild_id
                    || previous.channel_id != state.channel_id
                    || previous.ticket_number != state.ticket_number
                    || previous.panel_id != state.panel_id
                    || previous.terms != state.terms
                    || !valid_suggestion_state(&previous, now)
                {
                    return Ok(());
                }
                state = previous;
            }
            Expect::Revision(entry.revision)
        }
    };
    match ctx
        .store()
        .replace(&key, &state, expect, Some(SUGGESTION_STATE_TTL))
        .await
    {
        Ok(_) => {}
        Err(StorageError::Conflict) => return Ok(()),
        Err(error) => return Err(storage_module_error(&error)),
    }
    schedule_suggestion_job(ctx, &state, Duration::ZERO).await
}

pub(crate) async fn cancel_ticket_suggestions(
    ctx: &EventContext,
    channel: ChannelId,
    ticket_number: i64,
) -> Result<(), ModuleError> {
    let key = suggestion_state_key(channel.get());
    let Some(entry) = ctx
        .store()
        .entry(&key)
        .await
        .map_err(|error| storage_module_error(&error))?
    else {
        return Ok(());
    };
    let mut state: KnowledgeSuggestionState = serde_json::from_value(entry.value)
        .map_err(|_| ModuleError::other("ticket knowledge suggestion state is invalid"))?;
    if state.guild_id != ctx.guild_id.get()
        || state.channel_id != channel.get()
        || state.ticket_number != ticket_number
    {
        return Ok(());
    }
    state.notified = true;
    state.complete = true;
    state.cancelled = true;
    state.terms.clear();
    state.candidates.clear();
    PREFIX.clone_into(&mut state.after_key);
    state.expires_at = ctx
        .now()
        .saturating_add(SUGGESTION_STATE_TTL.as_secs().cast_signed());
    match ctx
        .store()
        .replace(
            &key,
            &state,
            Expect::Revision(entry.revision),
            Some(SUGGESTION_STATE_TTL),
        )
        .await
    {
        Ok(_) | Err(StorageError::Conflict) => Ok(()),
        Err(error) => Err(storage_module_error(&error)),
    }
}

pub(crate) async fn suggestion_job(cx: JobCx) -> Result<(), ModuleError> {
    let job: KnowledgeSuggestionJob = cx.payload()?;
    let ctx = cx.ctx();
    if job.channel_id == 0 || ctx.guild_id.get() == 0 {
        return Ok(());
    }
    let state_key = suggestion_state_key(job.channel_id);
    let Some(entry) = ctx
        .store()
        .entry(&state_key)
        .await
        .map_err(|error| storage_module_error(&error))?
    else {
        return Ok(());
    };
    let mut state: KnowledgeSuggestionState = serde_json::from_value(entry.value.clone())
        .map_err(|_| ModuleError::other("ticket knowledge suggestion state is invalid"))?;
    let now = ctx.now();
    if state.guild_id != ctx.guild_id.get()
        || state.channel_id != job.channel_id
        || state.token != job.token
        || !valid_suggestion_state(&state, now)
        || state.notified
    {
        return Ok(());
    }
    let mut complete = state.complete;
    for _ in 0..SUGGESTION_BATCHES_PER_JOB {
        if complete {
            break;
        }
        complete = scan_suggestion_batch(ctx, &mut state).await?;
    }
    state.complete = complete;
    state.expires_at = now.saturating_add(SUGGESTION_STATE_TTL.as_secs().cast_signed());
    let revision = match ctx
        .store()
        .replace(
            &state_key,
            &state,
            Expect::Revision(entry.revision),
            Some(SUGGESTION_STATE_TTL),
        )
        .await
    {
        Ok(revision) => revision,
        Err(StorageError::Conflict) => return Ok(()),
        Err(error) => return Err(storage_module_error(&error)),
    };
    if state.complete {
        finish_suggestion_job(&cx, ctx, &state_key, state, revision).await
    } else {
        schedule_suggestion_job(ctx, &state, SUGGESTION_JOB_DELAY).await
    }
}

async fn scan_suggestion_batch(
    ctx: &EventContext,
    state: &mut KnowledgeSuggestionState,
) -> Result<bool, ModuleError> {
    let entries = ctx
        .store()
        .list_after(PREFIX, &state.after_key, ARTICLE_SCAN_BATCH)
        .await
        .map_err(|error| storage_module_error(&error))?;
    let count = entries.len();
    for (key, entry) in entries {
        state.after_key.clone_from(&key);
        let article = article_from_entry(&key, entry)
            .map_err(|_| ModuleError::other("knowledge suggestions could not be loaded"))?;
        if article.deleted || !article.published {
            continue;
        }
        let score = suggestion_score(&article, &state.terms);
        if score == 0 {
            continue;
        }
        if let Some(candidate) = state
            .candidates
            .iter_mut()
            .find(|candidate| candidate.id == article.id)
        {
            candidate.score = score;
        } else {
            state.candidates.push(KnowledgeSuggestionCandidate {
                id: article.id,
                score,
            });
        }
        state.candidates.sort_by(|left, right| {
            right
                .score
                .cmp(&left.score)
                .then_with(|| left.id.cmp(&right.id))
        });
        state.candidates.truncate(MAX_TICKET_SUGGESTIONS);
    }
    Ok(count < ARTICLE_SCAN_BATCH)
}

async fn finish_suggestion_job(
    cx: &JobCx,
    ctx: &EventContext,
    state_key: &str,
    mut state: KnowledgeSuggestionState,
    revision: i64,
) -> Result<(), ModuleError> {
    if !ticket_is_open_for_suggestions(ctx, &state).await? {
        let _ = mark_suggestion_notified(ctx, state_key, &mut state, revision).await?;
        return Ok(());
    }
    let mut embeds = Vec::with_capacity(state.candidates.len() * 2);
    for candidate in &state.candidates {
        let Some((article, _)) = read_article(ctx, &candidate.id)
            .await
            .map_err(|_| ModuleError::other("knowledge suggestions could not be validated"))?
        else {
            continue;
        };
        if !article.published || suggestion_score(&article, &state.terms) == 0 {
            continue;
        }
        embeds.extend(article_embeds(&article)?);
    }
    if embeds.is_empty() {
        let _ = mark_suggestion_notified(ctx, state_key, &mut state, revision).await?;
        return Ok(());
    }
    if !mark_suggestion_notified(ctx, state_key, &mut state, revision).await? {
        return Ok(());
    }
    if !ticket_is_open_for_suggestions(ctx, &state).await? {
        return Ok(());
    }
    for batch in embeds.chunks(10) {
        if cx
            .api()
            .execute(Action::SendMessage {
                channel_id: ChannelId::new(state.channel_id),
                content: None,
                embeds: batch.to_vec(),
                reply_to: None,
                mentions: MentionPolicy::None,
            })
            .await
            .is_err()
        {
            tracing::warn!(
                guild = %ctx.guild_id,
                channel_id = state.channel_id,
                "ticket knowledge suggestions could not be sent"
            );
            break;
        }
    }
    Ok(())
}

async fn ticket_is_open_for_suggestions(
    ctx: &EventContext,
    state: &KnowledgeSuggestionState,
) -> Result<bool, ModuleError> {
    if ctx.guild_id.get() != state.guild_id {
        return Ok(false);
    }
    let key = format!("ticket:{}", state.channel_id);
    let Some(entry) = ctx
        .store()
        .entry(&key)
        .await
        .map_err(|error| storage_module_error(&error))?
    else {
        return Ok(false);
    };
    let ticket: Value = entry.value;
    let open = ticket.get("closed").and_then(Value::as_bool) == Some(false);
    let same_number = ticket.get("number").and_then(Value::as_i64) == Some(state.ticket_number);
    let panel_id = ticket
        .get("panel")
        .and_then(Value::as_object)
        .and_then(|panel| panel.get("id"))
        .and_then(Value::as_str);
    Ok(open && same_number && panel_id == state.panel_id.as_deref())
}

async fn mark_suggestion_notified(
    ctx: &EventContext,
    state_key: &str,
    state: &mut KnowledgeSuggestionState,
    revision: i64,
) -> Result<bool, ModuleError> {
    state.notified = true;
    state.complete = true;
    state.cancelled = true;
    state.terms.clear();
    state.candidates.clear();
    PREFIX.clone_into(&mut state.after_key);
    state.expires_at = ctx
        .now()
        .saturating_add(SUGGESTION_STATE_TTL.as_secs().cast_signed());
    match ctx
        .store()
        .replace(
            state_key,
            state,
            Expect::Revision(revision),
            Some(SUGGESTION_STATE_TTL),
        )
        .await
    {
        Ok(_) => Ok(true),
        Err(StorageError::Conflict) => Ok(false),
        Err(error) => Err(storage_module_error(&error)),
    }
}

async fn schedule_suggestion_job(
    ctx: &EventContext,
    state: &KnowledgeSuggestionState,
    delay: Duration,
) -> Result<(), ModuleError> {
    let payload = KnowledgeSuggestionJob {
        channel_id: state.channel_id,
        token: state.token.clone(),
    };
    let key = suggestion_job_key(state);
    ctx.scheduler()
        .job(KNOWLEDGE_SUGGESTION_JOB, &payload)
        .map_err(|_| ModuleError::other("ticket knowledge suggestion job could not be created"))?
        .key(&key)
        .if_absent()
        .after(delay)
        .await
        .map_err(|_| ModuleError::other("ticket knowledge suggestion job could not be scheduled"))
}

fn suggestion_state_key(channel_id: u64) -> String {
    format!("{SUGGESTION_PREFIX}{channel_id}")
}

fn suggestion_job_key(state: &KnowledgeSuggestionState) -> String {
    format!(
        "{SUGGESTION_PREFIX}job:{}:{}",
        state.channel_id, state.token
    )
}

fn valid_suggestion_state(state: &KnowledgeSuggestionState, now: i64) -> bool {
    state.guild_id > 0
        && state.channel_id > 0
        && state.ticket_number > 0
        && valid_search_token(&state.token)
        && state.expires_at > now
        && state.panel_id.as_deref().is_none_or(valid_id)
        && if state.cancelled {
            state.notified
                && state.complete
                && state.terms.is_empty()
                && state.candidates.is_empty()
        } else {
            !state.terms.is_empty()
                && state.terms.len() <= 40
                && state.terms.iter().all(|term| {
                    !term.is_empty()
                        && term.chars().count() <= MAX_QUERY_CHARS
                        && !term.chars().any(char::is_control)
                })
                && valid_article_after_key(&state.after_key)
                && state.candidates.len() <= MAX_TICKET_SUGGESTIONS
                && state
                    .candidates
                    .iter()
                    .all(|candidate| valid_id(&candidate.id) && candidate.score > 0)
                && (!state.notified || state.complete)
        }
}

pub(crate) async fn send_article(
    ctx: &EventContext,
    channel: ChannelId,
    slug: &str,
    can_manage: bool,
) -> Result<(), ModuleError> {
    if !valid_id(slug) {
        return Err(ModuleError::other("invalid knowledge article slug"));
    }
    let Some((article, _)) = read_article_record(ctx, slug)
        .await
        .map_err(|_| ModuleError::other("knowledge article could not be read"))?
    else {
        return Err(ModuleError::other("knowledge article was not found"));
    };
    if article.deleted || (!article.published && !can_manage) {
        return Err(ModuleError::other("knowledge article was not found"));
    }
    let embeds = article_embeds(&article)?;
    ctx.api
        .execute(Action::SendMessage {
            channel_id: channel,
            content: None,
            embeds,
            reply_to: None,
            mentions: MentionPolicy::None,
        })
        .await
        .map_err(|_| ModuleError::other("knowledge article could not be sent"))?;
    Ok(())
}

fn article_embeds(article: &Article) -> Result<Vec<Embed>, ModuleError> {
    let mut embeds = vec![
        EmbedBuilder::new()
            .title(article.title.clone())
            .description(article.body.clone())
            .build()
            .map_err(|_| ModuleError::other("knowledge article could not be rendered"))?,
    ];
    if let Some(custom_embed) = &article.custom_embed {
        embeds.push(
            parse_custom_embed(custom_embed)
                .ok_or_else(|| ModuleError::other("knowledge article embed is invalid"))?,
        );
    }
    Ok(embeds)
}

fn suggestion_terms(answers: &[(String, String)]) -> Vec<String> {
    let mut terms = std::collections::HashSet::new();
    for (_, answer) in answers {
        let normalized = answer.to_lowercase();
        if normalized.contains('@')
            || normalized.contains("http://")
            || normalized.contains("https://")
        {
            continue;
        }
        for term in normalized.split(|character: char| !character.is_alphanumeric()) {
            if (3..=MAX_QUERY_CHARS).contains(&term.chars().count())
                && term.chars().any(char::is_alphabetic)
                && !term.chars().any(|character| character.is_ascii_digit())
            {
                terms.insert(term.to_owned());
            }
            if terms.len() >= 40 {
                let mut terms: Vec<_> = terms.into_iter().collect();
                terms.sort_unstable();
                terms.truncate(40);
                return terms;
            }
        }
        if !normalized.chars().any(char::is_whitespace) {
            let chars: Vec<char> = normalized.chars().collect();
            for window in chars.windows(3).take(40) {
                let term: String = window.iter().collect();
                if term.chars().any(char::is_alphabetic)
                    && !term.chars().any(|character| character.is_ascii_digit())
                {
                    terms.insert(term);
                }
                if terms.len() >= 40 {
                    let mut terms: Vec<_> = terms.into_iter().collect();
                    terms.sort_unstable();
                    terms.truncate(40);
                    return terms;
                }
            }
        }
    }
    let mut terms: Vec<_> = terms.into_iter().collect();
    terms.sort_unstable();
    terms.truncate(40);
    terms
}

fn suggestion_score(article: &Article, terms: &[String]) -> usize {
    let id = article.id.to_lowercase();
    let title = article.title.to_lowercase();
    let body = article.body.to_lowercase();
    let mut score = 0;
    for term in terms {
        if title.contains(term) {
            score += 6;
        } else if id.contains(term) {
            score += 4;
        } else if article
            .keywords
            .iter()
            .any(|keyword| keyword.to_lowercase().contains(term))
        {
            score += 5;
        } else if article
            .categories
            .iter()
            .any(|category| category.to_lowercase().contains(term))
        {
            score += 3;
        } else if body.contains(term) {
            score += 1;
        }
    }
    score
}

pub(crate) async fn add(cx: CommandCx, args: AddArgs) -> Result<(), UserError> {
    let article = Article {
        id: args.id.trim().to_owned(),
        title: args.title.trim().to_owned(),
        body: args.body.trim().to_owned(),
        categories: Vec::new(),
        keywords: Vec::new(),
        published: false,
        custom_embed: None,
        deleted: false,
        deleted_from_revision: None,
        created_by_idempotency_key: None,
        last_update_idempotency_key: None,
    };
    if !valid_article(&article) {
        return Err(UserError::message("tickets-knowledge-invalid-article"));
    }
    cx.defer(true).await?;
    let ctx = cx.ctx();
    let key = article_key(&article.id);
    let expect = match ctx
        .store()
        .entry(&key)
        .await
        .map_err(|error| storage_error(&error))?
    {
        None => Expect::Missing,
        Some(entry) => {
            let revision = entry.revision;
            if !article_from_entry(&key, entry)?.deleted {
                return Err(UserError::message("tickets-knowledge-id-exists"));
            }
            Expect::Revision(revision)
        }
    };
    match ctx.store().replace(&key, &article, expect, None).await {
        Ok(_) => {
            record_knowledge_success(
                &cx,
                ctx,
                crate::audit::AuditAction::KnowledgeCreate,
                "knowledge-create",
            )
            .await;
            cx.reply_private_t(
                "tickets-knowledge-added",
                &Args::new().with("id", article.id),
            )
            .await
        }
        Err(StorageError::Conflict) => Err(UserError::message("tickets-knowledge-id-exists")),
        Err(error) => Err(storage_error(&error)),
    }
}

pub(crate) async fn edit(cx: CommandCx, args: EditArgs) -> Result<(), UserError> {
    let mut article = Article {
        id: args.id.trim().to_owned(),
        title: args.title.trim().to_owned(),
        body: args.body.trim().to_owned(),
        categories: Vec::new(),
        keywords: Vec::new(),
        published: false,
        custom_embed: None,
        deleted: false,
        deleted_from_revision: None,
        created_by_idempotency_key: None,
        last_update_idempotency_key: None,
    };
    if !valid_article(&article) {
        return Err(UserError::message("tickets-knowledge-invalid-article"));
    }
    cx.defer(true).await?;
    let ctx = cx.ctx();
    let Some((current, revision)) = read_article(ctx, &article.id).await? else {
        return Err(UserError::message("tickets-knowledge-not-found"));
    };
    article.categories = current.categories;
    article.keywords = current.keywords;
    article.published = current.published;
    article.custom_embed = current.custom_embed;
    article.created_by_idempotency_key = current.created_by_idempotency_key;
    match ctx
        .store()
        .replace(
            &article_key(&article.id),
            &article,
            Expect::Revision(revision),
            None,
        )
        .await
    {
        Ok(_) => {
            record_knowledge_success(
                &cx,
                ctx,
                crate::audit::AuditAction::KnowledgeUpdate,
                "knowledge-update",
            )
            .await;
            cx.reply_private_t(
                "tickets-knowledge-updated",
                &Args::new().with("id", article.id),
            )
            .await
        }
        Err(StorageError::Conflict) => Err(UserError::message("tickets-knowledge-changed")),
        Err(error) => Err(storage_error(&error)),
    }
}

pub(crate) async fn delete(cx: CommandCx, args: DeleteArgs) -> Result<(), UserError> {
    let id = args.id.trim();
    if !valid_id(id) {
        return Err(UserError::message("tickets-knowledge-invalid-id"));
    }
    cx.defer(true).await?;
    let ctx = cx.ctx();
    let Some((article, revision)) = read_article_record(ctx, id).await? else {
        return Err(UserError::message("tickets-knowledge-not-found"));
    };
    if article.deleted {
        return Err(UserError::message("tickets-knowledge-not-found"));
    }
    let tombstone = Article {
        id: article.id,
        title: String::new(),
        body: String::new(),
        categories: Vec::new(),
        keywords: Vec::new(),
        published: false,
        custom_embed: None,
        deleted: true,
        deleted_from_revision: Some(revision),
        created_by_idempotency_key: article.created_by_idempotency_key.clone(),
        last_update_idempotency_key: article.last_update_idempotency_key.clone(),
    };
    // Keep a versioned tombstone so concurrent edits and re-creates cannot be erased.
    match ctx
        .store()
        .replace(
            &article_key(id),
            &tombstone,
            Expect::Revision(revision),
            Some(TOMBSTONE_TTL),
        )
        .await
    {
        Ok(_) => {
            record_knowledge_success(
                &cx,
                ctx,
                crate::audit::AuditAction::KnowledgeDelete,
                "knowledge-delete",
            )
            .await;
            cx.reply_private_t(
                "tickets-knowledge-deleted",
                &Args::new().with("id", id.to_owned()),
            )
            .await
        }
        Err(StorageError::Conflict) => Err(UserError::message("tickets-knowledge-changed")),
        Err(error) => Err(storage_error(&error)),
    }
}

async fn scan_search_batch(
    ctx: &EventContext,
    state: &mut KnowledgeSearchState,
) -> Result<bool, UserError> {
    let page_start = state
        .page
        .checked_mul(PAGE_SIZE)
        .ok_or_else(|| UserError::message("tickets-knowledge-page-not-found"))?;
    let entries = ctx
        .store()
        .list_after(PREFIX, &state.after_key, ARTICLE_SCAN_BATCH)
        .await
        .map_err(|error| storage_error(&error))?;
    let entry_count = entries.len();
    for (index, (key, entry)) in entries.into_iter().enumerate() {
        let article = article_from_entry(&key, entry)?;
        state.after_key = key;
        if article.deleted || !article.published || !matches_query(&article, &state.query) {
            continue;
        }
        if state.matched >= page_start {
            state.results.push(KnowledgeSearchResult {
                id: article.id,
                title: article.title,
            });
        }
        state.matched += 1;
        if state.results.len() == PAGE_SIZE {
            return Ok(index + 1 == entry_count && entry_count < ARTICLE_SCAN_BATCH);
        }
    }
    Ok(entry_count < ARTICLE_SCAN_BATCH)
}

fn article_summaries(articles: &[KnowledgeSearchResult]) -> String {
    articles
        .iter()
        .map(|article| format!("{}: {}", article.id, article.title))
        .collect::<Vec<_>>()
        .join("\n")
}

fn knowledge_search_token(
    owner: u64,
    query: &str,
    after_key: &str,
    page: usize,
    now: i64,
    seed: &str,
) -> String {
    let mut hasher = DefaultHasher::new();
    owner.hash(&mut hasher);
    query.hash(&mut hasher);
    after_key.hash(&mut hasher);
    page.hash(&mut hasher);
    now.hash(&mut hasher);
    seed.hash(&mut hasher);
    format!("{:016x}", hasher.finish())
}

fn knowledge_search_state_key(owner: u64, token: &str) -> String {
    format!("{SEARCH_PREFIX}{owner}:{token}")
}

fn knowledge_search_request_key(owner: u64, query: &str, page: usize) -> String {
    let mut first = DefaultHasher::new();
    owner.hash(&mut first);
    query.hash(&mut first);
    page.hash(&mut first);
    let mut second = DefaultHasher::new();
    page.hash(&mut second);
    query.hash(&mut second);
    owner.hash(&mut second);
    format!(
        "{SEARCH_REQUEST_PREFIX}{owner}:{page}:{:016x}{:016x}",
        first.finish(),
        second.finish()
    )
}

fn knowledge_search_job_key(state: &KnowledgeSearchState) -> String {
    format!(
        "{KNOWLEDGE_SEARCH_JOB}:{}:{}:{}",
        state.owner, state.token, state.after_key
    )
}

fn valid_search_token(token: &str) -> bool {
    token.len() == 16 && token.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn valid_search_state(state: &KnowledgeSearchState, owner: u64, token: &str, now: i64) -> bool {
    state.owner == owner
        && state.token == token
        && valid_search_token(token)
        && state.expires_at > now
        && state.query == state.query.trim().to_lowercase()
        && !state.query.is_empty()
        && state.query.chars().count() <= MAX_QUERY_CHARS
        && (state.after_key == PREFIX || state.after_key.strip_prefix(PREFIX).is_some_and(valid_id))
        && state.page.checked_mul(PAGE_SIZE).is_some()
        && state.results.len() <= PAGE_SIZE
        && state.results.iter().all(|article| {
            valid_id(&article.id)
                && !article.title.trim().is_empty()
                && article.title.chars().count() <= MAX_TITLE_CHARS
                && !article.title.chars().any(char::is_control)
        })
        && state.matched >= state.results.len()
        && (!state.notified || state.complete)
}

fn search_expiry(now: i64, empty_result: bool) -> Result<i64, UserError> {
    let ttl = if empty_result {
        SEARCH_EMPTY_RESULT_TTL
    } else {
        SEARCH_TTL
    };
    now.checked_add(i64::try_from(ttl.as_secs()).unwrap_or(i64::MAX))
        .ok_or_else(|| UserError::message("tickets-knowledge-search-expired"))
}

fn search_state_ttl(state: &KnowledgeSearchState, now: i64) -> Duration {
    Duration::from_secs(
        u64::try_from(state.expires_at.saturating_sub(now))
            .unwrap_or(0)
            .max(1),
    )
}

async fn load_search_lookup(
    ctx: &EventContext,
    owner: u64,
    query: &str,
    page: usize,
    now: i64,
) -> Result<SearchLookup, UserError> {
    let request_key = knowledge_search_request_key(owner, query, page);
    let Some(request_entry) = ctx
        .store()
        .entry(&request_key)
        .await
        .map_err(|error| storage_error(&error))?
    else {
        return Ok(SearchLookup {
            request_revision: None,
            state_revision: None,
            state: None,
        });
    };
    let request: KnowledgeSearchRequest = serde_json::from_value(request_entry.value.clone())
        .map_err(|error| {
            tracing::warn!(?error, owner, "could not read a knowledge search request");
            UserError::message("tickets-knowledge-storage-failed")
        })?;
    if request.owner != owner || request.query != query || request.page != page {
        return Err(UserError::message("tickets-knowledge-storage-failed"));
    }
    if request.expires_at <= now {
        return Ok(SearchLookup {
            request_revision: Some(request_entry.revision),
            state_revision: None,
            state: None,
        });
    }
    let state_key = knowledge_search_state_key(owner, &request.token);
    let Some(state_entry) = ctx
        .store()
        .entry(&state_key)
        .await
        .map_err(|error| storage_error(&error))?
    else {
        return Ok(SearchLookup {
            request_revision: Some(request_entry.revision),
            state_revision: None,
            state: None,
        });
    };
    let state: KnowledgeSearchState =
        serde_json::from_value(state_entry.value.clone()).map_err(|error| {
            tracing::warn!(?error, owner, "could not read a knowledge search state");
            UserError::message("tickets-knowledge-storage-failed")
        })?;
    if !valid_search_state(&state, owner, &request.token, now)
        || state.query != query
        || state.page != page
    {
        return Ok(SearchLookup {
            request_revision: Some(request_entry.revision),
            state_revision: None,
            state: None,
        });
    }
    Ok(SearchLookup {
        request_revision: Some(request_entry.revision),
        state_revision: Some(state_entry.revision),
        state: Some(state),
    })
}

async fn start_search(
    cx: &CommandCx,
    ctx: &EventContext,
    owner: u64,
    query: &str,
    page: usize,
    request_revision: Option<i64>,
    now: i64,
) -> Result<(), UserError> {
    let seed = cx.interaction_id().get().to_string();
    let token = knowledge_search_token(owner, query, PREFIX, page, now, &seed);
    let expires_at = search_expiry(now, false)?;
    let state = KnowledgeSearchState {
        owner,
        token: token.clone(),
        query: query.to_owned(),
        locale: cx.locale(),
        after_key: PREFIX.to_owned(),
        page,
        matched: 0,
        results: Vec::new(),
        expires_at,
        complete: false,
        pages_known: false,
        notified: false,
        dm_closed: false,
        cancelled: false,
    };
    let state_key = knowledge_search_state_key(owner, &token);
    persist_search_state(ctx, &state_key, &state, Expect::Missing)
        .await
        .map_err(|error| storage_error(&error))?;
    let request_key = knowledge_search_request_key(owner, query, page);
    let request = KnowledgeSearchRequest {
        owner,
        query: query.to_owned(),
        page,
        token,
        expires_at,
    };
    let expect = request_revision.map_or(Expect::Missing, Expect::Revision);
    match ctx
        .store()
        .replace(&request_key, &request, expect, Some(SEARCH_TTL))
        .await
    {
        Ok(_) => {}
        Err(StorageError::Conflict) => {
            if let Some(state) = load_search_lookup(ctx, owner, query, page, ctx.now())
                .await?
                .state
            {
                schedule_search_job(ctx, &state, Duration::ZERO).await?;
            }
            return reply_search_queued(cx, query, page + 1).await;
        }
        Err(error) => return Err(storage_error(&error)),
    }
    schedule_search_job(ctx, &state, SEARCH_JOB_DELAY).await?;
    reply_search_queued(cx, query, page + 1).await
}

// shortcut: keep revisioned restart inputs explicit; bundle them only if this helper grows.
#[allow(clippy::too_many_arguments)]
async fn restart_search(
    cx: &CommandCx,
    ctx: &EventContext,
    owner: u64,
    query: &str,
    page: usize,
    state_key: &str,
    mut state: KnowledgeSearchState,
    state_revision: i64,
    request_revision: i64,
    now: i64,
) -> Result<(), UserError> {
    reset_search_state(&mut state, cx.locale(), now)?;
    if let Err(error) =
        persist_search_state(ctx, state_key, &state, Expect::Revision(state_revision)).await
    {
        if matches!(error, StorageError::Conflict) {
            return reply_search_queued(cx, query, page + 1).await;
        }
        return Err(storage_error(&error));
    }
    let request_key = knowledge_search_request_key(owner, query, page);
    let request = KnowledgeSearchRequest {
        owner,
        query: query.to_owned(),
        page,
        token: state.token.clone(),
        expires_at: state.expires_at,
    };
    if let Err(error) = ctx
        .store()
        .replace(
            &request_key,
            &request,
            Expect::Revision(request_revision),
            Some(SEARCH_TTL),
        )
        .await
    {
        if matches!(error, StorageError::Conflict) {
            return reply_search_queued(cx, query, page + 1).await;
        }
        return Err(storage_error(&error));
    }
    schedule_search_job(ctx, &state, SEARCH_JOB_DELAY).await?;
    reply_search_queued(cx, query, page + 1).await
}

fn reset_search_state(
    state: &mut KnowledgeSearchState,
    locale: Locale,
    now: i64,
) -> Result<(), UserError> {
    state.locale = locale;
    PREFIX.clone_into(&mut state.after_key);
    state.matched = 0;
    state.results.clear();
    state.expires_at = search_expiry(now, false)?;
    state.complete = false;
    state.pages_known = false;
    state.notified = false;
    state.dm_closed = false;
    state.cancelled = false;
    Ok(())
}

async fn schedule_search_job(
    ctx: &EventContext,
    state: &KnowledgeSearchState,
    delay: Duration,
) -> Result<(), UserError> {
    let payload = KnowledgeSearchJob {
        owner: state.owner,
        token: state.token.clone(),
    };
    ctx.scheduler()
        .job(KNOWLEDGE_SEARCH_JOB, &payload)
        .map_err(|error| {
            tracing::warn!(?error, "could not create a knowledge search job");
            UserError::message("tickets-knowledge-storage-failed")
        })?
        .key(&knowledge_search_job_key(state))
        .if_absent()
        .after(delay)
        .await
        .map_err(|error| {
            tracing::warn!(?error, "could not schedule a knowledge search job");
            UserError::message("tickets-knowledge-storage-failed")
        })
}

async fn persist_search_state(
    ctx: &EventContext,
    key: &str,
    state: &KnowledgeSearchState,
    expect: Expect,
) -> Result<i64, StorageError> {
    ctx.store()
        .replace(key, state, expect, Some(search_state_ttl(state, ctx.now())))
        .await
}

async fn refresh_search_request(
    ctx: &EventContext,
    key: &str,
    request: &KnowledgeSearchRequest,
    revision: i64,
    state: &KnowledgeSearchState,
) -> Result<i64, StorageError> {
    let updated = KnowledgeSearchRequest {
        expires_at: state.expires_at,
        ..request.clone()
    };
    ctx.store()
        .replace(
            key,
            &updated,
            Expect::Revision(revision),
            Some(search_state_ttl(state, ctx.now())),
        )
        .await
}

async fn reply_search_queued(cx: &CommandCx, query: &str, page: usize) -> Result<(), UserError> {
    cx.reply_private_t(
        "tickets-knowledge-search-queued",
        &Args::new()
            .with("query", query.to_owned())
            .with("page", page.to_string()),
    )
    .await
}

async fn reply_search_results(
    cx: &CommandCx,
    state: &KnowledgeSearchState,
) -> Result<(), UserError> {
    if state.results.is_empty() {
        return reply_empty_search(cx, state).await;
    }
    cx.reply_private_t(
        "tickets-knowledge-search-results",
        &Args::new()
            .with("query", state.query.clone())
            .with("results", article_summaries(&state.results))
            .with("page", (state.page + 1).to_string())
            .with(
                "pages",
                if state.pages_known {
                    state.matched.div_ceil(PAGE_SIZE).to_string()
                } else {
                    "?".to_owned()
                },
            ),
    )
    .await
}

async fn reply_empty_search(cx: &CommandCx, state: &KnowledgeSearchState) -> Result<(), UserError> {
    if state.matched == 0 {
        cx.reply_private_t(
            "tickets-knowledge-search-none",
            &Args::new().with("query", state.query.clone()),
        )
        .await
    } else {
        Err(UserError::message("tickets-knowledge-page-not-found"))
    }
}

async fn refresh_search_results(
    ctx: &EventContext,
    state: &mut KnowledgeSearchState,
) -> Result<bool, UserError> {
    let mut refreshed = Vec::with_capacity(state.results.len());
    for result in &state.results {
        let Some((article, _)) = read_article_record(ctx, &result.id).await? else {
            return Ok(false);
        };
        if article.deleted || !article.published || !matches_query(&article, &state.query) {
            return Ok(false);
        }
        refreshed.push(KnowledgeSearchResult {
            id: article.id,
            title: article.title,
        });
    }
    state.results = refreshed;
    Ok(true)
}

// shortcut: keep delivery and revision handling together; bundle inputs if another is added.
#[allow(clippy::too_many_arguments)]
async fn finish_search_job(
    cx: &JobCx,
    ctx: &EventContext,
    state_key: &str,
    mut state: KnowledgeSearchState,
    state_revision: i64,
    request_key: &str,
    request: &KnowledgeSearchRequest,
    request_revision: i64,
) -> Result<(), ModuleError> {
    if state.cancelled || state.notified {
        return Ok(());
    }
    match cx
        .api()
        .get_member(ctx.guild_id, UserId::new(state.owner))
        .await
    {
        Ok(_) => {}
        Err(DiscordError::NotFound) => {
            state.cancelled = true;
            state.notified = true;
            let revision = ctx
                .store()
                .replace(
                    state_key,
                    &state,
                    Expect::Revision(state_revision),
                    Some(search_state_ttl(&state, ctx.now())),
                )
                .await
                .map_err(|error| storage_module_error(&error))?;
            let _ = refresh_search_request(ctx, request_key, request, request_revision, &state)
                .await
                .map_err(|error| storage_module_error(&error))?;
            let _ = revision;
            return Ok(());
        }
        Err(error) => return Err(ModuleError::other(error.to_string())),
    }

    if !refresh_search_results(ctx, &mut state)
        .await
        .map_err(|_| ModuleError::other("knowledge search results could not be validated"))?
    {
        let locale = state.locale;
        reset_search_state(&mut state, locale, ctx.now())
            .map_err(|_| ModuleError::other("knowledge search expiry was out of range"))?;
        let revision = match ctx
            .store()
            .replace(
                state_key,
                &state,
                Expect::Revision(state_revision),
                Some(search_state_ttl(&state, ctx.now())),
            )
            .await
        {
            Ok(revision) => revision,
            Err(StorageError::Conflict) => return Ok(()),
            Err(error) => return Err(storage_module_error(&error)),
        };
        match refresh_search_request(ctx, request_key, request, request_revision, &state).await {
            Ok(_) | Err(StorageError::Conflict) => {}
            Err(error) => return Err(storage_module_error(&error)),
        }
        let _ = revision;
        return schedule_search_job(ctx, &state, SEARCH_JOB_DELAY)
            .await
            .map_err(|_| ModuleError::other("knowledge search job could not be rescheduled"));
    }

    let content = search_state_message(ctx, &state);
    match cx
        .direct_message(UserId::new(state.owner), DirectMessage::text(content))
        .await
        .map_err(|_| ModuleError::other("knowledge search results could not be delivered"))?
    {
        Sent::Delivered(_) => {
            state.notified = true;
            state.dm_closed = false;
        }
        Sent::Closed => {
            state.notified = true;
            state.dm_closed = true;
        }
    }
    match ctx
        .store()
        .replace(
            state_key,
            &state,
            Expect::Revision(state_revision),
            Some(search_state_ttl(&state, ctx.now())),
        )
        .await
    {
        Ok(_) | Err(StorageError::Conflict) => {}
        Err(error) => return Err(storage_module_error(&error)),
    }
    Ok(())
}

fn search_state_message(ctx: &EventContext, state: &KnowledgeSearchState) -> String {
    if state.results.is_empty() {
        if state.matched == 0 {
            ctx.t_in(
                state.locale,
                "tickets-knowledge-search-none",
                &Args::new().with("query", state.query.clone()),
            )
        } else {
            ctx.t_in(
                state.locale,
                "tickets-knowledge-page-not-found",
                &Args::new(),
            )
        }
    } else {
        ctx.t_in(
            state.locale,
            "tickets-knowledge-search-results",
            &Args::new()
                .with("query", state.query.clone())
                .with("results", article_summaries(&state.results))
                .with("page", (state.page + 1).to_string())
                .with(
                    "pages",
                    if state.pages_known {
                        state.matched.div_ceil(PAGE_SIZE).to_string()
                    } else {
                        "?".to_owned()
                    },
                ),
        )
    }
}
async fn read_article(ctx: &EventContext, id: &str) -> Result<Option<(Article, i64)>, UserError> {
    let Some((article, revision)) = read_article_record(ctx, id).await? else {
        return Ok(None);
    };
    Ok((!article.deleted).then_some((article, revision)))
}

fn article_key(id: &str) -> String {
    format!("{PREFIX}{id}")
}

async fn read_article_record(
    ctx: &EventContext,
    id: &str,
) -> Result<Option<(Article, i64)>, UserError> {
    let key = article_key(id);
    let Some(entry) = ctx
        .store()
        .entry(&key)
        .await
        .map_err(|error| storage_error(&error))?
    else {
        return Ok(None);
    };
    let revision = entry.revision;
    let article = article_from_entry(&key, entry)?;
    Ok(Some((article, revision)))
}

fn article_from_entry(key: &str, entry: Entry) -> Result<Article, UserError> {
    let article: Article = serde_json::from_value(entry.value).map_err(|error| {
        tracing::warn!(?error, "could not read a knowledge article");
        UserError::message("tickets-knowledge-storage-failed")
    })?;
    if key != article_key(&article.id) || !valid_article(&article) {
        return Err(UserError::message("tickets-knowledge-storage-failed"));
    }
    Ok(article)
}

fn valid_article(article: &Article) -> bool {
    valid_id(&article.id)
        && article
            .created_by_idempotency_key
            .as_deref()
            .is_none_or(valid_idempotency_key)
        && article
            .last_update_idempotency_key
            .as_deref()
            .is_none_or(valid_idempotency_key)
        && if article.deleted {
            article.title.is_empty()
                && article.body.is_empty()
                && article.categories.is_empty()
                && article.keywords.is_empty()
                && !article.published
                && article.custom_embed.is_none()
                && article
                    .deleted_from_revision
                    .is_none_or(|revision| revision > 0)
        } else {
            article.deleted_from_revision.is_none()
                && !article.title.trim().is_empty()
                && article.title.chars().count() <= MAX_TITLE_CHARS
                && !article.title.chars().any(char::is_control)
                && !article.body.trim().is_empty()
                && article.body.chars().count() <= MAX_BODY_CHARS
                && !article.body.chars().any(|character| {
                    character.is_control() && !matches!(character, '\n' | '\r' | '\t')
                })
                && valid_labels(&article.categories, MAX_CATEGORY_CHARS)
                && valid_labels(&article.keywords, MAX_KEYWORD_CHARS)
                && valid_custom_embed(article.custom_embed.as_ref())
                && serde_json::to_vec(article).is_ok_and(|value| value.len() <= MAX_VALUE_BYTES)
        }
}

fn valid_labels(values: &[String], max_chars: usize) -> bool {
    values.iter().all(|value| {
        !value.trim().is_empty()
            && value.trim() == value
            && value.chars().count() <= max_chars
            && !value.chars().any(char::is_control)
    }) && values
        .iter()
        .map(|value| value.to_lowercase())
        .collect::<std::collections::HashSet<_>>()
        .len()
        == values.len()
}

fn valid_custom_embed(embed: Option<&Value>) -> bool {
    let Some(embed) = embed else {
        return true;
    };
    embed.is_object()
        && serde_json::to_string(embed).is_ok_and(|serialized| serialized.chars().count() <= 6000)
        && parse_custom_embed(embed).is_some_and(|parsed| custom_embed_within_limits(&parsed))
}

// Discord rejects the whole message when any of these limits is exceeded.
fn custom_embed_within_limits(embed: &Embed) -> bool {
    embed
        .title
        .as_deref()
        .is_none_or(|title| title.chars().count() <= 256)
        && embed
            .description
            .as_deref()
            .is_none_or(|description| description.chars().count() <= 4096)
        && embed
            .author
            .as_ref()
            .is_none_or(|author| author.name.chars().count() <= 256)
        && embed
            .footer
            .as_ref()
            .is_none_or(|footer| footer.text.chars().count() <= 2048)
        && embed.fields.len() <= 25
        && embed.fields.iter().all(|field| {
            (1..=256).contains(&field.name.chars().count())
                && (1..=1024).contains(&field.value.chars().count())
        })
}

fn parse_custom_embed(value: &Value) -> Option<Embed> {
    let mut value = value.clone();
    let object = value.as_object_mut()?;
    match object.get("type") {
        None => {
            object.insert("type".to_owned(), Value::String("rich".to_owned()));
        }
        Some(Value::String(kind)) if kind == "rich" => {}
        Some(_) => return None,
    }
    serde_json::from_value(value).ok()
}

fn valid_idempotency_key(key: &str) -> bool {
    !key.is_empty()
        && key.len() <= 64
        && key
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
}

fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= MAX_ID_CHARS
        && id
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
}

fn matches_query(article: &Article, query: &str) -> bool {
    article.id.to_lowercase().contains(query)
        || article.title.to_lowercase().contains(query)
        || article.body.to_lowercase().contains(query)
        || article
            .categories
            .iter()
            .any(|category| category.to_lowercase().contains(query))
        || article
            .keywords
            .iter()
            .any(|keyword| keyword.to_lowercase().contains(query))
}

fn storage_error(error: &StorageError) -> UserError {
    tracing::warn!(?error, "knowledge article storage operation failed");
    UserError::message("tickets-knowledge-storage-failed")
}

fn storage_module_error(error: &StorageError) -> ModuleError {
    ModuleError::other(error.to_string())
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DashboardArticleInput {
    id: String,
    title: String,
    body: String,
    #[serde(default)]
    categories: Option<Vec<String>>,
    #[serde(default)]
    keywords: Option<Vec<String>>,
    #[serde(default)]
    published: Option<bool>,
    #[serde(default)]
    custom_embed: Value,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DashboardArticleId {
    id: String,
}

pub(crate) async fn dashboard_state(
    ctx: &EventContext,
    _actor: &closure_core::permission::Actor,
    query: Option<&str>,
    cursor: Option<&str>,
) -> Result<closure_core::module::DashboardActionResult, closure_core::module::DashboardActionError>
{
    use closure_core::module::{DashboardActionError, DashboardActionResult};

    let query = query.map(str::trim).filter(|query| !query.is_empty());
    if query.is_some_and(|query| {
        query.chars().count() > MAX_QUERY_CHARS || query.chars().any(char::is_control)
    }) {
        return Err(DashboardActionError::Invalid(
            "invalid search query".to_owned(),
        ));
    }
    let after = match cursor {
        Some(cursor) if valid_id(cursor) => article_key(cursor),
        Some(_) => return Err(DashboardActionError::Invalid("invalid cursor".to_owned())),
        None => PREFIX.to_owned(),
    };
    let query = query.map(str::to_lowercase);
    let entries = ctx
        .store()
        .list_after(PREFIX, &after, ARTICLE_SCAN_BATCH)
        .await
        .map_err(dashboard_storage_error)?;
    let entry_count = entries.len();
    let mut last_scanned = None;
    let mut articles = Vec::with_capacity(DASHBOARD_PAGE_SIZE);
    let mut categories = std::collections::BTreeSet::new();
    let mut more_results = false;
    for (key, entry) in entries {
        let revision = entry.revision;
        let article = dashboard_article_from_entry(&key, entry)?;
        if !article.deleted
            && query
                .as_deref()
                .is_none_or(|query| matches_query(&article, query))
        {
            if articles.len() == DASHBOARD_PAGE_SIZE {
                more_results = true;
                break;
            }
            categories.extend(article.categories.iter().cloned());
            articles.push(serde_json::json!({
                "id": article.id,
                "title": article.title,
                "body": article.body,
                "categories": article.categories,
                "keywords": article.keywords,
                "published": article.published,
                "custom_embed": article.custom_embed,
                "revision": revision,
            }));
        }
        last_scanned = Some(article.id);
    }
    let next_cursor = if more_results || entry_count == ARTICLE_SCAN_BATCH {
        last_scanned
    } else {
        None
    };
    Ok(DashboardActionResult {
        value: serde_json::json!({
            "articles": articles,
            "categories": categories,
            "next_cursor": next_cursor,
        }),
        revision: 0,
    })
}

pub(crate) async fn public_article(
    ctx: &EventContext,
    slug: &str,
) -> Result<Option<PublicKnowledgeArticle>, closure_core::module::DashboardActionError> {
    use closure_core::module::DashboardActionError;

    if !valid_id(slug) {
        return Err(DashboardActionError::Invalid(
            "invalid article slug".to_owned(),
        ));
    }
    let key = article_key(slug);
    let Some(entry) = ctx.store().entry(&key).await.map_err(|_| {
        DashboardActionError::Internal("public knowledge could not be loaded".to_owned())
    })?
    else {
        return Ok(None);
    };
    let article: Article = serde_json::from_value(entry.value).map_err(|_| {
        DashboardActionError::Internal("public knowledge could not be loaded".to_owned())
    })?;
    if key != article_key(&article.id)
        || !valid_article(&article)
        || article.deleted
        || !article.published
    {
        return Ok(None);
    }
    Ok(Some(public_projection(article)))
}

// shortcut: keep the bounded scan and projection together; split if another phase is added.
#[allow(clippy::too_many_lines)]
pub(crate) async fn public_state(
    ctx: &EventContext,
    query: Option<&str>,
    category: Option<&str>,
    cursor: Option<&str>,
) -> Result<closure_core::module::DashboardActionResult, closure_core::module::DashboardActionError>
{
    use closure_core::module::{DashboardActionError, DashboardActionResult};

    let query = query.map(str::trim).filter(|query| !query.is_empty());
    if query.is_some_and(|query| {
        query.chars().count() > MAX_QUERY_CHARS || query.chars().any(char::is_control)
    }) {
        return Err(DashboardActionError::Invalid(
            "invalid search query".to_owned(),
        ));
    }
    let category = category
        .map(str::trim)
        .filter(|category| !category.is_empty());
    if category.is_some_and(|category| {
        category.chars().count() > MAX_CATEGORY_CHARS || category.chars().any(char::is_control)
    }) {
        return Err(DashboardActionError::Invalid("invalid category".to_owned()));
    }
    let query = query.map(str::to_lowercase);
    let category = category.map(str::to_lowercase);
    let now = ctx.now();
    let after = match cursor {
        None => PREFIX.to_owned(),
        Some(token) if valid_public_cursor_token(token) => {
            let key = format!("{PUBLIC_CURSOR_PREFIX}{token}");
            let Some(entry) = ctx.store().entry(&key).await.map_err(|_| {
                DashboardActionError::Internal("public knowledge could not be loaded".to_owned())
            })?
            else {
                return Err(DashboardActionError::NotFound);
            };
            let state: PublicCursorState =
                serde_json::from_value(entry.value).map_err(|_| DashboardActionError::NotFound)?;
            if state.token != token
                || state.expires_at <= now
                || state.query != query
                || state.category != category
                || !valid_article_after_key(&state.after_key)
            {
                return Err(DashboardActionError::NotFound);
            }
            state.after_key
        }
        Some(_) => return Err(DashboardActionError::Invalid("invalid cursor".to_owned())),
    };
    let entries = ctx
        .store()
        .list_after(PREFIX, &after, ARTICLE_SCAN_BATCH)
        .await
        .map_err(|_| {
            DashboardActionError::Internal("public knowledge could not be loaded".to_owned())
        })?;
    let entry_count = entries.len();
    let mut last_scanned = None;
    let mut articles = Vec::with_capacity(DASHBOARD_PAGE_SIZE);
    let mut categories = std::collections::BTreeSet::new();
    let mut more_results = false;
    for (key, entry) in entries {
        let article: Article = serde_json::from_value(entry.value).map_err(|_| {
            DashboardActionError::Internal("public knowledge could not be loaded".to_owned())
        })?;
        if key != article_key(&article.id) || !valid_article(&article) {
            return Err(DashboardActionError::Internal(
                "public knowledge could not be loaded".to_owned(),
            ));
        }
        if !article.deleted && article.published {
            categories.extend(article.categories.iter().cloned());
            if query
                .as_deref()
                .is_none_or(|query| matches_query(&article, query))
                && category.as_deref().is_none_or(|category| {
                    article
                        .categories
                        .iter()
                        .any(|value| value.eq_ignore_ascii_case(category))
                })
            {
                if articles.len() == DASHBOARD_PAGE_SIZE {
                    more_results = true;
                    break;
                }
                articles.push(
                    serde_json::to_value(public_projection(article)).map_err(|_| {
                        DashboardActionError::Internal(
                            "public knowledge could not be loaded".to_owned(),
                        )
                    })?,
                );
            }
        }
        last_scanned = Some(key);
    }
    let next_cursor = if more_results || entry_count == ARTICLE_SCAN_BATCH {
        let after_key = last_scanned.unwrap_or(after);
        Some(create_public_cursor(ctx, query.clone(), category.clone(), after_key, now).await?)
    } else {
        None
    };
    Ok(DashboardActionResult {
        value: serde_json::json!({
            "articles": articles,
            "categories": categories,
            "next_cursor": next_cursor,
        }),
        revision: 0,
    })
}

fn public_projection(article: Article) -> PublicKnowledgeArticle {
    PublicKnowledgeArticle {
        id: article.id,
        title: article.title,
        body: article.body,
        categories: article.categories,
        keywords: article.keywords,
    }
}

fn valid_public_cursor_token(token: &str) -> bool {
    token.len() == 16 && token.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn valid_article_after_key(key: &str) -> bool {
    key == PREFIX || key.strip_prefix(PREFIX).is_some_and(valid_id)
}

async fn create_public_cursor(
    ctx: &EventContext,
    query: Option<String>,
    category: Option<String>,
    after_key: String,
    now: i64,
) -> Result<String, closure_core::module::DashboardActionError> {
    use closure_core::module::DashboardActionError;

    for _ in 0..3 {
        let sequence = PUBLIC_CURSOR_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let mut hasher = DefaultHasher::new();
        ctx.guild_id.hash(&mut hasher);
        query.hash(&mut hasher);
        category.hash(&mut hasher);
        after_key.hash(&mut hasher);
        now.hash(&mut hasher);
        sequence.hash(&mut hasher);
        let token = format!("{:016x}", hasher.finish());
        let state = PublicCursorState {
            token: token.clone(),
            query: query.clone(),
            category: category.clone(),
            after_key: after_key.clone(),
            expires_at: now.saturating_add(PUBLIC_CURSOR_TTL.as_secs().cast_signed()),
        };
        match ctx
            .store()
            .replace(
                &format!("{PUBLIC_CURSOR_PREFIX}{token}"),
                &state,
                Expect::Missing,
                Some(PUBLIC_CURSOR_TTL),
            )
            .await
        {
            Ok(_) => return Ok(token),
            Err(StorageError::Conflict) => (),
            Err(_) => {
                return Err(DashboardActionError::Internal(
                    "public knowledge could not be paged".to_owned(),
                ));
            }
        }
    }
    Err(DashboardActionError::Internal(
        "public knowledge could not be paged".to_owned(),
    ))
}

// shortcut: keep the authorized CRUD action flow together; split when another action is added.
#[allow(clippy::too_many_lines)]
pub(crate) async fn dashboard_action(
    ctx: &EventContext,
    _actor: &closure_core::permission::Actor,
    request: &closure_core::module::DashboardActionRequest,
    can_manage: bool,
) -> Result<closure_core::module::DashboardActionResult, closure_core::module::DashboardActionError>
{
    use closure_core::module::{DashboardActionError, DashboardActionResult, DashboardScope};

    if !can_manage {
        return Err(DashboardActionError::Forbidden);
    }
    if !matches!(&request.scope, DashboardScope::Knowledge { .. }) {
        return Err(DashboardActionError::Invalid(
            "invalid knowledge scope".to_owned(),
        ));
    }
    if request.idempotency_key.is_empty()
        || request.idempotency_key.len() > 64
        || !request
            .idempotency_key
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
    {
        return Err(DashboardActionError::Invalid(
            "invalid idempotency key".to_owned(),
        ));
    }
    if request.expected_revision < 0 {
        return Err(DashboardActionError::Invalid("invalid revision".to_owned()));
    }

    match request.action.as_str() {
        "knowledge.create" => {
            if request.expected_revision != 0 {
                return Err(DashboardActionError::Invalid(
                    "create requires revision 0".to_owned(),
                ));
            }
            let input: DashboardArticleInput = serde_json::from_value(request.data.clone())
                .map_err(|error| DashboardActionError::Invalid(error.to_string()))?;
            let mut article = dashboard_article(input, &request.data, None)?;
            article.created_by_idempotency_key = Some(request.idempotency_key.clone());
            let key = article_key(&article.id);
            let expect = match ctx
                .store()
                .entry(&key)
                .await
                .map_err(dashboard_storage_error)?
            {
                None => Expect::Missing,
                Some(entry) => {
                    let revision = entry.revision;
                    let current = dashboard_article_from_entry(&key, entry)?;
                    if !current.deleted {
                        if current.created_by_idempotency_key.as_deref()
                            == Some(request.idempotency_key.as_str())
                            && same_article_content(&current, &article)
                        {
                            return Ok(dashboard_article_result(&current, revision));
                        }
                        return Err(DashboardActionError::Conflict);
                    }
                    if current.created_by_idempotency_key.as_deref()
                        == Some(request.idempotency_key.as_str())
                    {
                        return Err(DashboardActionError::Conflict);
                    }
                    Expect::Revision(revision)
                }
            };
            let revision = ctx
                .store()
                .replace(&key, &article, expect, None)
                .await
                .map_err(dashboard_storage_error)?;
            Ok(dashboard_article_result(&article, revision))
        }
        "knowledge.update" => {
            let input: DashboardArticleInput = serde_json::from_value(request.data.clone())
                .map_err(|error| DashboardActionError::Invalid(error.to_string()))?;
            let key = article_key(input.id.trim());
            let Some(entry) = ctx
                .store()
                .entry(&key)
                .await
                .map_err(dashboard_storage_error)?
            else {
                return Err(DashboardActionError::NotFound);
            };
            let current_revision = entry.revision;
            let current = dashboard_article_from_entry(&key, entry)?;
            if current.deleted {
                return Err(DashboardActionError::NotFound);
            }
            let mut article = dashboard_article(input, &request.data, Some(&current))?;
            article.created_by_idempotency_key = current.created_by_idempotency_key.clone();
            article.last_update_idempotency_key = Some(request.idempotency_key.clone());
            if request.expected_revision == 0 || request.expected_revision != current_revision {
                if current.last_update_idempotency_key.as_deref()
                    == Some(request.idempotency_key.as_str())
                    && same_article_content(&current, &article)
                {
                    return Ok(dashboard_article_result(&current, current_revision));
                }
                return Err(DashboardActionError::PreconditionFailed {
                    current: current_revision,
                });
            }
            let revision = ctx
                .store()
                .replace(&key, &article, Expect::Revision(current_revision), None)
                .await
                .map_err(dashboard_conflict_error)?;
            Ok(dashboard_article_result(&article, revision))
        }
        "knowledge.delete" => {
            let input: DashboardArticleId = serde_json::from_value(request.data.clone())
                .map_err(|error| DashboardActionError::Invalid(error.to_string()))?;
            if !valid_id(&input.id) {
                return Err(DashboardActionError::Invalid(
                    "invalid article id".to_owned(),
                ));
            }
            let key = article_key(&input.id);
            let Some(entry) = ctx
                .store()
                .entry(&key)
                .await
                .map_err(dashboard_storage_error)?
            else {
                return Err(DashboardActionError::NotFound);
            };
            let current_revision = entry.revision;
            let current = dashboard_article_from_entry(&key, entry)?;
            if current.deleted {
                if current.deleted_from_revision == Some(request.expected_revision)
                    && request.expected_revision > 0
                {
                    return Ok(DashboardActionResult {
                        value: serde_json::json!({
                            "article": {
                                "id": input.id,
                                "deleted": true,
                                "revision": current_revision
                            }
                        }),
                        revision: current_revision,
                    });
                }
                return Err(DashboardActionError::NotFound);
            }
            if request.expected_revision == 0 || request.expected_revision != current_revision {
                return Err(DashboardActionError::PreconditionFailed {
                    current: current_revision,
                });
            }
            let tombstone = Article {
                id: input.id.clone(),
                title: String::new(),
                body: String::new(),
                categories: Vec::new(),
                keywords: Vec::new(),
                published: false,
                custom_embed: None,
                deleted: true,
                deleted_from_revision: Some(current_revision),
                created_by_idempotency_key: current.created_by_idempotency_key.clone(),
                last_update_idempotency_key: current.last_update_idempotency_key.clone(),
            };
            let revision = ctx
                .store()
                .replace(
                    &key,
                    &tombstone,
                    Expect::Revision(current_revision),
                    Some(TOMBSTONE_TTL),
                )
                .await
                .map_err(dashboard_conflict_error)?;
            Ok(DashboardActionResult {
                value: serde_json::json!({
                    "article": { "id": input.id, "deleted": true, "revision": revision }
                }),
                revision,
            })
        }
        _ => Err(DashboardActionError::Unsupported),
    }
}

fn dashboard_article(
    input: DashboardArticleInput,
    data: &Value,
    current: Option<&Article>,
) -> Result<Article, closure_core::module::DashboardActionError> {
    use closure_core::module::DashboardActionError;

    let categories = match (data.get("categories"), input.categories) {
        (None, _) => current.map_or_else(Vec::new, |article| article.categories.clone()),
        (Some(_), Some(values)) => values,
        (Some(_), None) => {
            return Err(DashboardActionError::Invalid(
                "invalid article categories".to_owned(),
            ));
        }
    };
    let keywords = match (data.get("keywords"), input.keywords) {
        (None, _) => current.map_or_else(Vec::new, |article| article.keywords.clone()),
        (Some(_), Some(values)) => values,
        (Some(_), None) => {
            return Err(DashboardActionError::Invalid(
                "invalid article keywords".to_owned(),
            ));
        }
    };
    let published = match (data.get("published"), input.published) {
        (None, _) => current.is_some_and(|article| article.published),
        (Some(_), Some(value)) => value,
        (Some(_), None) => {
            return Err(DashboardActionError::Invalid(
                "invalid article publication state".to_owned(),
            ));
        }
    };
    let custom_embed = match data.get("custom_embed") {
        None => current.and_then(|article| article.custom_embed.clone()),
        Some(Value::Null) => None,
        Some(value @ Value::Object(_)) => Some(value.clone()),
        Some(_) => {
            return Err(DashboardActionError::Invalid(
                "invalid article embed".to_owned(),
            ));
        }
    };
    let _ = input.custom_embed;
    let article = Article {
        id: input.id.trim().to_owned(),
        title: input.title.trim().to_owned(),
        body: input.body.trim().to_owned(),
        categories,
        keywords,
        published,
        custom_embed,
        deleted: false,
        deleted_from_revision: None,
        created_by_idempotency_key: None,
        last_update_idempotency_key: None,
    };
    if !valid_article(&article) {
        return Err(DashboardActionError::Invalid(
            "invalid knowledge article".to_owned(),
        ));
    }
    Ok(article)
}

fn dashboard_article_from_entry(
    key: &str,
    entry: Entry,
) -> Result<Article, closure_core::module::DashboardActionError> {
    use closure_core::module::DashboardActionError;

    let article: Article = serde_json::from_value(entry.value).map_err(|_| {
        DashboardActionError::Internal("stored knowledge article is invalid".to_owned())
    })?;
    if key != article_key(&article.id) || !valid_article(&article) {
        return Err(DashboardActionError::Internal(
            "stored knowledge article is invalid".to_owned(),
        ));
    }
    Ok(article)
}

fn dashboard_article_result(
    article: &Article,
    revision: i64,
) -> closure_core::module::DashboardActionResult {
    closure_core::module::DashboardActionResult {
        value: serde_json::json!({
            "article": {
                "id": article.id,
                "title": article.title,
                "body": article.body,
                "categories": article.categories,
                "keywords": article.keywords,
                "published": article.published,
                "custom_embed": article.custom_embed,
                "revision": revision
            }
        }),
        revision,
    }
}

fn same_article_content(left: &Article, right: &Article) -> bool {
    left.id == right.id
        && left.title == right.title
        && left.body == right.body
        && left.categories == right.categories
        && left.keywords == right.keywords
        && left.published == right.published
        && left.custom_embed == right.custom_embed
}

fn dashboard_storage_error(error: StorageError) -> closure_core::module::DashboardActionError {
    use closure_core::module::DashboardActionError;

    match error {
        StorageError::Conflict => DashboardActionError::Conflict,
        error => DashboardActionError::Internal(error.to_string()),
    }
}

fn dashboard_conflict_error(error: StorageError) -> closure_core::module::DashboardActionError {
    use closure_core::module::DashboardActionError;

    match error {
        StorageError::Conflict => DashboardActionError::Conflict,
        error => DashboardActionError::Internal(error.to_string()),
    }
}
#[cfg(test)]
mod tests {
    use super::{Article, matches_query, valid_article};

    #[test]
    fn knowledge_search_matches_article_content_without_case_sensitivity() {
        let article = Article {
            id: "refund-policy".to_owned(),
            title: "Returns".to_owned(),
            body: "Contact SUPPORT before shipping a return.".to_owned(),
            categories: Vec::new(),
            keywords: Vec::new(),
            published: true,
            custom_embed: None,
            deleted: false,
            deleted_from_revision: None,
            created_by_idempotency_key: None,
            last_update_idempotency_key: None,
        };
        assert!(matches_query(&article, "support"));
        assert!(valid_article(&Article {
            id: "refund-policy".to_owned(),
            title: String::new(),
            body: String::new(),
            categories: Vec::new(),
            keywords: Vec::new(),
            published: false,
            custom_embed: None,
            deleted: true,
            deleted_from_revision: Some(1),
            created_by_idempotency_key: None,
            last_update_idempotency_key: None,
        }));
    }
}

#[cfg(test)]
mod custom_embed_tests {
    use super::valid_custom_embed;

    #[test]
    fn custom_embed_rejects_fields_longer_than_discord_limits() {
        let long_title = "t".repeat(257);
        let title_over = serde_json::json!({ "title": long_title });
        assert!(!valid_custom_embed(Some(&title_over)));

        let title_at_limit = serde_json::json!({ "title": "t".repeat(256) });
        assert!(valid_custom_embed(Some(&title_at_limit)));

        let field_value_over = serde_json::json!({
            "fields": [{ "name": "Note", "value": "v".repeat(1025) }]
        });
        assert!(!valid_custom_embed(Some(&field_value_over)));
    }
}
