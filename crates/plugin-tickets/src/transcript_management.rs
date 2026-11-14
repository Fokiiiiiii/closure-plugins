//! Dashboard access and edits for retained closed-ticket history.
use std::collections::HashSet;
use std::time::Duration;

use closure_core::error::ModuleError;
use closure_core::event::ChannelId;
use closure_core::module::{DashboardActionError, EventContext};
use closure_core::permission::Actor;
use closure_core::storage::{Expect, MAX_KEY_CHARS, StorageError};
use closure_sdk::JobCx;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use unicode_normalization::UnicodeNormalization;

use super::{
    Config, HistoryStatus, Ticket, TicketHistory, history_id_key, history_ttl, may_view_history,
};

const LABEL_CATALOG_KEY: &str = "ticket-label-catalog:v1";
const LABEL_DEFINITION_PREFIX: &str = "ticket-label-definition:";
const LABEL_BACKFILL_KEY: &str = "ticket-label-catalog-backfill";
const LABEL_PAGE_SIZE: usize = 50;
const BACKFILL_BATCH_SIZE: usize = 50;
const BACKFILL_DELAY: Duration = Duration::from_secs(1);
const DEFAULT_LABEL_COLOR: &str = "#808080";
const LABEL_MAX_CHARS: usize = super::LABEL_MAX_CHARS;

pub(crate) const LABEL_BACKFILL_JOB: &str = "ticket-label-catalog-backfill";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
enum BackfillPhase {
    #[default]
    Tickets,
    Histories,
    Complete,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct LabelCatalog {
    #[serde(default)]
    generation: i64,
    #[serde(default)]
    phase: BackfillPhase,
    #[serde(default)]
    after_key: Option<String>,
    #[serde(default)]
    backfill_incomplete: bool,
}

impl Default for LabelCatalog {
    fn default() -> Self {
        Self {
            generation: 0,
            phase: BackfillPhase::Tickets,
            after_key: None,
            backfill_incomplete: false,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct LabelDefinition {
    normalized_name: String,
    name: String,
    color: String,
    #[serde(default)]
    deleted: bool,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct HistoryUpdate {
    reason: Option<String>,
    labels: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CreateLabel {
    name: String,
    color: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct DeleteLabel {
    name: String,
}

pub(super) async fn state(
    ctx: &EventContext,
    actor: &Actor,
    channel_id: u64,
    can_manage: bool,
) -> Result<(Value, i64), DashboardActionError> {
    let (_, history, revision) = load_history(ctx, channel_id).await?;
    if history.status != HistoryStatus::Closed {
        return Err(DashboardActionError::NotFound);
    }
    let config: Config = ctx.parse_config().map_err(super::dashboard::internal)?;
    if !may_view_history(
        actor.user.get(),
        &actor.roles,
        can_manage,
        &history,
        &config.staff_teams,
    ) {
        return Err(DashboardActionError::Forbidden);
    }
    Ok((history_projection(&history, channel_id, false), revision))
}

pub(super) async fn update(
    ctx: &EventContext,
    actor: &Actor,
    channel_id: u64,
    expected_revision: i64,
    data: &Value,
    can_manage: bool,
) -> Result<(Value, i64), DashboardActionError> {
    let mut payload: HistoryUpdate =
        serde_json::from_value(data.clone()).map_err(super::dashboard::internal)?;
    let (key, mut history, revision) = load_history(ctx, channel_id).await?;
    if history.status != HistoryStatus::Closed {
        return Err(DashboardActionError::NotFound);
    }
    let config: Config = ctx.parse_config().map_err(super::dashboard::internal)?;
    if !may_view_history(
        actor.user.get(),
        &actor.roles,
        can_manage,
        &history,
        &config.staff_teams,
    ) {
        return Err(DashboardActionError::Forbidden);
    }
    if revision != expected_revision {
        return Err(DashboardActionError::PreconditionFailed { current: revision });
    }

    let reason =
        super::dashboard::normalized_reason(payload.reason.take(), 1000)?.unwrap_or_default();
    let labels = validate_labels(ctx, &history.labels, payload.labels).await?;
    if history.reason == reason && history.labels == labels {
        return Ok((history_projection(&history, channel_id, false), revision));
    }
    history.reason = reason;
    history.labels = labels;
    let ttl = history_ttl(&history, ctx.now()).map_err(super::dashboard::internal)?;
    match ctx
        .store()
        .replace(&key, &history, Expect::Revision(revision), Some(ttl))
        .await
    {
        Ok(next) => Ok((history_projection(&history, channel_id, true), next)),
        Err(StorageError::Conflict) => {
            let current = ctx
                .store()
                .entry(&key)
                .await
                .map_err(super::dashboard::internal)?
                .map_or(0, |entry| entry.revision);
            Err(DashboardActionError::PreconditionFailed { current })
        }
        Err(error) => Err(super::dashboard::internal(error)),
    }
}

pub(super) async fn labels_state(
    ctx: &EventContext,
    actor: &Actor,
    cursor: Option<&str>,
) -> Result<(Value, i64), DashboardActionError> {
    if cursor.is_some_and(|value| !valid_label_cursor(value)) {
        return Err(DashboardActionError::Invalid("invalid label cursor".into()));
    }
    let (catalog, _) = load_catalog(ctx)
        .await
        .map_err(super::dashboard::internal)?;
    if catalog.phase != BackfillPhase::Complete {
        schedule_backfill(ctx)
            .await
            .map_err(super::dashboard::internal)?;
    }

    let entries = match cursor {
        Some(after) => {
            ctx.store()
                .list_after(LABEL_DEFINITION_PREFIX, after, LABEL_PAGE_SIZE)
                .await
        }
        None => {
            ctx.store()
                .list(LABEL_DEFINITION_PREFIX, LABEL_PAGE_SIZE)
                .await
        }
    }
    .map_err(super::dashboard::internal)?;
    let next_cursor = (entries.len() == LABEL_PAGE_SIZE)
        .then(|| entries.last().map(|(key, _)| key.clone()))
        .flatten();
    let mut labels = Vec::new();
    for (key, entry) in entries {
        let definition: LabelDefinition =
            serde_json::from_value(entry.value).map_err(super::dashboard::internal)?;
        if !valid_definition_key(&key, &definition.normalized_name) || definition.deleted {
            continue;
        }
        labels.push(json!({
            "name": definition.name,
            "color": definition.color,
        }));
    }
    let can_manage_labels = super::is_guild_owner_or_admin(ctx, actor.user, &actor.roles)
        .await
        .map_err(super::dashboard::user_error)?;
    Ok((
        json!({
            "labels": labels,
            "next_cursor": next_cursor,
            "can_manage_labels": can_manage_labels,
            "syncing": catalog.phase != BackfillPhase::Complete,
            "backfill_incomplete": catalog.backfill_incomplete,
        }),
        catalog.generation,
    ))
}

pub(super) async fn labels_action(
    ctx: &EventContext,
    actor: &Actor,
    action: &str,
    expected_revision: i64,
    data: &Value,
) -> Result<(Value, i64), DashboardActionError> {
    if !super::is_guild_owner_or_admin(ctx, actor.user, &actor.roles)
        .await
        .map_err(super::dashboard::user_error)?
    {
        return Err(DashboardActionError::Forbidden);
    }
    let (name, normalized, color, deleting) = match action {
        "ticket.label.create" => {
            let payload: CreateLabel =
                serde_json::from_value(data.clone()).map_err(super::dashboard::internal)?;
            let Some((name, normalized)) = normalized_label(&payload.name) else {
                return Err(DashboardActionError::Invalid("invalid label name".into()));
            };
            let Some(color) = normalized_color(&payload.color) else {
                return Err(DashboardActionError::Invalid("invalid label color".into()));
            };
            (name, normalized, color, false)
        }
        "ticket.label.delete" => {
            let payload: DeleteLabel =
                serde_json::from_value(data.clone()).map_err(super::dashboard::internal)?;
            let Some((name, normalized)) = normalized_label(&payload.name) else {
                return Err(DashboardActionError::Invalid("invalid label name".into()));
            };
            (name, normalized, DEFAULT_LABEL_COLOR.to_owned(), true)
        }
        _ => return Err(DashboardActionError::Unsupported),
    };
    let key = label_definition_key(&normalized);
    let (catalog, _) = load_catalog(ctx)
        .await
        .map_err(super::dashboard::internal)?;
    let (current, current_revision) = load_definition(ctx, &key, &normalized)
        .await
        .map_err(map_label_error)?;
    let desired = if deleting {
        current.as_ref().is_some_and(|label| label.deleted)
    } else {
        current
            .as_ref()
            .is_some_and(|label| !label.deleted && label.name == name && label.color == color)
    };
    if desired {
        return Ok((json!({"changed": false, "name": name}), catalog.generation));
    }
    if catalog.generation != expected_revision {
        return Err(DashboardActionError::PreconditionFailed {
            current: catalog.generation,
        });
    }

    let generation = bump_catalog(ctx, expected_revision).await?;
    let definition = LabelDefinition {
        normalized_name: normalized,
        name,
        color,
        deleted: deleting,
    };
    let write = match current_revision {
        Some(revision) => {
            ctx.store()
                .replace(&key, &definition, Expect::Revision(revision), None)
                .await
        }
        None => {
            ctx.store()
                .replace(&key, &definition, Expect::Missing, None)
                .await
        }
    };
    match write {
        Ok(_) => Ok((
            json!({"changed": true, "name": definition.name}),
            generation,
        )),
        Err(StorageError::Conflict) => Err(DashboardActionError::Conflict),
        Err(error) => Err(super::dashboard::internal(error)),
    }
}

pub(super) async fn register_label(ctx: &EventContext, value: &str) -> Result<(), ModuleError> {
    let Some((name, normalized)) = normalized_label(value) else {
        return Err(ModuleError::other("invalid ticket label"));
    };
    let key = label_definition_key(&normalized);
    let (current, current_revision) = load_definition(ctx, &key, &normalized).await?;
    if current.as_ref().is_some_and(|label| !label.deleted) {
        return Ok(());
    }
    bump_catalog_module(ctx).await?;
    let definition = LabelDefinition {
        normalized_name: normalized,
        name,
        color: DEFAULT_LABEL_COLOR.to_owned(),
        deleted: false,
    };
    let write = match current_revision {
        Some(revision) => {
            ctx.store()
                .replace(&key, &definition, Expect::Revision(revision), None)
                .await
        }
        None => {
            ctx.store()
                .replace(&key, &definition, Expect::Missing, None)
                .await
        }
    };
    match write {
        Ok(_) => Ok(()),
        Err(StorageError::Conflict) => {
            let (latest, _) = load_definition(ctx, &key, &definition.normalized_name).await?;
            if latest.as_ref().is_some_and(|label| !label.deleted) {
                Ok(())
            } else {
                Err(ModuleError::other("ticket label changed concurrently"))
            }
        }
        Err(error) => Err(super::stored(&error)),
    }
}

pub(crate) async fn backfill_job(cx: JobCx) -> Result<(), ModuleError> {
    let _: () = cx.payload()?;
    let ctx = cx.ctx();
    let (catalog, _) = load_catalog(ctx).await?;
    if catalog.phase == BackfillPhase::Complete {
        return Ok(());
    }
    let prefix = match catalog.phase {
        BackfillPhase::Tickets => "ticket:",
        BackfillPhase::Histories => "ticket-history:",
        BackfillPhase::Complete => return Ok(()),
    };
    let entries = match catalog.after_key.as_deref() {
        Some(after) => {
            ctx.store()
                .list_after(prefix, after, BACKFILL_BATCH_SIZE)
                .await
        }
        None => ctx.store().list(prefix, BACKFILL_BATCH_SIZE).await,
    }
    .map_err(|error| super::stored(&error))?;

    let mut incomplete = catalog.backfill_incomplete;
    for (_, entry) in &entries {
        match catalog.phase {
            BackfillPhase::Tickets => match serde_json::from_value::<Ticket>(entry.value.clone()) {
                Ok(ticket) => {
                    for label in &ticket.labels {
                        if import_label(ctx, label).await.is_err() {
                            incomplete = true;
                        }
                    }
                }
                Err(_) => incomplete = true,
            },
            BackfillPhase::Histories => {
                match serde_json::from_value::<TicketHistory>(entry.value.clone()) {
                    Ok(history) if history.status == HistoryStatus::Closed => {
                        for label in &history.labels {
                            if import_label(ctx, label).await.is_err() {
                                incomplete = true;
                            }
                        }
                    }
                    Ok(_) => {}
                    Err(_) => incomplete = true,
                }
            }
            BackfillPhase::Complete => break,
        }
    }

    let (mut latest, revision) = load_catalog(ctx).await?;
    if latest.phase != catalog.phase || latest.after_key != catalog.after_key {
        schedule_backfill(ctx).await?;
        return Ok(());
    }
    let mut phase = catalog.phase;
    let exhausted = entries.len() < BACKFILL_BATCH_SIZE;
    let after_key = entries.last().map(|(key, _)| key.clone());
    if exhausted {
        phase = match phase {
            BackfillPhase::Tickets => BackfillPhase::Histories,
            BackfillPhase::Histories | BackfillPhase::Complete => BackfillPhase::Complete,
        };
    }
    latest.phase = phase;
    latest.after_key = if exhausted { None } else { after_key };
    latest.backfill_incomplete |= incomplete;
    match ctx
        .store()
        .replace(LABEL_CATALOG_KEY, &latest, Expect::Revision(revision), None)
        .await
    {
        Ok(_) => {
            if latest.phase != BackfillPhase::Complete {
                schedule_backfill(ctx).await?;
            }
            Ok(())
        }
        Err(StorageError::Conflict) => {
            schedule_backfill(ctx).await?;
            Ok(())
        }
        Err(error) => Err(super::stored(&error)),
    }
}

async fn load_history(
    ctx: &EventContext,
    channel_id: u64,
) -> Result<(String, TicketHistory, i64), DashboardActionError> {
    let channel = ChannelId::new(channel_id);
    let key: Option<String> = ctx
        .store()
        .get(&history_id_key(channel))
        .await
        .map_err(super::dashboard::internal)?;
    let key = key.ok_or(DashboardActionError::NotFound)?;
    let entry = ctx
        .store()
        .entry(&key)
        .await
        .map_err(super::dashboard::internal)?
        .ok_or(DashboardActionError::NotFound)?;
    let history: TicketHistory =
        serde_json::from_value(entry.value).map_err(super::dashboard::internal)?;
    if history.channel_id != channel_id {
        return Err(DashboardActionError::NotFound);
    }
    Ok((key, history, entry.revision))
}

fn history_projection(history: &TicketHistory, channel_id: u64, changed: bool) -> Value {
    let panel = history.panel.as_ref().map(|panel| {
        json!({
            "id": panel.id,
            "name": panel.name.as_deref().unwrap_or(&panel.id),
        })
    });
    json!({
        "channel_id": channel_id.to_string(),
        "number": history.number,
        "opener": if history.opener_erased || history.opener == 0 {
            None::<String>
        } else {
            Some(history.opener.to_string())
        },
        "claimed_by": history.claimed_by.map(|user| user.to_string()),
        "panel": panel,
        "created": history.created,
        "closed": history.closed,
        "closer": history.closer.map(|user| user.to_string()),
        "reason": history.reason,
        "labels": history.labels,
        "rating": history.rating.as_ref().map(|rating| json!({
            "score": rating.score,
            "created": rating.created,
        })),
        "has_transcript": !history.opener_erased
            && history.transcript_pages > 0
            && history.transcript_char_count > 0,
        "changed": changed,
    })
}

async fn validate_labels(
    ctx: &EventContext,
    current: &[String],
    requested: Vec<String>,
) -> Result<Vec<String>, DashboardActionError> {
    let current: HashSet<String> = current
        .iter()
        .filter_map(|label| normalized_label(label).map(|(_, key)| key))
        .collect();
    let mut labels = Vec::with_capacity(requested.len());
    for raw in requested {
        let Some((_, normalized)) = normalized_label(&raw) else {
            return Err(DashboardActionError::Invalid("invalid ticket label".into()));
        };
        if current.contains(&normalized) {
            labels.push(
                super::display_label(&raw)
                    .ok_or_else(|| DashboardActionError::Invalid("invalid ticket label".into()))?,
            );
            continue;
        }
        let key = label_definition_key(&normalized);
        let (definition, _) = load_definition(ctx, &key, &normalized)
            .await
            .map_err(map_label_error)?;
        if let Some(definition) = definition.filter(|label| !label.deleted) {
            labels.push(definition.name);
        } else {
            return Err(DashboardActionError::Invalid("unknown ticket label".into()));
        }
    }
    Ok(labels)
}

fn normalized_label(value: &str) -> Option<(String, String)> {
    let display = super::display_label(value)?.nfc().collect::<String>();
    if display.is_empty()
        || display.chars().count() > LABEL_MAX_CHARS
        || display.chars().any(char::is_control)
    {
        return None;
    }
    let normalized = display
        .chars()
        .flat_map(char::to_lowercase)
        .collect::<String>()
        .nfc()
        .collect::<String>();
    (!normalized.is_empty()).then_some((display, normalized))
}

fn normalized_color(value: &str) -> Option<String> {
    let color = value.strip_prefix('#')?;
    (color.len() == 6 && color.bytes().all(|byte| byte.is_ascii_hexdigit()))
        .then(|| format!("#{}", color.to_ascii_lowercase()))
}

fn label_definition_key(normalized: &str) -> String {
    const FNV_OFFSET: u128 = 0x6c62_272e_07bb_0142_62b8_2175_6295_c58d;
    const FNV_PRIME: u128 = 0x0000_0000_0100_0000_0000_0000_0000_013b;
    let mut hash = FNV_OFFSET;
    for byte in normalized.bytes() {
        hash ^= u128::from(byte);
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    format!("{LABEL_DEFINITION_PREFIX}{hash:032x}")
}

fn valid_label_cursor(value: &str) -> bool {
    value.len() == LABEL_DEFINITION_PREFIX.len() + 32
        && value.starts_with(LABEL_DEFINITION_PREFIX)
        && value[LABEL_DEFINITION_PREFIX.len()..]
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn valid_definition_key(key: &str, normalized: &str) -> bool {
    key.len() <= MAX_KEY_CHARS && key == label_definition_key(normalized) && valid_label_cursor(key)
}

async fn load_definition(
    ctx: &EventContext,
    key: &str,
    normalized: &str,
) -> Result<(Option<LabelDefinition>, Option<i64>), ModuleError> {
    let Some(entry) = ctx
        .store()
        .entry(key)
        .await
        .map_err(|error| super::stored(&error))?
    else {
        return Ok((None, None));
    };
    let definition: LabelDefinition = serde_json::from_value(entry.value)
        .map_err(|error| ModuleError::other(error.to_string()))?;
    if definition.normalized_name != normalized
        || !valid_definition_key(key, &definition.normalized_name)
    {
        return Err(ModuleError::other("ticket label key collision"));
    }
    Ok((Some(definition), Some(entry.revision)))
}

async fn load_catalog(ctx: &EventContext) -> Result<(LabelCatalog, i64), ModuleError> {
    for _ in 0..5 {
        if let Some(entry) = ctx
            .store()
            .entry(LABEL_CATALOG_KEY)
            .await
            .map_err(|error| super::stored(&error))?
        {
            let catalog: LabelCatalog = serde_json::from_value(entry.value)
                .map_err(|error| ModuleError::other(error.to_string()))?;
            if catalog.generation < 0 {
                return Err(ModuleError::other("ticket label generation is invalid"));
            }
            return Ok((catalog, entry.revision));
        }
        let catalog = LabelCatalog::default();
        match ctx
            .store()
            .replace(LABEL_CATALOG_KEY, &catalog, Expect::Missing, None)
            .await
        {
            Ok(revision) => return Ok((catalog, revision)),
            Err(StorageError::Conflict) => {}
            Err(error) => return Err(super::stored(&error)),
        }
    }
    Err(ModuleError::other(
        "ticket label catalog changed repeatedly",
    ))
}

async fn bump_catalog(
    ctx: &EventContext,
    expected_generation: i64,
) -> Result<i64, DashboardActionError> {
    for _ in 0..5 {
        let (mut catalog, revision) = load_catalog(ctx)
            .await
            .map_err(super::dashboard::internal)?;
        if catalog.generation != expected_generation {
            return Err(DashboardActionError::PreconditionFailed {
                current: catalog.generation,
            });
        }
        catalog.generation = catalog
            .generation
            .checked_add(1)
            .ok_or(DashboardActionError::Unavailable)?;
        match ctx
            .store()
            .replace(
                LABEL_CATALOG_KEY,
                &catalog,
                Expect::Revision(revision),
                None,
            )
            .await
        {
            Ok(_) => return Ok(catalog.generation),
            Err(StorageError::Conflict) => {}
            Err(error) => return Err(super::dashboard::internal(error)),
        }
    }
    let (catalog, _) = load_catalog(ctx)
        .await
        .map_err(super::dashboard::internal)?;
    Err(DashboardActionError::PreconditionFailed {
        current: catalog.generation,
    })
}

async fn bump_catalog_module(ctx: &EventContext) -> Result<(), ModuleError> {
    for _ in 0..5 {
        let (mut catalog, revision) = load_catalog(ctx).await?;
        catalog.generation = catalog
            .generation
            .checked_add(1)
            .ok_or_else(|| ModuleError::other("ticket label generation is exhausted"))?;
        match ctx
            .store()
            .replace(
                LABEL_CATALOG_KEY,
                &catalog,
                Expect::Revision(revision),
                None,
            )
            .await
        {
            Ok(_) => return Ok(()),
            Err(StorageError::Conflict) => {}
            Err(error) => return Err(super::stored(&error)),
        }
    }
    Err(ModuleError::other(
        "ticket label catalog changed repeatedly",
    ))
}

async fn import_label(ctx: &EventContext, raw: &str) -> Result<(), ModuleError> {
    let Some((name, normalized)) = normalized_label(raw) else {
        return Err(ModuleError::other("invalid legacy ticket label"));
    };
    let key = label_definition_key(&normalized);
    let (current, _) = load_definition(ctx, &key, &normalized).await?;
    if current.is_some() {
        return Ok(());
    }
    let definition = LabelDefinition {
        normalized_name: normalized,
        name,
        color: DEFAULT_LABEL_COLOR.to_owned(),
        deleted: false,
    };
    match ctx
        .store()
        .replace(&key, &definition, Expect::Missing, None)
        .await
    {
        Ok(_) => Ok(()),
        Err(StorageError::Conflict) => {
            let (latest, _) = load_definition(ctx, &key, &definition.normalized_name).await?;
            if latest.is_some() {
                Ok(())
            } else {
                Err(ModuleError::other("ticket label changed concurrently"))
            }
        }
        Err(error) => Err(super::stored(&error)),
    }
}

async fn schedule_backfill(ctx: &EventContext) -> Result<(), ModuleError> {
    ctx.scheduler()
        .job(LABEL_BACKFILL_JOB, &())
        .map_err(|error| ModuleError::other(error.to_string()))?
        .key(LABEL_BACKFILL_KEY)
        .if_absent()
        .after(BACKFILL_DELAY)
        .await
        .map_err(|error| ModuleError::other(error.to_string()))
}

fn map_label_error(error: ModuleError) -> DashboardActionError {
    if error.to_string() == "ticket label key collision" {
        DashboardActionError::Conflict
    } else {
        super::dashboard::internal(error)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unicode_equivalents_share_a_bounded_key() {
        let composed = normalized_label("é").map(|(_, key)| key);
        let decomposed = normalized_label("e\u{301}").map(|(_, key)| key);
        assert_eq!(composed, decomposed);
        let key = composed.unwrap_or_default();
        let key = label_definition_key(&key);
        assert!(key.len() <= MAX_KEY_CHARS);
        assert!(valid_label_cursor(&key));
    }
}
