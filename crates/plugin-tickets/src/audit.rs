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
use serde_json::{Value, json};

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
    // Absent in events written before these identifiers were recorded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    target_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    request_id: Option<String>,
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

const MAX_TARGET_ID_BYTES: usize = 64;

pub(crate) async fn record_event(
    ctx: &EventContext,
    actor_id: u64,
    action: AuditAction,
    outcome: AuditOutcome,
    dedupe_key: &str,
) -> Result<(), DashboardActionError> {
    record_event_for(ctx, actor_id, action, outcome, dedupe_key, None).await
}

async fn reserved_event(
    ctx: &EventContext,
    dedupe_store_key: &str,
    outcome: AuditOutcome,
    build: impl Fn(i64) -> AuditEntry,
) -> Result<(), DashboardActionError> {
    let marker = ctx
        .store()
        .entry(dedupe_store_key)
        .await
        .map_err(|_| DashboardActionError::Unavailable)?
        .ok_or(DashboardActionError::Unavailable)?;
    let existing_key: String =
        serde_json::from_value(marker.value).map_err(|_| DashboardActionError::Unavailable)?;
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
        return record_outcome(ctx, &existing_key, outcome).await;
    }
    // The marker survived an interrupted first write; rebuild the event under the key it reserved.
    let timestamp =
        event_timestamp_from_key(&existing_key).ok_or(DashboardActionError::Unavailable)?;
    return match ctx
        .store()
        .replace(
            &existing_key,
            &build(timestamp),
            Expect::Missing,
            Some(RETENTION),
        )
        .await
    {
        Ok(_) => Ok(()),
        Err(StorageError::Conflict) => record_outcome(ctx, &existing_key, outcome).await,
        Err(_) => Err(DashboardActionError::Unavailable),
    };
}

pub(crate) async fn record_event_for(
    ctx: &EventContext,
    actor_id: u64,
    action: AuditAction,
    outcome: AuditOutcome,
    dedupe_key: &str,
    target_id: Option<String>,
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

    // A target that fails the same check as DashboardScope targets is stored as absent.
    let target_id = target_id.filter(|value| valid_target_id(value));
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
    let build = |timestamp: i64| AuditEntry {
        timestamp,
        actor_id,
        action,
        resource: action.resource(),
        outcome,
        target_id: target_id.clone(),
        request_id: Some(dedupe_key.to_owned()),
    };

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
            return reserved_event(ctx, &dedupe_store_key, outcome, &build).await;
        }
        Err(_) => return Err(DashboardActionError::Unavailable),
    }

    match ctx
        .store()
        .replace(
            &event_key,
            &build(timestamp),
            Expect::Missing,
            Some(RETENTION),
        )
        .await
    {
        Ok(_) => Ok(()),
        Err(StorageError::Conflict) => record_outcome(ctx, &event_key, outcome).await,
        Err(_) => {
            let _ = ctx.store().delete(&dedupe_store_key).await;
            Err(DashboardActionError::Unavailable)
        }
    }
}

// A retry that succeeds replaces the earlier failure it duplicates; the event key keeps its timestamp.
async fn record_outcome(
    ctx: &EventContext,
    event_key: &str,
    outcome: AuditOutcome,
) -> Result<(), DashboardActionError> {
    let stored = ctx
        .store()
        .entry(event_key)
        .await
        .map_err(|_| DashboardActionError::Unavailable)?
        .ok_or(DashboardActionError::Unavailable)?;
    let existing: AuditEntry =
        serde_json::from_value(stored.value).map_err(|_| DashboardActionError::Unavailable)?;
    let Some(updated) = outcome_update(&existing, outcome) else {
        return Ok(());
    };
    ctx.store()
        .replace(event_key, &updated, Expect::Any, Some(RETENTION))
        .await
        .map(|_| ())
        .map_err(|_| DashboardActionError::Unavailable)
}

// None when the stored outcome already stands; a recorded success is never replaced by a later non-success.
fn outcome_update(existing: &AuditEntry, outcome: AuditOutcome) -> Option<AuditEntry> {
    let standing = existing.outcome == outcome
        || (existing.outcome == AuditOutcome::Success && outcome != AuditOutcome::Success);
    (!standing).then(|| AuditEntry {
        outcome,
        ..existing.clone()
    })
}

// The object the action touched; secret values and message bodies are never read here.
fn target_id(scope: &DashboardScope, data: &Value) -> Option<String> {
    let raw = match scope {
        DashboardScope::Ticket { channel_id } | DashboardScope::TicketHistory { channel_id } => {
            Some(channel_id.to_string())
        }
        DashboardScope::TicketLabels { .. } => data["name"].as_str().map(str::to_owned),
        DashboardScope::IntegrationSecrets => data["integration_id"].as_str().map(str::to_owned),
        DashboardScope::Knowledge { .. } | DashboardScope::Tags { .. } => {
            data["id"].as_str().map(str::to_owned)
        }
        DashboardScope::Guild | DashboardScope::Panel { .. } => {
            data["target"]["id"].as_str().map(str::to_owned)
        }
        _ => None,
    };
    raw.map(|value| value.trim().to_owned())
        .filter(|value| valid_target_id(value))
}

fn valid_target_id(value: &str) -> bool {
    (1..=MAX_TARGET_ID_BYTES).contains(&value.len()) && !value.chars().any(char::is_control)
}

fn entry_json(entry: &AuditEntry) -> Value {
    json!({
        "timestamp": entry.timestamp,
        "actor_id": entry.actor_id.to_string(),
        "action": entry.action.as_str(),
        "resource": entry.resource.as_str(),
        "outcome": entry.outcome.as_str(),
        "target_id": entry.target_id.as_deref(),
        "request_id": entry.request_id.as_deref(),
    })
}

fn audit_action_for(scope: &DashboardScope, action: &str, data: &Value) -> Option<AuditAction> {
    match (scope, action) {
        (DashboardScope::Guild | DashboardScope::Panel { .. }, "blacklist.add") => {
            Some(AuditAction::BlacklistAdd)
        }
        (DashboardScope::Guild | DashboardScope::Panel { .. }, "blacklist.remove") => {
            match data["target"]["kind"].as_str() {
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
        (DashboardScope::TicketLabels { .. }, "ticket.label.create") => {
            Some(AuditAction::TicketLabelCreate)
        }
        (DashboardScope::TicketLabels { .. }, "ticket.label.delete")
        | (DashboardScope::Ticket { .. }, "ticket.remove_label") => {
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
    }
}

pub(crate) async fn record_dashboard_action(
    ctx: &EventContext,
    actor: &Actor,
    request: &DashboardActionRequest,
    result: &Result<DashboardActionResult, DashboardActionError>,
) -> Result<(), DashboardActionError> {
    let Some(action) = audit_action_for(&request.scope, &request.action, &request.data) else {
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
    record_event_for(
        ctx,
        actor.user.get(),
        action,
        outcome,
        &request.idempotency_key,
        target_id(&request.scope, &request.data),
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
        .map(|entry| entry_json(&entry))
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

#[cfg(test)]
mod mapping_tests {
    use std::collections::BTreeSet;

    use closure_core::module::DashboardScope;
    use serde_json::json;

    use super::{AuditAction, audit_action_for};

    // Every action registered in lib.rs. A new public action must be classified in `expected`.
    const PUBLIC_ACTIONS: [&str; 22] = [
        "blacklist.add",
        "blacklist.remove",
        "ticket.close",
        "ticket.request_close",
        "ticket.reply",
        "ticket.add_label",
        "ticket.remove_label",
        "knowledge.create",
        "knowledge.update",
        "knowledge.delete",
        "integration.secret.set",
        "integration.secret.remove",
        "tag.create",
        "tag.update",
        "tag.clone",
        "tag.delete",
        "gallery.import",
        "gallery.export",
        "audit.search",
        "ticket.history.update",
        "ticket.label.create",
        "ticket.label.delete",
    ];

    fn scopes() -> Vec<DashboardScope> {
        vec![
            DashboardScope::Guild,
            DashboardScope::PanelList,
            DashboardScope::Panel {
                panel_id: "panel".into(),
            },
            DashboardScope::Ticket { channel_id: 1 },
            DashboardScope::TicketHistory { channel_id: 1 },
            DashboardScope::TicketLabels { cursor: None },
            DashboardScope::Gallery {
                cursor: None,
                query: None,
            },
            DashboardScope::Knowledge {
                query: None,
                cursor: None,
            },
            DashboardScope::Tags {
                query: None,
                cursor: None,
            },
            DashboardScope::Analytics {
                period_days: 7,
                cursor: None,
            },
            DashboardScope::IntegrationSecrets,
        ]
    }

    // No wildcard arm: a new scope in closure_core fails to compile here until it is listed.
    fn scope_kind(scope: &DashboardScope) -> &'static str {
        match scope {
            DashboardScope::Guild => "guild",
            DashboardScope::PanelList => "panel_list",
            DashboardScope::Panel { .. } => "panel",
            DashboardScope::Ticket { .. } => "ticket",
            DashboardScope::TicketHistory { .. } => "ticket_history",
            DashboardScope::TicketLabels { .. } => "ticket_labels",
            DashboardScope::Gallery { .. } => "gallery",
            DashboardScope::Knowledge { .. } => "knowledge",
            DashboardScope::Tags { .. } => "tags",
            DashboardScope::Analytics { .. } => "analytics",
            DashboardScope::IntegrationSecrets => "integration_secrets",
        }
    }

    // The audit spec: what each (action, scope) pair records, None when it is not audited.
    fn expected(action: &str, kind: &str) -> Option<AuditAction> {
        Some(match (action, kind) {
            ("blacklist.add", "guild" | "panel") => AuditAction::BlacklistAdd,
            ("blacklist.remove", "guild" | "panel") => AuditAction::BlacklistRemove,
            ("ticket.close", "ticket") => AuditAction::TicketClose,
            ("ticket.request_close", "ticket") => AuditAction::TicketCloseRequest,
            ("ticket.reply", "ticket") => AuditAction::TicketSendMessage,
            ("ticket.add_label", "ticket") => AuditAction::TicketLabelAssign,
            ("ticket.label.create", "ticket_labels") => AuditAction::TicketLabelCreate,
            ("ticket.label.delete", "ticket_labels") | ("ticket.remove_label", "ticket") => {
                AuditAction::TicketLabelDelete
            }
            ("ticket.history.update", "ticket_history") => AuditAction::TicketHistoryUpdate,
            ("tag.create", "tags") => AuditAction::TagCreate,
            ("tag.update", "tags") => AuditAction::TagUpdate,
            ("tag.clone", "tags") => AuditAction::TagClone,
            ("tag.delete", "tags") => AuditAction::TagDelete,
            ("gallery.import", "gallery") => AuditAction::GalleryImport,
            ("gallery.export", "gallery") => AuditAction::GalleryExport,
            ("knowledge.create", "knowledge") => AuditAction::KnowledgeCreate,
            ("knowledge.update", "knowledge") => AuditAction::KnowledgeUpdate,
            ("knowledge.delete", "knowledge") => AuditAction::KnowledgeDelete,
            ("integration.secret.set", "integration_secrets") => AuditAction::IntegrationSecretSet,
            ("integration.secret.remove", "integration_secrets") => {
                AuditAction::IntegrationSecretRemove
            }
            _ => return None,
        })
    }

    #[test]
    fn ticket_label_actions_on_the_label_registry_scope_are_audited() {
        let scope = DashboardScope::TicketLabels { cursor: None };
        let data = json!({"name": "urgent", "color": "red"});

        assert_eq!(
            audit_action_for(&scope, "ticket.label.create", &data),
            Some(AuditAction::TicketLabelCreate)
        );
        assert_eq!(
            audit_action_for(&scope, "ticket.label.delete", &json!({"name": "urgent"})),
            Some(AuditAction::TicketLabelDelete)
        );
    }

    #[test]
    fn every_public_action_is_audited_exactly_as_specified_for_each_scope() {
        for action in PUBLIC_ACTIONS {
            for scope in scopes() {
                let kind = scope_kind(&scope);
                assert_eq!(
                    audit_action_for(&scope, action, &json!({})),
                    expected(action, kind),
                    "action {action} on scope {kind}"
                );
            }
        }
    }

    #[test]
    fn scope_list_covers_every_scope_and_every_recorded_action_is_classified() {
        let kinds: BTreeSet<&str> = scopes().iter().map(scope_kind).collect();
        assert_eq!(kinds.len(), 11, "scopes() must list every DashboardScope");

        for action in PUBLIC_ACTIONS
            .into_iter()
            .filter(|action| *action != "audit.search")
        {
            assert!(
                scopes()
                    .iter()
                    .any(|scope| expected(action, scope_kind(scope)).is_some()),
                "action {action} is not classified as audited on any scope"
            );
        }
    }
}

#[cfg(test)]
mod record_tests {
    use closure_core::module::DashboardScope;
    use serde_json::json;

    use super::{
        AuditAction, AuditEntry, AuditOutcome, AuditResource, entry_json, outcome_update, target_id,
    };

    fn entry(outcome: AuditOutcome) -> AuditEntry {
        AuditEntry {
            timestamp: 1_700_000_000,
            actor_id: 7,
            action: AuditAction::TicketClose,
            resource: AuditResource::Ticket,
            outcome,
            target_id: Some("42".into()),
            request_id: Some("key-1".into()),
        }
    }

    #[test]
    fn a_failed_attempt_reports_success_once_the_retry_succeeds() {
        let stored = entry(AuditOutcome::Failure);

        let updated = outcome_update(&stored, AuditOutcome::Success)
            .expect("a successful retry must replace the recorded failure");

        assert_eq!(updated.outcome, AuditOutcome::Success);
        assert_eq!(updated.timestamp, stored.timestamp);
        assert_eq!(updated.target_id, stored.target_id);
    }

    #[test]
    fn a_recorded_success_is_not_downgraded_by_a_duplicate() {
        assert!(outcome_update(&entry(AuditOutcome::Success), AuditOutcome::Failure).is_none());
        assert!(outcome_update(&entry(AuditOutcome::Success), AuditOutcome::Uncertain).is_none());
    }

    #[test]
    fn an_unchanged_outcome_needs_no_write() {
        assert!(outcome_update(&entry(AuditOutcome::Failure), AuditOutcome::Failure).is_none());
    }

    #[test]
    fn target_id_names_the_object_without_secret_values_or_bodies() {
        assert_eq!(
            target_id(&DashboardScope::Ticket { channel_id: 42 }, &json!({})),
            Some("42".into())
        );
        assert_eq!(
            target_id(
                &DashboardScope::Guild,
                &json!({"target": {"kind": "role", "id": "900"}})
            ),
            Some("900".into())
        );
        assert_eq!(
            target_id(
                &DashboardScope::Knowledge {
                    query: None,
                    cursor: None
                },
                &json!({"id": "faq-1", "body": "private"})
            ),
            Some("faq-1".into())
        );
        assert_eq!(
            target_id(
                &DashboardScope::TicketLabels { cursor: None },
                &json!({"name": "urgent", "color": "red"})
            ),
            Some("urgent".into())
        );
        assert_eq!(
            target_id(
                &DashboardScope::IntegrationSecrets,
                &json!({"integration_id": "github", "generation": "1", "secret_name": "token", "value": "hunter2"})
            ),
            Some("github".into())
        );
    }

    #[test]
    fn target_id_rejects_missing_empty_oversized_and_control_values() {
        let scope = DashboardScope::Knowledge {
            query: None,
            cursor: None,
        };

        assert_eq!(target_id(&scope, &json!({})), None);
        assert_eq!(target_id(&scope, &json!({"id": "  "})), None);
        assert_eq!(target_id(&scope, &json!({"id": "x".repeat(65)})), None);
        assert_eq!(target_id(&scope, &json!({"id": "a\nb"})), None);
    }

    #[test]
    fn events_written_before_targets_were_recorded_still_read() {
        let legacy = json!({
            "timestamp": 1_700_000_000,
            "actor_id": 7,
            "action": "ticket_close",
            "resource": "ticket",
            "outcome": "success"
        });

        let parsed: AuditEntry = serde_json::from_value(legacy).expect("legacy event must read");

        assert_eq!(parsed.target_id, None);
        assert_eq!(parsed.request_id, None);
    }

    #[test]
    fn query_rows_carry_target_and_request_identifiers() {
        let row = entry_json(&entry(AuditOutcome::Success));
        assert_eq!(row["target_id"], "42");
        assert_eq!(row["request_id"], "key-1");

        let bare = AuditEntry {
            target_id: None,
            request_id: None,
            ..entry(AuditOutcome::Success)
        };
        assert!(entry_json(&bare)["target_id"].is_null());
    }

    #[test]
    fn stored_entries_omit_absent_identifiers() {
        let bare = AuditEntry {
            target_id: None,
            request_id: None,
            ..entry(AuditOutcome::Success)
        };

        let value = serde_json::to_value(bare).expect("entry must serialize");

        assert!(value.get("target_id").is_none());
        assert!(value.get("request_id").is_none());
    }
}
