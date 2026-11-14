use std::time::Duration;

use closure_core::{
    module::{
        DashboardActionError, DashboardActionRequest, DashboardActionResult, DashboardScope,
        EventContext,
    },
    permission::Actor,
    storage::{Expect, StorageError},
};
use serde::{Deserialize, Serialize};
use serde_json::json;

const EVENT_PREFIX: &str = "tickets:audit:v1:e:";
const DEDUPE_PREFIX: &str = "tickets:audit:v1:d:";
const RETENTION: Duration = Duration::from_hours(2_160);
const PAGE_SIZE: usize = 25;
const MAX_PAGE_SIZE: usize = 2_500;
const MAX_PAGE: usize = 10_000;
const SCAN_BATCH: usize = 1_000;
// shortcut: searches inspect at most 10,000 retained records; revisit if a guild needs deeper history.
const MAX_QUERY_SCAN: usize = 10_000;

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum AuditAction {
    SettingsUpdate,
    PanelCreate,
    PanelUpdate,
    PanelDelete,
    PanelResend,
    MultiPanelCreate,
    MultiPanelUpdate,
    MultiPanelDelete,
    FormCreate,
    FormUpdate,
    FormDelete,
    TagCreate,
    TagDelete,
    TeamCreate,
    TeamUpdate,
    TeamDelete,
    TeamMemberAdd,
    TeamMemberRemove,
    BlacklistAdd,
    BlacklistRemove,
    TicketSendMessage,
    TicketClose,
    TicketCloseRequest,
    TicketLabelCreate,
    TicketLabelDelete,
    TicketLabelAssign,
    KnowledgeCreate,
    KnowledgeUpdate,
    KnowledgeDelete,
    SupportHoursUpdate,
    IntegrationSecretSet,
    IntegrationSecretRemove,
    TicketHistoryUpdate,
    TagUpdate,
    TagClone,
    GalleryImport,
    GalleryExport,
    TicketClaim,
    TicketUnclaim,
    TicketTransfer,
    TicketRename,
    TicketPanelChange,
    TicketWaitingUpdate,
    TicketMemberAdd,
    TicketMemberRemove,
    StaffNoteAdd,
    BlacklistRemoveUser,
    BlacklistRemoveRole,
}

impl AuditAction {
    const ALL: [Self; 48] = [
        Self::SettingsUpdate,
        Self::PanelCreate,
        Self::PanelUpdate,
        Self::PanelDelete,
        Self::PanelResend,
        Self::MultiPanelCreate,
        Self::MultiPanelUpdate,
        Self::MultiPanelDelete,
        Self::FormCreate,
        Self::FormUpdate,
        Self::FormDelete,
        Self::TagCreate,
        Self::TagDelete,
        Self::TeamCreate,
        Self::TeamUpdate,
        Self::TeamDelete,
        Self::TeamMemberAdd,
        Self::TeamMemberRemove,
        Self::BlacklistAdd,
        Self::BlacklistRemove,
        Self::TicketSendMessage,
        Self::TicketClose,
        Self::TicketCloseRequest,
        Self::TicketLabelCreate,
        Self::TicketLabelDelete,
        Self::TicketLabelAssign,
        Self::KnowledgeCreate,
        Self::KnowledgeUpdate,
        Self::KnowledgeDelete,
        Self::SupportHoursUpdate,
        Self::IntegrationSecretSet,
        Self::IntegrationSecretRemove,
        Self::TicketHistoryUpdate,
        Self::TagUpdate,
        Self::TagClone,
        Self::GalleryImport,
        Self::GalleryExport,
        Self::TicketClaim,
        Self::TicketUnclaim,
        Self::TicketTransfer,
        Self::TicketRename,
        Self::TicketPanelChange,
        Self::TicketWaitingUpdate,
        Self::TicketMemberAdd,
        Self::TicketMemberRemove,
        Self::StaffNoteAdd,
        Self::BlacklistRemoveUser,
        Self::BlacklistRemoveRole,
    ];

    fn code(self) -> u8 {
        match self {
            Self::SettingsUpdate => 1,
            Self::PanelCreate => 2,
            Self::PanelUpdate => 3,
            Self::PanelDelete => 4,
            Self::PanelResend => 5,
            Self::MultiPanelCreate => 6,
            Self::MultiPanelUpdate => 7,
            Self::MultiPanelDelete => 8,
            Self::FormCreate => 9,
            Self::FormUpdate => 10,
            Self::FormDelete => 11,
            Self::TagCreate => 12,
            Self::TagDelete => 13,
            Self::TeamCreate => 14,
            Self::TeamUpdate => 15,
            Self::TeamDelete => 16,
            Self::TeamMemberAdd => 17,
            Self::TeamMemberRemove => 18,
            Self::BlacklistAdd => 19,
            Self::BlacklistRemove => 20,
            Self::TicketSendMessage => 21,
            Self::TicketClose => 22,
            Self::TicketCloseRequest => 23,
            Self::TicketLabelCreate => 24,
            Self::TicketLabelDelete => 25,
            Self::TicketLabelAssign => 26,
            Self::KnowledgeCreate => 27,
            Self::KnowledgeUpdate => 28,
            Self::KnowledgeDelete => 29,
            Self::SupportHoursUpdate => 30,
            Self::IntegrationSecretSet => 31,
            Self::IntegrationSecretRemove => 32,
            Self::TicketHistoryUpdate => 33,
            Self::TagUpdate => 34,
            Self::TagClone => 35,
            Self::GalleryImport => 36,
            Self::TicketClaim => 42,
            Self::TicketUnclaim => 43,
            Self::TicketTransfer => 44,
            Self::TicketRename => 45,
            Self::TicketPanelChange => 52,
            Self::TicketWaitingUpdate => 53,
            Self::TicketMemberAdd => 46,
            Self::TicketMemberRemove => 47,
            Self::StaffNoteAdd => 48,
            Self::BlacklistRemoveUser => 49,
            Self::BlacklistRemoveRole => 50,
            Self::GalleryExport => 51,
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::SettingsUpdate => "settings_update",
            Self::PanelCreate => "panel_create",
            Self::PanelUpdate => "panel_update",
            Self::PanelDelete => "panel_delete",
            Self::PanelResend => "panel_resend",
            Self::MultiPanelCreate => "multi_panel_create",
            Self::MultiPanelUpdate => "multi_panel_update",
            Self::MultiPanelDelete => "multi_panel_delete",
            Self::FormCreate => "form_create",
            Self::FormUpdate => "form_update",
            Self::FormDelete => "form_delete",
            Self::TagCreate => "tag_create",
            Self::TagDelete => "tag_delete",
            Self::TeamCreate => "team_create",
            Self::TeamUpdate => "team_update",
            Self::TeamDelete => "team_delete",
            Self::TeamMemberAdd => "team_member_add",
            Self::TeamMemberRemove => "team_member_remove",
            Self::BlacklistAdd => "blacklist_add",
            Self::BlacklistRemove => "blacklist_remove",
            Self::TicketSendMessage => "ticket_send_message",
            Self::TicketClose => "ticket_close",
            Self::TicketCloseRequest => "ticket_close_request",
            Self::TicketLabelCreate => "ticket_label_create",
            Self::TicketLabelDelete => "ticket_label_delete",
            Self::TicketLabelAssign => "ticket_label_assign",
            Self::KnowledgeCreate => "knowledge_create",
            Self::KnowledgeUpdate => "knowledge_update",
            Self::KnowledgeDelete => "knowledge_delete",
            Self::SupportHoursUpdate => "support_hours_update",
            Self::IntegrationSecretSet => "integration_secret_set",
            Self::IntegrationSecretRemove => "integration_secret_remove",
            Self::TicketHistoryUpdate => "ticket_history_update",
            Self::TagUpdate => "tag_update",
            Self::TagClone => "tag_clone",
            Self::GalleryImport => "gallery_import",
            Self::GalleryExport => "gallery_export",
            Self::TicketClaim => "ticket_claim",
            Self::TicketUnclaim => "ticket_unclaim",
            Self::TicketTransfer => "ticket_transfer",
            Self::TicketRename => "ticket_rename",
            Self::TicketPanelChange => "ticket_panel_change",
            Self::TicketWaitingUpdate => "ticket_waiting_update",
            Self::TicketMemberAdd => "ticket_member_add",
            Self::TicketMemberRemove => "ticket_member_remove",
            Self::StaffNoteAdd => "staff_note_add",
            Self::BlacklistRemoveUser => "blacklist_remove_user",
            Self::BlacklistRemoveRole => "blacklist_remove_role",
        }
    }

    fn parse(value: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|action| action.as_str() == value)
    }

    fn resource(self) -> AuditResource {
        match self {
            Self::SettingsUpdate => AuditResource::Settings,
            Self::PanelCreate | Self::PanelUpdate | Self::PanelDelete | Self::PanelResend => {
                AuditResource::Panel
            }
            Self::MultiPanelCreate | Self::MultiPanelUpdate | Self::MultiPanelDelete => {
                AuditResource::MultiPanel
            }
            Self::FormCreate | Self::FormUpdate | Self::FormDelete => AuditResource::Form,
            Self::TagCreate | Self::TagUpdate | Self::TagClone | Self::TagDelete => {
                AuditResource::Tag
            }
            Self::TeamCreate
            | Self::TeamUpdate
            | Self::TeamDelete
            | Self::TeamMemberAdd
            | Self::TeamMemberRemove => AuditResource::Team,
            Self::BlacklistAdd
            | Self::BlacklistRemove
            | Self::BlacklistRemoveUser
            | Self::BlacklistRemoveRole => AuditResource::Blacklist,
            Self::TicketSendMessage | Self::TicketClose | Self::TicketCloseRequest => {
                AuditResource::Ticket
            }
            Self::TicketLabelCreate | Self::TicketLabelDelete | Self::TicketLabelAssign => {
                AuditResource::TicketLabel
            }
            Self::KnowledgeCreate | Self::KnowledgeUpdate | Self::KnowledgeDelete => {
                AuditResource::Knowledge
            }
            Self::SupportHoursUpdate => AuditResource::SupportHours,
            Self::IntegrationSecretSet | Self::IntegrationSecretRemove => {
                AuditResource::IntegrationSecret
            }
            Self::TicketHistoryUpdate => AuditResource::TicketHistory,
            Self::GalleryImport | Self::GalleryExport => AuditResource::Gallery,
            Self::TicketClaim
            | Self::TicketUnclaim
            | Self::TicketTransfer
            | Self::TicketRename
            | Self::TicketMemberAdd
            | Self::TicketMemberRemove
            | Self::TicketPanelChange
            | Self::TicketWaitingUpdate => AuditResource::Ticket,
            Self::StaffNoteAdd => AuditResource::TicketNote,
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum AuditResource {
    Settings,
    Panel,
    MultiPanel,
    Form,
    Tag,
    Team,
    Blacklist,
    Ticket,
    TicketLabel,
    Knowledge,
    SupportHours,
    IntegrationSecret,
    TicketHistory,
    Gallery,
    TicketNote,
}

impl AuditResource {
    const ALL: [Self; 15] = [
        Self::Settings,
        Self::Panel,
        Self::MultiPanel,
        Self::Form,
        Self::Tag,
        Self::Team,
        Self::Blacklist,
        Self::Ticket,
        Self::TicketLabel,
        Self::Knowledge,
        Self::SupportHours,
        Self::IntegrationSecret,
        Self::TicketHistory,
        Self::Gallery,
        Self::TicketNote,
    ];

    fn as_str(self) -> &'static str {
        match self {
            Self::Settings => "settings",
            Self::Panel => "panel",
            Self::MultiPanel => "multi_panel",
            Self::Form => "form",
            Self::Tag => "tag",
            Self::Team => "team",
            Self::Blacklist => "blacklist",
            Self::Ticket => "ticket",
            Self::TicketLabel => "ticket_label",
            Self::Knowledge => "knowledge",
            Self::SupportHours => "support_hours",
            Self::IntegrationSecret => "integration_secret",
            Self::TicketHistory => "ticket_history",
            Self::Gallery => "gallery",
            Self::TicketNote => "ticket_note",
        }
    }

    fn parse(value: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|resource| resource.as_str() == value)
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum AuditOutcome {
    Success,
    Failure,
    Uncertain,
}

impl AuditOutcome {
    const ALL: [Self; 3] = [Self::Success, Self::Failure, Self::Uncertain];

    fn as_str(self) -> &'static str {
        match self {
            Self::Success => "success",
            Self::Failure => "failure",
            Self::Uncertain => "uncertain",
        }
    }

    fn parse(value: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|outcome| outcome.as_str() == value)
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct AuditEntry {
    timestamp: i64,
    actor_id: u64,
    action: AuditAction,
    resource: AuditResource,
    outcome: AuditOutcome,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AuditQuery {
    user_id: Option<String>,
    action_type: Option<String>,
    resource_type: Option<String>,
    outcome: Option<String>,
    date_from: Option<i64>,
    date_to: Option<i64>,
    page: Option<usize>,
    page_size: Option<usize>,
}

fn event_timestamp_from_key(event_key: &str) -> Option<i64> {
    let reverse = event_key
        .strip_prefix(EVENT_PREFIX)?
        .get(..20)?
        .parse::<u64>()
        .ok()?;
    i64::try_from(u64::MAX - reverse).ok()
}

pub(crate) async fn record_event(
    ctx: &EventContext,
    actor_id: u64,
    action: AuditAction,
    outcome: AuditOutcome,
    dedupe_key: &str,
) -> Result<(), DashboardActionError> {
    if actor_id == 0
        || !(1..=64).contains(&dedupe_key.len())
        || !dedupe_key
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        return Err(DashboardActionError::Invalid(
            "invalid audit event key".into(),
        ));
    }

    let timestamp = ctx.now();
    if timestamp < 0 {
        return Err(DashboardActionError::Unavailable);
    }
    let reverse_timestamp = u64::MAX - timestamp.cast_unsigned();
    let event_key = format!(
        "{EVENT_PREFIX}{reverse_timestamp:020}:{actor_id:016x}:{:02}:{dedupe_key}",
        action.code()
    );
    let dedupe_store_key = format!(
        "{DEDUPE_PREFIX}{actor_id:016x}:{:02}:{dedupe_key}",
        action.code()
    );

    match ctx
        .store()
        .replace(
            &dedupe_store_key,
            &event_key,
            Expect::Missing,
            Some(RETENTION),
        )
        .await
    {
        Ok(_) => {}
        Err(StorageError::Conflict) => {
            let marker = ctx
                .store()
                .entry(&dedupe_store_key)
                .await
                .map_err(|_| DashboardActionError::Unavailable)?
                .ok_or(DashboardActionError::Unavailable)?;
            let existing_key: String = serde_json::from_value(marker.value)
                .map_err(|_| DashboardActionError::Unavailable)?;
            if !existing_key.starts_with(EVENT_PREFIX) {
                return Err(DashboardActionError::Unavailable);
            }
            if ctx
                .store()
                .entry(&existing_key)
                .await
                .map_err(|_| DashboardActionError::Unavailable)?
                .is_some()
            {
                return Ok(());
            }
            // The marker survived an interrupted first write; rebuild the event under the key it reserved.
            let timestamp =
                event_timestamp_from_key(&existing_key).ok_or(DashboardActionError::Unavailable)?;
            let entry = AuditEntry {
                timestamp,
                actor_id,
                action,
                resource: action.resource(),
                outcome,
            };
            return match ctx
                .store()
                .replace(&existing_key, &entry, Expect::Missing, Some(RETENTION))
                .await
            {
                Ok(_) | Err(StorageError::Conflict) => Ok(()),
                Err(_) => Err(DashboardActionError::Unavailable),
            };
        }
        Err(_) => return Err(DashboardActionError::Unavailable),
    }

    let entry = AuditEntry {
        timestamp,
        actor_id,
        action,
        resource: action.resource(),
        outcome,
    };
    match ctx
        .store()
        .replace(&event_key, &entry, Expect::Missing, Some(RETENTION))
        .await
    {
        Ok(_) | Err(StorageError::Conflict) => Ok(()),
        Err(_) => {
            let _ = ctx.store().delete(&dedupe_store_key).await;
            Err(DashboardActionError::Unavailable)
        }
    }
}

pub(crate) async fn record_dashboard_action(
    ctx: &EventContext,
    actor: &Actor,
    request: &DashboardActionRequest,
    result: &Result<DashboardActionResult, DashboardActionError>,
) -> Result<(), DashboardActionError> {
    let action = match (&request.scope, request.action.as_str()) {
        (DashboardScope::Guild | DashboardScope::Panel { .. }, "blacklist.add") => {
            Some(AuditAction::BlacklistAdd)
        }
        (DashboardScope::Guild | DashboardScope::Panel { .. }, "blacklist.remove") => {
            match request.data["target"]["kind"].as_str() {
                Some("user") => Some(AuditAction::BlacklistRemoveUser),
                Some("role") => Some(AuditAction::BlacklistRemoveRole),
                _ => Some(AuditAction::BlacklistRemove),
            }
        }
        (DashboardScope::Ticket { .. }, "ticket.close") => Some(AuditAction::TicketClose),
        (DashboardScope::Ticket { .. }, "ticket.request_close") => {
            Some(AuditAction::TicketCloseRequest)
        }
        (DashboardScope::Ticket { .. }, "ticket.reply") => Some(AuditAction::TicketSendMessage),
        (DashboardScope::Ticket { .. }, "ticket.add_label") => Some(AuditAction::TicketLabelAssign),
        (
            DashboardScope::Ticket { .. } | DashboardScope::Guild | DashboardScope::Tags { .. },
            "ticket.label.create",
        ) => Some(AuditAction::TicketLabelCreate),
        (
            DashboardScope::Ticket { .. } | DashboardScope::Guild | DashboardScope::Tags { .. },
            "ticket.label.delete",
        ) => Some(AuditAction::TicketLabelDelete),
        (DashboardScope::Ticket { .. }, "ticket.remove_label") => {
            Some(AuditAction::TicketLabelDelete)
        }
        (DashboardScope::TicketHistory { .. }, "ticket.history.update") => {
            Some(AuditAction::TicketHistoryUpdate)
        }
        (DashboardScope::Tags { .. }, "tag.create") => Some(AuditAction::TagCreate),
        (DashboardScope::Tags { .. }, "tag.update") => Some(AuditAction::TagUpdate),
        (DashboardScope::Tags { .. }, "tag.clone") => Some(AuditAction::TagClone),
        (DashboardScope::Tags { .. }, "tag.delete") => Some(AuditAction::TagDelete),
        (DashboardScope::Gallery { .. }, "gallery.import") => Some(AuditAction::GalleryImport),
        (DashboardScope::Gallery { .. }, "gallery.export") => Some(AuditAction::GalleryExport),
        (DashboardScope::Knowledge { .. }, "knowledge.create") => {
            Some(AuditAction::KnowledgeCreate)
        }
        (DashboardScope::Knowledge { .. }, "knowledge.update") => {
            Some(AuditAction::KnowledgeUpdate)
        }
        (DashboardScope::Knowledge { .. }, "knowledge.delete") => {
            Some(AuditAction::KnowledgeDelete)
        }
        (DashboardScope::IntegrationSecrets, "integration.secret.set") => {
            Some(AuditAction::IntegrationSecretSet)
        }
        (DashboardScope::IntegrationSecrets, "integration.secret.remove") => {
            Some(AuditAction::IntegrationSecretRemove)
        }
        _ => None,
    };
    let Some(action) = action else {
        return Ok(());
    };

    let outcome = match result {
        Ok(_) => AuditOutcome::Success,
        Err(
            DashboardActionError::Forbidden
            | DashboardActionError::NotFound
            | DashboardActionError::Unsupported,
        ) => return Ok(()),
        Err(DashboardActionError::Unavailable) => AuditOutcome::Uncertain,
        Err(_) => AuditOutcome::Failure,
    };
    if !(1..=64).contains(&request.idempotency_key.len())
        || !request
            .idempotency_key
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
    {
        return Ok(());
    }
    record_event(
        ctx,
        actor.user.get(),
        action,
        outcome,
        &request.idempotency_key,
    )
    .await
}

#[allow(
    clippy::too_many_lines,
    reason = "bounded audit search keeps validation, ACL checks, and pagination together"
)]
pub(crate) async fn query(
    ctx: &EventContext,
    request: &DashboardActionRequest,
    can_manage: bool,
) -> Result<DashboardActionResult, DashboardActionError> {
    if request.action != "audit.search"
        || request.expected_revision != 0
        || !matches!(request.scope, DashboardScope::Guild)
    {
        return Err(DashboardActionError::Unsupported);
    }
    if !can_manage {
        return Err(DashboardActionError::Forbidden);
    }

    let query: AuditQuery = serde_json::from_value(request.data.clone())
        .map_err(|_| DashboardActionError::Invalid("invalid audit filters".into()))?;
    let actor_id = query
        .user_id
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(|value| {
            if value.len() > 20 || !value.bytes().all(|byte| byte.is_ascii_digit()) {
                return Err(DashboardActionError::Invalid(
                    "invalid audit user ID".into(),
                ));
            }
            let id = value
                .parse::<u64>()
                .map_err(|_| DashboardActionError::Invalid("invalid audit user ID".into()))?;
            (id > 0)
                .then_some(id)
                .ok_or_else(|| DashboardActionError::Invalid("invalid audit user ID".into()))
        })
        .transpose()?;
    let action = query
        .action_type
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty() && *value != "all")
        .map(|value| {
            AuditAction::parse(value)
                .ok_or_else(|| DashboardActionError::Invalid("invalid audit action".into()))
        })
        .transpose()?;
    let resource = query
        .resource_type
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty() && *value != "all")
        .map(|value| {
            AuditResource::parse(value)
                .ok_or_else(|| DashboardActionError::Invalid("invalid audit resource".into()))
        })
        .transpose()?;
    let outcome = query
        .outcome
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty() && *value != "all")
        .map(|value| {
            AuditOutcome::parse(value)
                .ok_or_else(|| DashboardActionError::Invalid("invalid audit outcome".into()))
        })
        .transpose()?;
    if query.date_from.is_some_and(|date| date < 0)
        || query.date_to.is_some_and(|date| date < 0)
        || query
            .date_from
            .zip(query.date_to)
            .is_some_and(|(from, to)| from > to)
    {
        return Err(DashboardActionError::Invalid(
            "invalid audit date range".into(),
        ));
    }

    let page = query.page.unwrap_or(1);
    let page_size = query.page_size.unwrap_or(PAGE_SIZE);
    if !(1..=MAX_PAGE).contains(&page) || !(1..=MAX_PAGE_SIZE).contains(&page_size) {
        return Err(DashboardActionError::Invalid("invalid audit page".into()));
    }
    let offset = page
        .checked_sub(1)
        .and_then(|page| page.checked_mul(page_size))
        .ok_or_else(|| DashboardActionError::Invalid("invalid audit page".into()))?;

    let mut after = query
        .date_to
        .map(|date| format!("{EVENT_PREFIX}{:020}", u64::MAX - date.cast_unsigned()));
    let mut scanned = 0;
    let mut total = 0;
    let mut entries = Vec::with_capacity(page_size);
    let mut exhausted = false;

    while scanned < MAX_QUERY_SCAN {
        let limit = SCAN_BATCH.min(MAX_QUERY_SCAN - scanned);
        let batch = match after.as_deref() {
            Some(after) => ctx.store().list_after(EVENT_PREFIX, after, limit).await,
            None => ctx.store().list(EVENT_PREFIX, limit).await,
        }
        .map_err(|_| DashboardActionError::Unavailable)?;
        if batch.is_empty() {
            exhausted = true;
            break;
        }
        let batch_len = batch.len();
        let mut last_key = None;
        for (key, stored) in batch {
            last_key = Some(key);
            scanned += 1;
            let entry: AuditEntry = serde_json::from_value(stored.value)
                .map_err(|_| DashboardActionError::Unavailable)?;
            if query.date_from.is_some_and(|date| entry.timestamp < date) {
                exhausted = true;
                break;
            }
            if query.date_to.is_some_and(|date| entry.timestamp > date)
                || actor_id.is_some_and(|id| entry.actor_id != id)
                || action.is_some_and(|action| entry.action != action)
                || resource.is_some_and(|resource| entry.resource != resource)
                || outcome.is_some_and(|outcome| entry.outcome != outcome)
            {
                continue;
            }
            if total >= offset && entries.len() < page_size {
                entries.push(entry.clone());
            }
            total += 1;
        }
        if exhausted || batch_len < limit {
            exhausted = true;
            break;
        }
        after = last_key;
    }

    let truncated = !exhausted && scanned >= MAX_QUERY_SCAN;
    let has_more = total > offset + entries.len();
    let entries = entries
        .into_iter()
        .map(|entry| {
            json!({
                "timestamp": entry.timestamp,
                "actor_id": entry.actor_id.to_string(),
                "action": entry.action.as_str(),
                "resource": entry.resource.as_str(),
                "outcome": entry.outcome.as_str(),
            })
        })
        .collect::<Vec<_>>();
    let action_options = AuditAction::ALL
        .into_iter()
        .map(AuditAction::as_str)
        .collect::<Vec<_>>();
    let resource_options = AuditResource::ALL
        .into_iter()
        .map(AuditResource::as_str)
        .collect::<Vec<_>>();
    let outcome_options = AuditOutcome::ALL
        .into_iter()
        .map(AuditOutcome::as_str)
        .collect::<Vec<_>>();

    Ok(DashboardActionResult {
        value: json!({
            "entries": entries,
            "total": total,
            "page": page,
            "page_size": page_size,
            "has_more": has_more,
            "truncated": truncated,
            "action_options": action_options,
            "resource_options": resource_options,
            "outcome_options": outcome_options,
        }),
        revision: 0,
    })
}

#[cfg(test)]
mod event_key_tests {
    use super::{EVENT_PREFIX, event_timestamp_from_key};

    #[test]
    fn event_timestamp_round_trips_through_the_key() {
        let timestamp: i64 = 1_700_000_000;
        let reverse = u64::MAX - timestamp.cast_unsigned();
        let key = format!("{EVENT_PREFIX}{reverse:020}:0000000000000001:01:abc");
        assert_eq!(event_timestamp_from_key(&key), Some(timestamp));
        assert_eq!(event_timestamp_from_key("tickets:other:key"), None);
    }
}
